//! The job scheduler.
//!
//! A background task that periodically places `queued` jobs onto eligible hosts.
//! It coordinates with the per-host [`SupervisorWSWorker`] **entirely through the
//! database** — it never holds an in-process handle to a worker — so the two can
//! be distributed across processes later. The scheduler writes the assignment
//! (`hosts.current_job` + `jobs.job_state = 'assigned'`); the host's own worker
//! observes it and issues `StartJob`. Passes run when a jobs/hosts change
//! notification arrives (debounced; see [`crate::events`]), with the periodic
//! `match_interval` timer as the staleness fallback.
//!
//! Each pass:
//!   1. streams `queued` jobs oldest-first;
//!   2. for each, asks the DB for eligible hosts via
//!      [`tml_switchboard.eligible_hosts`](../../SCHEMA.sql) (idle + live + not in
//!      maintenance — the set logic SQL does well), then drops the candidates
//!      whose host spec fails the job's CEL predicate;
//!   3. attempts each candidate under a row lock in [`Scheduler::try_assign`],
//!      which layers on the image resolution (which does not belong in SQL) and
//!      commits the assignment;
//!   4. failing that, tries to free a host in [`Scheduler::try_reclaim`] by
//!      stopping an expired `preempt`-lease job on one.
//!
//! [`SupervisorWSWorker`]: crate::supervisor_ws_worker::SupervisorWSWorker

use std::collections::HashMap;

use chrono::{DateTime, TimeDelta, Utc};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use treadmill_rs::host_spec::HostSpecV1;
use uuid::Uuid;

use crate::audit::model::{Host as AuditHost, Job as AuditJob, Subject as AuditSubject};
use crate::audit::{self, SYSTEM_ACTOR_ID, events};
use crate::auth::engine::{self, HostPermission};
use crate::events::{Debounced, EventBus, EventFilter};
use crate::predicate::{CelEngine, Engine};
use crate::sql;
use crate::sql::job::{ImageResolveError, SqlJobState};

/// Dispatches queued jobs onto eligible hosts. Holds only a pool handle and
/// change-notification subscriptions (DB-only coordination).
pub struct Scheduler {
    pool: PgPool,
    /// Interval between scheduling passes when no change notification arrives.
    match_interval: TimeDelta,
    /// How recently a host must have heartbeat to be considered live.
    host_liveness_timeout: TimeDelta,
    /// Debounced wake on any jobs/hosts change. Filtering finer than
    /// table-wide isn't worth it here: a pass with nothing queued is one
    /// indexed query.
    wake: Debounced,
}

/// Outcome of attempting to place one job on one candidate host.
#[derive(Debug, PartialEq, Eq)]
enum AssignOutcome {
    /// The job was assigned to the host.
    Assigned,
    /// The host does not admit this job (no image set member matches it) — try
    /// the next candidate host.
    HostRejected,
    /// The host was taken/no-longer-live by the time we locked it — try the next
    /// candidate host.
    HostTaken,
    /// The job is no longer schedulable (already taken by another scheduler, or
    /// finalized here as an image error) — stop considering it this pass.
    JobDone,
}

impl Scheduler {
    pub fn new(
        pool: PgPool,
        match_interval: TimeDelta,
        host_liveness_timeout: TimeDelta,
        event_bus: &EventBus,
        event_debounce: std::time::Duration,
    ) -> Self {
        let wake = Debounced::new(
            vec![
                event_bus.subscribe(&[EventFilter {
                    table: "jobs",
                    key: None,
                }]),
                event_bus.subscribe(&[EventFilter {
                    table: "hosts",
                    key: None,
                }]),
            ],
            event_debounce,
        );
        Self {
            pool,
            match_interval,
            host_liveness_timeout,
            wake,
        }
    }

    /// Run the scheduling loop forever (until the task is dropped).
    pub async fn run(mut self) {
        let period = self
            .match_interval
            .to_std()
            .unwrap_or_else(|_| std::time::Duration::from_secs(10));
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = self.wake.wait() => {}
            }
            if let Err(e) = self.tick().await {
                tracing::error!("scheduler pass failed: {e:?}");
            }
        }
    }

    /// One scheduling pass: stream queued jobs oldest-first and try to place each.
    async fn tick(&self) -> anyhow::Result<()> {
        let cutoff = Utc::now() - self.host_liveness_timeout;
        let specs = self.host_specs().await?;

        // Stream (rather than collect) the queue: the cursor holds one pooled
        // connection while per-job work below borrows others.
        let mut queued = sqlx::query!(
            r#"select job_id, host_cel_predicate
               from tml_switchboard.jobs
               where job_state = 'queued'
               order by queued_at"#
        )
        .fetch(&self.pool);

        while let Some(row) = queued.try_next().await? {
            let job_id = row.job_id;

            // The DB set-filter: idle + live + not in maintenance + the job
            // owner is authorized to `start` on the host (ownership / `start`
            // grant / admin, via `principals()` -- folded into `eligible_hosts`
            // itself, so an unauthorized host never becomes a candidate). A host
            // placed earlier in this same pass is already excluded here (it reads
            // committed `current_job`), so no in-memory host bookkeeping.
            let candidates = sqlx::query_scalar!(
                r#"select eligible_hosts as "host_id!"
                   from tml_switchboard.eligible_hosts($1, $2)"#,
                job_id,
                cutoff,
            )
            .fetch_all(&self.pool)
            .await?;

            let candidates =
                admitted_by_predicate(job_id, &row.host_cel_predicate, candidates, &specs);

            let mut settled = false;
            for host_id in candidates {
                match self
                    .try_assign(job_id, host_id, specs.get(&host_id))
                    .await?
                {
                    AssignOutcome::Assigned | AssignOutcome::JobDone => {
                        settled = true;
                        break;
                    }
                    AssignOutcome::HostRejected | AssignOutcome::HostTaken => continue,
                }
            }

            if !settled {
                self.try_reclaim(job_id, cutoff).await?;
            }
        }

        Ok(())
    }

    /// Every host's current spec, normalized, read once per pass rather than
    /// once per (job, host) pair.
    ///
    /// A row that does not deserialize is dropped with a log line rather than
    /// failing the pass: one corrupt document must not stop the fleet from
    /// scheduling.
    async fn host_specs(&self) -> anyhow::Result<HashMap<Uuid, HostSpecV1>> {
        let rows = sql::host_spec::current_for_all_hosts(&self.pool).await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match row {
                Ok(stored) => Some((stored.host_id, stored.normalize())),
                Err(e) => {
                    tracing::error!("skipping host spec in scheduling pass: {e}");
                    None
                }
            })
            .collect())
    }

    /// Free a host for a queued job no idle host could take, by stopping the
    /// expired `preempt`-lease job on one. Returns the job it signalled.
    ///
    /// The queued job is *not* held for that host: it stays queued and a later
    /// pass places it wherever it fits once the host is released, so the
    /// oldest-first order still decides who gets the freed capacity. What the
    /// pass must not do is evict twice for one job, which is what
    /// `reclaim_pending` guards -- a reclaim already in flight on a host this
    /// job could use is capacity on the way, so we wait for it instead.
    async fn try_reclaim(
        &self,
        job_id: Uuid,
        cutoff: DateTime<Utc>,
    ) -> anyhow::Result<Option<Uuid>> {
        let now = Utc::now();
        let candidates = sqlx::query!(
            r#"select host_id as "host_id!", reclaim_pending as "reclaim_pending!"
               from tml_switchboard.reclaimable_hosts($1, $2, $3)"#,
            job_id,
            cutoff,
            now,
        )
        .fetch_all(&self.pool)
        .await?;

        if candidates.iter().any(|c| c.reclaim_pending) {
            return Ok(None);
        }
        let Some(candidate) = candidates.first() else {
            return Ok(None);
        };
        let host_id = candidate.host_id;

        let mut txn = self.pool.begin().await?;
        let Some(victim) = sql::job::request_preempt(host_id, now, &mut txn).await? else {
            return Ok(None);
        };
        audit::emit(
            &mut txn,
            &events::JobPreempted {
                actor: AuditSubject(SYSTEM_ACTOR_ID),
                job: AuditJob(victim),
                host: AuditHost(host_id),
            },
        )
        .await?;
        txn.commit().await?;

        tracing::info!(
            %host_id,
            %victim,
            %job_id,
            "reclaiming a host with an expired lease to place a queued job"
        );
        Ok(Some(victim))
    }

    /// Attempt to place `job_id` on `host_id` in a single guarded transaction.
    ///
    /// Locks the host row first (host-before-job order, matching the worker, so
    /// the two never deadlock), re-validates idle/live/host-tag and the DUT
    /// match under the lock, resolves the image *in the transaction*, then writes
    /// the assignment with `WHERE current_job IS NULL` / `WHERE job_state =
    /// 'queued'` guards. A lost race against another scheduler is therefore a
    /// clean no-op, which is what makes this safe to run in multiple processes.
    async fn try_assign(
        &self,
        job_id: Uuid,
        host_id: Uuid,
        host_spec: Option<&HostSpecV1>,
    ) -> anyhow::Result<AssignOutcome> {
        let cutoff = Utc::now() - self.host_liveness_timeout;
        let mut txn = self.pool.begin().await?;

        // Lock the host and re-assert it is idle and live.
        let host = sqlx::query!(
            r#"select current_job, last_seen_at
               from tml_switchboard.hosts
               where host_id = $1
               for update"#,
            host_id,
        )
        .fetch_one(&mut *txn)
        .await?;
        let live = host.last_seen_at.is_some_and(|t| t > cutoff);
        if host.current_job.is_some() || !live {
            return Ok(AssignOutcome::HostTaken); // txn rolls back on drop
        }

        // Lock the job and re-assert it is still queued.
        let state = sqlx::query_scalar!(
            r#"select job_state as "state: SqlJobState"
               from tml_switchboard.jobs
               where job_id = $1
               for update"#,
            job_id,
        )
        .fetch_optional(&mut *txn)
        .await?;
        if state != Some(SqlJobState::Queued) {
            return Ok(AssignOutcome::JobDone);
        }
        let job = sql::job::fetch_by_job_id(job_id, &mut *txn).await?;

        // Re-check that the job's owner may `start` on this host, under the same
        // lock and mirroring `eligible_hosts`' authorization predicate. That
        // function is the authoritative gate, but host ownership/grants can
        // change between the candidate scan and here; re-validating closes that
        // window. An orphaned job (owner_id NULL) is never authorized.
        let authorized = match job.owner_id() {
            Some(owner) => {
                engine::can_access_host(&mut *txn, owner, host_id, HostPermission::Start).await?
            }
            None => false,
        };
        if !authorized {
            return Ok(AssignOutcome::HostRejected);
        }

        // Resolve the image against the chosen host, inside the transaction.
        // The resolved spec itself is rebuilt at dispatch from the recorded
        // `resolved_image_id`; here we only need resolution to succeed (validating
        // the image / picking the set member) and the id to pin.
        let (_spec, resolved_image_id) = match job.resolve_image_spec(host_spec, &mut txn).await {
            Ok(resolved) => resolved,
            // No set member matches this host: a different host might, so this
            // is a host rejection, not a job failure.
            Err(ImageResolveError::NoMatchingMember) => return Ok(AssignOutcome::HostRejected),
            // The image itself is unusable (unregistered / no registry location /
            // malformed row): the job can never run, so finalize it.
            Err(
                e @ (ImageResolveError::NotRegistered(_)
                | ImageResolveError::NoLocations(_)
                | ImageResolveError::MalformedJob(_)),
            ) => {
                tracing::warn!("finalizing job {job_id} as image_error: {e}");
                sql::job::finalize_unscheduled_as_image_error(job_id, Utc::now(), &mut *txn)
                    .await?;
                if let Some(reason) = sql::job::finalized_reason(job_id, &mut *txn).await? {
                    audit::emit(
                        &mut txn,
                        &events::JobFinalized {
                            actor: AuditSubject(SYSTEM_ACTOR_ID),
                            job: AuditJob(job_id),
                            host: AuditHost(host_id),
                            reason,
                        },
                    )
                    .await?;
                }
                txn.commit().await?;
                return Ok(AssignOutcome::JobDone);
            }
            Err(ImageResolveError::Db(e)) => return Err(e.into()),
        };

        // Commit the assignment. The guards are belt-and-suspenders given the
        // row locks above, but keep the writes self-validating.
        let claimed = sqlx::query!(
            r#"update tml_switchboard.hosts
               set current_job = $1
               where host_id = $2 and current_job is null
               returning host_id"#,
            job_id,
            host_id,
        )
        .fetch_optional(&mut *txn)
        .await?;
        if claimed.is_none() {
            return Ok(AssignOutcome::HostTaken);
        }
        sqlx::query!(
            r#"update tml_switchboard.jobs
               set job_state = 'assigned',
                   dispatched_on_host_id = $2
               where job_id = $1 and job_state = 'queued'"#,
            job_id,
            host_id,
        )
        .execute(&mut *txn)
        .await?;
        if let Some(image_id) = resolved_image_id {
            sql::job::set_resolved_image(job_id, image_id, &mut *txn).await?;
        }

        audit::emit(
            &mut txn,
            &events::JobAssigned {
                actor: AuditSubject(SYSTEM_ACTOR_ID),
                job: AuditJob(job_id),
                host: AuditHost(host_id),
            },
        )
        .await?;

        txn.commit().await?;

        tracing::debug!(
            %host_id,
            %job_id,
            "assigned job to host"
        );
        Ok(AssignOutcome::Assigned)
    }
}

/// Drop the candidates whose spec does not satisfy the job's CEL predicate.
///
/// Per the evaluation contract an error means *this host* does not match, never
/// that the job fails; the same holds for a predicate that no longer compiles,
/// which simply admits nothing and leaves the job queued.
///
/// A host with no spec matches nothing, including the default predicate: with
/// no description there is nothing to evaluate against and no platform profile
/// for an image set to match, so it could not be dispatched onto anyway.
fn admitted_by_predicate(
    job_id: Uuid,
    source: &str,
    candidates: Vec<Uuid>,
    specs: &HashMap<Uuid, HostSpecV1>,
) -> Vec<Uuid> {
    let compiled = match CelEngine.compile(source) {
        Ok(compiled) => compiled,
        Err(e) => {
            tracing::warn!("job {job_id} has an uncompilable host predicate: {e}");
            return Vec::new();
        }
    };

    candidates
        .into_iter()
        .filter(|host_id| match specs.get(host_id) {
            Some(spec) => compiled.eval(spec).unwrap_or_else(|e| {
                tracing::debug!("job {job_id} predicate errored on host {host_id}: {e}");
                false
            }),
            None => false,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    //! DB-backed scheduler tests. Each is `#[ignore]`d (needs Postgres via
    //! `DATABASE_URL`); run them in the ephemeral-Postgres devshell:
    //!
    //!     nix develop '.#database'
    //!     cargo nextest run --run-ignored only -p treadmill-switchboard
    //!
    //! CI runs them via the `nextest-db` Nix check. Helpers use runtime
    //! (non-macro) queries so they don't add to the `.sqlx` cache.

    use super::*;
    use chrono::{DateTime, Duration, Utc};
    use sqlx::PgPool;
    use sqlx::postgres::types::PgInterval;
    use std::collections::HashMap;
    use treadmill_rs::api::switchboard::jobs::RestartPolicy;
    use treadmill_rs::api::switchboard::{DEFAULT_HOST_CEL_PREDICATE, JobInitSpec, JobRequest};
    use treadmill_rs::image::{Digest, media_types};

    fn scheduler(pool: PgPool) -> Scheduler {
        Scheduler::new(
            pool,
            Duration::seconds(1),
            Duration::seconds(60),
            &EventBus::default(),
            std::time::Duration::from_millis(10),
        )
    }

    /// A deterministic distinct digest per `seed`.
    fn digest(seed: u8) -> Digest {
        Digest::from_sha256([seed; 32])
    }

    async fn insert_user(pool: &PgPool) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user')")
            .bind(id)
            .execute(pool)
            .await?;
        sqlx::query("insert into tml_switchboard.users (subject_id, name) values ($1, $2)")
            .bind(id)
            .bind(format!("user-{id}"))
            .execute(pool)
            .await?;
        Ok(id)
    }

    /// Insert a group subject (usable as a host owner or a grantee).
    async fn insert_group(pool: &PgPool) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'group')")
            .bind(id)
            .execute(pool)
            .await?;
        sqlx::query("insert into tml_switchboard.groups (subject_id, name) values ($1, $2)")
            .bind(id)
            .bind(format!("group-{id}"))
            .execute(pool)
            .await?;
        Ok(id)
    }

    async fn insert_token(pool: &PgPool, user_id: Uuid) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "insert into tml_switchboard.api_tokens \
             (token_id, token, user_id, revoked, created_at, expires_at) \
             values ($1, $2, $3, null, now(), now() + interval '1 day')",
        )
        .bind(id)
        .bind(vec![0u8; 32])
        .bind(user_id)
        .execute(pool)
        .await?;
        Ok(id)
    }

    /// Insert an undescribed host owned by `owner`. `last_seen` of `None`
    /// leaves it not-live (no connected worker). Having no spec, it is never
    /// dispatchable, so scheduling tests want [`insert_live_host`]; this is for
    /// the SQL eligibility filter, which does not consult specs. Ownership
    /// matters there: `eligible_hosts` only admits hosts the job's owner may
    /// `start` on (owned, `start`-granted, or admin).
    async fn insert_host(
        pool: &PgPool,
        owner: Uuid,
        last_seen: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        // `auth_token` is UNIQUE and must be exactly 32 bytes; seed it from the
        // host id so multiple hosts in one test don't collide.
        let mut auth_token = vec![0u8; 32];
        auth_token[..16].copy_from_slice(id.as_bytes());
        sqlx::query(
            "insert into tml_switchboard.hosts \
             (host_id, name, auth_token, worker_instance_id, last_seen_at, owner_id) \
             values ($1, $2, $3, 0, $4, $5)",
        )
        .bind(id)
        .bind(format!("host-{id}"))
        .bind(auth_token)
        .bind(last_seen)
        .bind(owner)
        .execute(pool)
        .await?;
        Ok(id)
    }

    /// A live (heartbeat now), described host owned by `owner`, advertising
    /// the default profile. A host must have a spec to be dispatchable at all,
    /// so scheduling tests want this rather than [`insert_host`].
    async fn insert_live_host(pool: &PgPool, owner: Uuid) -> anyhow::Result<Uuid> {
        let id = insert_host(pool, owner, Some(Utc::now())).await?;
        describe_host_with_profile(pool, id, DEFAULT_TEST_PROFILE, 8192).await?;
        Ok(id)
    }

    /// The platform profile [`insert_live_host`] advertises.
    const DEFAULT_TEST_PROFILE: &str = "q35-virtio-uefi";

    /// Grant `subject` the `start` permission on `host` (revocable).
    async fn grant_host_start(pool: &PgPool, host: Uuid, subject: Uuid) -> anyhow::Result<()> {
        sqlx::query(
            "insert into tml_switchboard.host_grants (host_id, subject_id, permission) \
             values ($1, $2, 'start')",
        )
        .bind(host)
        .bind(subject)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Add `member` to `group` (a manual `group_members` edge).
    async fn add_group_member(pool: &PgPool, group: Uuid, member: Uuid) -> anyhow::Result<()> {
        sqlx::query(
            "insert into tml_switchboard.group_members (group_id, member_id, source) \
             values ($1, $2, 'manual')",
        )
        .bind(group)
        .bind(member)
        .execute(pool)
        .await?;
        Ok(())
    }

    /// Register a concrete image (with `with_location` controlling whether it has
    /// a registry location). Returns its catalog id and manifest digest.
    async fn register_image(
        pool: &PgPool,
        owner: Uuid,
        seed: u8,
        with_location: bool,
    ) -> anyhow::Result<(Uuid, Digest)> {
        let d = digest(seed);
        let id = Uuid::new_v4();
        let mut tx = pool.begin().await?;
        sql::image::insert(
            &mut *tx,
            id,
            &d.encoded(),
            media_types::IMAGE_ARTIFACT_TYPE,
            None,
        )
        .await?;
        if with_location {
            // Source owned by the job owner so the dispatch source gate passes.
            sql::image::insert_source(
                &mut *tx,
                Uuid::new_v4(),
                id,
                "reg.example:5000",
                "repo",
                "external",
                Some(owner),
            )
            .await?;
        }
        tx.commit().await?;
        Ok((id, d))
    }

    /// Register an image set (named `set-{name_seed}`) with one generation
    /// whose members are `(seed, platform_profile, predicate)`, in selection
    /// order. Returns the set's id and each member's digest.
    async fn register_set(
        pool: &PgPool,
        owner: Uuid,
        name_seed: u8,
        members: &[(u8, &str, Option<&str>)],
    ) -> anyhow::Result<(Uuid, Vec<Digest>)> {
        let gid = Uuid::new_v4();
        let mut tx = pool.begin().await?;
        sql::image::create_set(&mut *tx, gid, &format!("set-{name_seed}"), owner, None).await?;
        let mut member_rows = Vec::new();
        let mut member_digests = Vec::new();
        for (index, (seed, profile, predicate)) in members.iter().enumerate() {
            let md = digest(*seed);
            // Two members may name the same image (one build serving several
            // profiles), so register it only the first time it appears.
            let img_id = match sql::image::fetch_by_digest(&mut *tx, &md.encoded()).await? {
                Some(existing) => existing.id,
                None => {
                    let img_id = Uuid::new_v4();
                    sql::image::insert(
                        &mut *tx,
                        img_id,
                        &md.encoded(),
                        media_types::IMAGE_ARTIFACT_TYPE,
                        None,
                    )
                    .await?;
                    sql::image::insert_source(
                        &mut *tx,
                        Uuid::new_v4(),
                        img_id,
                        "reg.example:5000",
                        "repo",
                        "external",
                        Some(owner),
                    )
                    .await?;
                    img_id
                }
            };
            member_rows.push(sql::image::NewSetMember {
                image_id: img_id,
                platform_profile: (*profile).to_string(),
                predicate: predicate.map(str::to_string),
                index: index as i32,
            });
            member_digests.push(md);
        }
        sql::image::create_generation(&mut tx, gid, owner, &member_rows).await?;
        tx.commit().await?;
        Ok((gid, member_digests))
    }

    async fn enqueue(
        pool: &PgPool,
        token: Uuid,
        init_spec: JobInitSpec,
        host_cel_predicate: &str,
        queued_at: DateTime<Utc>,
    ) -> anyhow::Result<Uuid> {
        let job_id = Uuid::new_v4();
        let req = JobRequest {
            init_spec,
            label: None,
            owner: None,
            restart_policy: RestartPolicy { max_restarts: 0 },
            parameters: HashMap::new(),
            host_cel_predicate: host_cel_predicate.to_string(),
            lease_duration: None,
            lease_expiry_action: None,
        };
        // Mirror the enqueue route: the job is owned by the enqueuing token's
        // user. Host authorization (`eligible_hosts`) is evaluated against this
        // owner.
        let owner: Uuid = sqlx::query_scalar(
            "select user_id from tml_switchboard.api_tokens where token_id = $1",
        )
        .bind(token)
        .fetch_one(pool)
        .await?;
        let mut tx = pool.begin().await?;
        sql::job::insert(
            req,
            job_id,
            token,
            Some(owner),
            PgInterval::try_from(Duration::hours(1)).unwrap(),
            sql::job::SqlLeaseExpiryAction::Terminate,
            queued_at,
            &mut tx,
        )
        .await?;
        tx.commit().await?;
        Ok(job_id)
    }

    /// Convenience: enqueue a concrete-image job by manifest digest, with the
    /// default (match-anything) predicate.
    async fn enqueue_image(pool: &PgPool, token: Uuid, image: Digest) -> anyhow::Result<Uuid> {
        enqueue(
            pool,
            token,
            JobInitSpec::Image {
                manifest_digest: image,
            },
            DEFAULT_HOST_CEL_PREDICATE,
            Utc::now(),
        )
        .await
    }

    async fn job_state(pool: &PgPool, job_id: Uuid) -> anyhow::Result<String> {
        Ok(
            sqlx::query_scalar(
                "select job_state::text from tml_switchboard.jobs where job_id = $1",
            )
            .bind(job_id)
            .fetch_one(pool)
            .await?,
        )
    }

    async fn job_dispatched_host(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<Uuid>> {
        Ok(sqlx::query_scalar(
            "select dispatched_on_host_id from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?)
    }

    async fn job_resolved_digest(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "select (select i.manifest_digest from tml_switchboard.images i \
                     where i.id = j.resolved_image_id) \
             from tml_switchboard.jobs j where j.job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?)
    }

    async fn job_started_at(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<DateTime<Utc>>> {
        Ok(
            sqlx::query_scalar("select started_at from tml_switchboard.jobs where job_id = $1")
                .bind(job_id)
                .fetch_one(pool)
                .await?,
        )
    }

    async fn job_termination(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "select termination_reason::text from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?)
    }

    /// The audit event types related to `job_id` (via any relation), oldest-first.
    async fn audit_event_types_for_job(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Vec<String>> {
        Ok(sqlx::query_scalar(
            "select e.event_type \
             from tml_switchboard.audit_events e \
             join tml_switchboard.audit_event_relations r on e.event_id = r.event_id \
             where r.entity_kind = 'job' and r.entity_id = $1 \
             order by e.created_at",
        )
        .bind(job_id)
        .fetch_all(pool)
        .await?)
    }

    async fn host_current_job(pool: &PgPool, host_id: Uuid) -> anyhow::Result<Option<Uuid>> {
        Ok(
            sqlx::query_scalar("select current_job from tml_switchboard.hosts where host_id = $1")
                .bind(host_id)
                .fetch_one(pool)
                .await?,
        )
    }

    /// Call the `eligible_hosts` SQL function directly.
    async fn eligible(
        pool: &PgPool,
        job_id: Uuid,
        cutoff: DateTime<Utc>,
    ) -> anyhow::Result<Vec<Uuid>> {
        Ok(
            sqlx::query_scalar("select tml_switchboard.eligible_hosts($1, $2)")
                .bind(job_id)
                .bind(cutoff)
                .fetch_all(pool)
                .await?,
        )
    }

    /// Write revision 1 of a host's spec: a virtual host at `site` with the
    /// given memory and DUT boards, enough to exercise predicates at both
    /// levels.
    async fn describe_host(
        pool: &PgPool,
        host_id: Uuid,
        site: &str,
        memory_mb: u32,
        dut_boards: &[&str],
    ) -> anyhow::Result<()> {
        use treadmill_rs::host_spec::{
            Dut, HostSpec, HostSpecV1, Platform, Resources, SpecVersionV1,
        };

        let spec = HostSpec::V1(HostSpecV1 {
            spec_version: SpecVersionV1::V1,
            id: host_id,
            name: format!("host-{host_id}"),
            description: None,
            site: site.to_string(),
            location: None,
            platform: Platform::Virtual {
                arch: "x86_64".into(),
                profiles: vec!["q35-virtio-uefi".into()],
                hypervisor: "qemu".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb,
                storage_gb: 64,
            },
            labels: Default::default(),
            duts: dut_boards
                .iter()
                .map(|board| Dut {
                    name: None,
                    serial: None,
                    vendor: "ACME".into(),
                    board: (*board).to_string(),
                    arch: vec![],
                    connectivity: vec![],
                    debug: None,
                    console: None,
                    labels: Default::default(),
                })
                .collect(),
        });
        let mut conn = pool.acquire().await?;
        sql::host_spec::append(host_id, &spec, None, &mut conn).await?;
        Ok(())
    }

    /// Describe a host advertising exactly one platform profile.
    async fn describe_host_with_profile(
        pool: &PgPool,
        host_id: Uuid,
        profile: &str,
        memory_mb: u32,
    ) -> anyhow::Result<()> {
        use treadmill_rs::host_spec::{HostSpec, HostSpecV1, Platform, Resources, SpecVersionV1};

        let spec = HostSpec::V1(HostSpecV1 {
            spec_version: SpecVersionV1::V1,
            id: host_id,
            name: format!("host-{host_id}"),
            description: None,
            site: "cambridge".into(),
            location: None,
            platform: Platform::Virtual {
                arch: "x86_64".into(),
                profiles: vec![profile.to_string()],
                hypervisor: "qemu".into(),
            },
            resources: Resources {
                cpu_cores: 4,
                memory_mb,
                storage_gb: 64,
            },
            labels: Default::default(),
            duts: vec![],
        });
        let mut conn = pool.acquire().await?;
        sql::host_spec::append(host_id, &spec, None, &mut conn).await?;
        Ok(())
    }

    /// The concrete image the scheduler pinned onto a job, by digest.
    async fn resolved_digest(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Digest> {
        let encoded: String = sqlx::query_scalar(
            "select i.manifest_digest from tml_switchboard.jobs j \
             join tml_switchboard.images i on i.id = j.resolved_image_id \
             where j.job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?;
        Ok(encoded.parse()?)
    }

    async fn set_maintenance(pool: &PgPool, host_id: Uuid, on: bool) -> anyhow::Result<()> {
        sqlx::query("update tml_switchboard.hosts set maintenance = $2 where host_id = $1")
            .bind(host_id)
            .bind(on)
            .execute(pool)
            .await?;
        Ok(())
    }

    // -- eligible_hosts SQL function ----------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_filters_idle_and_live(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let cutoff = Utc::now() - Duration::seconds(60);

        let good = insert_live_host(&pool, user).await?;
        let _dead = insert_host(&pool, user, None).await?;
        let _stale = insert_host(&pool, user, Some(Utc::now() - Duration::seconds(120))).await?;
        let busy = insert_live_host(&pool, user).await?;

        let (_, img) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;
        // Make `busy` busy by pointing its current_job at an unrelated job.
        let other = enqueue_image(&pool, token, img).await?;
        sqlx::query("update tml_switchboard.hosts set current_job = $1 where host_id = $2")
            .bind(other)
            .bind(busy)
            .execute(&pool)
            .await?;

        let got = eligible(&pool, job, cutoff).await?;
        assert_eq!(got, vec![good], "only the idle, live host");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_matches_every_idle_live_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let cutoff = Utc::now() - Duration::seconds(60);
        let a = insert_live_host(&pool, user).await?;
        let b = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;
        let mut got = eligible(&pool, job, cutoff).await?;
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        Ok(())
    }

    // -- host-start authorization ------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_enforces_start_authorization(pool: PgPool) -> anyhow::Result<()> {
        let owner = insert_user(&pool).await?;
        let token = insert_token(&pool, owner).await?;
        let other = insert_user(&pool).await?;
        let cutoff = Utc::now() - Duration::seconds(60);

        // All three hosts are idle, live, and tag-match; only authorization
        // separates them.
        let owned = insert_live_host(&pool, owner).await?;
        let foreign = insert_live_host(&pool, other).await?;
        let granted = insert_live_host(&pool, other).await?;
        grant_host_start(&pool, granted, owner).await?;

        let (_, img) = register_image(&pool, owner, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        let mut got = eligible(&pool, job, cutoff).await?;
        assert!(
            !got.contains(&foreign),
            "the foreign host the owner may not start on is excluded"
        );
        got.sort();
        let mut want = vec![owned, granted];
        want.sort();
        assert_eq!(
            got, want,
            "only the owner's own host and the start-granted host are eligible"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_authorizes_via_owning_group(pool: PgPool) -> anyhow::Result<()> {
        // A host owned by a group the job owner belongs to is eligible: host
        // authorization is evaluated over the owner's transitive principals.
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let group = insert_group(&pool).await?;
        add_group_member(&pool, group, user).await?;
        let cutoff = Utc::now() - Duration::seconds(60);

        let group_owned = insert_live_host(&pool, group).await?;

        let (_, img) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        assert_eq!(eligible(&pool, job, cutoff).await?, vec![group_owned]);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_admin_owner_matches_any_host(pool: PgPool) -> anyhow::Result<()> {
        let owner = insert_user(&pool).await?;
        let token = insert_token(&pool, owner).await?;
        let other = insert_user(&pool).await?;
        add_group_member(&pool, engine::ADMINS_GROUP_ID, owner).await?;
        let cutoff = Utc::now() - Duration::seconds(60);

        // Owned by someone else, no grant -- but the job owner is an admin.
        let foreign = insert_live_host(&pool, other).await?;

        let (_, img) = register_image(&pool, owner, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        assert_eq!(
            eligible(&pool, job, cutoff).await?,
            vec![foreign],
            "an admin owner may start on any host"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_schedule_onto_unauthorized_host(pool: PgPool) -> anyhow::Result<()> {
        let owner = insert_user(&pool).await?;
        let token = insert_token(&pool, owner).await?;
        let other = insert_user(&pool).await?;
        // The only live, tag-matching host belongs to another user; the job
        // owner holds no grant on it.
        let foreign = insert_live_host(&pool, other).await?;
        let (_, img) = register_image(&pool, owner, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, foreign).await?, None);
        assert_eq!(
            job_state(&pool, job).await?,
            "queued",
            "a job with no authorized host stays queued (ages out via queue timeout)"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn schedules_onto_start_granted_host(pool: PgPool) -> anyhow::Result<()> {
        let owner = insert_user(&pool).await?;
        let token = insert_token(&pool, owner).await?;
        let other = insert_user(&pool).await?;
        let host = insert_live_host(&pool, other).await?;
        grant_host_start(&pool, host, owner).await?;
        let (_, img) = register_image(&pool, owner, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        assert_eq!(job_state(&pool, job).await?, "assigned");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn try_assign_rejects_unauthorized_host(pool: PgPool) -> anyhow::Result<()> {
        // The under-lock re-check in `try_assign` is a second gate independent of
        // `eligible_hosts` (it covers a grant revoked between the candidate scan
        // and the lock). Drive it directly with a host the owner may not use.
        let owner = insert_user(&pool).await?;
        let token = insert_token(&pool, owner).await?;
        let other = insert_user(&pool).await?;
        let foreign = insert_live_host(&pool, other).await?;
        let (_, img) = register_image(&pool, owner, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        let outcome = scheduler(pool.clone())
            .try_assign(job, foreign, None)
            .await?;
        assert_eq!(outcome, AssignOutcome::HostRejected);
        assert_eq!(job_state(&pool, job).await?, "queued");
        assert_eq!(host_current_job(&pool, foreign).await?, None);
        Ok(())
    }

    // -- scheduler dispatch -------------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn schedules_concrete_image_onto_eligible_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img_digest) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img_digest).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        assert_eq!(job_state(&pool, job).await?, "assigned");
        assert_eq!(job_dispatched_host(&pool, job).await?, Some(host));
        assert_eq!(
            job_resolved_digest(&pool, job).await?,
            Some(img_digest.encoded())
        );
        assert!(
            job_started_at(&pool, job).await?.is_none(),
            "started_at stays null until the job actually initializes"
        );
        let types = audit_event_types_for_job(&pool, job).await?;
        assert!(
            types.contains(&"job_assigned.v1".to_string()),
            "expected a job_assigned event, got {types:?}"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_schedule_when_no_eligible_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        // The only host never heartbeat, so nothing is live.
        let _dead = insert_host(&pool, user, None).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(job_state(&pool, job).await?, "queued", "job stays queued");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn oldest_job_wins_the_single_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let now = Utc::now();
        let older = enqueue(
            &pool,
            token,
            JobInitSpec::Image {
                manifest_digest: img,
            },
            DEFAULT_HOST_CEL_PREDICATE,
            now - Duration::seconds(10),
        )
        .await?;
        let newer = enqueue(
            &pool,
            token,
            JobInitSpec::Image {
                manifest_digest: img,
            },
            DEFAULT_HOST_CEL_PREDICATE,
            now,
        )
        .await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(older));
        assert_eq!(job_state(&pool, older).await?, "assigned");
        assert_eq!(
            job_state(&pool, newer).await?,
            "queued",
            "the newer job waits for a host"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn enqueue_rejects_an_unregistered_image(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let _host = insert_live_host(&pool, user).await?;
        // A digest that was never registered in the catalog. Resolving it to an
        // image row at insert fails outright, instead of deferring to a
        // dispatch-time image error as the old digest column did.
        let unregistered = digest(99);
        let result = enqueue_image(&pool, token, unregistered).await;

        assert!(
            result.is_err(),
            "enqueue must reject a job referencing an unregistered image id"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn image_without_location_finalizes_as_image_error(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let _host = insert_live_host(&pool, user).await?;
        // Registered, but with no registry location to pull from.
        let (_, img) = register_image(&pool, user, 1, false).await?;
        let job = enqueue_image(&pool, token, img).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(job_state(&pool, job).await?, "finalized");
        assert_eq!(
            job_termination(&pool, job).await?.as_deref(),
            Some("image_error")
        );
        let types = audit_event_types_for_job(&pool, job).await?;
        assert!(
            types.contains(&"job_finalized.v1".to_string()),
            "expected a job_finalized event, got {types:?}"
        );
        Ok(())
    }

    /// Sources can be deleted or restricted between enqueue and dispatch, so the
    /// dispatch-time re-check is load-bearing: a source that still *exists* but
    /// is no longer usable by the job owner must finalize the job `image_error`,
    /// exactly like a missing one.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn source_lost_after_enqueue_finalizes_as_image_error(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let _host = insert_live_host(&pool, user).await?;
        let (img_id, img) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img).await?;

        // The owner loses the source before the scheduler runs: it now belongs
        // to an unrelated user and carries no grants.
        let stranger = insert_user(&pool).await?;
        sqlx::query(
            "update tml_switchboard.image_sources set owner_subject = $1 where image_id = $2",
        )
        .bind(stranger)
        .bind(img_id)
        .execute(&pool)
        .await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(job_state(&pool, job).await?, "finalized");
        assert_eq!(
            job_termination(&pool, job).await?.as_deref(),
            Some("image_error")
        );
        Ok(())
    }

    /// A change notification, not the `match_interval` timer, drives the pass:
    /// with the timer effectively disabled, an enqueue after startup must still
    /// be assigned promptly.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn enqueue_event_drives_a_pass_without_the_timer(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let bus = EventBus::default();
        tokio::spawn(bus.listener(pool.clone()));
        tokio::spawn(
            Scheduler::new(
                pool.clone(),
                Duration::hours(1),
                Duration::seconds(60),
                &bus,
                std::time::Duration::from_millis(10),
            )
            .run(),
        );

        // Wait until the listener demonstrably delivers wakes (its LISTEN may
        // not be up yet; a write committed before that is a lost notification,
        // which only the timer would cover). Probes a throwaway host so the
        // eligible host is left alone.
        let probe_host = insert_host(&pool, user, None).await?;
        let mut probe = bus.subscribe(&[EventFilter {
            table: "hosts",
            key: Some(("host_id", probe_host)),
        }]);
        probe.changed().await;
        loop {
            sqlx::query(
                "update tml_switchboard.hosts set name = md5(random()::text) \
                 where host_id = $1",
            )
            .bind(probe_host)
            .execute(&pool)
            .await?;
            if tokio::time::timeout(std::time::Duration::from_millis(200), probe.changed())
                .await
                .is_ok()
            {
                break;
            }
        }

        let job = enqueue_image(&pool, token, img).await?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while job_state(&pool, job).await? != "assigned" {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the enqueue notification did not drive a scheduling pass"
            );
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_reassign_a_busy_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        // Host already running a job.
        let running = enqueue_image(&pool, token, img).await?;
        sqlx::query("update tml_switchboard.hosts set current_job = $1 where host_id = $2")
            .bind(running)
            .bind(host)
            .execute(&pool)
            .await?;
        sqlx::query("update tml_switchboard.jobs set job_state='assigned', dispatched_on_host_id=$2 where job_id=$1")
            .bind(running)
            .bind(host)
            .execute(&pool)
            .await?;

        let waiting = enqueue_image(&pool, token, img).await?;
        scheduler(pool.clone()).tick().await?;

        assert_eq!(
            host_current_job(&pool, host).await?,
            Some(running),
            "the busy host keeps its running job"
        );
        assert_eq!(
            job_state(&pool, waiting).await?,
            "queued",
            "the waiting job is not placed on the busy host"
        );
        Ok(())
    }

    // -- reclamation of expired `preempt` leases -------------------------------

    /// Put `job` on `host` as a running job whose lease started `lease_age_mins`
    /// ago and runs for `lease_mins`, with the given expiry action.
    async fn occupy_host(
        pool: &PgPool,
        host: Uuid,
        job: Uuid,
        lease_age_mins: i64,
        lease_mins: i32,
        expiry_action: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("update tml_switchboard.hosts set current_job = $1 where host_id = $2")
            .bind(job)
            .bind(host)
            .execute(pool)
            .await?;
        sqlx::query(
            "update tml_switchboard.jobs \
             set job_state = 'ready', dispatched_on_host_id = $2, started_at = $3, \
                 lease_duration = make_interval(mins => $4), \
                 lease_expiry_action = $5::tml_switchboard.lease_expiry_action \
             where job_id = $1",
        )
        .bind(job)
        .bind(host)
        .bind(Utc::now() - Duration::minutes(lease_age_mins))
        .bind(lease_mins)
        .bind(expiry_action)
        .execute(pool)
        .await?;
        Ok(())
    }

    async fn terminate_request(pool: &PgPool, job_id: Uuid) -> anyhow::Result<Option<String>> {
        Ok(sqlx::query_scalar(
            "select terminate_requested_reason::text from tml_switchboard.jobs where job_id = $1",
        )
        .bind(job_id)
        .fetch_one(pool)
        .await?)
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn reclaims_a_host_whose_preempt_lease_expired(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let running = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, host, running, 120, 60, "preempt").await?;

        let waiting = enqueue_image(&pool, token, img).await?;
        scheduler(pool.clone()).tick().await?;

        assert_eq!(
            terminate_request(&pool, running).await?.as_deref(),
            Some("preempted")
        );
        assert!(
            audit_event_types_for_job(&pool, running)
                .await?
                .contains(&"job_preempted.v1".to_string())
        );
        // The queued job is not held for the host: the victim's worker has to
        // release it first, so the job is still queued after this pass.
        assert_eq!(job_state(&pool, waiting).await?, "queued");
        assert_eq!(host_current_job(&pool, host).await?, Some(running));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_reclaim_a_live_lease_or_a_terminate_lease(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let unexpired = insert_live_host(&pool, user).await?;
        let hard = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let a = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, unexpired, a, 10, 60, "preempt").await?;
        let b = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, hard, b, 120, 60, "terminate").await?;

        enqueue_image(&pool, token, img).await?;
        scheduler(pool.clone()).tick().await?;

        assert_eq!(terminate_request(&pool, a).await?, None);
        assert_eq!(terminate_request(&pool, b).await?, None);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn one_queued_job_never_reclaims_two_hosts(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let first = insert_live_host(&pool, user).await?;
        let second = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let a = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, first, a, 180, 60, "preempt").await?;
        let b = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, second, b, 120, 60, "preempt").await?;

        enqueue_image(&pool, token, img).await?;
        // The victim keeps running until its worker converges, so the second
        // pass sees the same still-queued job and must wait for the reclaim it
        // already asked for rather than taking another host.
        let sched = scheduler(pool.clone());
        sched.tick().await?;
        sched.tick().await?;

        assert_eq!(
            terminate_request(&pool, a).await?.as_deref(),
            Some("preempted"),
            "the longest-unprotected lease is the one reclaimed"
        );
        assert_eq!(
            terminate_request(&pool, b).await?,
            None,
            "the second host is left alone while the first reclaim is in flight"
        );
        Ok(())
    }

    // -- CEL host predicate --------------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn predicate_selects_among_eligible_hosts(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let small = insert_live_host(&pool, user).await?;
        let big = insert_live_host(&pool, user).await?;
        describe_host(&pool, small, "cambridge", 4096, &[]).await?;
        describe_host(&pool, big, "cambridge", 16384, &[]).await?;

        let job = enqueue(
            &pool,
            token,
            JobInitSpec::Image {
                manifest_digest: img,
            },
            "host.resources.memory_mb >= 16384",
            Utc::now(),
        )
        .await?;

        // Both hosts clear the SQL filter; only the predicate separates them.
        assert_eq!(
            eligible(&pool, job, Utc::now() - Duration::seconds(60))
                .await?
                .len(),
            2
        );
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, big).await?, Some(job));
        assert_eq!(host_current_job(&pool, small).await?, None);
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn predicate_reaches_into_duts(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let wrong = insert_live_host(&pool, user).await?;
        let right = insert_live_host(&pool, user).await?;
        describe_host(&pool, wrong, "cambridge", 4096, &["stm32f4discovery"]).await?;
        describe_host(&pool, right, "cambridge", 4096, &["nrf52840dk"]).await?;

        let job = enqueue(
            &pool,
            token,
            JobInitSpec::Image {
                manifest_digest: img,
            },
            "host.duts.exists(d, d.board == 'nrf52840dk')",
            Utc::now(),
        )
        .await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, right).await?, Some(job));
        assert_eq!(host_current_job(&pool, wrong).await?, None);
        Ok(())
    }

    /// An evaluation error is a host rejection, not a job failure: the job
    /// stays queued rather than being finalized.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn predicate_error_leaves_the_job_queued(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let host = insert_live_host(&pool, user).await?;
        describe_host(&pool, host, "cambridge", 4096, &[]).await?;

        let job = enqueue(
            &pool,
            token,
            JobInitSpec::Image {
                manifest_digest: img,
            },
            // No such field: errors on every host.
            "host.definitely_not_a_field == 1",
            Utc::now(),
        )
        .await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, None);
        assert_eq!(job_state(&pool, job).await?, "queued");
        Ok(())
    }

    /// An undescribed host is not dispatchable at all: there is nothing to
    /// evaluate a predicate against and no profile for an image set to match,
    /// so even the default predicate declines it.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn host_without_a_spec_is_never_dispatched_onto(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;
        let host = insert_host(&pool, user, Some(Utc::now())).await?;

        let job = enqueue_image(&pool, token, img).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, None);
        assert_eq!(job_state(&pool, job).await?, "queued");

        // Describing it is what makes it dispatchable.
        describe_host_with_profile(&pool, host, DEFAULT_TEST_PROFILE, 4096).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        Ok(())
    }

    // -- image-set platform profiles -----------------------------------------

    /// The host's advertised profiles pick the member, and the first
    /// admissible one in author order wins.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn image_set_selects_by_platform_profile(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (set, digests) = register_set(
            &pool,
            user,
            9,
            &[
                (
                    1,
                    "q35-virtio-uefi",
                    Some("host.resources.memory_mb >= 16384"),
                ),
                (2, "q35-virtio-uefi", None),
                (3, "rpi4-uboot-sd", None),
            ],
        )
        .await?;

        // A big UEFI host takes the refined member; a small one falls through
        // to the catch-all behind it; an SD-boot host takes the third.
        for (memory_mb, profile, expected) in [
            (16384, "q35-virtio-uefi", &digests[0]),
            (4096, "q35-virtio-uefi", &digests[1]),
            (4096, "rpi4-uboot-sd", &digests[2]),
        ] {
            let host = insert_live_host(&pool, user).await?;
            describe_host_with_profile(&pool, host, profile, memory_mb).await?;
            let job = enqueue(
                &pool,
                token,
                JobInitSpec::ImageSet {
                    set_id: set,
                    generation: None,
                },
                DEFAULT_HOST_CEL_PREDICATE,
                Utc::now(),
            )
            .await?;
            scheduler(pool.clone()).tick().await?;
            assert_eq!(host_current_job(&pool, host).await?, Some(job));
            assert_eq!(&resolved_digest(&pool, job).await?, expected);
        }
        Ok(())
    }

    /// No member for the host's profiles is a host rejection, not a job
    /// failure: the job stays queued for a host that does match.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn no_member_for_profile_leaves_the_job_queued(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (set, _) = register_set(&pool, user, 9, &[(1, "rpi4-uboot-sd", None)]).await?;

        let host = insert_live_host(&pool, user).await?;
        describe_host_with_profile(&pool, host, "q35-virtio-uefi", 4096).await?;
        let job = enqueue(
            &pool,
            token,
            JobInitSpec::ImageSet {
                set_id: set,
                generation: None,
            },
            DEFAULT_HOST_CEL_PREDICATE,
            Utc::now(),
        )
        .await?;

        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, None);
        assert_eq!(job_state(&pool, job).await?, "queued");

        // A host that does advertise the profile takes it.
        let arm = insert_live_host(&pool, user).await?;
        describe_host_with_profile(&pool, arm, "rpi4-uboot-sd", 4096).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, arm).await?, Some(job));
        Ok(())
    }

    /// One image may serve several profiles now that the primary key is
    /// (set_id, generation, index) rather than keyed on the image.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn one_image_may_appear_under_several_profiles(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;

        // Both members name the same seed, hence the same image.
        let (set, digests) = register_set(
            &pool,
            user,
            9,
            &[(1, "q35-virtio-uefi", None), (1, "q35-virtio-bios", None)],
        )
        .await?;
        assert_eq!(digests[0], digests[1]);

        let host = insert_live_host(&pool, user).await?;
        describe_host_with_profile(&pool, host, "q35-virtio-bios", 4096).await?;
        let job = enqueue(
            &pool,
            token,
            JobInitSpec::ImageSet {
                set_id: set,
                generation: None,
            },
            DEFAULT_HOST_CEL_PREDICATE,
            Utc::now(),
        )
        .await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        assert_eq!(resolved_digest(&pool, job).await?, digests[0]);
        Ok(())
    }

    // -- maintenance ---------------------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn maintenance_hosts_are_not_eligible(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;
        let host = insert_live_host(&pool, user).await?;
        let job = enqueue_image(&pool, token, img).await?;

        set_maintenance(&pool, host, true).await?;
        assert!(
            eligible(&pool, job, Utc::now() - Duration::seconds(60))
                .await?
                .is_empty()
        );
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, None);

        set_maintenance(&pool, host, false).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        Ok(())
    }

    /// A host in maintenance must not be preempted either, or a running job is
    /// killed to free capacity nothing can use.
    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn maintenance_hosts_are_not_reclaimable(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, user).await?;
        let (_, img) = register_image(&pool, user, 1, true).await?;

        let running = enqueue_image(&pool, token, img).await?;
        occupy_host(&pool, host, running, 120, 60, "preempt").await?;
        let waiting = enqueue_image(&pool, token, img).await?;

        set_maintenance(&pool, host, true).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(
            terminate_request(&pool, running).await?,
            None,
            "a host in maintenance must not be preempted"
        );
        assert_eq!(job_state(&pool, waiting).await?, "queued");

        // Out of maintenance the same host is reclaimable again, so the test
        // above is about maintenance and not about some other missing condition.
        set_maintenance(&pool, host, false).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(
            terminate_request(&pool, running).await?.as_deref(),
            Some("preempted")
        );
        Ok(())
    }
}

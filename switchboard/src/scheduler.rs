//! The job scheduler (`doc/oci-image-migration-plan.md` §8.3).
//!
//! A background task that periodically places `queued` jobs onto eligible hosts.
//! It coordinates with the per-host [`SupervisorWSWorker`] **entirely through the
//! database** — it never holds an in-process handle to a worker — so the two can
//! be distributed across processes later. The scheduler writes the assignment
//! (`hosts.current_job` + `jobs.job_state = 'scheduled'`); the host's own worker
//! observes it and issues `StartJob`. For now this is poll-based; Postgres
//! `LISTEN`/`NOTIFY` can replace the polling later without schema changes.
//!
//! Each pass:
//!   1. streams `queued` jobs oldest-first;
//!   2. for each, asks the DB for eligible hosts via
//!      [`tml_switchboard.eligible_hosts`](../../SCHEMA.sql) (idle + live + host-tag
//!      containment — the set logic SQL does well);
//!   3. attempts each candidate under a row lock in [`Scheduler::try_assign`],
//!      which layers on the target/DUT bipartite match and the image resolution
//!      (neither of which belongs in SQL) and commits the assignment.
//!
//! [`SupervisorWSWorker`]: crate::supervisor_ws_worker::SupervisorWSWorker

use chrono::{TimeDelta, Utc};
use futures_util::TryStreamExt;
use sqlx::PgPool;
use uuid::Uuid;

use crate::matcher::{self, TargetCandidate};
use crate::sql;
use crate::sql::job::{ImageResolveError, SqlJobState};

/// Periodically dispatches queued jobs onto eligible hosts. Holds only a pool
/// handle (DB-only coordination).
pub struct Scheduler {
    pool: PgPool,
    /// Interval between scheduling passes.
    match_interval: TimeDelta,
    /// How recently a host must have heartbeat to be considered live.
    host_liveness_timeout: TimeDelta,
}

/// Outcome of attempting to place one job on one candidate host.
#[derive(Debug, PartialEq, Eq)]
enum AssignOutcome {
    /// The job was scheduled onto the host.
    Assigned,
    /// The host does not admit this job (DUT requirements unmet, or no image
    /// group member matches it) — try the next candidate host.
    HostRejected,
    /// The host was taken/no-longer-live by the time we locked it — try the next
    /// candidate host.
    HostTaken,
    /// The job is no longer schedulable (already taken by another scheduler, or
    /// finalized here as an image error) — stop considering it this pass.
    JobDone,
}

impl Scheduler {
    pub fn new(pool: PgPool, match_interval: TimeDelta, host_liveness_timeout: TimeDelta) -> Self {
        Self {
            pool,
            match_interval,
            host_liveness_timeout,
        }
    }

    /// Run the scheduling loop forever (until the task is dropped).
    pub async fn run(self) {
        let period = self
            .match_interval
            .to_std()
            .unwrap_or_else(|_| std::time::Duration::from_secs(10));
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            if let Err(e) = self.tick().await {
                tracing::error!("scheduler pass failed: {e:?}");
            }
        }
    }

    /// One scheduling pass: stream queued jobs oldest-first and try to place each.
    async fn tick(&self) -> anyhow::Result<()> {
        let cutoff = Utc::now() - self.host_liveness_timeout;

        // Stream (rather than collect) the queue: the cursor holds one pooled
        // connection while per-job work below borrows others.
        let mut queued = sqlx::query!(
            r#"select job_id
               from tml_switchboard.jobs
               where job_state = 'queued'
               order by queued_at"#
        )
        .fetch(&self.pool);

        while let Some(row) = queued.try_next().await? {
            let job_id = row.job_id;

            // The DB set-filter: idle + live + host-tag eligible. A host placed
            // earlier in this same pass is already excluded here (it reads
            // committed `current_job`), so no in-memory host bookkeeping.
            //
            // TODO(authz): `eligible_hosts` does not yet restrict to hosts the
            // job's enqueuing principal may use (ownership / `start` grant via
            // `principals()`). Until that predicate is folded into the SQL
            // function, a job can be placed on any tag-eligible host.
            let candidates = sqlx::query_scalar!(
                r#"select eligible_hosts as "host_id!"
                   from tml_switchboard.eligible_hosts($1, $2)"#,
                job_id,
                cutoff,
            )
            .fetch_all(&self.pool)
            .await?;

            for host_id in candidates {
                match self.try_assign(job_id, host_id).await? {
                    AssignOutcome::Assigned | AssignOutcome::JobDone => break,
                    AssignOutcome::HostRejected | AssignOutcome::HostTaken => continue,
                }
            }
        }

        Ok(())
    }

    /// Attempt to place `job_id` on `host_id` in a single guarded transaction.
    ///
    /// Locks the host row first (host-before-job order, matching the worker, so
    /// the two never deadlock), re-validates idle/live/host-tag and the DUT
    /// match under the lock, resolves the image *in the transaction*, then writes
    /// the assignment with `WHERE current_job IS NULL` / `WHERE job_state =
    /// 'queued'` guards. A lost race against another scheduler is therefore a
    /// clean no-op, which is what makes this safe to run in multiple processes.
    async fn try_assign(&self, job_id: Uuid, host_id: Uuid) -> anyhow::Result<AssignOutcome> {
        let cutoff = Utc::now() - self.host_liveness_timeout;
        let mut txn = self.pool.begin().await?;

        // Lock the host and re-assert it is idle and live.
        let host = sqlx::query!(
            r#"select current_job, tags, last_seen_at
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

        // Re-check host-tag eligibility under the lock (defensive: `tags` could
        // have changed since `eligible_hosts` ran).
        let host_tags = matcher::host_tag_set(&host.tags);
        if !job
            .host_tag_requirements()
            .iter()
            .all(|t| host_tags.contains(t))
        {
            return Ok(AssignOutcome::HostRejected);
        }

        // Target/DUT admission: every requested target must map to a distinct
        // wired DUT satisfying it. Admission only — nothing is stored.
        let duts = sql::host::targets_for_host(host_id, &mut *txn).await?;
        let reqs = sql::job::target_requirements_for_job(job_id, &mut *txn).await?;
        let dut_candidates: Vec<TargetCandidate<Uuid>> = duts
            .into_iter()
            .map(|d| TargetCandidate {
                handle: d.target_id,
                tags: d.tags.into_iter().collect(),
            })
            .collect();
        if matcher::match_targets(&reqs, &dut_candidates).is_none() {
            return Ok(AssignOutcome::HostRejected);
        }

        // Resolve the image against the chosen host, inside the transaction.
        // The resolved spec itself is rebuilt at dispatch from the recorded
        // `resolved_image_id`; here we only need resolution to succeed (validating
        // the image / picking the group member) and the id to pin.
        let (_spec, resolved_image_id) = match job.resolve_image_spec(&host_tags, &mut txn).await {
            Ok(resolved) => resolved,
            // No group member matches this host: a different host might, so this
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
               set job_state = 'scheduled',
                   dispatched_on_host_id = $2,
                   last_updated_at = default
               where job_id = $1 and job_state = 'queued'"#,
            job_id,
            host_id,
        )
        .execute(&mut *txn)
        .await?;
        if let Some(image_id) = resolved_image_id {
            sql::job::set_resolved_image(job_id, image_id, &mut *txn).await?;
        }

        txn.commit().await?;

        tracing::debug!(
            %host_id,
            %job_id,
            "assigned job to host"
        );
        Ok(AssignOutcome::Assigned)
    }
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
    use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest};
    use treadmill_rs::api::switchboard_supervisor::RestartPolicy;
    use treadmill_rs::image::{Digest, media_types};

    fn scheduler(pool: PgPool) -> Scheduler {
        Scheduler::new(pool, Duration::seconds(1), Duration::seconds(60))
    }

    /// A deterministic distinct digest per `seed`.
    fn digest(seed: u8) -> Digest {
        Digest::from_sha256([seed; 32])
    }

    fn tags(ts: &[&str]) -> Vec<String> {
        ts.iter().map(|s| s.to_string()).collect()
    }

    async fn insert_user(pool: &PgPool) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("insert into tml_switchboard.subjects (subject_id, kind) values ($1, 'user')")
            .bind(id)
            .execute(pool)
            .await?;
        sqlx::query("insert into tml_switchboard.users (subject_id, username) values ($1, $2)")
            .bind(id)
            .bind(format!("user-{id}"))
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
        .bind(vec![0u8; 128])
        .bind(user_id)
        .execute(pool)
        .await?;
        Ok(id)
    }

    /// Insert a host with the given tags and liveness. `last_seen` of `None`
    /// leaves the host not-live (no connected worker).
    async fn insert_host(
        pool: &PgPool,
        host_tags: &[&str],
        last_seen: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        // `auth_token` is UNIQUE and must be exactly 128 bytes; seed it from the
        // host id so multiple hosts in one test don't collide.
        let mut auth_token = vec![0u8; 128];
        auth_token[..16].copy_from_slice(id.as_bytes());
        sqlx::query(
            "insert into tml_switchboard.hosts \
             (host_id, name, auth_token, tags, ssh_endpoints, worker_instance_id, last_seen_at) \
             values ($1, $2, $3, $4, '{}'::tml_switchboard.ssh_endpoint[], 0, $5)",
        )
        .bind(id)
        .bind(format!("host-{id}"))
        .bind(auth_token)
        .bind(tags(host_tags))
        .bind(last_seen)
        .execute(pool)
        .await?;
        Ok(id)
    }

    /// A live host (heartbeat now).
    async fn insert_live_host(pool: &PgPool, host_tags: &[&str]) -> anyhow::Result<Uuid> {
        insert_host(pool, host_tags, Some(Utc::now())).await
    }

    async fn add_target(pool: &PgPool, host_id: Uuid, target_tags: &[&str]) -> anyhow::Result<()> {
        sqlx::query(
            "insert into tml_switchboard.host_targets (target_id, host_id, name, tags) \
             values ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(host_id)
        .bind(format!("dut-{}", Uuid::new_v4()))
        .bind(tags(target_tags))
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
            owner,
            None,
            &serde_json::json!({}),
        )
        .await?;
        if with_location {
            sql::image::upsert_location(&mut *tx, id, "reg.example:5000", "repo", "external")
                .await?;
        }
        tx.commit().await?;
        Ok((id, d))
    }

    /// Register an image group (named `group-{name_seed}`) with one generation
    /// whose members are `(seed, required_host_tags)`. Returns the group's id and
    /// each member's manifest digest (in member order).
    async fn register_group(
        pool: &PgPool,
        owner: Uuid,
        name_seed: u8,
        members: &[(u8, &[&str])],
    ) -> anyhow::Result<(Uuid, Vec<Digest>)> {
        let gid = Uuid::new_v4();
        let mut tx = pool.begin().await?;
        sql::image::create_group(&mut *tx, gid, &format!("group-{name_seed}"), owner, None).await?;
        let mut member_rows = Vec::new();
        let mut member_digests = Vec::new();
        for (index, (seed, req_tags)) in members.iter().enumerate() {
            let md = digest(*seed);
            let img_id = Uuid::new_v4();
            sql::image::insert(
                &mut *tx,
                img_id,
                &md.encoded(),
                media_types::IMAGE_ARTIFACT_TYPE,
                owner,
                None,
                &serde_json::json!({}),
            )
            .await?;
            sql::image::upsert_location(&mut *tx, img_id, "reg.example:5000", "repo", "external")
                .await?;
            member_rows.push((img_id, tags(req_tags), index as i32));
            member_digests.push(md);
        }
        sql::image::create_generation(&mut tx, gid, owner, &member_rows).await?;
        tx.commit().await?;
        Ok((gid, member_digests))
    }

    #[allow(clippy::too_many_arguments)]
    async fn enqueue(
        pool: &PgPool,
        token: Uuid,
        init_spec: JobInitSpec,
        host_tag_requirements: &[&str],
        target_requirements: &[&[&str]],
        queued_at: DateTime<Utc>,
    ) -> anyhow::Result<Uuid> {
        let job_id = Uuid::new_v4();
        let req = JobRequest {
            init_spec,
            owner: None,
            ssh_keys: vec![],
            restart_policy: RestartPolicy {
                remaining_restart_count: 0,
            },
            parameters: HashMap::new(),
            host_tag_requirements: tags(host_tag_requirements),
            target_requirements: target_requirements.iter().map(|r| tags(r)).collect(),
            override_timeout: None,
        };
        let mut tx = pool.begin().await?;
        sql::job::insert(
            req,
            job_id,
            token,
            None,
            PgInterval::try_from(Duration::hours(1)).unwrap(),
            queued_at,
            &mut tx,
        )
        .await?;
        tx.commit().await?;
        Ok(job_id)
    }

    /// Convenience: enqueue a concrete-image job by image id.
    async fn enqueue_image(
        pool: &PgPool,
        token: Uuid,
        image: Uuid,
        host_tag_requirements: &[&str],
        target_requirements: &[&[&str]],
    ) -> anyhow::Result<Uuid> {
        enqueue(
            pool,
            token,
            JobInitSpec::Image { image },
            host_tag_requirements,
            target_requirements,
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

    // -- eligible_hosts SQL function ----------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_filters_idle_live_and_tags(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let cutoff = Utc::now() - Duration::seconds(60);

        let good = insert_live_host(&pool, &["arch=arm64", "rack=1"]).await?;
        let _missing_tag = insert_live_host(&pool, &["arch=amd64"]).await?;
        let _dead = insert_host(&pool, &["arch=arm64"], None).await?;
        let _stale = insert_host(
            &pool,
            &["arch=arm64"],
            Some(Utc::now() - Duration::seconds(120)),
        )
        .await?;
        let busy = insert_live_host(&pool, &["arch=arm64"]).await?;

        let (img, _) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;
        // Make `busy` busy by pointing its current_job at an unrelated job.
        let other = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;
        sqlx::query("update tml_switchboard.hosts set current_job = $1 where host_id = $2")
            .bind(other)
            .bind(busy)
            .execute(&pool)
            .await?;

        let got = eligible(&pool, job, cutoff).await?;
        assert_eq!(got, vec![good], "only the idle, live, tag-matching host");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn eligible_hosts_empty_requirements_matches_all_idle_live(
        pool: PgPool,
    ) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let cutoff = Utc::now() - Duration::seconds(60);
        let a = insert_live_host(&pool, &["x"]).await?;
        let b = insert_live_host(&pool, &[]).await?;
        let (img, _) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img, &[], &[]).await?;
        let mut got = eligible(&pool, job, cutoff).await?;
        got.sort();
        let mut want = vec![a, b];
        want.sort();
        assert_eq!(got, want);
        Ok(())
    }

    // -- scheduler dispatch -------------------------------------------------

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn schedules_concrete_image_onto_eligible_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, &["arch=arm64", "rack=1"]).await?;
        let (img, img_digest) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        assert_eq!(job_state(&pool, job).await?, "scheduled");
        assert_eq!(job_dispatched_host(&pool, job).await?, Some(host));
        assert_eq!(
            job_resolved_digest(&pool, job).await?,
            Some(img_digest.encoded())
        );
        assert!(
            job_started_at(&pool, job).await?.is_none(),
            "started_at stays null until the job actually initializes"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn image_group_resolves_most_specific_member(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, &["arch=arm64", "rpi4"]).await?;
        // Member 0 is generic; member 1 is more specific and also admissible.
        let (group, members) = register_group(
            &pool,
            user,
            9,
            &[(1, &["arch=arm64"]), (2, &["arch=arm64", "rpi4"])],
        )
        .await?;
        let job = enqueue(
            &pool,
            token,
            JobInitSpec::ImageGroup {
                image_group: group,
                generation: None,
            },
            &["arch=arm64"],
            &[],
            Utc::now(),
        )
        .await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(job));
        assert_eq!(
            job_resolved_digest(&pool, job).await?,
            Some(members[1].encoded()),
            "the most-specific admissible member is chosen"
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_schedule_when_no_eligible_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        // Dead host (no heartbeat) and a live host missing the required tag.
        let _dead = insert_host(&pool, &["arch=arm64"], None).await?;
        let _wrong = insert_live_host(&pool, &["arch=amd64"]).await?;
        let (img, _) = register_image(&pool, user, 1, true).await?;
        let job = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(job_state(&pool, job).await?, "queued", "job stays queued");
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn target_requirements_gate_scheduling(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let (img, _) = register_image(&pool, user, 1, true).await?;

        // Host has a single nRF DUT; a job needing one nRF schedules.
        let host = insert_live_host(&pool, &["arch=arm64"]).await?;
        add_target(&pool, host, &["board=nrf52840dk", "ble"]).await?;
        let ok =
            enqueue_image(&pool, token, img, &["arch=arm64"], &[&["board=nrf52840dk"]]).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(job_state(&pool, ok).await?, "scheduled");
        assert_eq!(host_current_job(&pool, host).await?, Some(ok));

        // A second host with only ONE nRF DUT cannot satisfy a job needing TWO.
        let host2 = insert_live_host(&pool, &["arch=arm64"]).await?;
        add_target(&pool, host2, &["board=nrf52840dk"]).await?;
        let needs_two = enqueue_image(
            &pool,
            token,
            img,
            &["arch=arm64"],
            &[&["board=nrf52840dk"], &["board=nrf52840dk"]],
        )
        .await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(
            job_state(&pool, needs_two).await?,
            "queued",
            "one DUT cannot satisfy two distinct requirements"
        );

        // Wire a second nRF DUT onto host2 → now it can.
        add_target(&pool, host2, &["board=nrf52840dk"]).await?;
        scheduler(pool.clone()).tick().await?;
        assert_eq!(job_state(&pool, needs_two).await?, "scheduled");
        assert_eq!(host_current_job(&pool, host2).await?, Some(needs_two));
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn oldest_job_wins_the_single_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, &["arch=arm64"]).await?;
        let (img, _) = register_image(&pool, user, 1, true).await?;

        let now = Utc::now();
        let older = enqueue(
            &pool,
            token,
            JobInitSpec::Image { image: img },
            &["arch=arm64"],
            &[],
            now - Duration::seconds(10),
        )
        .await?;
        let newer = enqueue(
            &pool,
            token,
            JobInitSpec::Image { image: img },
            &["arch=arm64"],
            &[],
            now,
        )
        .await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(host_current_job(&pool, host).await?, Some(older));
        assert_eq!(job_state(&pool, older).await?, "scheduled");
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
        let _host = insert_live_host(&pool, &["arch=arm64"]).await?;
        // An image id that was never registered in the catalog. The
        // `jobs.image_id` foreign key rejects the enqueue outright, instead of
        // deferring to a dispatch-time image error as the old digest column did.
        let unregistered = Uuid::new_v4();
        let result = enqueue_image(&pool, token, unregistered, &["arch=arm64"], &[]).await;

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
        let _host = insert_live_host(&pool, &["arch=arm64"]).await?;
        // Registered, but with no registry location to pull from.
        let (img, _) = register_image(&pool, user, 1, false).await?;
        let job = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;

        scheduler(pool.clone()).tick().await?;

        assert_eq!(job_state(&pool, job).await?, "finalized");
        assert_eq!(
            job_termination(&pool, job).await?.as_deref(),
            Some("image_error")
        );
        Ok(())
    }

    #[sqlx::test(migrations = "./migrations")]
    #[ignore = "needs Postgres; run via `cargo nextest run --run-ignored only`"]
    async fn does_not_reassign_a_busy_host(pool: PgPool) -> anyhow::Result<()> {
        let user = insert_user(&pool).await?;
        let token = insert_token(&pool, user).await?;
        let host = insert_live_host(&pool, &["arch=arm64"]).await?;
        let (img, _) = register_image(&pool, user, 1, true).await?;

        // Host already running a job.
        let running = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;
        sqlx::query("update tml_switchboard.hosts set current_job = $1 where host_id = $2")
            .bind(running)
            .bind(host)
            .execute(&pool)
            .await?;
        sqlx::query("update tml_switchboard.jobs set job_state='scheduled', dispatched_on_host_id=$2 where job_id=$1")
            .bind(running)
            .bind(host)
            .execute(&pool)
            .await?;

        let waiting = enqueue_image(&pool, token, img, &["arch=arm64"], &[]).await?;
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
}

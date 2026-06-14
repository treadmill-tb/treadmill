use super::SqlSshEndpoint;
use super::image;
use crate::matcher::{GroupMember, select_member};
use chrono::{DateTime, TimeDelta, Utc};
use sqlx::postgres::types::PgInterval;
use sqlx::{PgExecutor, Postgres, Transaction};
use std::collections::BTreeSet;
use treadmill_rs::api::switchboard::jobs::{
    JobImageRef, JobInfo, JobParameterView, JobSummary, SshEndpoint,
};
use treadmill_rs::api::switchboard::{JobInitSpec, JobRequest, JobState, TerminationReason};
use treadmill_rs::api::switchboard_supervisor::{
    ImageLocation, ImageSpecification, JobInitializingStage, RestartPolicy, RunningJobState,
    StartJobMessage, TaskExitStatus,
};
use treadmill_rs::connector::JobErrorKind;
use treadmill_rs::image::Digest;
use uuid::Uuid;

pub mod parameters;

#[derive(Debug, Copy, Clone, PartialEq, Eq, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.job_state", rename_all = "snake_case")]
pub enum SqlJobState {
    Queued,
    Scheduled,
    Initializing,
    Ready,
    Terminating,
    Finalized,
}
impl From<SqlJobState> for JobState {
    fn from(value: SqlJobState) -> Self {
        match value {
            SqlJobState::Queued => JobState::Queued,
            SqlJobState::Scheduled => JobState::Scheduled,
            SqlJobState::Initializing => JobState::Initializing,
            SqlJobState::Ready => JobState::Ready,
            SqlJobState::Terminating => JobState::Terminating,
            SqlJobState::Finalized => JobState::Finalized,
        }
    }
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.job_initializing_stage",
    rename_all = "snake_case"
)]
pub enum SqlJobInitializingStage {
    Starting,
    FetchingImage,
    Allocating,
    Provisioning,
    Booting,
}
impl From<SqlJobInitializingStage> for JobInitializingStage {
    fn from(value: SqlJobInitializingStage) -> Self {
        match value {
            SqlJobInitializingStage::Starting => JobInitializingStage::Starting,
            SqlJobInitializingStage::FetchingImage => JobInitializingStage::FetchingImage,
            SqlJobInitializingStage::Allocating => JobInitializingStage::Allocating,
            SqlJobInitializingStage::Provisioning => JobInitializingStage::Provisioning,
            SqlJobInitializingStage::Booting => JobInitializingStage::Booting,
        }
    }
}
impl From<JobInitializingStage> for SqlJobInitializingStage {
    fn from(value: JobInitializingStage) -> Self {
        match value {
            JobInitializingStage::Starting => SqlJobInitializingStage::Starting,
            JobInitializingStage::FetchingImage => SqlJobInitializingStage::FetchingImage,
            JobInitializingStage::Allocating => SqlJobInitializingStage::Allocating,
            JobInitializingStage::Provisioning => SqlJobInitializingStage::Provisioning,
            JobInitializingStage::Booting => SqlJobInitializingStage::Booting,
        }
    }
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.termination_reason",
    rename_all = "snake_case"
)]
pub enum SqlTerminationReason {
    WorkloadExited,
    WorkloadSelfCanceled,
    UserCanceled,
    QueueTimeout,
    ExecutionTimeout,
    ImageError,
    HostMatchError,
    HostStartFailure,
    HostDroppedJob,
    HostUnreachable,
    ResumeFailed,
    InternalError,
}
impl From<SqlTerminationReason> for TerminationReason {
    fn from(value: SqlTerminationReason) -> Self {
        match value {
            SqlTerminationReason::WorkloadExited => TerminationReason::WorkloadExited,
            SqlTerminationReason::WorkloadSelfCanceled => TerminationReason::WorkloadSelfCanceled,
            SqlTerminationReason::UserCanceled => TerminationReason::UserCanceled,
            SqlTerminationReason::QueueTimeout => TerminationReason::QueueTimeout,
            SqlTerminationReason::ExecutionTimeout => TerminationReason::ExecutionTimeout,
            SqlTerminationReason::ImageError => TerminationReason::ImageError,
            SqlTerminationReason::HostMatchError => TerminationReason::HostMatchError,
            SqlTerminationReason::HostStartFailure => TerminationReason::HostStartFailure,
            SqlTerminationReason::HostDroppedJob => TerminationReason::HostDroppedJob,
            SqlTerminationReason::HostUnreachable => TerminationReason::HostUnreachable,
            SqlTerminationReason::ResumeFailed => TerminationReason::ResumeFailed,
            SqlTerminationReason::InternalError => TerminationReason::InternalError,
        }
    }
}
impl From<TerminationReason> for SqlTerminationReason {
    fn from(value: TerminationReason) -> Self {
        match value {
            TerminationReason::WorkloadExited => SqlTerminationReason::WorkloadExited,
            TerminationReason::WorkloadSelfCanceled => SqlTerminationReason::WorkloadSelfCanceled,
            TerminationReason::UserCanceled => SqlTerminationReason::UserCanceled,
            TerminationReason::QueueTimeout => SqlTerminationReason::QueueTimeout,
            TerminationReason::ExecutionTimeout => SqlTerminationReason::ExecutionTimeout,
            TerminationReason::ImageError => SqlTerminationReason::ImageError,
            TerminationReason::HostMatchError => SqlTerminationReason::HostMatchError,
            TerminationReason::HostStartFailure => SqlTerminationReason::HostStartFailure,
            TerminationReason::HostDroppedJob => SqlTerminationReason::HostDroppedJob,
            TerminationReason::HostUnreachable => SqlTerminationReason::HostUnreachable,
            TerminationReason::ResumeFailed => SqlTerminationReason::ResumeFailed,
            TerminationReason::InternalError => SqlTerminationReason::InternalError,
        }
    }
}

#[derive(Debug, Copy, Clone, sqlx::Type)]
#[sqlx(
    type_name = "tml_switchboard.task_exit_status",
    rename_all = "snake_case"
)]
pub enum SqlTaskExitStatus {
    Pending,
    Success,
    Failure,
}
impl From<SqlTaskExitStatus> for TaskExitStatus {
    fn from(value: SqlTaskExitStatus) -> Self {
        match value {
            SqlTaskExitStatus::Pending => TaskExitStatus::Pending,
            SqlTaskExitStatus::Success => TaskExitStatus::Success,
            SqlTaskExitStatus::Failure => TaskExitStatus::Failure,
        }
    }
}
impl From<TaskExitStatus> for SqlTaskExitStatus {
    fn from(value: TaskExitStatus) -> Self {
        match value {
            TaskExitStatus::Pending => SqlTaskExitStatus::Pending,
            TaskExitStatus::Success => SqlTaskExitStatus::Success,
            TaskExitStatus::Failure => SqlTaskExitStatus::Failure,
        }
    }
}

#[derive(Debug, Clone, sqlx::Type)]
#[sqlx(type_name = "tml_switchboard.restart_policy")]
pub struct SqlRestartPolicy {
    pub remaining_restart_count: i32,
}
impl From<SqlRestartPolicy> for RestartPolicy {
    fn from(value: SqlRestartPolicy) -> Self {
        Self {
            remaining_restart_count: usize::try_from(value.remaining_restart_count).unwrap_or(0),
        }
    }
}

pub async fn insert(
    job_request: JobRequest,
    as_job_id: Uuid,
    as_token_id: Uuid,
    owner: Option<Uuid>,
    job_timeout: PgInterval,
    queued_at: DateTime<Utc>,
    conn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let (resume_job_id, restart_job_id, image_digest, image_group_digest): (
        Option<Uuid>,
        Option<Uuid>,
        Option<String>,
        Option<String>,
    ) = match job_request.init_spec {
        JobInitSpec::ResumeJob { job_id } => (Some(job_id), None, None, None),
        JobInitSpec::RestartJob { job_id } => {
            let predecessor = sqlx::query!(
                r#"
                    select resume_job_id, restart_job_id, image_digest, image_group_digest
                    from tml_switchboard.jobs
                    where job_id = $1
                    "#,
                job_id
            )
            .fetch_one(conn.as_mut())
            .await?;
            (
                predecessor.resume_job_id,
                Some(job_id),
                predecessor.image_digest,
                predecessor.image_group_digest,
            )
        }
        JobInitSpec::Image { image } => (None, None, Some(image.encoded()), None),
        JobInitSpec::ImageGroup { image_group } => (None, None, None, Some(image_group.encoded())),
    };

    sqlx::query!(
        r#"
        insert into tml_switchboard.jobs
        (
          job_id,
          owner_id,
          resume_job_id,
          restart_job_id,
          image_digest,
          image_group_digest,
          ssh_keys,
          restart_policy,
          enqueued_by_token_id,
          host_tag_requirements,
          job_timeout,
          job_state,
          initializing_stage,
          queued_at,
          started_at,
          dispatched_on_host_id,
          ssh_endpoints,
          termination_reason,
          task_exit_status,
          exit_message,
          terminated_at,
          last_updated_at
        )
        values (
          $1,       -- job_id
          $12,	    -- owner_id
          $2,	    -- resume_job_id
          $3,	    -- restart_job_id
          $4,	    -- image_digest
          $5,	    -- image_group_digest
          $6,	    -- ssh_keys
          $7,	    -- restart_policy
          $8,	    -- enqueued_by_token_id
          $9,	    -- host_tag_requirements
          $10,	    -- job_timeout
          'queued', -- job_state
          null,	    -- initializing_stage
          $11,	    -- queued_at
          null,	    -- started_at
          null,	    -- dispatched_on_host_id
          null,	    -- ssh_endpoints
          null,	    -- termination_reason
          null,	    -- task_exit_status
          null,	    -- exit_message
          null,	    -- terminated_at
          default   -- last_updated_at
        )
        "#,
        as_job_id,
        resume_job_id,
        restart_job_id,
        image_digest,
        image_group_digest,
        job_request.ssh_keys.as_slice(),
        SqlRestartPolicy {
            remaining_restart_count: i32::try_from(
                job_request.restart_policy.remaining_restart_count
            )
            .unwrap(),
        } as SqlRestartPolicy,
        as_token_id,
        job_request.host_tag_requirements.as_slice(),
        job_timeout,
        queued_at,
        owner,
    )
    .execute(conn.as_mut())
    .await?;

    // Record the requested targets (DUTs), one row per requested target,
    // numbered by position in the submitted array.
    for (req_index, tags) in job_request.target_requirements.iter().enumerate() {
        sqlx::query!(
            r#"insert into tml_switchboard.job_target_requirements
                 (job_id, req_index, tags)
               values ($1, $2, $3)"#,
            as_job_id,
            i32::try_from(req_index).expect("more than i32::MAX target requirements"),
            tags.as_slice(),
        )
        .execute(conn.as_mut())
        .await?;
    }

    Ok(())
}

/// The ordered target (DUT) requirements of a job: one tag set per requested
/// target, in submission order (`req_index`).
pub async fn target_requirements_for_job(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<Vec<String>>, sqlx::Error> {
    let rows = sqlx::query!(
        r#"select tags
           from tml_switchboard.job_target_requirements
           where job_id = $1
           order by req_index"#,
        job_id,
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|r| r.tags).collect())
}

#[allow(dead_code)]
pub struct SqlJob {
    job_id: Uuid,
    owner_id: Option<Uuid>,
    resume_job_id: Option<Uuid>,
    #[allow(dead_code)]
    restart_job_id: Option<Uuid>,
    // The job's image reference: a concrete image manifest digest, or an image
    // group's index digest (resolved to a concrete member at dispatch). Exactly
    // one is set for a non-resume job; both are null for a resume.
    image_digest: Option<String>,
    image_group_digest: Option<String>,
    // The concrete manifest digest actually dispatched, recorded at dispatch.
    #[allow(dead_code)]
    resolved_image_digest: Option<String>,

    ssh_keys: Vec<String>,
    sql_restart_policy: SqlRestartPolicy,
    enqueued_by_token_id: Uuid,
    host_tag_requirements: Vec<String>,
    job_timeout: PgInterval,

    job_state: SqlJobState,

    // Filled out while `job_state = 'initializing'`; null otherwise.
    #[allow(dead_code)]
    initializing_stage: Option<SqlJobInitializingStage>,

    // Filled out when initialized into `queued` job state
    queued_at: DateTime<Utc>,

    // Filled out if and when the job is dispatched onto a host
    started_at: Option<DateTime<Utc>>,
    dispatched_on_host_id: Option<Uuid>,
    ssh_endpoints: Option<Vec<SqlSshEndpoint>>,

    // The DB side of user-cancel: set when cancellation is requested, consumed by
    // the worker's reconcile (see `switchboard_stop_reason`).
    cancel_requested_at: Option<DateTime<Utc>>,

    // Filled out when transitioned into `finalized` job state
    #[allow(dead_code)]
    termination_reason: Option<SqlTerminationReason>,
    #[allow(dead_code)]
    task_exit_status: Option<SqlTaskExitStatus>,
    #[allow(dead_code)]
    exit_message: Option<String>,
    #[allow(dead_code)]
    terminated_at: Option<DateTime<Utc>>,

    #[allow(dead_code)]
    last_updated_at: DateTime<Utc>,
}

#[allow(dead_code)]
impl SqlJob {
    pub fn job_id(&self) -> Uuid {
        self.job_id
    }
    /// Resolve this job's image reference into the content-addressed
    /// [`ImageSpecification`] handed to the supervisor at dispatch.
    ///
    /// For a resume job this is simply [`ImageSpecification::ResumeJob`]. For a
    /// concrete image, the manifest digest is paired with its catalog locations.
    /// For an image *group*, the chosen host's `host_tags` select the concrete
    /// member (the §8.3 matcher) whose digest + locations are then dispatched.
    /// `host_tags` is ignored for the non-group variants.
    ///
    /// The returned [`ImageSpecification::Image::manifest_digest`] is the
    /// concrete digest to record on the job row via [`set_resolved_image`].
    pub async fn resolve_image_spec(
        &self,
        host_tags: &BTreeSet<String>,
        conn: &mut sqlx::PgConnection,
    ) -> Result<ImageSpecification, ImageResolveError> {
        if let Some(resume_job_id) = self.resume_job_id {
            return Ok(ImageSpecification::ResumeJob {
                job_id: resume_job_id,
            });
        }

        if let Some(digest) = &self.image_digest {
            let rec = image::fetch_by_digest(&mut *conn, digest)
                .await?
                .ok_or_else(|| ImageResolveError::NotRegistered(digest.clone()))?;
            return concrete_image_spec(rec.id, digest, conn).await;
        }

        if let Some(group_digest) = &self.image_group_digest {
            let group = image::fetch_group_by_digest(&mut *conn, group_digest)
                .await?
                .ok_or_else(|| ImageResolveError::NotRegistered(group_digest.clone()))?;
            let members = image::members_for_group(&mut *conn, group.id).await?;
            let candidates: Vec<GroupMember<(Uuid, String)>> = members
                .into_iter()
                .map(|m| GroupMember {
                    handle: (m.image_id, m.manifest_digest),
                    required_host_tags: m.required_host_tags,
                })
                .collect();
            let chosen =
                select_member(&candidates, host_tags).ok_or(ImageResolveError::NoMatchingMember)?;
            let (image_id, manifest_digest) = &chosen.handle;
            return concrete_image_spec(*image_id, manifest_digest, conn).await;
        }

        // `valid_init_spec` guarantees one of the branches above fires for a
        // non-resume job; reaching here means a row violated that invariant.
        Err(ImageResolveError::MalformedJob(self.job_id))
    }
    pub fn ssh_keys(&self) -> &[String] {
        &self.ssh_keys
    }
    pub fn restart_policy(&self) -> RestartPolicy {
        self.sql_restart_policy.clone().into()
    }
    pub fn enqueued_by_token_id(&self) -> Uuid {
        self.enqueued_by_token_id
    }
    pub fn host_tag_requirements(&self) -> &[String] {
        &self.host_tag_requirements
    }
    pub fn timeout(&self) -> TimeDelta {
        assert_eq!(
            self.job_timeout.months, 0,
            "invariant violation: job_timeout.months SHALL BE 0"
        );
        TimeDelta::microseconds(self.job_timeout.microseconds)
            + TimeDelta::days(i64::from(self.job_timeout.days))
    }
    pub fn queued_at(&self) -> &DateTime<Utc> {
        &self.queued_at
    }
    pub fn started_at(&self) -> Option<&DateTime<Utc>> {
        self.started_at.as_ref()
    }
    pub fn dispatched_on_host_id(&self) -> Option<Uuid> {
        self.dispatched_on_host_id
    }
    pub fn ssh_endpoints(&self) -> Option<&Vec<SqlSshEndpoint>> {
        self.ssh_endpoints.as_ref()
    }
    pub fn job_state(&self) -> SqlJobState {
        self.job_state
    }

    /// Render this job row into the [`JobInfo`] API view returned by
    /// `GET /jobs/{id}`.
    ///
    /// Reads the job's ordered target requirements and parameters (the latter
    /// **redacted**: secret values are withheld, see [`JobParameterView`]) and
    /// folds the four mutually-exclusive image columns into a single
    /// [`JobImageRef`] (resume → restart → concrete image → image group, matching
    /// the row invariants in `SCHEMA.sql`). Stored digests are re-parsed; a
    /// malformed one is a data-integrity fault surfaced as
    /// [`JobInfoError::Digest`].
    pub async fn into_info(self, conn: &mut sqlx::PgConnection) -> Result<JobInfo, JobInfoError> {
        let target_requirements = target_requirements_for_job(self.job_id, &mut *conn).await?;
        let parameters = parameters::fetch_by_job_id(self.job_id, &mut *conn)
            .await?
            .into_iter()
            .map(|(key, value)| {
                let view = JobParameterView {
                    secret: value.secret,
                    // Withhold the plaintext of a secret parameter.
                    value: (!value.secret).then_some(value.value),
                };
                (key, view)
            })
            .collect();

        // Borrows `self.job_timeout`; compute before the image match moves the
        // digest fields out of `self`.
        let timeout_secs = self.timeout().num_seconds();

        let image = job_image_ref(
            self.resume_job_id,
            self.restart_job_id,
            self.image_digest,
            self.image_group_digest,
            self.job_id,
        )?;

        let resolved_image_digest = self
            .resolved_image_digest
            .map(|d| d.parse().map_err(|_| JobInfoError::Digest(d)))
            .transpose()?;

        Ok(JobInfo {
            job_id: self.job_id,
            owner_id: self.owner_id,
            state: self.job_state.into(),
            initializing_stage: self.initializing_stage.map(Into::into),
            image,
            resolved_image_digest,
            ssh_keys: self.ssh_keys,
            restart_policy: self.sql_restart_policy.into(),
            host_tag_requirements: self.host_tag_requirements,
            target_requirements,
            parameters,
            timeout_secs,
            queued_at: self.queued_at,
            started_at: self.started_at,
            dispatched_on_host_id: self.dispatched_on_host_id,
            ssh_endpoints: self.ssh_endpoints.map(|eps| {
                eps.into_iter()
                    .map(|ep| SshEndpoint {
                        ssh_host: ep.ssh_host.into(),
                        ssh_port: ep.ssh_port.into(),
                    })
                    .collect()
            }),
            termination_reason: self.termination_reason.map(Into::into),
            task_exit_status: self.task_exit_status.map(Into::into),
            exit_message: self.exit_message,
            terminated_at: self.terminated_at,
            last_updated_at: self.last_updated_at,
        })
    }

    /// The switchboard-side reason this *assigned* job should be stopped, if any
    /// — the convergence pre-check shared by execution-timeout and user-cancel.
    ///
    /// Re-derived from fresh DB state on every reconcile pass (never cached), so
    /// an extended deadline or a (future) cleared cancel is honored right up to
    /// the moment the job is actually stopped:
    ///   - `cancel_requested_at` set                  → `UserCanceled`
    ///   - started and past `started_at + job_timeout` → `ExecutionTimeout`
    ///
    /// User-cancel takes precedence over an also-expired deadline. Returns `None`
    /// for a job that should keep running — including a not-yet-started
    /// (`scheduled`) job that is not canceled, since queue-timeout is the
    /// scheduler's concern, not the worker's.
    pub fn switchboard_stop_reason(&self, now: DateTime<Utc>) -> Option<SqlTerminationReason> {
        if self.cancel_requested_at.is_some() {
            return Some(SqlTerminationReason::UserCanceled);
        }
        if let Some(started_at) = self.started_at
            && started_at + self.timeout() <= now
        {
            return Some(SqlTerminationReason::ExecutionTimeout);
        }
        None
    }
}

pub async fn fetch_by_job_id(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<SqlJob, sqlx::Error> {
    sqlx::query_as!(
        SqlJob,
        r#"
        select job_id, owner_id, resume_job_id, restart_job_id, image_digest, image_group_digest,
        resolved_image_digest, ssh_keys,
        restart_policy as "sql_restart_policy: _", enqueued_by_token_id,
        host_tag_requirements, job_timeout,
        queued_at, job_state as "job_state: _",
        initializing_stage as "initializing_stage: _", started_at,
        dispatched_on_host_id, ssh_endpoints as "ssh_endpoints: _",
        cancel_requested_at,
        termination_reason as "termination_reason: _",
        task_exit_status as "task_exit_status: _", exit_message, terminated_at,
        last_updated_at
        from tml_switchboard.jobs where job_id = $1;
        "#,
        job_id
    )
    .fetch_one(conn)
    .await
}

/// Why resolving a job's image reference to a concrete dispatch spec failed.
#[derive(Debug)]
pub enum ImageResolveError {
    /// An underlying database error.
    Db(sqlx::Error),
    /// The referenced image/group digest is not in the catalog (e.g. it was
    /// deregistered after the job was queued).
    NotRegistered(String),
    /// The image is registered but has no location to pull it from (D16: a
    /// user's only external registry became unavailable).
    NoLocations(String),
    /// No group member matched the chosen host's attributes (§8.3).
    NoMatchingMember,
    /// The job row violated `valid_init_spec` (neither image nor group set on a
    /// non-resume job). Should be impossible given the DB constraint.
    MalformedJob(Uuid),
}

impl From<sqlx::Error> for ImageResolveError {
    fn from(e: sqlx::Error) -> Self {
        ImageResolveError::Db(e)
    }
}

impl std::fmt::Display for ImageResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageResolveError::Db(e) => write!(f, "database error resolving image: {e}"),
            ImageResolveError::NotRegistered(d) => {
                write!(f, "image/group {d} is not registered in the catalog")
            }
            ImageResolveError::NoLocations(d) => {
                write!(f, "image {d} has no registry locations")
            }
            ImageResolveError::NoMatchingMember => {
                write!(f, "no group member matched the host's attributes")
            }
            ImageResolveError::MalformedJob(j) => {
                write!(f, "job {j} has neither an image nor a group digest")
            }
        }
    }
}

impl std::error::Error for ImageResolveError {}

/// Why rendering a job row into its [`JobInfo`] API view failed.
#[derive(Debug)]
pub enum JobInfoError {
    /// An underlying database error (target/parameter lookup).
    Db(sqlx::Error),
    /// A digest stored on the row did not parse (data-integrity fault).
    Digest(String),
    /// The row set neither an image, a group, nor a resume/restart reference,
    /// violating the `valid_init_spec` CHECK. Should be impossible.
    Malformed(Uuid),
}
impl From<sqlx::Error> for JobInfoError {
    fn from(e: sqlx::Error) -> Self {
        JobInfoError::Db(e)
    }
}
impl std::fmt::Display for JobInfoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JobInfoError::Db(e) => write!(f, "database error building JobInfo: {e}"),
            JobInfoError::Digest(d) => write!(f, "job row holds an unparseable digest {d}"),
            JobInfoError::Malformed(j) => {
                write!(
                    f,
                    "job {j} has no image, group, or resume/restart reference"
                )
            }
        }
    }
}
impl std::error::Error for JobInfoError {}

/// Fold a job row's four mutually-exclusive image columns into a single
/// [`JobImageRef`], following the row invariants in `SCHEMA.sql` (resume →
/// restart → concrete image → image group). A stored digest that does not parse
/// is a data-integrity fault ([`JobInfoError::Digest`]); a row that sets none of
/// the four violates `valid_init_spec` ([`JobInfoError::Malformed`]).
fn job_image_ref(
    resume_job_id: Option<Uuid>,
    restart_job_id: Option<Uuid>,
    image_digest: Option<String>,
    image_group_digest: Option<String>,
    job_id: Uuid,
) -> Result<JobImageRef, JobInfoError> {
    let parse_digest = |d: String| d.parse().map_err(|_| JobInfoError::Digest(d));
    if let Some(job_id) = resume_job_id {
        Ok(JobImageRef::Resume { job_id })
    } else if let Some(job_id) = restart_job_id {
        Ok(JobImageRef::Restart { job_id })
    } else if let Some(digest) = image_digest {
        Ok(JobImageRef::Image {
            digest: parse_digest(digest)?,
        })
    } else if let Some(digest) = image_group_digest {
        Ok(JobImageRef::ImageGroup {
            digest: parse_digest(digest)?,
        })
    } else {
        Err(JobInfoError::Malformed(job_id))
    }
}

/// Fetch a page of jobs the subject `caller` may **read**, newest first.
///
/// Visibility mirrors [`crate::auth::engine::can_access_job`] as a set query: a
/// job is included when `caller` is a global admin (member of the admins group),
/// owns it via `principals(caller)`, or holds any `job_grant` on it through a
/// principal. Ordered by `(queued_at, job_id)` descending; when `after` is
/// `Some((queued_at, job_id))`, only rows strictly before that key are returned
/// (keyset pagination). At most `limit` rows come back.
pub async fn list_visible(
    caller: Uuid,
    after: Option<(DateTime<Utc>, Uuid)>,
    limit: i64,
    conn: impl PgExecutor<'_>,
) -> Result<Vec<JobSummary>, JobInfoError> {
    let (after_queued_at, after_job_id) = match after {
        Some((q, id)) => (Some(q), Some(id)),
        None => (None, None),
    };
    let rows = sqlx::query!(
        r#"
        with p(id) as (select id from tml_switchboard.principals($1))
        select
          j.job_id,
          j.owner_id,
          j.job_state as "job_state: SqlJobState",
          j.resume_job_id,
          j.restart_job_id,
          j.image_digest,
          j.image_group_digest,
          j.queued_at,
          j.started_at,
          j.terminated_at,
          j.dispatched_on_host_id,
          j.termination_reason as "termination_reason: SqlTerminationReason",
          j.task_exit_status as "task_exit_status: SqlTaskExitStatus"
        from tml_switchboard.jobs j
        where (
            exists (select 1 from p where p.id = $2)
            or j.owner_id in (select id from p)
            or exists (
                select 1 from tml_switchboard.job_grants g
                join p on g.subject_id = p.id
                where g.job_id = j.job_id
            )
        )
        and ($3::timestamptz is null or (j.queued_at, j.job_id) < ($3, $4))
        order by j.queued_at desc, j.job_id desc
        limit $5
        "#,
        caller,
        crate::auth::engine::ADMINS_GROUP_ID,
        after_queued_at,
        after_job_id,
        limit,
    )
    .fetch_all(conn)
    .await?;

    rows.into_iter()
        .map(|r| {
            Ok(JobSummary {
                job_id: r.job_id,
                owner_id: r.owner_id,
                state: r.job_state.into(),
                image: job_image_ref(
                    r.resume_job_id,
                    r.restart_job_id,
                    r.image_digest,
                    r.image_group_digest,
                    r.job_id,
                )?,
                queued_at: r.queued_at,
                started_at: r.started_at,
                terminated_at: r.terminated_at,
                dispatched_on_host_id: r.dispatched_on_host_id,
                termination_reason: r.termination_reason.map(Into::into),
                task_exit_status: r.task_exit_status.map(Into::into),
            })
        })
        .collect()
}

/// Build a concrete [`ImageSpecification::Image`] from an image's id + digest,
/// reading its catalog locations (canonical/system preferred).
async fn concrete_image_spec(
    image_id: Uuid,
    manifest_digest: &str,
    conn: &mut sqlx::PgConnection,
) -> Result<ImageSpecification, ImageResolveError> {
    let locations = image::locations_for_image(conn, image_id).await?;
    if locations.is_empty() {
        return Err(ImageResolveError::NoLocations(manifest_digest.to_string()));
    }
    let digest = manifest_digest
        .parse::<Digest>()
        .map_err(|_| ImageResolveError::NotRegistered(manifest_digest.to_string()))?;
    Ok(ImageSpecification::Image {
        manifest_digest: digest,
        locations: locations
            .into_iter()
            .map(|l| ImageLocation {
                registry: l.registry,
                repository: l.repository,
            })
            .collect(),
    })
}

/// Record the concrete manifest digest dispatched for a job (for
/// reproducibility/audit), set by the scheduler once the image is resolved.
pub async fn set_resolved_image(
    job_id: Uuid,
    manifest_digest: &Digest,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"update tml_switchboard.jobs
           set resolved_image_digest = $2, last_updated_at = default
           where job_id = $1"#,
        job_id,
        manifest_digest.encoded(),
    )
    .execute(conn)
    .await
    .map(|_| ())
}

/// Why building a [`StartJobMessage`] for dispatch failed.
#[derive(Debug)]
pub enum BuildStartJobError {
    /// An underlying database error (job/params lookup).
    Db(sqlx::Error),
    /// Resolving the recorded image digest to a concrete dispatch spec failed.
    Image(ImageResolveError),
    /// The job has no `resolved_image_digest`, but the scheduler records one for
    /// every concrete-image job at assignment. Reaching here means the row was
    /// not scheduled through the normal path (or the column was cleared).
    NoResolvedImage(Uuid),
    /// Minting the per-job log-streaming write token failed (bad account seed).
    LogStreamingMint(crate::log_streaming::MintError),
}
impl From<sqlx::Error> for BuildStartJobError {
    fn from(e: sqlx::Error) -> Self {
        BuildStartJobError::Db(e)
    }
}
impl From<ImageResolveError> for BuildStartJobError {
    fn from(e: ImageResolveError) -> Self {
        BuildStartJobError::Image(e)
    }
}
impl From<crate::log_streaming::MintError> for BuildStartJobError {
    fn from(e: crate::log_streaming::MintError) -> Self {
        BuildStartJobError::LogStreamingMint(e)
    }
}
impl std::fmt::Display for BuildStartJobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildStartJobError::Db(e) => write!(f, "database error building StartJob message: {e}"),
            BuildStartJobError::Image(e) => write!(f, "{e}"),
            BuildStartJobError::NoResolvedImage(j) => {
                write!(f, "job {j} has no resolved_image_digest to dispatch")
            }
            BuildStartJobError::LogStreamingMint(e) => {
                write!(f, "minting log-streaming write token: {e}")
            }
        }
    }
}
impl std::error::Error for BuildStartJobError {}

/// Build the [`StartJobMessage`] the worker hands the supervisor to dispatch
/// `job`.
///
/// The image spec honors the scheduler's already-recorded choice rather than
/// re-resolving: a resume becomes [`ImageSpecification::ResumeJob`]; otherwise
/// the concrete `resolved_image_digest` the scheduler pinned at assignment is
/// paired with that image's catalog locations (a group is *not* re-selected — the
/// scheduler's chosen member stands). The remaining fields are read straight off
/// the job row and the `job_parameters` table.
///
/// When `log_streaming` is `Some`, the message carries a per-job
/// [`LogStreamingDispatch`](treadmill_rs::api::switchboard_supervisor::LogStreamingDispatch)
/// with a freshly minted, publish-scoped write token (see
/// [`crate::log_streaming`]); when `None`, log streaming is off and the field
/// stays `None`. Minting is pure (no I/O), so it is safe under the worker's
/// transaction; the *stream* is created separately, outside the row lock, by the
/// caller (see the worker's `reconcile`).
///
/// Read-only; safe to call inside the worker's `with_txn` (it issues no writes).
pub async fn build_start_job_message(
    job: &SqlJob,
    conn: &mut sqlx::PgConnection,
    log_streaming: Option<&crate::config::LogStreamingConfig>,
) -> Result<StartJobMessage, BuildStartJobError> {
    let image_spec = if let Some(resume_job_id) = job.resume_job_id {
        ImageSpecification::ResumeJob {
            job_id: resume_job_id,
        }
    } else {
        let digest = job
            .resolved_image_digest
            .as_deref()
            .ok_or(BuildStartJobError::NoResolvedImage(job.job_id))?;
        let rec = image::fetch_by_digest(&mut *conn, digest)
            .await?
            .ok_or_else(|| ImageResolveError::NotRegistered(digest.to_string()))?;
        concrete_image_spec(rec.id, digest, conn).await?
    };

    let parameters = parameters::fetch_by_job_id(job.job_id, &mut *conn).await?;

    // Mint the per-job write token when log streaming is enabled. The matching
    // JetStream stream is created by the caller outside the host row lock (NATS
    // I/O must not run inside `with_txn`).
    let log_streaming = log_streaming
        .map(|cfg| crate::log_streaming::build_dispatch(cfg, job.job_id))
        .transpose()?;

    Ok(StartJobMessage {
        job_id: job.job_id,
        image_spec,
        ssh_keys: job.ssh_keys.clone(),
        restart_policy: job.restart_policy(),
        parameters,
        log_streaming,
    })
}

/// Finalize a still-`queued` job that can never be dispatched because its image
/// is unresolvable (unregistered, or has no registry location). The job was
/// never placed on a host, so there is no assignment to release; we just record
/// the terminal `image_error`.
///
/// Guarded on `job_state = 'queued'`: returns `true` iff this call performed the
/// transition (so a concurrent scheduler that already finalized or scheduled the
/// job is a no-op). Honoring the restart policy is intentionally *not* done here
/// — a broken image will not fix itself on retry.
pub async fn finalize_unscheduled_as_image_error(
    job_id: Uuid,
    at: DateTime<Utc>,
    conn: impl PgExecutor<'_>,
) -> Result<bool, sqlx::Error> {
    let row = sqlx::query!(
        r#"update tml_switchboard.jobs
           set job_state = 'finalized',
               termination_reason = 'image_error',
               task_exit_status = null,
               terminated_at = $2,
               last_updated_at = default
           where job_id = $1 and job_state = 'queued'
           returning job_id"#,
        job_id,
        at,
    )
    .fetch_optional(conn)
    .await?;
    Ok(row.is_some())
}

/// Outcome of a user-requested job cancellation (`DELETE /jobs/{id}`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The job was still `queued` (on no host) and was finalized as
    /// `user_canceled` immediately.
    FinalizedNow,
    /// The job was dispatched; `cancel_requested_at` was set and the owning
    /// host's worker will converge (StopJob, then finalize).
    SignalRequested,
    /// The job was already finalized (or gone); nothing to do.
    AlreadyFinalized,
}

/// Request cancellation of a job, within the caller's transaction.
///
/// Locks the job row, then dispatches on its state:
///   - `finalized` (or missing) → no-op ([`CancelOutcome::AlreadyFinalized`]);
///   - `queued` → finalized as `user_canceled` now, since no host is involved
///     ([`CancelOutcome::FinalizedNow`]);
///   - otherwise (dispatched: `scheduled`/`initializing`/`ready`/`terminating`)
///     → sets `cancel_requested_at` idempotently so the host's worker stops and
///     finalizes it ([`CancelOutcome::SignalRequested`]).
///
/// The `for update` lock serializes against the scheduler's assignment (which
/// also locks the job row), so the queued-vs-dispatched decision cannot race a
/// concurrent placement.
pub async fn request_cancel(
    job_id: Uuid,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<CancelOutcome, sqlx::Error> {
    let state = sqlx::query_scalar!(
        r#"select job_state as "state: SqlJobState"
           from tml_switchboard.jobs
           where job_id = $1
           for update"#,
        job_id,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // The row vanished between the route's authz check and here (e.g. its owner
    // was deleted): treat as already gone.
    let Some(state) = state else {
        return Ok(CancelOutcome::AlreadyFinalized);
    };

    match state {
        SqlJobState::Finalized => Ok(CancelOutcome::AlreadyFinalized),
        SqlJobState::Queued => {
            // No host involved; finalize directly. The dispatch columns are
            // already null on a queued job, satisfying the finalized invariants.
            sqlx::query!(
                r#"update tml_switchboard.jobs
                   set job_state = 'finalized',
                       termination_reason = 'user_canceled',
                       task_exit_status = null,
                       terminated_at = $2,
                       last_updated_at = default
                   where job_id = $1 and job_state = 'queued'"#,
                job_id,
                at,
            )
            .execute(&mut **txn)
            .await?;
            Ok(CancelOutcome::FinalizedNow)
        }
        SqlJobState::Scheduled
        | SqlJobState::Initializing
        | SqlJobState::Ready
        | SqlJobState::Terminating => {
            // Dispatched: leave the stop to the host's worker, which re-derives
            // `switchboard_stop_reason` each reconcile pass. Idempotent: an
            // already-set signal is preserved.
            sqlx::query!(
                r#"update tml_switchboard.jobs
                   set cancel_requested_at = coalesce(cancel_requested_at, $2),
                       last_updated_at = default
                   where job_id = $1"#,
                job_id,
                at,
            )
            .execute(&mut **txn)
            .await?;
            Ok(CancelOutcome::SignalRequested)
        }
    }
}

// pub async fn fetch_all_queued(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
//     sqlx::query_as!(
//         SqlJob,
//         r#"
//         select job_id, resume_job_id, restart_job_id, image_digest, image_group_digest,
//         resolved_image_digest, ssh_keys,
//         restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
//         queued_at, job_state as "job_state: _",
//         initializing_stage as "initializing_stage: _", started_at,
//         dispatched_on_host_id, ssh_endpoints as "ssh_endpoints: _",
//         termination_reason as "termination_reason: _",
//         task_exit_status as "task_exit_status: _", exit_message, terminated_at,
//         last_updated_at
//         from tml_switchboard.jobs where job_state = 'queued';
//         "#
//     )
//     .fetch_all(conn)
//     .await
// }

// pub async fn fetch_all_dispatched(conn: impl PgExecutor<'_>) -> Result<Vec<SqlJob>, sqlx::Error> {
//     sqlx::query_as!(
//         SqlJob,
//         r#"
//         select job_id, resume_job_id, restart_job_id, image_digest, image_group_digest,
//         resolved_image_digest, ssh_keys,
//         restart_policy as "sql_restart_policy: _", enqueued_by_token_id, tag_config, job_timeout,
//         queued_at, job_state as "job_state: _",
//         initializing_stage as "initializing_stage: _", started_at,
//         dispatched_on_host_id, ssh_endpoints as "ssh_endpoints: _",
//         termination_reason as "termination_reason: _",
//         task_exit_status as "task_exit_status: _", exit_message, terminated_at,
//         last_updated_at
//         from tml_switchboard.jobs
//         where job_state in ('scheduled', 'initializing', 'ready', 'terminating');
//         "#
//     )
//     .fetch_all(conn)
//     .await
// }

/// Finalize a job that its host's supervisor dropped, releasing the host and --
/// if the restart policy permits -- enqueuing a successor, all in one
/// transaction. Backs reconciliation cases 3 and 5 (`Phase 5`): the switchboard
/// believed `job_id` was assigned to `host_id`, but the supervisor no longer
/// reports running it.
///
/// The job is finalized with [`TerminationReason::HostDroppedJob`], its
/// assignment columns (`dispatched_on_host_id`, `started_at`,
/// `initializing_stage`) are cleared to satisfy the finalized-state invariants,
/// a `FinalizeResult` event is appended to the audit log, and
/// `hosts.current_job` is released (guarded on `job_id` so a pointer that has
/// since been reassigned is left alone).
///
/// **Idempotent.** The finalize only fires when the job is not already
/// finalized; a replayed reconciliation therefore neither double-finalizes nor
/// spawns a second restart successor. Returns the successor's job id if one was
/// enqueued.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
/// Adopt a supervisor-reported *executing* state into the DB `job_state`, used
/// by reconciliation case 4 (the supervisor reports `OngoingJob(J_sb)` for the
/// assigned job, so the switchboard takes the reported state as ground truth).
///
/// `job_state` must be one of the executing states (`initializing`, `ready`,
/// `terminating`); `initializing_stage` is required exactly when `job_state` is
/// `initializing` and must be `None` otherwise. The terminal `finalized` state
/// is deliberately *not* expressible here: a supervisor-reported `Terminated`
/// needs a [`TerminationReason`] and so goes through a finalize path, not this
/// one (see the `RunningJobState` Rustdoc on the `Terminated` fold).
///
/// `started_at` is back-filled to `at` if it was null (e.g. adopting `ready`
/// over a still-`scheduled` row), satisfying the `started_at_iso_executing`
/// CHECK. Idempotent: re-adopting the same state is a harmless rewrite.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
pub async fn set_running_state(
    job_id: Uuid,
    job_state: SqlJobState,
    initializing_stage: Option<SqlJobInitializingStage>,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = $2,
            initializing_stage = $3,
            started_at = coalesce(started_at, $4),
            last_updated_at = default
        where job_id = $1
        "#,
        job_id,
        job_state as SqlJobState,
        initializing_stage as Option<SqlJobInitializingStage>,
        at,
    )
    .execute(&mut **txn)
    .await?;
    Ok(())
}

/// Adopt a supervisor-reported [`RunningJobState`] into the DB, within the
/// caller's transaction. This is the single place that maps a running state to
/// its DB transition, shared by `reconcile` (adopting the status snapshot) and
/// the asynchronous [`SupervisorJobEvent::StateTransition`] handler so the two
/// paths can never diverge.
///
/// The executing states (`Initializing`/`Ready`/`Terminating`) go through
/// [`set_running_state`] (which back-fills `started_at`, so adopting over a
/// still-`scheduled` row is valid). `Terminated` finalizes the job via
/// [`finalize_terminated`] (`termination_reason = workload_exited`, no restart):
/// in that case the function returns `true`, signalling the caller it must
/// `StopJob`-ack so the supervisor can release its retained terminal record.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness guard
/// covers it.
///
/// [`SupervisorJobEvent::StateTransition`]: treadmill_rs::api::switchboard_supervisor::SupervisorJobEvent::StateTransition
pub async fn apply_running_state(
    job_id: Uuid,
    host_id: Uuid,
    state: RunningJobState,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    match state {
        RunningJobState::Initializing { stage } => {
            set_running_state(
                job_id,
                SqlJobState::Initializing,
                Some(stage.into()),
                at,
                txn,
            )
            .await?;
            Ok(false)
        }
        RunningJobState::Ready => {
            set_running_state(job_id, SqlJobState::Ready, None, at, txn).await?;
            Ok(false)
        }
        RunningJobState::Terminating => {
            set_running_state(job_id, SqlJobState::Terminating, None, at, txn).await?;
            Ok(false)
        }
        RunningJobState::Terminated => {
            finalize_terminated(job_id, host_id, at, txn).await?;
            Ok(true)
        }
    }
}

/// Finalize a job the supervisor reports as `Terminated`, within the caller's
/// transaction. Backs reconciliation case 4 when the adopted `RunningJobState`
/// is `Terminated`: the host's workload exited, so the job finalizes with
/// [`TerminationReason::WorkloadExited`].
///
/// The task outcome (`task_exit_status` / `exit_message`) is *not* set here — it
/// is reported out-of-band via [`set_task_outcome`], so whatever the supervisor
/// last declared is preserved across this transition. The assignment columns
/// (`dispatched_on_host_id`, `started_at`, `initializing_stage`) are
/// cleared to satisfy the finalized-state invariants and `hosts.current_job` is
/// released (guarded on `job_id`). Unlike
/// [`finalize_dropped_and_maybe_restart`], the restart policy is **not** applied:
/// a clean workload exit is a completion, not a failure to retry.
///
/// **Idempotent.** The finalize only fires when the job is not already
/// finalized, so a replayed reconciliation is a no-op.
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness
/// guard covers it.
pub async fn finalize_terminated(
    job_id: Uuid,
    host_id: Uuid,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<(), sqlx::Error> {
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = 'workload_exited',
            dispatched_on_host_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $2,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(());
    }

    // Release the host's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.hosts
        set current_job = null
        where host_id = $1 and current_job = $2
        "#,
        host_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    Ok(())
}

/// Finalize a job with an explicit, switchboard-determined `reason`, within the
/// caller's transaction. Backs reconcile's stop pre-check (execution-timeout and
/// user-cancel): when the worker decides an assigned job must stop, the eventual
/// terminal record carries `reason` (e.g. `execution_timeout` / `user_canceled`)
/// rather than a workload-driven one.
///
/// Records only the reason and `terminated_at`, clears the assignment columns,
/// and releases `hosts.current_job`. The orthogonal `task_exit_status` /
/// `exit_message` are left untouched. Unlike
/// [`finalize_dropped_and_maybe_restart`], the restart policy is **not** applied:
/// a deliberate stop is not retried.
///
/// **Idempotent.** Guarded on `job_state <> 'finalized'`, so a replay is a no-op
/// (returns `false`).
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness guard
/// covers it.
pub async fn finalize_with_reason(
    job_id: Uuid,
    host_id: Uuid,
    reason: SqlTerminationReason,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = $2,
            dispatched_on_host_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $3,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        reason as SqlTerminationReason,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(false);
    }

    // Release the host's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.hosts
        set current_job = null
        where host_id = $1 and current_job = $2
        "#,
        host_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    Ok(true)
}

/// Map a supervisor-reported [`JobErrorKind`] to the terminal
/// [`TerminationReason`] the switchboard records for it. Image problems become
/// `ImageError`, a failed resume `ResumeFailed`, an explicit supervisor-internal
/// fault `InternalError`; the remaining start-time faults (a duplicate/missing
/// job, capacity) fold into `HostStartFailure` — the job never started.
///
/// `#[non_exhaustive]` on `JobErrorKind` forces a catch-all; new kinds default
/// to `InternalError` until classified.
pub fn termination_reason_for_job_error(kind: &JobErrorKind) -> SqlTerminationReason {
    match kind {
        JobErrorKind::ImageNotFound
        | JobErrorKind::ImageInvalid
        | JobErrorKind::ImageNotCompatible => SqlTerminationReason::ImageError,
        JobErrorKind::CannotResume => SqlTerminationReason::ResumeFailed,
        JobErrorKind::AlreadyRunning
        | JobErrorKind::AlreadyStopping
        | JobErrorKind::JobAlreadyExists
        | JobErrorKind::JobNotFound
        | JobErrorKind::MaxConcurrentJobs => SqlTerminationReason::HostStartFailure,
        JobErrorKind::InternalError => SqlTerminationReason::InternalError,
        // `JobErrorKind` is `#[non_exhaustive]`: classify unknown future kinds
        // conservatively as an internal error rather than failing to finalize.
        _ => SqlTerminationReason::InternalError,
    }
}

/// Finalize a job the supervisor reported a [`SupervisorJobEvent::Error`] for,
/// within the caller's transaction. Backs the event path's error handling:
/// records the terminal `termination_reason` (see
/// [`termination_reason_for_job_error`]) and the error's `description` as
/// `exit_message`, clears the assignment columns, and releases
/// `hosts.current_job`. The orthogonal `task_exit_status` is left untouched (the
/// protocol keeps the *why-it-stopped* and the *workload outcome* separate).
///
/// Unlike [`finalize_dropped_and_maybe_restart`], the restart policy is **not**
/// applied: a supervisor-reported job error is recorded terminally.
///
/// **Idempotent.** Guarded on `job_state <> 'finalized'`, so a replay is a no-op
/// (returns `false`).
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness guard
/// covers it.
///
/// [`SupervisorJobEvent::Error`]: treadmill_rs::api::switchboard_supervisor::SupervisorJobEvent::Error
pub async fn finalize_errored(
    job_id: Uuid,
    host_id: Uuid,
    reason: SqlTerminationReason,
    message: Option<String>,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = $2,
            exit_message = $3,
            dispatched_on_host_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $4,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        reason as SqlTerminationReason,
        message,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(false);
    }

    // Release the host's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.hosts
        set current_job = null
        where host_id = $1 and current_job = $2
        "#,
        host_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    Ok(true)
}

/// Record the supervisor's *task outcome* for a job it is currently assigned to
/// `host_id`, within the caller's transaction.
///
/// This is the dedicated setter behind [`SupervisorJobEvent::DeclareExitStatus`].
/// It is independent of the job's lifecycle state: the supervisor may set the
/// outcome at any point while the job is dispatched to its host, as many times
/// as it likes, each call overriding the previous `task_exit_status` and
/// replacing `exit_message` (passing `None` clears the message). The outcome
/// itself (`pending` / `success` / `failure`) can never be cleared back to unset.
///
/// The write is guarded on `dispatched_on_host_id = host_id`, which is precisely
/// "the job is assigned to this host": it is a no-op (returns `false`) for a job
/// assigned to a different host, never dispatched, or already finalized (whose
/// dispatch pointer has been cleared).
///
/// Must be called inside the worker's `with_txn` so the takeover/staleness guard
/// covers it.
pub async fn set_task_outcome(
    job_id: Uuid,
    host_id: Uuid,
    outcome: SqlTaskExitStatus,
    message: Option<String>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<bool, sqlx::Error> {
    let updated = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set task_exit_status = $3,
            exit_message = $4,
            last_updated_at = default
        where job_id = $1 and dispatched_on_host_id = $2
        returning job_id
        "#,
        job_id,
        host_id,
        outcome as SqlTaskExitStatus,
        message,
    )
    .fetch_optional(&mut **txn)
    .await?;

    Ok(updated.is_some())
}

pub async fn finalize_dropped_and_maybe_restart(
    job_id: Uuid,
    host_id: Uuid,
    at: DateTime<Utc>,
    txn: &mut Transaction<'_, Postgres>,
) -> Result<Option<Uuid>, sqlx::Error> {
    // Read the predecessor's restart-relevant fields before finalizing.
    let predecessor = fetch_by_job_id(job_id, &mut **txn).await?;

    // Finalize only if not already finalized; the returned row tells us whether
    // this pass is the one that actually performed the transition.
    let transitioned = sqlx::query!(
        r#"
        update tml_switchboard.jobs
        set job_state = 'finalized',
            termination_reason = 'host_dropped_job',
            task_exit_status = null,
            dispatched_on_host_id = null,
            started_at = null,
            initializing_stage = null,
            terminated_at = $2,
            last_updated_at = default
        where job_id = $1 and job_state <> 'finalized'
        returning job_id
        "#,
        job_id,
        at,
    )
    .fetch_optional(&mut **txn)
    .await?;

    // Some earlier pass already finalized this job: nothing left to do.
    if transitioned.is_none() {
        return Ok(None);
    }

    // Release the host's assignment pointer.
    sqlx::query!(
        r#"
        update tml_switchboard.hosts
        set current_job = null
        where host_id = $1 and current_job = $2
        "#,
        host_id,
        job_id,
    )
    .execute(&mut **txn)
    .await?;

    // Honor the restart policy: enqueue a successor with one fewer restart.
    let remaining = predecessor.sql_restart_policy.remaining_restart_count;
    if remaining <= 0 {
        return Ok(None);
    }

    let successor_id = Uuid::new_v4();
    let parameters = parameters::fetch_by_job_id(job_id, &mut **txn).await?;
    let target_requirements = target_requirements_for_job(job_id, &mut **txn).await?;
    let job_request = JobRequest {
        init_spec: JobInitSpec::RestartJob { job_id },
        // Ownership is passed to `insert` explicitly (below); this field is the
        // user-facing requested owner and is unused on the internal path.
        owner: None,
        ssh_keys: predecessor.ssh_keys.clone(),
        restart_policy: RestartPolicy {
            remaining_restart_count: usize::try_from(remaining - 1).unwrap_or(0),
        },
        parameters: parameters.clone(),
        host_tag_requirements: predecessor.host_tag_requirements.clone(),
        target_requirements,
        override_timeout: None,
    };
    insert(
        job_request,
        successor_id,
        predecessor.enqueued_by_token_id,
        // The successor inherits the predecessor's ownership.
        predecessor.owner_id,
        predecessor.job_timeout,
        at,
        txn,
    )
    .await?;
    parameters::insert(successor_id, parameters, &mut **txn).await?;

    Ok(Some(successor_id))
}

use chrono::{DateTime, Utc};
use sqlx::types::Json;
use sqlx::PgExecutor;
use treadmill_rs::api::switchboard::{JobEvent, JobResult, JobState};
use uuid::Uuid;

pub struct SqlJobEvent {
    pub job_id: Uuid,
    pub job_event: Json<JobEvent>,
    pub logged_at: DateTime<Utc>,
}

pub async fn fetch_most_recent_state_by_job_id(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Option<(JobState, Option<String>)>, sqlx::Error> {
    Ok(sqlx::query_as!(
        SqlJobEvent,
        r#"
        select job_id, job_event as "job_event: _", logged_at
        from tml_switchboard.job_events
        where job_id = $1 and job_event->>'event_type' = 'state_transition'
        order by logged_at desc
        limit 1;
        "#,
        job_id
    )
    .fetch_optional(conn)
    .await?
    .map(|je| {
        let JobEvent::StateTransition {
            state,
            status_message,
        } = je.job_event.0
        else {
            unreachable!()
        };
        (state, status_message)
    }))
}
pub async fn fetch_finalized_result(
    job_id: Uuid,
    conn: impl PgExecutor<'_>,
) -> Result<Option<JobResult>, sqlx::Error> {
    Ok(sqlx::query_as!(
        SqlJobEvent,
        r#"
        select job_id, job_event as "job_event: _", logged_at
        from tml_switchboard.job_events
        where job_id = $1 and job_event ->> 'event_type' = 'finalize_result'
        order by logged_at desc
        limit 1;
        "#,
        job_id
    )
    .fetch_optional(conn)
    .await?
    .map(|je| {
        let JobEvent::FinalizeResult { job_result } = je.job_event.0 else {
            unreachable!()
        };
        job_result
    }))
}
pub async fn insert(
    job_id: Uuid,
    event: JobEvent,
    logged_at: DateTime<Utc>,
    conn: impl PgExecutor<'_>,
) -> Result<(), sqlx::Error> {
    sqlx::query!(
        r#"
        insert into tml_switchboard.job_events
        values ($1, $2, $3)
        "#,
        job_id,
        Json(event) as Json<JobEvent>,
        logged_at
    )
    .execute(conn)
    .await
    .map(|_| ())
}

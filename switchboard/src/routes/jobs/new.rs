use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::auth::AuthorizationSource;
use crate::perms::SubmitJobError;
use crate::routes::proxy::{proxy_err, proxy_val, Proxied};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms};
use axum::extract::State;
use axum::Json;
use treadmill_rs::api::switchboard::jobs::submit::{
    EnqueueJobRequest as EJRequest, EnqueueJobResponse as EJResponse,
};
// Response

impl_from_auth_err!(EJResponse, Database => Internal, Unauthorized => Unauthorized);

// Route

#[tracing::instrument(skip(state, auth))]
pub async fn submit(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Json(request): Json<EJRequest>,
) -> Proxied<EJResponse> {
    let submit_job_priv = auth.authorize(perms::SubmitJob).await.map_err(proxy_err)?;

    let job_id = perms::submit_job(&state, submit_job_priv, request.job_request)
        .await
        .map_err(|e| {
            proxy_err(match e {
                SubmitJobError::Internal => EJResponse::Internal,
                SubmitJobError::FailedToMatch => EJResponse::FailedToMatch,
            })
        })?;

    proxy_val(EJResponse::Ok { job_id })
}

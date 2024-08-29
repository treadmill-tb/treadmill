use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::auth::AuthorizationSource;
use crate::perms::access_supervisor_status;
use crate::routes::proxy::{proxy_err, proxy_val, Proxied};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms};
use axum::extract::{Path, State};
use treadmill_rs::api::switchboard::supervisors::status::Response as SSResponse;
use uuid::Uuid;

// Response

impl_from_auth_err!(SSResponse, Database => Internal, Unauthorized => Invalid);

// Route

#[tracing::instrument(skip(state, auth))]
pub async fn status(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Path(supervisor_id): Path<Uuid>,
) -> Proxied<SSResponse> {
    let access = auth
        .authorize(perms::AccessSupervisorStatus { supervisor_id })
        .await
        .map_err(proxy_err)?;
    let supervisor_status = access_supervisor_status(&state, access)
        .await
        .map_err(|_| proxy_err(SSResponse::Invalid))?;

    proxy_val(SSResponse::Ok {
        status: supervisor_status,
    })
}

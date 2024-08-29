use crate::auth::db::DbAuth;
use crate::auth::extract::AuthSource;
use crate::auth::AuthorizationSource;
use crate::routes::proxy::{proxy_err, proxy_val, Proxied};
use crate::serve::AppState;
use crate::{impl_from_auth_err, perms};
use axum::extract::{Query, State};
use futures_util::stream::FuturesOrdered;
use futures_util::{StreamExt, TryStreamExt};
use treadmill_rs::api::switchboard::supervisors::list::{Filter, Response as LSResponse};

// Response

impl_from_auth_err!(LSResponse, Database => Internal, Unauthorized => Unauthorized);

// Route

#[tracing::instrument(skip(state, auth))]
pub async fn list(
    State(state): State<AppState>,
    AuthSource(auth): AuthSource<DbAuth>,
    Query(filter): Query<Filter>,
) -> Proxied<LSResponse> {
    let list_supervisors = auth
        .authorize(perms::ListSupervisors { filter })
        .await
        .map_err(proxy_err)?;
    let perm_queries: FuturesOrdered<_> = state
        .service()
        .list_supervisors()
        .await
        .into_iter()
        .map(|supervisor_id| auth.authorize(perms::AccessSupervisorStatus { supervisor_id }))
        .collect();
    let supervisor_perms: Vec<_> = perm_queries
        .filter_map(|x| async move { x.ok() })
        .collect()
        .await;
    let supervisors = perms::list_supervisors(&state, list_supervisors, supervisor_perms).await;
    proxy_val(LSResponse::Ok { supervisors })
}

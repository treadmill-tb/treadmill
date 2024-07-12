use crate::server::auth::{
    AuthSource, AuthorizationError, AuthorizationSource, DbPermSource, PermissionQueryExecutor,
    Privilege, PrivilegedAction,
};
use axum::async_trait;

#[derive(Debug, Clone)]
pub struct EnqueueCIJobAction {
    pub supervisor_id: String,
}
#[async_trait]
impl PrivilegedAction for EnqueueCIJobAction {
    async fn authorize<'s, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'s, Self>, AuthorizationError> {
        perm_query_exec
            .query(format!("enqueue_ci_job:{}", &self.supervisor_id))
            .await
            .try_into_privilege(self)
    }
}

fn enqueue_ci_job(p: Privilege<EnqueueCIJobAction>) {
    // do things
    let subject = p.subject();
    let object = &p.action().supervisor_id;
    println!("[subject:{subject:?}] enqueuing a job on [object:{object:?}]");
}

#[axum::debug_handler(state = crate::server::AppState)]
pub async fn example(
    AuthSource(auth_source): AuthSource<DbPermSource>,
    // State(_): State<AppState>,
) -> Result<(), AuthorizationError> {
    let privilege = auth_source
        .authorize(EnqueueCIJobAction {
            supervisor_id: "foobar".to_string(),
        })
        .await?;

    enqueue_ci_job(privilege);

    Ok(())
}

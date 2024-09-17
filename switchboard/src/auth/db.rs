use crate::auth::{
    AuthorizationError, AuthorizationSource, PermissionQueryExecutor, PermissionResult,
    PermissionResultInner, Privilege, PrivilegedAction, Subject, SubjectDetail,
};
use crate::serve::AppState;
use crate::sql;
use crate::sql::api_token::TokenError;
use async_trait::async_trait;
use sqlx::PgPool;
use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use uuid::Uuid;

/// Implementation of [`PermissionQueryExecutor`] that is backed by the switchboard database.
pub struct DbPermSearcher {
    #[allow(dead_code)]
    db: PgPool,
    subject: Subject,
}
#[async_trait]
impl PermissionQueryExecutor for DbPermSearcher {
    async fn query(&self, perm_str: String) -> PermissionResult {
        let token_info = &self.subject.0.token_info;

        let ok = if token_info.inherits_user_permissions {
            let perms = match sqlx::query!(
                r#"select user_id, permission from tml_switchboard.user_privileges where user_id = $1;"#,
                token_info.user_id
            )
                .fetch_all(&self.db)
                .await
            {
                Ok(v) => v,
                Err(e) => return PermissionResult::unauthorized(AuthorizationError::Database(e)),
            };
            perms.iter().any(|r| r.permission == perm_str)
        } else {
            let perms = match sqlx::query!(
                r#"select token_id,permission from tml_switchboard.api_token_privileges where token_id = $1;"#,
                token_info.token_id
            )
                .fetch_all(&self.db)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    return PermissionResult::unauthorized(AuthorizationError::Database(e));
                }
            };
            perms.iter().any(|r| r.permission == perm_str)
        };

        if ok {
            PermissionResult(PermissionResultInner::Authorized(Subject(SubjectDetail {
                token_info: Arc::clone(token_info),
            })))
        } else {
            PermissionResult::unauthorized(AuthorizationError::Unauthorized(perm_str))
        }
    }
}

/// Implementation of [`AuthorizationSource`] that is backed by the switchboard database.
pub struct DbAuth {
    db: PgPool,
    subject: Subject,
}
impl DbAuth {
    pub async fn from_pool_with_token_id(
        pool: &PgPool,
        token_id: Uuid,
    ) -> Result<Self, TokenError> {
        Ok(Self {
            db: pool.clone(),
            subject: Subject(SubjectDetail {
                token_info: Arc::new(sql::api_token::fetch_metadata_by_id(pool, token_id).await?),
            }),
        })
    }
}
#[async_trait]
impl AuthorizationSource for DbAuth {
    fn from_state_with_subject(app_state: &AppState, subject: Subject) -> Self {
        Self {
            db: app_state.pool().clone(),
            subject,
        }
    }

    async fn authorize<'a, A: PrivilegedAction + Send>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, AuthorizationError> {
        action
            .authorize(DbPermSearcher {
                db: self.db.clone(),
                subject: Subject(SubjectDetail {
                    token_info: self.subject.0.token_info.clone(),
                }),
            })
            .await
    }
}
impl Debug for DbAuth {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "token ({})", self.subject.0.token_info.token_id)
    }
}

use crate::server::session::{SessionError, XCsrfToken, SESSION_ID_COOKIE};
use crate::server::token::{TokenError, XApiToken};
use crate::server::{session, token, AppState};
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use axum::{async_trait, RequestPartsExt};
use axum_extra::extract::cookie::Key;
use axum_extra::extract::SignedCookieJar;
use axum_extra::typed_header::TypedHeaderRejectionReason;
use axum_extra::TypedHeader;
use chrono::Utc;
use http::request::Parts;
use http::StatusCode;
use sqlx::PgPool;
use std::convert::Infallible;
use std::fmt::Debug;
use std::marker::PhantomData;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct UserId(Uuid);
#[derive(Debug, Clone)]
struct TokenId(#[allow(dead_code)] Uuid);

// -- SECTION: SUBJECT DERIVATION

#[derive(Debug, Clone)]
enum SubjectInner {
    #[allow(dead_code)]
    User(UserId),
    #[allow(dead_code)]
    Token(TokenId),
    Guest,
}
#[derive(Debug)]
pub struct Subject(SubjectInner);

#[async_trait]
impl FromRequestParts<AppState> for Subject {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // check for a token
        let maybe_token = match parts.extract::<TypedHeader<XApiToken>>().await {
            Ok(x) => Some(x.0 .0),
            Err(rejection) => match &rejection {
                rej => match rej.reason() {
                    TypedHeaderRejectionReason::Missing => None,
                    TypedHeaderRejectionReason::Error(_) => return Err(rejection.into_response()),
                    _ => unreachable!(),
                },
            },
        };
        let token_subject;
        if let Some(token) = maybe_token {
            let token_info = match token::token_lookup(&state.db_pool, token).await {
                Ok(tib) => tib,
                Err(TokenError::InvalidToken) => {
                    tracing::warn!("failed to derive subject: invalid token {token}");
                    return Err(StatusCode::UNAUTHORIZED.into_response());
                }
                Err(e) => {
                    tracing::error!("failed to look up token {token}: {e}");
                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            };
            if token_info.expires_at < Utc::now() {
                tracing::warn!(
                    "failed to derive subject: token ({}) expired at {}",
                    token_info.token_id,
                    token_info.expires_at
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            if let Some(cancellation) = token_info.canceled {
                tracing::warn!(
                    "failed to derive subject: canceled token ({}): {cancellation}",
                    token_info.token_id,
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            token_subject = Some(TokenId(token_info.token_id));
        } else {
            token_subject = None;
        }

        // check for a session
        let signed_cookie_jar = match SignedCookieJar::<Key>::from_request_parts(parts, state).await
        {
            Ok(jar) => jar,
            Err(infallible) => {
                // regression guard, in case anything changes upstream
                let _: Infallible = infallible;
                unreachable!()
            }
        };
        let maybe_csrf_token = match parts.extract::<TypedHeader<XCsrfToken>>().await {
            Ok(x) => Some(x.0 .0),
            Err(rejection) => match &rejection {
                rej => match rej.reason() {
                    TypedHeaderRejectionReason::Missing => None,
                    TypedHeaderRejectionReason::Error(_) => return Err(rejection.into_response()),
                    _ => unreachable!(),
                },
            },
        };
        let user_subject;
        if let Some(session_id_cookie) = signed_cookie_jar.get(SESSION_ID_COOKIE) {
            let Some(csrf_token) = maybe_csrf_token else {
                tracing::warn!(
                    "failed to deserialize subject: session is not accompanied by valid CSRF header"
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            };
            let session_id: Uuid = match serde_json::from_str(session_id_cookie.value()) {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!("failed to deserialize session ID cookie: {e}");
                    return Err(StatusCode::UNAUTHORIZED.into_response());
                }
            };
            let session = match session::session_lookup(&state.db_pool, session_id).await {
                Ok(session) => {
                    if session.expires_at < Utc::now() {
                        tracing::warn!(
                            "failed to derive subject: session ({session_id}) expired at {}",
                            session.expires_at
                        );
                        return Err(StatusCode::UNAUTHORIZED.into_response());
                    }
                    session
                }
                Err(SessionError::InvalidSession) => {
                    tracing::warn!("failed to derive subject: invalid session ({session_id})");
                    return Err(StatusCode::UNAUTHORIZED.into_response());
                }
                Err(e) => {
                    tracing::error!("failed to look up session ({session_id}): {e}");
                    return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
                }
            };
            if csrf_token != session.csrf_token {
                tracing::warn!(
                    "mismatched CSRF tokens for session ID ({session_id}); correct ({}) got ({})",
                    session.csrf_token,
                    csrf_token
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }

            user_subject = Some(UserId(session.user_id));
        } else {
            user_subject = None;
        }

        if maybe_csrf_token.is_some() && user_subject.is_none() {
            tracing::warn!(
                "failed to derive subject: CSRF header is unaccompanied by session ID cookie"
            );
            return Err(StatusCode::UNAUTHORIZED.into_response());
        }

        match (token_subject, user_subject) {
            (Some(t), None) => return Ok(Self(SubjectInner::Token(t))),
            (None, Some(u)) => return Ok(Self(SubjectInner::User(u))),
            (Some(t), Some(u)) => {
                tracing::warn!(
                    "received both a token ({}) subject and a user ({}) subject",
                    t.0,
                    u.0
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            (None, None) => Ok(Self(SubjectInner::Guest)),
        }
    }
}

// -- SECTION: AUTHORIZATIONS

// Note: we make use of #[async_trait] here as we ran into this odd issue when using the native
// async-in-traits-support: https://github.com/rust-lang/rust/issues/100013.

#[derive(Debug, Error)]
pub enum AuthorizationError {
    #[error("failed to fetch permissions: {0}")]
    Database(sqlx::Error),
    #[error("subject does not have permission: {0}")]
    Unauthorized(String),
}
impl IntoResponse for AuthorizationError {
    fn into_response(self) -> Response {
        match self {
            AuthorizationError::Database(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
            AuthorizationError::Unauthorized(_) => StatusCode::UNAUTHORIZED.into_response(),
        }
    }
}

#[derive(Debug)]
pub enum PermissionResult {
    Authorized(Subject),
    #[allow(dead_code)]
    Unauthorized(AuthorizationError),
}
impl PermissionResult {
    pub fn try_into_privilege<'source, A: PrivilegedAction>(
        self,
        action: A,
    ) -> Result<Privilege<'source, A>, AuthorizationError> {
        match self {
            PermissionResult::Authorized(subject) => Ok(Privilege {
                subject,
                action,
                _pd: Default::default(),
            }),
            PermissionResult::Unauthorized(e) => Err(e),
        }
    }
}

#[async_trait]
pub trait PrivilegedAction: Debug + Sized {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError>;
}

#[derive(Debug)]
pub struct Privilege<'a, A: PrivilegedAction> {
    subject: Subject,
    action: A,
    _pd: PhantomData<&'a ()>,
}

impl<A: PrivilegedAction> Privilege<'_, A> {
    pub fn subject(&self) -> &Subject {
        &self.subject
    }
    pub fn action(&self) -> &A {
        &self.action
    }
}

#[async_trait]
pub trait PermissionQueryExecutor {
    async fn query(&self, s: String) -> PermissionResult;
}
#[async_trait]
pub trait AuthorizationSource {
    fn from_state_with_subject(app_state: &AppState, subject: Subject) -> Self;
    async fn authorize<'a, A: PrivilegedAction + Send>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, AuthorizationError>;
}

pub struct AuthSource<AS: AuthorizationSource>(pub AS);
#[async_trait]
impl<AS: AuthorizationSource> FromRequestParts<AppState> for AuthSource<AS> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let subject: Subject = Subject::from_request_parts(parts, state).await?;
        Ok(AuthSource(AS::from_state_with_subject(state, subject)))
    }
}

// -- EXAMPLE

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

pub struct MockSearcher {
    #[allow(dead_code)]
    db: PgPool,
    subject: Subject,
}
#[async_trait]
impl PermissionQueryExecutor for MockSearcher {
    async fn query(&self, perm_str: String) -> PermissionResult {
        match &self.subject {
            Subject(SubjectInner::User(user)) => {
                let perms = match sqlx::query!(
                    r#"select user_id,permission from user_privileges where user_id = $1;"#,
                    user.0
                )
                .fetch_all(&self.db)
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return PermissionResult::Unauthorized(AuthorizationError::Database(e))
                    }
                };
                if perms.iter().any(|r| r.permission == perm_str) {
                    PermissionResult::Authorized(Subject(self.subject.0.clone()))
                } else {
                    PermissionResult::Unauthorized(AuthorizationError::Unauthorized(perm_str))
                }
            }
            Subject(SubjectInner::Token(token)) => {
                let perms = match sqlx::query!(
                    r#"select token_id,permission from api_token_privileges where token_id = $1;"#,
                    token.0
                )
                .fetch_all(&self.db)
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return PermissionResult::Unauthorized(AuthorizationError::Database(e))
                    }
                };
                if perms.iter().any(|r| r.permission == perm_str) {
                    PermissionResult::Authorized(Subject(self.subject.0.clone()))
                } else {
                    PermissionResult::Unauthorized(AuthorizationError::Unauthorized(perm_str))
                }
            }
            Subject(SubjectInner::Guest) => {
                PermissionResult::Unauthorized(AuthorizationError::Unauthorized(perm_str))
            }
        }
    }
}

pub struct MockPrivilegeSource {
    db: PgPool,
    subject: Subject,
}
#[async_trait]
impl AuthorizationSource for MockPrivilegeSource {
    fn from_state_with_subject(app_state: &AppState, subject: Subject) -> Self {
        Self {
            db: app_state.db_pool.clone(),
            subject,
        }
    }

    async fn authorize<'a, A: PrivilegedAction + Send>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, AuthorizationError> {
        action
            .authorize(MockSearcher {
                db: self.db.clone(),
                subject: Subject(self.subject.0.clone()),
            })
            .await
    }
}

fn enqueue_ci_job(p: Privilege<EnqueueCIJobAction>) {
    // do things
    let subject = p.subject();
    let object = &p.action().supervisor_id;
    println!("[subject:{subject:?}] enqueuing a job on [object:{object:?}]");
}

pub async fn example(
    AuthSource(auth_source): AuthSource<MockPrivilegeSource>,
    TypedHeader(_): TypedHeader<XApiToken>,
) -> Result<(), AuthorizationError> {
    let privilege = auth_source
        .authorize(EnqueueCIJobAction {
            supervisor_id: "foobar".to_string(),
        })
        .await?;

    enqueue_ci_job(privilege);

    Ok(())
}

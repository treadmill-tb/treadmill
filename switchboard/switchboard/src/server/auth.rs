//! Per-request authentication systems.
//!
//! The security system uses a subject-privilege abstraction:
//!   - a _permission_ is an action paired with an object that is acted on (e.g. enqueueing a job on
//!     a particular supervisor is a permission).
//!   - a _subject_ is an entity that can perform an action on an object, or, to use the parlance
//!     we've established, can have privileges (e.g. a user or an API token).
//!   - a _privilege_ is a _(subject, permission)_ pair.
//!
//! In this system, permissions are represented by [`PrivilegedAction`], subjects are represented
//! by [`Subject`]s, and privileges by [`Privilege`]s.

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
pub struct UserId(Uuid);
impl From<UserId> for Uuid {
    fn from(value: UserId) -> Self {
        value.0
    }
}
#[derive(Debug, Clone)]
pub struct TokenId(#[allow(dead_code)] Uuid);
impl From<TokenId> for Uuid {
    fn from(value: TokenId) -> Self {
        value.0
    }
}

// -- SECTION: SUBJECT DERIVATION

/// Accessible _subject_ information (see module docs).
#[derive(Debug, Clone)]
pub enum SubjectDetail {
    #[allow(dead_code)]
    User(UserId),
    #[allow(dead_code)]
    Token(TokenId),
    /// No token or user subject could be detected, so instead this fallback subject is produced.
    Anonymous,
}
/// Opaque _subject_ type (see module docs).
///
/// The actual information inside the subject (i.e. the [`SubjectDetail`]) can only be accessed
/// through [`Privilege::subject`]. In this way, it is not possible to forge a `SubjectDetail` _and_
/// pass it to a privileged function that requires a [`Privilege`].
///
/// This is an `axum` extractor, though in most cases it should not be used directly; see the
/// [`AuthSource`] extractor.
///
/// The rejections it produces are empty-bodied, with status codes as follows:
/// ```text
/// 400 BAD REQUEST  failed to run secondary extractor
/// 401 UNAUTHORIZED user's request tried to authenticate itself but fails; note that if no
///                  authentication information is passed, then the extractor will succeed with an
///                  `Anonymous` subject.
/// 500 I.S.E.       internal error
/// ```
// TODO: should SubjectDetail be expose-able via `Debug` or should we use a custom impl instead?
#[derive(Debug)]
pub struct Subject(SubjectDetail);
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
                    TypedHeaderRejectionReason::Error(e) => {
                        tracing::error!("failed to extract XApiToken: {e:?}");
                        return Err(rejection.into_response());
                    }
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
            (Some(t), None) => return Ok(Self(SubjectDetail::Token(t))),
            (None, Some(u)) => return Ok(Self(SubjectDetail::User(u))),
            (Some(t), Some(u)) => {
                tracing::warn!(
                    "received both a token ({}) subject and a user ({}) subject",
                    t.0,
                    u.0
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            (None, None) => Ok(Self(SubjectDetail::Anonymous)),
        }
    }
}

// -- SECTION: AUTHORIZATIONS

// Note: we make use of #[async_trait] here as we ran into this odd issue when using the native
// async-in-traits-support: https://github.com/rust-lang/rust/issues/100013.

/// Things that may go wrong with an attempt to authorize an action.
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

/// Internal type of [`PermissionResult`]. Not exposed publicly to prevent arbitrary construction.
#[derive(Debug)]
enum PermissionResultInner {
    /// The query succeeded, and [`Self::try_into_privilege`] can be used to make `self` into a
    /// [`Privilege`].
    Authorized(Subject),
    /// The query failed.
    #[allow(dead_code)]
    Unauthorized(AuthorizationError),
}
/// Result of a permission query.
#[derive(Debug)]
pub struct PermissionResult(PermissionResultInner);
impl PermissionResult {
    fn unauthorized(error: AuthorizationError) -> Self {
        Self(PermissionResultInner::Unauthorized(error))
    }

    /// Try to create a [`Privilege`] from `self` and a given [`PrivilegedAction`].
    pub fn try_into_privilege<'source, A: PrivilegedAction>(
        self,
        action: A,
    ) -> Result<Privilege<'source, A>, AuthorizationError> {
        match self {
            Self(PermissionResultInner::Authorized(subject)) => Ok(Privilege {
                subject,
                action,
                _pd: Default::default(),
            }),
            Self(PermissionResultInner::Unauthorized(e)) => Err(e),
        }
    }
}

/// Semantically, represents a type of _permission_ (see module level docs).
#[async_trait]
pub trait PrivilegedAction: Debug + Sized {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError>;
}

/// Represents a _privilege_ (see module level docs).
#[derive(Debug)]
pub struct Privilege<'a, A: PrivilegedAction> {
    subject: Subject,
    action: A,
    _pd: PhantomData<&'a ()>,
}
impl<A: PrivilegedAction> Privilege<'_, A> {
    /// Extract the subject information. This is the _only_ way to access the inside of a
    /// `SubjectDetail`.
    pub fn subject(&self) -> &SubjectDetail {
        &self.subject.0
    }
    pub fn action(&self) -> &A {
        &self.action
    }
}

/// Abstracts the ability to query whether a certain subject has a permission.
#[async_trait]
pub trait PermissionQueryExecutor {
    async fn query(&self, s: String) -> PermissionResult;
}

/// Abstracts a source of permissions; semantically, a subject and a capability to look up
/// permissions for that subject (and only that subject).
#[async_trait]
pub trait AuthorizationSource {
    fn from_state_with_subject(app_state: &AppState, subject: Subject) -> Self;

    /// Attempt to authorize the subject embedded in this source to use a permission.
    async fn authorize<'a, A: PrivilegedAction + Send>(
        &'a self,
        action: A,
    ) -> Result<Privilege<'a, A>, AuthorizationError>;
}

/// `axum` extractor for authorization sources.
/// Uses [`AuthorizationSource::from_state_with_subject`] internally.
#[derive(Debug)]
pub struct AuthSource<AS: AuthorizationSource>(pub AS);
#[async_trait]
impl<AS: AuthorizationSource> FromRequestParts<AppState> for AuthSource<AS> {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        tracing::trace!("AuthSource::FromRequestParts");
        let subject: Subject = Subject::from_request_parts(parts, state).await?;
        Ok(AuthSource(AS::from_state_with_subject(state, subject)))
    }
}

/// Implementation of [`PermissionQueryExecutor`] that is backed by the switchboard database.
pub struct DbPermSearcher {
    #[allow(dead_code)]
    db: PgPool,
    subject: Subject,
}
#[async_trait]
impl PermissionQueryExecutor for DbPermSearcher {
    async fn query(&self, perm_str: String) -> PermissionResult {
        let ok = match &self.subject {
            Subject(SubjectDetail::User(user)) => {
                let perms = match sqlx::query!(
                    r#"select user_id,permission from user_privileges where user_id = $1;"#,
                    user.0
                )
                .fetch_all(&self.db)
                .await
                {
                    Ok(v) => v,
                    Err(e) => {
                        return PermissionResult::unauthorized(AuthorizationError::Database(e))
                    }
                };
                perms.iter().any(|r| r.permission == perm_str)
            }
            Subject(SubjectDetail::Token(token)) => {
                let perms = match sqlx::query!(
                    r#"select token_id,permission from api_token_privileges where token_id = $1;"#,
                    token.0
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
            }
            Subject(SubjectDetail::Anonymous) => false,
        };
        if ok {
            PermissionResult(PermissionResultInner::Authorized(Subject(
                self.subject.0.clone(),
            )))
        } else {
            PermissionResult::unauthorized(AuthorizationError::Unauthorized(perm_str))
        }
    }
}

/// Implementation of [`AuthorizationSource`] that is backed by the switchboard database.
pub struct DbPermSource {
    db: PgPool,
    subject: Subject,
}
#[async_trait]
impl AuthorizationSource for DbPermSource {
    fn from_state_with_subject(app_state: &AppState, subject: Subject) -> Self {
        Self {
            db: app_state.db_pool.clone(),
            subject,
        }
    }

    async fn authorize<A: PrivilegedAction + Send>(
        &self,
        action: A,
    ) -> Result<Privilege<A>, AuthorizationError> {
        action
            .authorize(DbPermSearcher {
                db: self.db.clone(),
                subject: Subject(self.subject.0.clone()),
            })
            .await
    }
}

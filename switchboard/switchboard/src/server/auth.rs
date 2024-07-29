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

use crate::server::token::{ApiToken, TokenError, TokenInfoBare};
use crate::server::AppState;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use axum::{async_trait, RequestPartsExt};
use axum_extra::typed_header::TypedHeaderRejectionReason;
use axum_extra::TypedHeader;
use chrono::Utc;
use headers::authorization::Bearer;
use headers::Authorization;
use http::request::Parts;
use http::StatusCode;
use sqlx::{PgExecutor, PgPool};
use std::fmt::{Debug, Display, Formatter};
use std::marker::PhantomData;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Copy, Clone)]
pub struct UserId(Uuid);
impl From<UserId> for Uuid {
    fn from(value: UserId) -> Self {
        value.0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct TokenId(#[allow(dead_code)] Uuid);
impl From<TokenId> for Uuid {
    fn from(value: TokenId) -> Self {
        value.0
    }
}
impl Display for TokenId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "token ({})", self.0)
    }
}

impl TokenId {
    pub async fn fetch_user(&self, db: impl PgExecutor<'_>) -> Result<UserId, sqlx::Error> {
        sqlx::query!(
            "select user_id from api_tokens where api_tokens.token_id = $1;",
            self.0
        )
        .fetch_one(db)
        .await
        .map(|r| UserId(r.user_id))
    }
}

// -- SECTION: SUBJECT DERIVATION

/// Accessible _subject_ information (see module docs).
#[derive(Debug, Clone)]
pub struct SubjectDetail {
    token_info: Arc<TokenInfoBare>,
}
impl SubjectDetail {
    pub fn token_id(&self) -> TokenId {
        TokenId(self.token_info.token_id)
    }
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
        let maybe_bearer = match parts.extract::<TypedHeader<Authorization<Bearer>>>().await {
            Ok(x) => Some(x.0 .0),
            Err(rejection) => match &rejection {
                rej => match rej.reason() {
                    TypedHeaderRejectionReason::Missing => None,
                    TypedHeaderRejectionReason::Error(e) => {
                        tracing::error!("failed to extract Authorization<Bearer>: {e:?}");
                        return Err(rejection.into_response());
                    }
                    _ => unreachable!(),
                },
            },
        };
        if let Some(bearer) = maybe_bearer {
            let token = ApiToken::try_from(bearer).map_err(|e| {
                tracing::warn!("failed to decode bearer token: {e}");
                StatusCode::BAD_REQUEST.into_response()
            })?;
            let token_info = match TokenInfoBare::lookup(&state.db_pool, token).await {
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
            Ok(Self(SubjectDetail {
                token_info: Arc::new(token_info),
            }))
        } else {
            tracing::warn!("no token present for request");
            Err(StatusCode::UNAUTHORIZED.into_response())
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
        let token_info = &self.subject.0.token_info;

        let ok = if token_info.inherits_user_permissions {
            let perms = match sqlx::query!(
                r#"select user_id,permission from user_privileges where user_id = $1;"#,
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
                r#"select token_id,permission from api_token_privileges where token_id = $1;"#,
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
impl Debug for DbPermSource {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "token ({})", self.subject.0.token_info.token_id)
    }
}

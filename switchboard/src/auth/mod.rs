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

use crate::serve::AppState;
use crate::sql::api_token::SqlApiTokenMetadata;
use async_trait::async_trait;
use std::marker::PhantomData;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

pub mod db;
pub mod extract;
pub mod token;

/// Accessible _subject_ information (see module docs).
pub struct SubjectDetail {
    token_info: Arc<SqlApiTokenMetadata>,
}
impl SubjectDetail {
    pub fn token_id(&self) -> Uuid {
        self.token_info.token_id
    }
    pub fn user_id(&self) -> Uuid {
        self.token_info.user_id
    }
}

/// Opaque _subject_ type (see module docs).
///
/// The actual information inside the subject (i.e. the [`SubjectDetail`]) can only be accessed
/// through [`Privilege::subject`]. In this way, it is not possible to forge a `SubjectDetail` _and_
/// pass it to a privileged function that requires a [`Privilege`].
//
// Note that an Axum extractor for `Subject` is implemented in the `extract` module.
pub struct Subject(SubjectDetail);

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

/// Internal type of [`PermissionResult`]. Not exposed publicly to prevent arbitrary construction.
enum PermissionResultInner {
    /// The query succeeded, and [`Self::try_into_privilege`] can be used to make `self` into a
    /// [`Privilege`].
    Authorized(Subject),
    /// The query failed.
    #[allow(dead_code)]
    Unauthorized(AuthorizationError),
}
/// Result of a permission query.
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
pub trait PrivilegedAction: Sized {
    async fn authorize<'source, PQE: PermissionQueryExecutor + Send>(
        self,
        perm_query_exec: PQE,
    ) -> Result<Privilege<'source, Self>, AuthorizationError>;
}

/// Represents a _privilege_ (see module level docs).
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

//--

/// Helper macro to implement simple [`PrivilegedAction`]s.
///
/// Syntax:
/// ```rs
/// struct MyPerm;
/// impl_simple_perm!(MyPerm, "my_perm");
///
/// struct MyOtherPerm {
///     a_variable: uuid::Uuid,
/// }
/// impl_simple_perm!(MyOtherPerm, "access_that_one_thing:{}", self, self.a_variable);
/// ```
///
/// Note that in the second form, where the permission query string has interpolations, it is
/// required (due to macro hygiene) to explicitly pass through the `self` identifier as a literal.
/// Not the prettiest, but probably the best we can do without a proc macro.
#[macro_export]
macro_rules! impl_simple_perm {
    ($t:ty, $perm:literal) => {
        #[::async_trait::async_trait]
        impl $crate::auth::PrivilegedAction for $t {
            async fn authorize<
                'source,
                PQE: $crate::auth::PermissionQueryExecutor + ::core::marker::Send,
            >(
                self,
                perm_query_exec: PQE,
            ) -> ::core::result::Result<
                $crate::auth::Privilege<'source, Self>,
                $crate::auth::AuthorizationError,
            > {
                perm_query_exec
                    .query(str::to_string($perm))
                    .await
                    .try_into_privilege(self)
            }
        }
    };
    ($t:ty, $perm:literal , $this:ident, $($ex:expr),+ $(,)?) => {
        #[::async_trait::async_trait]
        impl $crate::auth::PrivilegedAction for $t {
            async fn authorize<
                'source,
                PQE: $crate::auth::PermissionQueryExecutor + ::core::marker::Send,
            >(
                $this,
                perm_query_exec: PQE,
            ) -> ::core::result::Result<
                $crate::auth::Privilege<'source, Self>,
                $crate::auth::AuthorizationError,
            > {
                perm_query_exec
                    .query(format!($perm, $($ex),+))
                    .await
                    .try_into_privilege($this)
            }
        }
    };
}
pub use impl_simple_perm;

/// Helper macro for adding a [`From`] impl for [`AuthorizationError`] to a given type.
///
/// Syntax:
/// ```rs
/// enum MyResponseTy {
///     Internal,
///     CustomUnauthorized,
/// }
/// impl_from_auth_err!(MyResponseTy, Database => Internal, Unauthorized => CustomUnauthorized);
/// ```
#[macro_export]
macro_rules! impl_from_auth_err {
    ($t:ident, Database => $db:ident, Unauthorized => $unauth:ident) => {
        impl ::core::convert::From<$crate::auth::AuthorizationError> for $t {
            fn from(value: $crate::auth::AuthorizationError) -> Self {
                match value {
                    $crate::auth::AuthorizationError::Database(_) => $t::$db,
                    $crate::auth::AuthorizationError::Unauthorized(_) => $t::$unauth,
                }
            }
        }
    };
}
pub use impl_from_auth_err;

use crate::sql::api_token::SqlApiTokenMetadata;
use std::sync::Arc;
use uuid::Uuid;

pub mod admission;
pub mod engine;
pub mod extract;
pub mod oauth;
pub mod pending_secret;
pub mod token;

/// Name under which the user bearer-token security scheme is registered in the
/// OpenAPI components. Referenced by every operation that extracts a [`Subject`]
/// (see `Subject`'s `aide::OperationInput` impl) and registered once in
/// [`crate::routes::openapi_spec`].
pub const SECURITY_SCHEME: &str = "token";

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
/// The actual information inside the subject (i.e. the [`SubjectDetail`]) can
/// only be accessed through [`Privilege::subject`]. In this way, it is not
/// possible to forge a `SubjectDetail` _and_ pass it to a privileged function
/// that requires a [`Privilege`].
// Note that an Axum extractor for `Subject` is implemented in the `extract`
// module.
pub struct Subject(SubjectDetail);
impl Subject {
    /// The authenticated user's subject id.
    pub fn user_id(&self) -> Uuid {
        self.0.user_id()
    }
    /// The id of the token the request authenticated with.
    pub fn token_id(&self) -> Uuid {
        self.0.token_id()
    }
}

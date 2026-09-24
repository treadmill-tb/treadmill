use super::{Caller, JobSubject, Subject, SubjectDetail, token::SecurityToken};
use crate::serve::AppState;
use crate::sql::{self, api_token::TokenError};
use axum::RequestPartsExt;
use axum::extract::FromRequestParts;
use axum::response::{IntoResponse, Response};
use axum_extra::TypedHeader;
use axum_extra::typed_header::TypedHeaderRejectionReason;
use chrono::Utc;
use headers::Authorization;
use headers::authorization::Bearer;
use http::StatusCode;
use http::request::Parts;
use std::sync::Arc;

async fn bearer_token(parts: &mut Parts) -> Result<SecurityToken, Response> {
    let bearer = match parts.extract::<TypedHeader<Authorization<Bearer>>>().await {
        Ok(x) => x.0.0,
        Err(rejection) => match rejection.reason() {
            TypedHeaderRejectionReason::Missing => {
                tracing::debug!("no token present for request");
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            TypedHeaderRejectionReason::Error(e) => {
                tracing::debug!("failed to extract Authorization<Bearer>: {e:?}");
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            _ => unreachable!(),
        },
    };
    SecurityToken::try_from(bearer).map_err(|e| {
        tracing::debug!("failed to decode bearer token: {e}");
        StatusCode::UNAUTHORIZED.into_response()
    })
}

async fn user_subject(state: &AppState, token: SecurityToken) -> Result<Option<Subject>, Response> {
    let token_info = match sql::api_token::fetch_metadata_by_token(state.pool(), token).await {
        Ok(tib) => tib,
        Err(TokenError::InvalidToken) => return Ok(None),
        Err(e) => {
            tracing::error!("failed to look up a bearer token: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    if token_info.expires_at < Utc::now() {
        tracing::debug!(
            "failed to derive subject: token ({}) expired at {}",
            token_info.token_id,
            token_info.expires_at
        );
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    if let Some(revocation) = token_info.revoked {
        tracing::debug!(
            "failed to derive subject: revoked token ({}): {revocation}",
            token_info.token_id,
        );
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    if token_info.locked {
        tracing::debug!(
            "failed to derive subject: owning user {} is locked (token {})",
            token_info.user_id,
            token_info.token_id,
        );
        return Err(StatusCode::FORBIDDEN.into_response());
    }
    Ok(Some(Subject(SubjectDetail {
        token_info: Arc::new(token_info),
    })))
}

async fn job_subject(
    state: &AppState,
    token: SecurityToken,
) -> Result<Option<JobSubject>, Response> {
    let token_info = match sql::api_token::fetch_job_token_metadata(state.pool(), token).await {
        Ok(info) => info,
        Err(TokenError::InvalidToken) => return Ok(None),
        Err(e) => {
            tracing::error!("failed to look up a job token: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };
    if token_info.finalized {
        tracing::debug!(
            "failed to derive job subject: job {} of token ({}) is finalized",
            token_info.job_id,
            token_info.token_id,
        );
        return Err(StatusCode::UNAUTHORIZED.into_response());
    }
    Ok(Some(JobSubject {
        job_id: token_info.job_id,
    }))
}

fn no_such_token() -> Response {
    tracing::debug!("failed to derive subject: no such token");
    StatusCode::UNAUTHORIZED.into_response()
}

impl FromRequestParts<AppState> for Subject {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).await?;
        user_subject(state, token).await?.ok_or_else(no_such_token)
    }
}

impl FromRequestParts<AppState> for JobSubject {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).await?;
        job_subject(state, token).await?.ok_or_else(no_such_token)
    }
}

impl FromRequestParts<AppState> for Caller {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = bearer_token(parts).await?;
        if let Some(subject) = user_subject(state, token).await? {
            return Ok(Caller::User(subject));
        }
        job_subject(state, token)
            .await?
            .map(Caller::Job)
            .ok_or_else(no_such_token)
    }
}

fn document_auth(
    operation: &mut aide::openapi::Operation,
    schemes: &[&str],
    unauthorized: &str,
    forbidden: &str,
) {
    use aide::openapi::{ReferenceOr, Response, SecurityRequirement, StatusCode};

    for scheme in schemes {
        let mut requirement = SecurityRequirement::new();
        requirement.insert(scheme.to_string(), Vec::new());
        operation.security.push(requirement);
    }

    let responses = operation.responses.get_or_insert_with(Default::default);
    for (code, description) in [(401, unauthorized), (403, forbidden)] {
        responses
            .responses
            .entry(StatusCode::Code(code))
            .or_insert_with(|| {
                ReferenceOr::Item(Response {
                    description: description.to_string(),
                    ..Default::default()
                })
            });
    }
}

/// Every authenticated operation extracts a [`Subject`], so this impl is where
/// the shared auth contract is documented: the operation requires the bearer
/// [`SECURITY_SCHEME`](crate::auth::SECURITY_SCHEME), and the extractor itself
/// can reject with `401` (missing/malformed/expired/revoked token) or `403`
/// (the account is locked) before the handler runs.
impl aide::OperationInput for Subject {
    fn operation_input(
        _ctx: &mut aide::generate::GenContext,
        operation: &mut aide::openapi::Operation,
    ) {
        document_auth(
            operation,
            &[super::SECURITY_SCHEME],
            "Authentication failed: the bearer token is missing, malformed, expired, or revoked.",
            "The authenticated account is locked, or lacks permission for this resource.",
        );
    }
}

impl aide::OperationInput for JobSubject {
    fn operation_input(
        _ctx: &mut aide::generate::GenContext,
        operation: &mut aide::openapi::Operation,
    ) {
        document_auth(
            operation,
            &[super::JOB_SECURITY_SCHEME],
            "Authentication failed: the job token is missing, malformed, or its job has finalized.",
            "The job token belongs to a different job.",
        );
    }
}

impl aide::OperationInput for Caller {
    fn operation_input(
        _ctx: &mut aide::generate::GenContext,
        operation: &mut aide::openapi::Operation,
    ) {
        document_auth(
            operation,
            &[super::SECURITY_SCHEME, super::JOB_SECURITY_SCHEME],
            "Authentication failed: the token is missing, malformed, expired, or revoked, \
             or its job has finalized.",
            "The account is locked or lacks permission for this resource, or the job token \
             belongs to a different job.",
        );
    }
}

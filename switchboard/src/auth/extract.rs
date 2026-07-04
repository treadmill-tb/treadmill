use super::{Subject, SubjectDetail, token::SecurityToken};
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

impl FromRequestParts<AppState> for Subject {
    type Rejection = Response;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // check for a token
        let maybe_bearer = match parts.extract::<TypedHeader<Authorization<Bearer>>>().await {
            Ok(x) => Some(x.0.0),
            Err(rejection) => match rejection.reason() {
                TypedHeaderRejectionReason::Missing => None,
                TypedHeaderRejectionReason::Error(e) => {
                    tracing::error!("failed to extract Authorization<Bearer>: {e:?}");
                    return Err(StatusCode::UNAUTHORIZED.into_response());
                }
                _ => unreachable!(),
            },
        };
        if let Some(bearer) = maybe_bearer {
            let token = SecurityToken::try_from(bearer).map_err(|e| {
                tracing::warn!("failed to decode bearer token: {e}");
                StatusCode::UNAUTHORIZED.into_response()
            })?;
            let token_info =
                match sql::api_token::fetch_metadata_by_token(state.pool(), token).await {
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
            if let Some(revocation) = token_info.revoked {
                tracing::warn!(
                    "failed to derive subject: revoked token ({}): {revocation}",
                    token_info.token_id,
                );
                return Err(StatusCode::UNAUTHORIZED.into_response());
            }
            if token_info.locked {
                tracing::warn!(
                    "failed to derive subject: owning user {} is locked (token {})",
                    token_info.user_id,
                    token_info.token_id,
                );
                return Err(StatusCode::FORBIDDEN.into_response());
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
        use aide::openapi::{ReferenceOr, Response, SecurityRequirement, StatusCode};

        let mut requirement = SecurityRequirement::new();
        requirement.insert(super::SECURITY_SCHEME.to_string(), Vec::new());
        operation.security.push(requirement);

        let responses = operation.responses.get_or_insert_with(Default::default);
        responses
            .responses
            .entry(StatusCode::Code(401))
            .or_insert_with(|| {
                ReferenceOr::Item(Response {
                    description: "Authentication failed: the bearer token is missing, \
                              malformed, expired, or revoked."
                        .to_string(),
                    ..Default::default()
                })
            });
        responses
            .responses
            .entry(StatusCode::Code(403))
            .or_insert_with(|| {
                ReferenceOr::Item(Response {
                    description: "The authenticated account is locked, or lacks permission \
                              for this resource."
                        .to_string(),
                    ..Default::default()
                })
            });
    }
}

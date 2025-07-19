use super::{AuthorizationSource, Subject, SubjectDetail, token::SecurityToken};
use crate::serve::AppState;
use crate::sql::{self, api_token::TokenError};
use async_trait::async_trait;
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

#[async_trait]
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

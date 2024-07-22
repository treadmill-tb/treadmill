use crate::connector::StartJobMessage;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_with::{base64::Base64, serde_as};
use subtle::{Choice, ConstantTimeEq};
use uuid::Uuid;

/// Request Body that [`login_handler`] expects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginRequest {
    pub user_identifier: String,
    pub password: String,
}

#[serde_as]
#[derive(Debug, Serialize, Deserialize, Eq, Copy, Clone)]
pub struct AuthToken(#[serde_as(as = "Base64")] pub [u8; 128]);
impl ConstantTimeEq for AuthToken {
    fn ct_eq(&self, other: &Self) -> Choice {
        // IMPORTANT: use ConstantTimeEq to mitigate possible timing attacks:
        // [`subtle::ConstantTimeEq`] is implemented for [u8] so this is sufficient
        ConstantTimeEq::ct_eq(&self.0[..], &other.0[..])
    }
}
impl PartialEq for AuthToken {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other).into()
    }
}

/// Response Body that [`login_handler`] emits.
///
/// Indicates that the user successfully authenticated, and was issued `token`, which inherits the
/// user's credentials, and will expire at `expires_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: AuthToken,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnqueueJobRequest {
    /// Supervisor to enqueue job on.
    pub supervisor_id: Uuid,
    /// Job request.
    pub start_job_request: StartJobMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EnqueueJobResponse {
    /// Succeeded. (HTTP 200)
    Ok,
    /// Requested supervisor does not exist. (HTTP 404)
    SupervisorNotFound,
    /// Authorization subject does not have sufficient privileges. (HTTP 401)
    Unauthorized,
    /// Job request is invalid. (HTTP 400)
    Invalid { reason: String },
    /// Internal error. (HTTP 500)
    Internal,
    /// Unable to fulfill request due to lack of available time slos.
    Conflict,
}

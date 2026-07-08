//! A typed HTTP client for the switchboard REST API.
//!
//! Every method returns the *same* `api::switchboard::*` types the switchboard
//! serializes from, so a change to a request/response shape — or to an endpoint
//! method/path defined here — is a compile error in every consumer (the web
//! console, an eventual CLI). That closes both the payload gap and the route
//! gap with no codegen and no spec to drift.
//!
//! Enabled by the `client` cargo feature (off by default, so non-HTTP consumers
//! don't pull `reqwest`).

use uuid::Uuid;

use crate::api::switchboard::AuthProvidersResponse;
use crate::api::switchboard::JobRequest;
use crate::api::switchboard::TosInfoResponse;
use crate::api::switchboard::WhoAmIResponse;
use crate::api::switchboard::audit::AuditFeedResponse;
use crate::api::switchboard::hosts::HostInfo;
use crate::api::switchboard::images::ImageSetInfo;
use crate::api::switchboard::jobs::{EnqueueJobResponse, JobInfo, JobListResponse};
use crate::api::switchboard::users::{PublicUserProfile, SelfUserProfile, SessionInfo};
use crate::api::switchboard::{LoginCompleteRequest, LoginResponse, LoginStagedResponse};

/// All the ways a switchboard API call can fail, from the caller's point of
/// view. `Unauthorized` is split out from other error statuses because callers
/// (e.g. the web console) act on it specifically: drop the session and ask the
/// user to log in again.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The request never produced a valid HTTP response (DNS, TLS, connect,
    /// timeout, body decode, …).
    #[error("switchboard request failed: {0}")]
    Transport(#[from] reqwest::Error),
    /// `401 Unauthorized`: the bearer token is missing, invalid, or expired.
    #[error("unauthorized: the session token is missing, invalid, or expired")]
    Unauthorized,
    /// Any other non-success status, with the response body for context.
    #[error("switchboard returned HTTP {status}: {body}")]
    Status { status: u16, body: String },
}

/// The two live outcomes of [`SwitchboardClient::login_complete`]: the login
/// finished and yielded the session, or a step is still required and the
/// response carries a fresh staged pair (the presented one was consumed).
#[derive(Debug, Clone)]
pub enum LoginCompleteOutcome {
    Complete(LoginResponse),
    Staged(LoginStagedResponse),
}

/// A handle to one switchboard instance, optionally carrying a bearer token for
/// authenticated calls.
///
/// `base_url` is the switchboard origin (e.g. `https://sb.example`); the
/// `/api/v1` prefix is added internally. Cheap to clone (wraps a `reqwest`
/// client, which is itself an `Arc` internally).
#[derive(Clone)]
pub struct SwitchboardClient {
    http: reqwest::Client,
    /// Origin without a trailing slash, e.g. `https://sb.example`.
    base_url: String,
    /// The bearer token to send, already encoded for HTTP (base64), if any.
    token: Option<String>,
}

impl SwitchboardClient {
    /// Build a client for `base_url`. Pass `token = None` for anonymous calls
    /// (e.g. building the login URL) or `Some(token)` for authenticated ones.
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        Self {
            http: reqwest::Client::new(),
            base_url,
            token,
        }
    }

    /// `GET /auth/providers` — the login methods this switchboard offers, so a
    /// frontend can render the right buttons. Unauthenticated.
    pub async fn auth_providers(&self) -> Result<AuthProvidersResponse, ClientError> {
        self.get_json("/api/v1/auth/providers").await
    }

    /// Turn a `login_path` from [`auth_providers`](Self::auth_providers) (the
    /// `login_path` field on each advertised provider/identity) into an absolute
    /// URL a browser can be redirected to. The console links its login buttons
    /// here; switchboard then carries the user through the flow.
    pub fn login_url(&self, login_path: &str) -> String {
        format!("{}{login_path}", self.base_url)
    }

    /// `GET /auth/tos` — the Terms of Service text + version a login must
    /// accept before it completes (the ToS interstitial). Unauthenticated.
    pub async fn tos_info(&self) -> Result<TosInfoResponse, ClientError> {
        self.get_json("/api/v1/auth/tos").await
    }

    /// Absolute URL of `POST /auth/login/complete`, for a browser frontend to
    /// use as its completion form's `action` (the endpoint accepts the staged
    /// pair form-encoded, so a no-JS HTML form can finish the login directly).
    pub fn login_complete_url(&self) -> String {
        format!("{}/api/v1/auth/login/complete", self.base_url)
    }

    /// `POST /auth/login/complete` — claim a staged login by presenting its
    /// single-use pair, JSON and server-to-server. Unauthenticated (the pair
    /// is the capability). A `200` yields the session
    /// ([`LoginCompleteOutcome::Complete`]); a `409` means a step is still
    /// required and carries a fresh pair to retry with
    /// ([`LoginCompleteOutcome::Staged`]); anything else (notably the `410`
    /// for an unknown/expired/used pair) maps to [`ClientError`].
    pub async fn login_complete(
        &self,
        request: &LoginCompleteRequest,
    ) -> Result<LoginCompleteOutcome, ClientError> {
        let resp = self
            .http
            .post(format!("{}/api/v1/auth/login/complete", self.base_url))
            .json(request)
            .send()
            .await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(LoginCompleteOutcome::Complete(resp.json().await?));
        }
        if status == reqwest::StatusCode::CONFLICT {
            return Ok(LoginCompleteOutcome::Staged(resp.json().await?));
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ClientError::Status {
            status: status.as_u16(),
            body,
        })
    }

    /// `GET /auth/whoami` — the identity behind the current token.
    pub async fn whoami(&self) -> Result<WhoAmIResponse, ClientError> {
        self.get_json("/api/v1/auth/whoami").await
    }

    /// `GET /users/me` — the caller's own profile (emails, groups, lock state).
    pub async fn get_me(&self) -> Result<SelfUserProfile, ClientError> {
        self.get_json("/api/v1/users/me").await
    }

    /// `GET /users/{id}` — the public subset of another user's profile.
    pub async fn get_user(&self, user_id: Uuid) -> Result<PublicUserProfile, ClientError> {
        self.get_json(&format!("/api/v1/users/{user_id}")).await
    }

    /// `GET /users/me/tokens` — the caller's sessions/tokens.
    pub async fn list_my_tokens(&self) -> Result<Vec<SessionInfo>, ClientError> {
        self.get_json("/api/v1/users/me/tokens").await
    }

    /// `GET /users/{id}/events` — a user's audit feed (self/admin only).
    pub async fn user_events(&self, user_id: Uuid) -> Result<AuditFeedResponse, ClientError> {
        self.get_json(&format!("/api/v1/users/{user_id}/events"))
            .await
    }

    /// `GET /jobs` — a keyset-paginated page of the jobs the caller may read,
    /// newest first. Pass `cursor` from a previous response's `next_cursor` to
    /// fetch the next page, and an optional `limit` (the switchboard clamps it).
    pub async fn list_jobs(
        &self,
        limit: Option<u32>,
        cursor: Option<&str>,
    ) -> Result<JobListResponse, ClientError> {
        // Both parameters are URL-safe as-is (limit is digits; cursor is
        // URL_SAFE_NO_PAD base64), so no percent-encoding is needed here.
        let mut params: Vec<String> = Vec::new();
        if let Some(limit) = limit {
            params.push(format!("limit={limit}"));
        }
        if let Some(cursor) = cursor {
            params.push(format!("cursor={cursor}"));
        }
        let path = if params.is_empty() {
            "/api/v1/jobs".to_string()
        } else {
            format!("/api/v1/jobs?{}", params.join("&"))
        };
        self.get_json(&path).await
    }

    /// `GET /jobs/{id}` — the full view of one job (caller must be able to read
    /// it).
    pub async fn get_job(&self, job_id: Uuid) -> Result<JobInfo, ClientError> {
        self.get_json(&format!("/api/v1/jobs/{job_id}")).await
    }

    /// `GET /jobs/{id}/events` — one job's audit feed.
    pub async fn job_events(&self, job_id: Uuid) -> Result<AuditFeedResponse, ClientError> {
        self.get_json(&format!("/api/v1/jobs/{job_id}/events"))
            .await
    }

    /// `POST /jobs` — enqueue a new job, returning its assigned id.
    pub async fn enqueue_job(&self, req: &JobRequest) -> Result<EnqueueJobResponse, ClientError> {
        self.post_json("/api/v1/jobs", req).await
    }

    /// `DELETE /jobs/{id}` — request termination of a job. Idempotent: a job
    /// that is already finalized is a no-op (the switchboard returns `204`).
    pub async fn terminate_job(&self, job_id: Uuid) -> Result<(), ClientError> {
        self.delete(&format!("/api/v1/jobs/{job_id}")).await
    }

    /// `GET /hosts` — the read-only host listing (tags, targets, liveness),
    /// e.g. to populate a host picker.
    pub async fn list_hosts(&self) -> Result<Vec<HostInfo>, ClientError> {
        self.get_json("/api/v1/hosts").await
    }

    /// `GET /image-sets` — the image sets the caller owns, e.g. to populate
    /// a job's image selector.
    pub async fn list_image_sets(&self) -> Result<Vec<ImageSetInfo>, ClientError> {
        self.get_json("/api/v1/image-sets").await
    }

    /// Issue an authenticated `GET` for `path` and deserialize the JSON body,
    /// mapping `401` to [`ClientError::Unauthorized`] and any other non-success
    /// status to [`ClientError::Status`].
    async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T, ClientError> {
        let mut req = self.http.get(format!("{}{path}", self.base_url));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json::<T>().await?);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ClientError::Status {
            status: status.as_u16(),
            body,
        })
    }

    /// Issue an authenticated `POST` of `body` (as JSON) to `path` and
    /// deserialize the JSON response, with the same error mapping as
    /// [`get_json`](Self::get_json). Any 2xx is treated as success.
    async fn post_json<B: serde::Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ClientError> {
        let mut req = self
            .http
            .post(format!("{}{path}", self.base_url))
            .json(body);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(resp.json::<T>().await?);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ClientError::Status {
            status: status.as_u16(),
            body,
        })
    }

    /// Issue an authenticated `DELETE` for `path`, discarding the (typically
    /// empty) response body. Any 2xx — including `202 Accepted` and `204 No
    /// Content` — is success; error mapping matches [`get_json`](Self::get_json).
    async fn delete(&self, path: &str) -> Result<(), ClientError> {
        let mut req = self.http.delete(format!("{}{path}", self.base_url));
        if let Some(token) = &self.token {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await?;

        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(ClientError::Unauthorized);
        }
        let body = resp.text().await.unwrap_or_default();
        Err(ClientError::Status {
            status: status.as_u16(),
            body,
        })
    }
}

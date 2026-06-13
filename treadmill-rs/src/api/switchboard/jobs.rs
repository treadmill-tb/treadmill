//! Job-scoped client API types.

use serde::{Deserialize, Serialize};

/// Connection credentials for tailing/replaying a job's console logs over NATS,
/// returned by `POST /jobs/{id}/log-token`.
///
/// The token is a short-lived **bearer** user JWT scoped to *subscribe* to this
/// job's log subjects (`subject`); the client connects to `nats_url` with the
/// token string alone (no nkey seed). The token only needs to be valid at
/// connect time — an established NATS connection is not dropped when the JWT
/// expires — so a client re-requests credentials when it next reconnects, after
/// roughly `expires_in_secs`.
#[derive(schemars::JsonSchema, Debug, Clone, Serialize, Deserialize)]
pub struct LogStreamCredentials {
    /// NATS client URL to connect to (e.g. `nats://nats.example:4222`).
    pub nats_url: String,
    /// Subject wildcard covering all of this job's log channels:
    /// `logs.<job-id>.>`.
    pub subject: String,
    /// Bearer user JWT authorizing subscribe on `subject`.
    pub token: String,
    /// Seconds until the token's `exp`; re-request credentials after this
    /// elapses (only needed to open a *new* connection).
    pub expires_in_secs: u64,
}

/// Challenge-based authentication for switchboard-supervisor websocket connections.
pub mod challenge {
    use serde::{Deserialize, Serialize};
    use uuid::Uuid;

    pub const NONCE_LEN: usize = 32;

    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub struct ChallengeRequest {
        pub uuid: Uuid,
    }
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Challenge {
        pub switchboard_nonce: [u8; NONCE_LEN],
    }
    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub struct ChallengeResponse {
        pub switchboard_nonce_signature: ed25519_dalek::Signature,
    }
    #[derive(Debug, Copy, Clone, Serialize, Deserialize)]
    pub enum ChallengeResult {
        Authenticated,
        Unauthenticated,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "type")]
    #[non_exhaustive]
    pub enum ChallengeMessage {
        ChallengeRequest(ChallengeRequest),
        Challenge(Challenge),
        ChallengeResponse(ChallengeResponse),
        ChallengeResult(ChallengeResult),
    }
}

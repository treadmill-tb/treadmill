use base64::Engine;
use base64::prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD};
use uuid::Uuid;

pub fn encode(staged_id: Uuid, staged_secret: &str) -> String {
    let mut bytes = staged_id.as_bytes().to_vec();
    bytes.extend(
        BASE64_STANDARD
            .decode(staged_secret)
            .expect("staged secrets are base64"),
    );
    BASE64_URL_SAFE_NO_PAD.encode(bytes)
}

pub fn decode(code: &str) -> Option<(Uuid, String)> {
    let bytes = BASE64_URL_SAFE_NO_PAD.decode(code.trim()).ok()?;
    let (id, secret) = bytes.split_at_checked(16)?;
    (secret.len() == 32).then(|| {
        (
            Uuid::from_slice(id).expect("16 bytes"),
            BASE64_STANDARD.encode(secret),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::staged_secret;

    #[test]
    fn roundtrip() {
        let id = Uuid::new_v4();
        let secret = staged_secret::generate();
        let code = encode(id, &secret);
        assert_eq!(code.len(), 64);
        assert_eq!(decode(&code), Some((id, secret)));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode(""), None);
        assert_eq!(decode("not base64!"), None);
        assert_eq!(decode(&BASE64_URL_SAFE_NO_PAD.encode([0u8; 47])), None);
    }
}

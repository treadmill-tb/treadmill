//! The one-time secret that guards a staged login (`pending_registrations`).
//!
//! The staging row's id is deliberately NOT a capability (ids are never
//! secrets): completing a login at `POST /auth/login/complete` requires
//! presenting the id together with this secret, which is generated alongside
//! the row and handed only to the just-authenticated party. The database
//! stores a salted argon2id hash (a PHC string), so read access to the table
//! does not yield the ability to complete anyone's login.

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};

use crate::auth::token::SecurityToken;

/// Generate a fresh completion secret: a 32-byte CSPRNG token in its base-64
/// wire form (the same shape as session tokens).
pub fn generate() -> String {
    SecurityToken::generate().to_string()
}

/// Hash `secret` for storage, returning the salted argon2id PHC string.
pub fn hash(secret: &str) -> Result<String, argon2::password_hash::Error> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(secret.as_bytes(), &salt)?
        .to_string())
}

/// Verify a presented `secret` against the stored PHC string. Any malformed
/// hash is treated as a mismatch (fail closed).
pub fn verify(secret: &str, phc: &str) -> bool {
    PasswordHash::new(phc)
        .map(|parsed| {
            Argon2::default()
                .verify_password(secret.as_bytes(), &parsed)
                .is_ok()
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_mismatch() {
        let secret = generate();
        let phc = hash(&secret).unwrap();
        assert!(phc.starts_with("$argon2id$"), "salted argon2id PHC string");
        assert!(verify(&secret, &phc));
        assert!(!verify(&generate(), &phc), "wrong secret rejected");
        assert!(
            !verify(&secret, "not-a-phc-string"),
            "garbage hash rejected"
        );
    }
}

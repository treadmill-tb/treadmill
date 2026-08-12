//! Minting the tokens that admit a request to one of a job's services.
//!
//! A job is not reachable from the internet: a request arrives instead at a
//! stateless gateway as `<service>-<job-id>.<domain>`, which admits it only
//! against a switchboard-minted, EdDSA-signed token and then proxies it to the
//! job's own address. The job validates the same token again, so reaching a
//! service takes a valid token at both ends.
//!
//! A token's `aud` is the bare DNS label `<service>-<job-id>`, not a URL: a
//! gateway authorizes with one string comparison against the first label of the
//! `Host` header, and one token is therefore good at whichever gateway the user
//! reaches. The flat `tml_*` claims carry what `aud` already encodes, so a
//! gateway can template them without parsing it.
//!
//! There is one signing key per deployment, with its `kid` derived from the
//! public key: rotating the key yields a new `kid` on its own, and a gateway
//! never needs a per-job key lookup.
//!
//! Minting is pure and unit-tested; nothing here touches the database.

use std::net::IpAddr;

use chrono::{DateTime, TimeDelta, Utc};
use ed25519_dalek::SigningKey;
use ed25519_dalek::pkcs8::{EncodePrivateKey, EncodePublicKey, spki::der::pem::LineEnding};
use jsonwebtoken::jwk::{
    AlgorithmParameters, EllipticCurve, Jwk, OctetKeyPairParameters, OctetKeyPairType,
    ThumbprintHash,
};
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use treadmill_rs::api;
use treadmill_rs::api::switchboard::jobs::JobServiceEndpoint;
use uuid::Uuid;

use crate::config::{self, JobGatewayConfig};

/// Longest service name a job may announce, mirroring the `job_services`
/// CHECK: names are job-supplied, and the cap is what keeps
/// `<service>-<job-id>` inside a DNS label.
const MAX_SERVICE_NAME_LEN: usize = 16;

/// Failure to load the configured signing material.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// No gateways configured, leaving no host to build a URL from.
    #[error("job_gateway.endpoints must list at least one endpoint")]
    NoEndpoints,
    /// The configured signing key is not a hex-encoded Ed25519 seed. Carries
    /// nothing of what it was given.
    #[error("job_gateway.signing_key must be a hex-encoded 32-byte Ed25519 seed")]
    SigningKey,
    /// The public key derived from the seed could not be encoded for the
    /// gateways.
    #[error("encoding the job gateway public key: {0}")]
    PublicKey(#[from] ed25519_dalek::pkcs8::spki::Error),
}

/// The job-gateway signing material, derived once at startup. Cloneable; a
/// single instance is built by [`crate::serve`] and shared by every route.
#[derive(Clone)]
pub struct JobGateway {
    config: JobGatewayConfig,
    encoding_key: EncodingKey,
    public_key_pem: String,
    key_id: String,
}

impl JobGateway {
    /// Derive the signing material from `config`, rejecting an unusable
    /// configuration outright so a deployment fails at startup rather than at
    /// the first mint.
    pub fn new(config: JobGatewayConfig) -> Result<Self, KeyError> {
        if config.endpoints.is_empty() {
            return Err(KeyError::NoEndpoints);
        }

        let seed: [u8; 32] = hex::decode(config.signing_key.trim())
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or(KeyError::SigningKey)?;
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        let encoding_key = EncodingKey::from_ed_der(
            signing_key
                .to_pkcs8_der()
                .map_err(|_| KeyError::SigningKey)?
                .as_bytes(),
        );

        Ok(Self {
            config,
            encoding_key,
            public_key_pem: verifying_key.to_public_key_pem(LineEnding::LF)?,
            key_id: key_id(&verifying_key),
        })
    }

    pub fn config(&self) -> &JobGatewayConfig {
        &self.config
    }

    /// Identifier of the signing key, carried as a minted token's `kid`.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// The signing public key, in the form handed to the gateways and to the
    /// job itself for validating minted tokens.
    pub fn public_key_pem(&self) -> &str {
        &self.public_key_pem
    }

    /// The (base_domain, port) tuple minted URLs are built from.
    /// [`JobGateway::new`] rejects an empty domain list, so there is always
    /// one.
    pub fn primary_endpoint(&self) -> (&str, u16) {
        let ep = &self.config.endpoints[0];
        (&ep.base_domain, ep.port)
    }
}

/// The RFC 7638 JWK thumbprint of the public key: what a gateway resolving a
/// token's `kid` against a published key set computes for itself.
fn key_id(verifying_key: &ed25519_dalek::VerifyingKey) -> String {
    use base64::Engine as _;

    Jwk {
        common: Default::default(),
        algorithm: AlgorithmParameters::OctetKeyPair(OctetKeyPairParameters {
            key_type: OctetKeyPairType::OctetKeyPair,
            curve: EllipticCurve::Ed25519,
            x: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(verifying_key.as_bytes()),
        }),
    }
    .thumbprint(ThumbprintHash::SHA256)
    .expect("Failed to generate JWK thumbprint")
}

/// The claims of a service token. Beyond the registered ones, `tml_job`,
/// `tml_service` and `tml_addr` restate what `aud` encodes plus the address the
/// gateway is to dial, in a shape a gateway can template directly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct ServiceTokenClaims {
    iss: String,
    sub: Uuid,
    aud: String,
    exp: i64,
    iat: i64,
    nbf: i64,
    jti: Uuid,
    tml_job: Uuid,
    tml_service: String,
    tml_addr: IpAddr,
}

/// A minted service token and the moment it stops being accepted.
#[derive(Debug, Clone)]
pub struct MintedToken {
    pub token: String,
    pub expires_at: DateTime<Utc>,
}

/// Failure to mint a service token.
#[derive(Debug, thiserror::Error)]
pub enum MintError {
    /// The service name would not survive being made into a DNS label.
    #[error("invalid service name: {0:?}")]
    ServiceName(String),
    /// The claims could not be signed.
    #[error("signing a job service token: {0}")]
    Sign(#[from] jsonwebtoken::errors::Error),
}

/// The DNS label a job's service is reached under, `<service>-<job-id>`. A
/// service name holds no hyphen, so the label always splits back at its first
/// one.
pub fn service_label(job_id: Uuid, service: &str) -> String {
    format!("{service}-{job_id}")
}

/// The service endpoint for a given gateway (as an `(fqdn, port)` tuple).
pub fn service_endpoint(
    job_id: Uuid,
    service: &str,
    endpoint_base_domain: &str,
    endpoint_port: u16,
) -> JobServiceEndpoint {
    JobServiceEndpoint {
        hostname: format!("{}.{endpoint_base_domain}", service_label(job_id, service)),
        port: endpoint_port,
    }
}

/// Build the [`JobGatewayDispatch`] handed to a supervisor in `StartJobMessage`
/// and relayed by it into the job.
///
/// Carries no token, unlike its log-streaming counterpart: the job mints
/// nothing and only validates the tokens its callers arrive with, for which the
/// public key and the domains it is published under are all it needs.
pub fn build_dispatch(gateway: &JobGateway) -> api::switchboard_supervisor::JobGatewayDispatch {
    api::switchboard_supervisor::JobGatewayDispatch {
        issuer: gateway.config.issuer.clone(),
        signing_public_key: gateway.public_key_pem.clone(),
        key_id: gateway.key_id.clone(),
        endpoints: gateway
            .config
            .endpoints
            .iter()
            .cloned()
            .map(|config::JobGatewayEndpoint { base_domain, port }| {
                api::switchboard_supervisor::JobGatewayEndpoint { base_domain, port }
            })
            .collect(),
    }
}

/// Mint a token admitting `subject_id` to `service` of `job_id`, at any gateway
/// and for the configured lifetime. `address` is the job's own address, which
/// the gateway dials and which is supervisor-reported, never job-reported.
///
/// The name is re-validated here: it ends up in a DNS label, and a caller may
/// have it from somewhere looser than the `job_services` CHECK.
pub fn mint_token(
    gateway: &JobGateway,
    job_id: Uuid,
    service: &str,
    subject_id: Uuid,
    address: IpAddr,
) -> Result<MintedToken, MintError> {
    validate_service_name(service)?;

    let issued_at = Utc::now();
    // Saturating throughout: an absurd TTL must not wrap into a past `exp`.
    let expires_at = TimeDelta::from_std(gateway.config.token_ttl)
        .ok()
        .and_then(|ttl| issued_at.checked_add_signed(ttl))
        .unwrap_or(DateTime::<Utc>::MAX_UTC);

    let claims = ServiceTokenClaims {
        iss: gateway.config.issuer.clone(),
        sub: subject_id,
        aud: service_label(job_id, service),
        exp: expires_at.timestamp(),
        iat: issued_at.timestamp(),
        nbf: issued_at.timestamp(),
        jti: Uuid::new_v4(),
        tml_job: job_id,
        tml_service: service.to_string(),
        tml_addr: address,
    };

    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(gateway.key_id.clone());

    Ok(MintedToken {
        token: jsonwebtoken::encode(&header, &claims, &gateway.encoding_key)?,
        expires_at,
    })
}

fn validate_service_name(service: &str) -> Result<(), MintError> {
    let mut chars = service.chars();
    let acceptable = service.len() <= MAX_SERVICE_NAME_LEN
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit());

    acceptable
        .then_some(())
        .ok_or_else(|| MintError::ServiceName(service.to_string()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ed25519_dalek::VerifyingKey;
    use ed25519_dalek::pkcs8::DecodePublicKey;
    use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};

    use super::*;

    const ISSUER: &str = "https://switchboard.example";
    const PRIMARY_DOMAIN: &str = "gw-us-east-1.treadmillusercontent.com";

    /// A throwaway signing key per run keeps a real secret out of the tree.
    fn test_config() -> JobGatewayConfig {
        JobGatewayConfig {
            issuer: ISSUER.to_string(),
            endpoints: vec![
                config::JobGatewayEndpoint {
                    base_domain: PRIMARY_DOMAIN.to_string(),
                    port: 443,
                },
                config::JobGatewayEndpoint {
                    base_domain: "gw-eu-central-1.treadmillusercontent.com".to_string(),
                    port: 4433,
                },
            ],
            token_ttl: Duration::from_secs(7 * 24 * 60 * 60),
            signing_key: hex::encode(rand::random::<[u8; 32]>()),
        }
    }

    fn test_gateway(config: JobGatewayConfig) -> JobGateway {
        JobGateway::new(config).expect("gateway")
    }

    /// The public key a gateway would hold, recovered from the material the
    /// switchboard publishes rather than from the seed.
    fn published_key(gateway: &JobGateway) -> VerifyingKey {
        VerifyingKey::from_public_key_pem(gateway.public_key_pem()).expect("public key is SPKI PEM")
    }

    /// Validate a token as a gateway does: EdDSA only, this issuer, and an
    /// audience of exactly the label the request arrived on.
    fn validate_as_gateway(
        gateway: &JobGateway,
        token: &str,
        label: &str,
    ) -> jsonwebtoken::errors::Result<ServiceTokenClaims> {
        let mut validation = Validation::new(Algorithm::EdDSA);
        validation.set_issuer(&[ISSUER]);
        validation.set_audience(&[label]);
        let key = DecodingKey::from_ed_der(published_key(gateway).as_bytes());
        decode::<ServiceTokenClaims>(token, &key, &validation).map(|data| data.claims)
    }

    #[test]
    fn claims_carry_the_job_service_and_address() {
        let gateway = test_gateway(test_config());
        let job_id = Uuid::new_v4();
        let subject_id = Uuid::new_v4();
        let address: IpAddr = "fd00::2".parse().unwrap();

        let minted = mint_token(&gateway, job_id, "webide", subject_id, address).expect("mint");
        let claims = validate_as_gateway(&gateway, &minted.token, &service_label(job_id, "webide"))
            .expect("token validates under its own label");

        assert_eq!(claims.iss, ISSUER);
        assert_eq!(claims.sub, subject_id);
        assert_eq!(claims.tml_job, job_id);
        assert_eq!(claims.tml_service, "webide");
        assert_eq!(claims.tml_addr, address);

        // Valid from issuance until the configured lifetime is up, which is
        // also what the caller is told.
        assert_eq!(claims.nbf, claims.iat);
        assert_eq!(
            claims.exp - claims.iat,
            gateway.config().token_ttl.as_secs() as i64
        );
        assert_eq!(minted.expires_at.timestamp(), claims.exp);
    }

    #[test]
    fn aud_is_the_bare_dns_label() {
        let gateway = test_gateway(test_config());
        let job_id = Uuid::new_v4();

        let minted = mint_token(
            &gateway,
            job_id,
            "webide",
            Uuid::new_v4(),
            "fd00::2".parse().unwrap(),
        )
        .expect("mint");
        let claims =
            validate_as_gateway(&gateway, &minted.token, &service_label(job_id, "webide")).unwrap();

        assert_eq!(claims.aud, format!("webide-{job_id}"));
        // Neither the FQDN nor the URL: a gateway compares it against the first
        // label of the Host header, so it must carry no domain of its own.
        assert!(!claims.aud.contains('.'));
        assert!(!claims.aud.contains('/'));

        // A service name holds no hyphen, so the label splits at its first one.
        let (service, id) = claims.aud.split_once('-').expect("label has a separator");
        assert_eq!(service, "webide");
        assert_eq!(id.parse::<Uuid>().unwrap(), job_id);
    }

    #[test]
    fn a_token_verifies_under_the_derived_key_id() {
        let gateway = test_gateway(test_config());
        let job_id = Uuid::new_v4();

        let minted = mint_token(
            &gateway,
            job_id,
            "webide",
            Uuid::new_v4(),
            "fd00::2".parse().unwrap(),
        )
        .expect("mint");

        let header = decode_header(&minted.token).expect("header");
        assert_eq!(header.alg, Algorithm::EdDSA);
        assert_eq!(header.kid.as_deref(), Some(gateway.key_id()));

        validate_as_gateway(&gateway, &minted.token, &service_label(job_id, "webide"))
            .expect("token validates under the published key");
    }

    #[test]
    fn a_token_is_rejected_off_its_own_label_or_key() {
        let gateway = test_gateway(test_config());
        let job_id = Uuid::new_v4();
        let minted = mint_token(
            &gateway,
            job_id,
            "webide",
            Uuid::new_v4(),
            "fd00::2".parse().unwrap(),
        )
        .expect("mint");

        // Another service of the same job, and the same service of another
        // job, are each a different audience.
        assert!(
            validate_as_gateway(&gateway, &minted.token, &service_label(job_id, "shell")).is_err()
        );
        assert!(
            validate_as_gateway(
                &gateway,
                &minted.token,
                &service_label(Uuid::new_v4(), "webide")
            )
            .is_err()
        );

        // A token from a rotated (or foreign) key does not validate under this
        // one.
        let other = test_gateway(test_config());
        let foreign = mint_token(
            &other,
            job_id,
            "webide",
            Uuid::new_v4(),
            "fd00::2".parse().unwrap(),
        )
        .expect("mint");
        assert_ne!(other.key_id(), gateway.key_id());
        assert!(
            validate_as_gateway(&gateway, &foreign.token, &service_label(job_id, "webide"))
                .is_err()
        );
    }

    #[test]
    fn the_key_id_follows_the_public_key() {
        let config = test_config();
        let gateway = test_gateway(config.clone());

        // Derived from the key alone: the same seed always yields the same
        // `kid`, so a restart does not invalidate tokens in flight.
        assert_eq!(gateway.key_id(), JobGateway::new(config).unwrap().key_id());

        // ...and it is the thumbprint of the published key, which is what a
        // gateway holding only that key computes.
        assert_eq!(gateway.key_id(), key_id(&published_key(&gateway)));
        assert!(!gateway.key_id().is_empty());
    }

    /// The label is what has to fit in DNS; the cap on a service name is what
    /// keeps it there, whatever the domain it is published under.
    #[test]
    fn the_longest_label_fits_a_dns_label() {
        let service = "a".repeat(MAX_SERVICE_NAME_LEN);
        let label = service_label(Uuid::new_v4(), &service);

        // The longest a job can make it: the longest permitted name, a hyphen
        // and a job id.
        assert_eq!(label.len(), MAX_SERVICE_NAME_LEN + 1 + 36);
        assert!(label.len() <= 63, "{label} exceeds a DNS label");
    }

    #[test]
    fn a_service_name_that_is_not_a_label_is_refused() {
        let gateway = test_gateway(test_config());
        let mint = |service: &str| {
            mint_token(
                &gateway,
                Uuid::new_v4(),
                service,
                Uuid::new_v4(),
                "fd00::2".parse().unwrap(),
            )
        };

        for service in ["webide", "s", "web2", &"a".repeat(MAX_SERVICE_NAME_LEN)] {
            assert!(mint(service).is_ok(), "{service:?} is a valid name");
        }

        for service in [
            "",
            &"a".repeat(MAX_SERVICE_NAME_LEN + 1),
            "Webide",
            "web-ide",
            "web_ide",
            "web.ide",
            "2web",
            "web ide",
            "wébide",
            "*",
        ] {
            assert!(
                matches!(mint(service), Err(MintError::ServiceName(_))),
                "{service:?} must not become a label"
            );
        }
    }

    #[test]
    fn unusable_configuration_is_refused() {
        assert!(matches!(
            JobGateway::new(JobGatewayConfig {
                endpoints: Vec::new(),
                ..test_config()
            }),
            Err(KeyError::NoEndpoints)
        ));

        for signing_key in [
            "",
            "not-hex",
            &hex::encode([0u8; 31]),
            &hex::encode([0u8; 33]),
        ] {
            assert!(
                matches!(
                    JobGateway::new(JobGatewayConfig {
                        signing_key: signing_key.to_string(),
                        ..test_config()
                    }),
                    Err(KeyError::SigningKey)
                ),
                "{signing_key:?} is not a usable seed"
            );
        }
    }
}

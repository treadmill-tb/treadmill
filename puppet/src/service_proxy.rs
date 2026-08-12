use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use log::{info, warn};
use serde::Deserialize;
use uuid::Uuid;

use treadmill_rs::api::supervisor_puppet::{JobGatewayInfo, JobService};

/// Longest service name the switchboard accepts.
pub const MAX_SERVICE_NAME_LEN: usize = 16;

pub fn service_name_valid(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= MAX_SERVICE_NAME_LEN
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// One `*.json` file under the services directory: a [`JobService`] to announce,
/// plus the local address a reverse proxy reaches it at. `upstream` stays on
/// disk and is never announced; the switchboard has no business knowing where a
/// service listens inside its job.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ServiceDeclaration {
    #[serde(flatten)]
    pub service: JobService,
    pub upstream: Option<String>,
}

fn upstream_valid(upstream: &str) -> bool {
    !upstream.is_empty()
        && upstream.len() <= 255
        && upstream
            .chars()
            .all(|c| c.is_ascii_graphic() && !matches!(c, '{' | '}' | '"' | '\'' | '\\' | '#'))
}

fn base_domain_valid(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '-'))
}

/// What a job needs to accept the same tokens its gateways accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GatewayMaterial {
    pub issuer: String,
    pub sign_key: String,
    pub base_domains: Vec<String>,
}

impl GatewayMaterial {
    pub fn from_info(info: &JobGatewayInfo) -> Result<Self> {
        Ok(GatewayMaterial {
            issuer: info.issuer.clone(),
            sign_key: sign_key_base64(&info.signing_public_key)?,
            base_domains: info
                .endpoints
                .iter()
                .filter_map(|endpoint| {
                    if base_domain_valid(&endpoint.base_domain) {
                        Some(endpoint.base_domain.clone())
                    } else {
                        warn!(
                            "Ignoring gateway endpoint with unusable base domain {:?}",
                            endpoint.base_domain
                        );
                        None
                    }
                })
                .collect(),
        })
    }
}

/// The switchboard publishes its verifying key as SPKI PEM; caddy-jwt takes an
/// EdDSA key as base64 of the raw 32 bytes, and accepts PEM only for the other
/// algorithms.
fn sign_key_base64(public_key_pem: &str) -> Result<String> {
    use base64::Engine as _;
    use ed25519_dalek::pkcs8::DecodePublicKey;

    let key = ed25519_dalek::VerifyingKey::from_public_key_pem(public_key_pem.trim())
        .context("Parsing the gateway signing key as SPKI PEM")?;

    Ok(base64::engine::general_purpose::STANDARD.encode(key.as_bytes()))
}

/// Render the vhost definitions the image's Caddyfile imports.
///
/// Each service is pinned to the exact `aud` its tokens must carry, rather than
/// the gateway's trick of comparing a host label against the token's claims: the
/// job knows its own id and service names when it writes this, so it has no
/// reason to derive a label offset from a domain it would have to count.
///
/// This repeats the check the gateway already made, and deliberately so. The
/// gateway is what stops a job publishing unauthenticated content to the world;
/// this is what stops a sibling job on the same trusted network reaching a
/// service directly, without a token, having skipped the gateway entirely.
///
/// `stream_close_delay` is load-bearing: Caddy severs proxied WebSockets when its
/// config is reloaded, and this file is rewritten whenever the job changes its
/// service set. Without it, declaring a new service drops every open terminal and
/// SSH-over-WebSocket session in the job.
pub fn render(
    job_id: Uuid,
    gateway: &GatewayMaterial,
    declarations: &[ServiceDeclaration],
) -> String {
    let mut out = String::new();

    for declaration in declarations {
        let name = &declaration.service.name;

        let Some(upstream) = declaration.upstream.as_deref() else {
            continue;
        };

        if !service_name_valid(name) {
            warn!("Not proxying service {name:?}: not a usable service name.");
            continue;
        }

        if !upstream_valid(upstream) {
            warn!("Not proxying service {name:?}: {upstream:?} is not a usable upstream.");
            continue;
        }

        let label = format!("{name}-{job_id}");
        let hosts: Vec<String> = gateway
            .base_domains
            .iter()
            .map(|domain| format!("{label}.{domain}"))
            .collect();

        if hosts.is_empty() {
            warn!("Not proxying service {name:?}: no usable gateway base domain.");
            continue;
        }

        out.push_str(&format!("@{name} host {}\n", hosts.join(" ")));
        out.push_str(&format!("handle @{name} {{\n"));
        out.push_str("\troute {\n");
        out.push_str("\t\tjwtauth {\n");
        out.push_str(&format!("\t\t\tsign_key {}\n", gateway.sign_key));
        out.push_str("\t\t\tsign_alg EdDSA\n");
        out.push_str("\t\t\tfrom_query tml_token\n");
        out.push_str("\t\t\tfrom_header X-Tml-Token\n");
        out.push_str("\t\t\tfrom_cookies __Host-tml_token\n");
        out.push_str(&format!("\t\t\tissuer_whitelist \"{}\"\n", gateway.issuer));
        out.push_str(&format!("\t\t\taudience_whitelist \"{label}\"\n"));
        out.push_str("\t\t\tuser_claims sub\n");
        out.push_str("\t\t}\n");
        out.push_str(&format!("\t\treverse_proxy \"{upstream}\" {{\n"));
        out.push_str("\t\t\tstream_close_delay 24h\n");
        out.push_str("\t\t}\n");
        out.push_str("\t}\n");
        out.push_str("}\n\n");
    }

    out.push_str("handle {\n\trespond \"no such service\" 404\n}\n");
    out
}

/// Writes the generated vhost definitions and reloads the server that serves
/// them. Built only when the puppet is asked for one and the job actually has a
/// gateway.
pub struct ServiceProxy {
    config_path: PathBuf,
    reload_command: Option<String>,
    job_id: Uuid,
    gateway: GatewayMaterial,
}

impl ServiceProxy {
    pub fn new(
        config_path: PathBuf,
        reload_command: Option<String>,
        job_id: Uuid,
        gateway: &JobGatewayInfo,
    ) -> Result<Self> {
        Ok(ServiceProxy {
            config_path,
            reload_command,
            job_id,
            gateway: GatewayMaterial::from_info(gateway)?,
        })
    }

    pub async fn apply(&self, declarations: &[ServiceDeclaration]) -> Result<()> {
        let rendered = render(self.job_id, &self.gateway, declarations);

        if tokio::fs::read_to_string(&self.config_path).await.ok() == Some(rendered.clone()) {
            info!(
                "Service proxy config {:?} is already current, not reloading.",
                self.config_path
            );
            return Ok(());
        }

        write_atomically(&self.config_path, &rendered)
            .await
            .with_context(|| format!("Writing the service proxy config {:?}", self.config_path))?;

        let Some(command) = self.reload_command.as_deref() else {
            return Ok(());
        };

        info!("Reloading the service proxy: {command}");
        let status = tokio::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::null())
            .status()
            .await
            .with_context(|| format!("Spawning the service proxy reload command {command:?}"))?;

        if !status.success() {
            bail!("Service proxy reload command {command:?} failed: {status}");
        }

        Ok(())
    }
}

async fn write_atomically(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("Creating the config directory")?;
    }

    let tmp_path = path.with_extension("tmp");
    tokio::fs::write(&tmp_path, contents.as_bytes())
        .await
        .context("Writing the temporary config file")?;
    tokio::fs::rename(&tmp_path, path)
        .await
        .context("Renaming the temporary config file into place")
}

#[cfg(test)]
mod tests {
    use treadmill_rs::api::supervisor_puppet::JobGatewayEndpoint;

    use super::*;

    const KEY_PEM: &str = "-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAxdOGigVkbd8LI8KNP6rTQUnhfITL8FuukzoisxX9jCY=\n-----END PUBLIC KEY-----\n";

    fn material() -> GatewayMaterial {
        GatewayMaterial::from_info(&JobGatewayInfo {
            issuer: "https://switchboard.example".to_string(),
            signing_public_key: KEY_PEM.to_string(),
            key_id: "wI9c-yvsF8".to_string(),
            endpoints: vec![
                JobGatewayEndpoint {
                    base_domain: "gw-us-east-1.treadmillusercontent.com".to_string(),
                    port: 443,
                },
                JobGatewayEndpoint {
                    base_domain: "gw-eu-central-1.treadmillusercontent.com".to_string(),
                    port: 4433,
                },
            ],
        })
        .unwrap()
    }

    fn declaration(name: &str, upstream: Option<&str>) -> ServiceDeclaration {
        ServiceDeclaration {
            service: JobService {
                name: name.to_string(),
                label: None,
                protocol: "webapp".to_string(),
            },
            upstream: upstream.map(str::to_string),
        }
    }

    /// caddy-jwt wants the raw key bytes for EdDSA, not the PEM the switchboard
    /// publishes.
    #[test]
    fn the_signing_key_is_converted_to_raw_base64() {
        assert_eq!(
            material().sign_key,
            "xdOGigVkbd8LI8KNP6rTQUnhfITL8FuukzoisxX9jCY="
        );
    }

    #[test]
    fn an_unparseable_signing_key_is_an_error() {
        assert!(
            sign_key_base64("-----BEGIN PUBLIC KEY-----\nstub\n-----END PUBLIC KEY-----").is_err()
        );
    }

    #[test]
    fn a_service_is_pinned_to_its_own_audience_at_every_gateway() {
        let job_id = Uuid::new_v4();
        let rendered = render(
            job_id,
            &material(),
            &[declaration("webterm", Some("unix//run/tml-ttyd/ttyd.sock"))],
        );

        assert!(rendered.contains(&format!(
            "@webterm host webterm-{job_id}.gw-us-east-1.treadmillusercontent.com \
             webterm-{job_id}.gw-eu-central-1.treadmillusercontent.com\n"
        )));
        assert!(rendered.contains(&format!("audience_whitelist \"webterm-{job_id}\"")));
        assert!(rendered.contains("issuer_whitelist \"https://switchboard.example\""));
        assert!(rendered.contains("reverse_proxy \"unix//run/tml-ttyd/ttyd.sock\""));
    }

    /// A reload severs live WebSockets without it, and the config is rewritten
    /// every time the job's service set changes.
    #[test]
    fn every_upstream_delays_closing_streams() {
        let rendered = render(
            Uuid::new_v4(),
            &material(),
            &[
                declaration("webterm", Some("unix//run/tml-ttyd/ttyd.sock")),
                declaration("webide", Some("127.0.0.1:8080")),
            ],
        );

        assert_eq!(rendered.matches("stream_close_delay 24h").count(), 2);
    }

    /// Announcing a service and proxying it are separate: a job may offer
    /// something that is not reached over the gateway at all.
    #[test]
    fn a_declaration_without_an_upstream_gets_no_vhost() {
        let rendered = render(Uuid::new_v4(), &material(), &[declaration("shell", None)]);

        assert!(!rendered.contains("@shell"));
        assert!(!rendered.contains("jwtauth"));
    }

    /// The job is root-owned, so a declaration is only as trustworthy as the
    /// job; a bad upstream must not be able to rewrite the surrounding config.
    #[test]
    fn an_upstream_that_would_escape_the_config_is_refused() {
        for bad in [
            "127.0.0.1:80 }\nhandle { reverse_proxy 127.0.0.1:80",
            "127.0.0.1:80\"",
            "127.0.0.1:80 # comment",
            "",
        ] {
            let rendered = render(
                Uuid::new_v4(),
                &material(),
                &[declaration("evil", Some(bad))],
            );
            assert!(!rendered.contains("@evil"), "{bad:?} was rendered");
        }
    }

    /// Anything not matching a declared service is refused outright, rather than
    /// falling through to whatever handler happens to be next.
    #[test]
    fn an_unknown_host_is_refused() {
        let rendered = render(Uuid::new_v4(), &material(), &[]);
        assert_eq!(rendered, "handle {\n\trespond \"no such service\" 404\n}\n");
    }

    #[test]
    fn a_service_without_a_reachable_gateway_gets_no_vhost() {
        let gateway = GatewayMaterial {
            base_domains: Vec::new(),
            ..material()
        };
        let rendered = render(
            Uuid::new_v4(),
            &gateway,
            &[declaration("webterm", Some("127.0.0.1:7681"))],
        );

        assert!(!rendered.contains("@webterm"));
    }

    #[test]
    fn an_unusable_base_domain_is_dropped() {
        let gateway = GatewayMaterial::from_info(&JobGatewayInfo {
            issuer: "https://switchboard.example".to_string(),
            signing_public_key: KEY_PEM.to_string(),
            key_id: "wI9c-yvsF8".to_string(),
            endpoints: vec![
                JobGatewayEndpoint {
                    base_domain: "gw one.example".to_string(),
                    port: 443,
                },
                JobGatewayEndpoint {
                    base_domain: "gw-two.example".to_string(),
                    port: 443,
                },
            ],
        })
        .unwrap();

        assert_eq!(gateway.base_domains, ["gw-two.example"]);
    }

    #[test]
    fn an_upstream_is_read_from_the_declaration_but_never_announced() {
        let declaration: ServiceDeclaration = serde_json::from_str(
            r#"{"name": "webterm", "label": "Terminal", "protocol": "webapp",
                "upstream": "unix//run/tml-ttyd/ttyd.sock"}"#,
        )
        .unwrap();

        assert_eq!(
            declaration.upstream.as_deref(),
            Some("unix//run/tml-ttyd/ttyd.sock")
        );
        assert_eq!(
            serde_json::to_value(&declaration.service).unwrap(),
            serde_json::json!({"name": "webterm", "label": "Terminal", "protocol": "webapp"})
        );
    }
}

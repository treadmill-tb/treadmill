//! Credentials for one service a job announces.
//!
//! Reaching a job's service goes through a gateway, which admits the
//! connection against a short-lived JWT the switchboard mints. Minting one
//! costs a round trip, so they are cached in the session state until they
//! expire — or until a handshake proves one is no longer good, at which point
//! [`crate::state::State::invalidate_job_service_token`] drops it.

use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::ClientError;
use treadmill_rs::api::switchboard::jobs::{JobServiceCredentials, JobServiceEndpoint};
use uuid::Uuid;

use crate::error::hinted;
use crate::job::short;
use crate::session::Session;
use crate::state::CachedJobServiceToken;

/// A valid credential for `service` on `job_id`: the cached one if there is
/// one, otherwise freshly minted and cached.
pub async fn credentials(
    session: &mut Session,
    job_id: Uuid,
    service: &str,
) -> Result<CachedJobServiceToken> {
    if let Some(cached) = session
        .state
        .valid_job_service_token(job_id, service)
        .cloned()
    {
        return Ok(cached);
    }

    let credentials = session
        .authenticated_client()?
        .create_job_service_token(job_id, service)
        .await
        .map_err(|error| token_error(error, job_id, service))?;
    if credentials.endpoints.is_empty() {
        bail!("the switchboard returned no gateway endpoint for this service");
    }
    let cached = CachedJobServiceToken {
        endpoints: credentials.endpoints.clone(),
        token: expose(&credentials),
        expires_at: credentials.expires_at,
    };
    session.update_state(|state| {
        state
            .job_service_tokens
            .entry(job_id)
            .or_default()
            .insert(service.to_string(), cached.clone());
    })?;
    Ok(cached)
}

fn expose(credentials: &JobServiceCredentials) -> String {
    credentials.token.expose().clone()
}

/// The statuses the token endpoint answers with that say something specific
/// about the job, rather than about the request.
fn token_error(error: ClientError, job_id: Uuid, service: &str) -> anyhow::Error {
    match &error {
        ClientError::Status { status: 404, .. } => hinted(
            anyhow::anyhow!("job {job_id} has no service named {service:?}"),
            format!("The job's status lists its services ({}).", short(job_id)),
        ),
        ClientError::Status { status: 409, .. } => hinted(
            anyhow::anyhow!("job {job_id} has no network address yet"),
            format!("Wait until the job is ready ({}).", short(job_id)),
        ),
        ClientError::Status { status: 503, .. } => anyhow::anyhow!(
            "this switchboard has no job service gateway, so jobs cannot be reached over SSH"
        ),
        _ => anyhow::Error::new(error).context("minting a job service token"),
    }
}

/// Pick the endpoint to connect to.
///
/// With no `domain` asked for, the first endpoint is the switchboard's own
/// preference. With one, an endpoint published under it wins; a domain the
/// profile configures but no endpoint names falls back to the primary, since
/// the gateway there may serve several domains it does not announce.
pub fn select_endpoint<'a>(
    cached: &'a CachedJobServiceToken,
    domain: Option<&str>,
    ssh_domains: &[String],
) -> Result<&'a JobServiceEndpoint> {
    let primary = || {
        cached
            .endpoints
            .first()
            .context("the switchboard returned no gateway endpoint for this service")
    };

    let Some(domain) = domain else {
        return primary();
    };

    if let Some(endpoint) = cached
        .endpoints
        .iter()
        .find(|endpoint| base_domain(&endpoint.hostname) == Some(domain))
    {
        return Ok(endpoint);
    }

    if ssh_domains.iter().any(|configured| configured == domain) {
        return primary();
    }

    let published: Vec<&str> = cached
        .endpoints
        .iter()
        .filter_map(|endpoint| base_domain(&endpoint.hostname))
        .collect();
    bail!(
        "no gateway at {domain:?} publishes this service; it is published at: {}",
        published.join(", ")
    );
}

/// The part of a gateway hostname below its per-job label.
pub fn base_domain(hostname: &str) -> Option<&str> {
    hostname.split_once('.').map(|(_, domain)| domain)
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb";

    fn cached(hostnames: &[&str]) -> CachedJobServiceToken {
        CachedJobServiceToken {
            endpoints: hostnames
                .iter()
                .map(|hostname| JobServiceEndpoint {
                    hostname: hostname.to_string(),
                    port: 443,
                })
                .collect(),
            token: "token".to_string(),
            expires_at: chrono::Utc::now(),
        }
    }

    fn domains(domains: &[&str]) -> Vec<String> {
        domains.iter().map(|domain| domain.to_string()).collect()
    }

    #[test]
    fn a_configured_domain_takes_the_primary_endpoint() {
        let cached = cached(&[
            &format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev"),
            &format!("sshws-{JOB}.eu-central-1.user-gw.treadmill.dev"),
        ]);
        let domains = domains(&["job.treadmill.dev", "user-gw.treadmill.dev"]);
        let endpoint = select_endpoint(&cached, Some("job.treadmill.dev"), &domains).unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[0].hostname);
    }

    #[test]
    fn a_published_domain_takes_its_own_endpoint() {
        let cached = cached(&[
            &format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev"),
            &format!("sshws-{JOB}.eu-central-1.user-gw.treadmill.dev"),
        ]);
        let endpoint =
            select_endpoint(&cached, Some("eu-central-1.user-gw.treadmill.dev"), &[]).unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[1].hostname);
    }

    #[test]
    fn no_requested_domain_takes_the_primary_endpoint() {
        let cached = cached(&[
            &format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev"),
            &format!("sshws-{JOB}.eu-central-1.user-gw.treadmill.dev"),
        ]);
        let endpoint = select_endpoint(&cached, None, &[]).unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[0].hostname);
    }

    #[test]
    fn an_unpublished_domain_is_refused() {
        let cached = cached(&[&format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev")]);
        assert!(select_endpoint(&cached, Some("elsewhere.example"), &[]).is_err());
    }
}

//! Reaching a job over SSH, through the job service gateway.
//!
//! SSH itself is not spoken here: a client is launched against a destination
//! and a `ProxyCommand` that bridges the connection (see [`crate::wsproxy`]).
//! What this module supplies is everything the client needs to be told —
//! which host, as whom, and under which options.

use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::job::short;
#[cfg(feature = "profile")]
use crate::{service, session::Session};

/// The protocol a job announces for a service that speaks SSH over a
/// WebSocket.
pub const SSHWS_PROTOCOL: &str = "sshws";

/// Everything needed to launch an SSH client against one job service.
pub struct Connection {
    /// `user@host`, as ssh takes it.
    pub destination: String,
    /// The `ProxyCommand` that bridges the connection.
    pub proxy_command: String,
    /// Options to append, which the user asked for.
    pub ssh_opts: Vec<String>,
}

impl Connection {
    /// The arguments to pass before the destination.
    ///
    /// The connection is to a single-use host reached through a bridge, so
    /// none of ssh's usual per-host state applies: the user's config, known
    /// hosts and control sockets are all turned off rather than consulted,
    /// since any of them could redirect or multiplex the connection somewhere
    /// it should not go. The user's own options come last, and win.
    pub fn options(&self) -> Vec<String> {
        vec![
            "-F".into(),
            "/dev/null".into(),
            "-o".into(),
            format!("ProxyCommand={}", self.proxy_command),
            "-o".into(),
            "BatchMode=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=no".into(),
            "-o".into(),
            "UserKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "GlobalKnownHostsFile=/dev/null".into(),
            "-o".into(),
            "ControlMaster=no".into(),
            "-o".into(),
            "ControlPath=none".into(),
            "-o".into(),
            "LogLevel=ERROR".into(),
        ]
        .into_iter()
        .chain(self.ssh_opts.iter().cloned())
        .collect()
    }
}

/// `user@host` for `service` on `job_id`, at the gateway the switchboard
/// prefers. Mints or reuses the service credential, since the endpoint comes
/// with it.
#[cfg(feature = "profile")]
pub async fn destination(
    session: &mut Session,
    job_id: Uuid,
    service: &str,
    user: &str,
) -> Result<String> {
    let cached = service::credentials(session, job_id, service).await?;
    let endpoint = service::select_endpoint(&cached, None, &[])?;
    Ok(format!("{user}@{}", endpoint.hostname))
}

/// The name of the job's SSH service, when the caller did not name one. Only
/// unambiguous if the job announces exactly one.
pub async fn discover_service(client: &SwitchboardClient, job_id: Uuid) -> Result<String> {
    let job = client
        .get_job(job_id)
        .await
        .with_context(|| format!("reading job {} to find its SSH service", short(job_id)))?;

    let names: Vec<&str> = job
        .services
        .iter()
        .filter(|service| service.protocol == SSHWS_PROTOCOL)
        .map(|service| service.name.as_str())
        .collect();

    match names.len() {
        0 => bail!(
            "job {} announces no {SSHWS_PROTOCOL} service (state: {:?})",
            short(job_id),
            job.state
        ),
        1 => Ok(names[0].to_string()),
        _ => bail!(
            "job {} announces several {SSHWS_PROTOCOL} services ({})",
            short(job_id),
            names.join(", ")
        ),
    }
}

/// Read a gateway hostname back: `<job-uuid>.<domain>`, or
/// `<service>-<job-uuid>.<domain>` when the service is named in it.
///
/// This is the inverse of how the gateway publishes a job's services, and is
/// what a `ProxyCommand` invoked with `%h` has to work from.
pub fn parse_destination(host: &str) -> Result<(Option<&str>, Uuid, &str)> {
    let (label, domain) = host
        .split_once('.')
        .with_context(|| format!("{host:?} carries no domain to resolve a job under"))?;

    if let Ok(job_id) = Uuid::parse_str(label) {
        return Ok((None, job_id, domain));
    }

    let (service, job_id) = label
        .split_once('-')
        .with_context(|| format!("{label:?} is neither a job UUID nor <service>-<job-uuid>"))?;
    let job_id = Uuid::parse_str(job_id)
        .with_context(|| format!("{job_id:?} is not the job UUID of {label:?}"))?;
    Ok((Some(service), job_id, domain))
}

/// POSIX single-quoting: ssh hands a `ProxyCommand` to `/bin/sh -c`, and sftp
/// splits a batch line on whitespace unless a path is quoted.
pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const JOB: &str = "1b4e28ba-2fa1-11d2-883f-b9a761bde3fb";

    #[test]
    fn a_bare_job_id_leaves_the_service_to_be_discovered() {
        let host = format!("{JOB}.job.treadmill.dev");
        let (service, job_id, domain) = parse_destination(&host).unwrap();
        assert_eq!(service, None);
        assert_eq!(job_id, Uuid::parse_str(JOB).unwrap());
        assert_eq!(domain, "job.treadmill.dev");
    }

    #[test]
    fn a_published_hostname_names_its_service() {
        let host = format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev");
        let (service, job_id, domain) = parse_destination(&host).unwrap();
        assert_eq!(service, Some("sshws"));
        assert_eq!(job_id, Uuid::parse_str(JOB).unwrap());
        assert_eq!(domain, "us-east-1.user-gw.treadmill.dev");
    }

    #[test]
    fn a_label_that_is_neither_is_refused() {
        assert!(parse_destination("nonsense.job.treadmill.dev").is_err());
        assert!(parse_destination(&format!("sshws-not-a-uuid.{JOB}")).is_err());
        assert!(parse_destination(JOB).is_err());
    }

    /// A ProxyCommand is handed to `/bin/sh -c`, so a quote in a path must not
    /// end the quoting.
    #[test]
    fn quoting_survives_a_quote() {
        assert_eq!(quote("/tmp/a b"), "'/tmp/a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    /// Whatever else is passed, the options that keep ssh from consulting the
    /// user's own state have to be there.
    #[test]
    fn the_hardened_options_precede_the_users() {
        let connection = Connection {
            destination: "tml@host".into(),
            proxy_command: "bridge".into(),
            ssh_opts: vec!["-o".into(), "LogLevel=DEBUG".into()],
        };
        let options = connection.options();
        assert_eq!(options[..2], ["-F", "/dev/null"]);
        assert!(options.contains(&"ProxyCommand=bridge".to_string()));
        assert!(options.contains(&"UserKnownHostsFile=/dev/null".to_string()));
        assert_eq!(options[options.len() - 2..], ["-o", "LogLevel=DEBUG"]);
    }
}

use anyhow::{Context as _, Result, bail};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use treadmill_rs::api::switchboard::jobs::{JobServiceCredentials, JobServiceEndpoint};
use uuid::Uuid;

use crate::cli::JobTarget;
use crate::ctx::{Ctx, short};
use crate::state::CachedJobServiceToken;

pub const SSHWS_PROTOCOL: &str = "sshws";
pub const PROGRAM: &str = "tml";
/// Everything needed to launch a client against one job service.
struct Connection {
    destination: String,
    proxy_command: String,
    ssh_opts: Vec<String>,
}

pub async fn ssh(ctx: &mut Ctx, target: &JobTarget, args: &[String]) -> Result<()> {
    let connection = connect(ctx, target).await?;
    let mut argv = connection.ssh_options();
    argv.push(connection.destination.clone());
    argv.extend(args.iter().cloned());
    exec(ctx, "ssh", &argv)
}

pub async fn proxy(ctx: &mut Ctx, host: &str) -> Result<()> {
    let (service, job_id, domain) = parse_destination(host)?;
    let service = match service {
        Some(service) => service.to_string(),
        None => discover_service(&ctx.authenticated_client()?, job_id).await?,
    };
    ctx.detail(&format!(
        "{host} is job {} service {service} at {domain}",
        short(job_id)
    ));
    crate::wsproxy::run(ctx, job_id, &service, Some(domain)).await
}

fn parse_destination(host: &str) -> Result<(Option<&str>, Uuid, &str)> {
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

pub(crate) fn select_endpoint<'a>(
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
    )
}

fn base_domain(hostname: &str) -> Option<&str> {
    hostname.split_once('.').map(|(_, domain)| domain)
}

pub async fn exec_command(ctx: &mut Ctx, target: &JobTarget, command: &[String]) -> Result<()> {
    let connection = connect(ctx, target).await?;
    let mut argv = connection.ssh_options();
    argv.push(connection.destination.clone());
    argv.extend(command.iter().cloned());
    exec(ctx, "ssh", &argv)
}

pub async fn sftp(ctx: &mut Ctx, target: &JobTarget, args: &[String]) -> Result<()> {
    let connection = connect(ctx, target).await?;
    let mut argv = connection.sftp_options();
    argv.push(connection.destination.clone());
    argv.extend(args.iter().cloned());
    exec(ctx, "sftp", &argv)
}

pub async fn upload(
    ctx: &mut Ctx,
    target: &JobTarget,
    local: &Path,
    remote: Option<&str>,
) -> Result<()> {
    let remote = match remote {
        Some(remote) => remote.to_string(),
        None => basename(local)?,
    };
    let local = local.to_str().context("local path is not valid UTF-8")?;
    batch(
        ctx,
        target,
        &format!("put {} {}", quote(local), quote(&remote)),
    )
    .await
}

pub async fn download(
    ctx: &mut Ctx,
    target: &JobTarget,
    remote: &str,
    local: Option<&Path>,
) -> Result<()> {
    let local = match local {
        Some(local) => local
            .to_str()
            .context("local path is not valid UTF-8")?
            .to_string(),
        None => basename(Path::new(remote))?,
    };
    batch(
        ctx,
        target,
        &format!("get {} {}", quote(remote), quote(&local)),
    )
    .await
}

/// Drive sftp through a one-line batch script on its stdin, leaving its own
/// stdout and stderr attached.
async fn batch(ctx: &mut Ctx, target: &JobTarget, line: &str) -> Result<()> {
    let connection = connect(ctx, target).await?;
    let mut argv = connection.sftp_options();
    argv.push("-b".into());
    argv.push("-".into());
    argv.push(connection.destination.clone());

    ctx.detail(&command_line("sftp", &argv));
    ctx.detail(&format!("batch: {line}"));
    let mut child = Command::new("sftp")
        .args(&argv)
        .stdin(Stdio::piped())
        .spawn()
        .context("launching sftp; is OpenSSH installed and on PATH?")?;

    child
        .stdin
        .take()
        .context("sftp stdin was not piped")?
        .write_all(format!("{line}\n").as_bytes())
        .context("writing the sftp batch script")?;

    let status = child.wait().context("waiting for sftp")?;
    std::process::exit(status.code().unwrap_or(1));
}

impl Connection {
    /// SSH arguments.
    ///
    /// These take precedence over any user-supplied options.
    fn ssh_options(&self) -> Vec<String> {
        vec![
            // Ignore the user's SSH config, it's defaults like control socket
            // or known hosts file could mess with this subcommand.
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

    fn sftp_options(&self) -> Vec<String> {
        self.ssh_options()
    }
}

async fn connect(ctx: &mut Ctx, target: &JobTarget) -> Result<Connection> {
    let client = ctx.authenticated_client()?;
    let job_id = ctx.resolve_job(target.job.as_deref())?;

    let service = match &target.service {
        Some(service) => service.clone(),
        None => discover_service(&client, job_id).await?,
    };

    ctx.note(&format!("Connecting to service {service}…"));
    let cached = service_credentials(ctx, job_id, &service).await?;

    connection(ctx, target, job_id, service, cached)
}

fn connection(
    ctx: &Ctx,
    target: &JobTarget,
    job_id: Uuid,
    service: String,
    cached: CachedJobServiceToken,
) -> Result<Connection> {
    let user = target
        .user
        .clone()
        .unwrap_or_else(|| ctx.config.user.clone());

    let endpoint = select_endpoint(&cached, None, &[])?;

    Ok(Connection {
        destination: format!("{user}@{}", endpoint.hostname),
        proxy_command: proxy_command(ctx, job_id, &service)?,
        ssh_opts: target.ssh_opt.clone(),
    })
}

async fn discover_service(
    client: &treadmill_rs::api::switchboard::client::SwitchboardClient,
    job_id: Uuid,
) -> Result<String> {
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
            "job {} announces several {SSHWS_PROTOCOL} services ({}); pass --service",
            short(job_id),
            names.join(", ")
        ),
    }
}

fn tml_command(program: &str, ctx: &Ctx) -> Result<String> {
    Ok(format!(
        "{program} --profile {} --config {}",
        quote(&ctx.profile),
        quote_path(&ctx.config_path)?,
    ))
}

/// The bridge is this same binary, addressed by its absolute path so that the
/// connection works whether or not `tml` is on the user's `PATH`.
fn proxy_command(ctx: &Ctx, job_id: Uuid, service: &str) -> Result<String> {
    let exe = std::env::current_exe().context("locating the running tml binary")?;
    let exe = exe
        .to_str()
        .context("the tml binary path is not valid UTF-8")?;

    let mut command = format!(
        "{} --switchboard {} job ws-proxy {job_id} {}",
        tml_command(&quote(exe), ctx)?,
        quote(&ctx.config.switchboard),
        quote(service),
    );
    if ctx.insecure_tls {
        command.push_str(" --insecure-tls");
    }
    for _ in 0..ctx.verbose {
        command.push_str(" -v");
    }
    Ok(command)
}

pub(crate) fn ssh_proxy_command(ctx: &Ctx) -> Result<String> {
    Ok(format!("{} ssh proxy '%h'", tml_command(PROGRAM, ctx)?))
}

fn exec(ctx: &Ctx, program: &str, argv: &[String]) -> Result<()> {
    ctx.detail(&command_line(program, argv));
    let error = Command::new(program)
        .args(argv.iter().map(OsString::from))
        .exec();
    Err(error).with_context(|| format!("launching {program}; is OpenSSH installed and on PATH?"))
}

fn service_token_error(
    error: treadmill_rs::api::switchboard::client::ClientError,
    service: &str,
) -> anyhow::Error {
    use treadmill_rs::api::switchboard::client::ClientError;

    match &error {
        ClientError::Status { status: 404, .. } => anyhow::anyhow!(
            "the job announces no service named {service:?} any more; re-check it and retry"
        ),
        ClientError::Status { status: 409, .. } => {
            anyhow::anyhow!("the job has no network address yet; wait for it to become ready")
        }
        ClientError::Status { status: 503, .. } => {
            anyhow::anyhow!("this deployment runs no job service gateway, so SSH is unavailable")
        }
        _ => anyhow::Error::new(error).context("minting a job service token"),
    }
}

fn expose(credentials: &JobServiceCredentials) -> String {
    credentials.token.expose().clone()
}

pub(crate) async fn service_credentials(
    ctx: &mut Ctx,
    job_id: Uuid,
    service: &str,
) -> Result<CachedJobServiceToken> {
    if let Some(cached) = ctx.state.valid_job_service_token(job_id, service).cloned() {
        return Ok(cached);
    }

    let credentials = ctx
        .authenticated_client()?
        .create_job_service_token(job_id, service)
        .await
        .map_err(|error| service_token_error(error, service))?;
    if credentials.endpoints.is_empty() {
        bail!("the switchboard returned no gateway endpoint for this service");
    }
    let cached = CachedJobServiceToken {
        endpoints: credentials.endpoints.clone(),
        token: expose(&credentials),
        expires_at: credentials.expires_at,
    };
    ctx.state
        .job_service_tokens
        .entry(job_id)
        .or_default()
        .insert(service.to_string(), cached.clone());
    ctx.state.store(&ctx.state_path)?;
    Ok(cached)
}

fn quote_path(path: &Path) -> Result<String> {
    path.to_str()
        .map(quote)
        .context("the configuration path is not valid UTF-8")
}

fn basename(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .with_context(|| format!("{} has no file name to preserve", path.display()))
}

fn command_line(program: &str, argv: &[String]) -> String {
    let mut line = format!("running: {program}");
    for arg in argv {
        line.push(' ');
        line.push_str(&display_arg(arg));
    }
    line
}

fn display_arg(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%_+=:,./-".contains(c));
    if plain { arg.to_string() } else { quote(arg) }
}

/// POSIX single-quoting: ssh hands a ProxyCommand to `/bin/sh -c`, and sftp
/// splits a batch line on whitespace unless a path is quoted.
fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
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
    fn a_gateway_under_a_configured_domain_is_pinned() {
        let cached = cached(&[
            &format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev"),
            &format!("sshws-{JOB}.eu-central-1.user-gw.treadmill.dev"),
        ]);
        let domains = domains(&["job.treadmill.dev", "user-gw.treadmill.dev"]);
        let endpoint = select_endpoint(
            &cached,
            Some("eu-central-1.user-gw.treadmill.dev"),
            &domains,
        )
        .unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[1].hostname);
    }

    #[test]
    fn a_gateway_that_is_itself_a_configured_domain_is_pinned() {
        let cached = cached(&[
            &format!("sshws-{JOB}.gw-us-east-1.example.com"),
            &format!("sshws-{JOB}.gw-eu-central-1.example.com"),
        ]);
        let domains = domains(&["gw-us-east-1.example.com", "gw-eu-central-1.example.com"]);
        let endpoint =
            select_endpoint(&cached, Some("gw-eu-central-1.example.com"), &domains).unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[1].hostname);
    }

    #[test]
    fn an_unpublished_domain_is_refused() {
        let cached = cached(&[&format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev")]);
        let domains = domains(&["job.treadmill.dev", "user-gw.treadmill.dev"]);
        let error = select_endpoint(&cached, Some("nosuch.user-gw.treadmill.dev"), &domains)
            .unwrap_err()
            .to_string();
        assert!(error.contains("us-east-1.user-gw.treadmill.dev"), "{error}");
    }

    #[test]
    fn no_requested_domain_takes_the_primary_endpoint() {
        let cached = cached(&[&format!("sshws-{JOB}.us-east-1.user-gw.treadmill.dev")]);
        let endpoint = select_endpoint(&cached, None, &[]).unwrap();
        assert_eq!(endpoint.hostname, cached.endpoints[0].hostname);
    }
}

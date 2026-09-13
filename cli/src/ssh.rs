use anyhow::{Context as _, Result, bail};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use treadmill_rs::api::switchboard::jobs::JobServiceCredentials;
use uuid::Uuid;

use crate::cli::JobTarget;
use crate::ctx::{Ctx, short};
use crate::state::CachedJobServiceToken;

pub const SSHWS_PROTOCOL: &str = "sshws";
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

    Ok(Connection {
        destination: format!("{user}@{}", cached.hostname),
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

/// The bridge is this same binary, addressed by its absolute path so that the
/// connection works whether or not `tml` is on the user's `PATH`.
fn proxy_command(ctx: &Ctx, job_id: Uuid, service: &str) -> Result<String> {
    let exe = std::env::current_exe().context("locating the running tml binary")?;
    let exe = exe
        .to_str()
        .context("the tml binary path is not valid UTF-8")?;

    let mut command = format!(
        "{} --profile {} --config {} --switchboard {} job ws-proxy {job_id} {}",
        quote(exe),
        quote(&ctx.profile),
        quote_path(&ctx.config_path)?,
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
    let endpoint = credentials
        .endpoints
        .first()
        .context("the switchboard returned no gateway endpoint for this service")?;
    let cached = CachedJobServiceToken {
        hostname: endpoint.hostname.clone(),
        port: endpoint.port,
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

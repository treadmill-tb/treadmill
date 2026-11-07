use anyhow::{Context as _, Result, bail};
use std::ffi::OsString;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use uuid::Uuid;

use crate::cli::SshJob;
use crate::ctx::Ctx;
use crate::display::{SSHWS_PROTOCOL, short};
use treadmill_client::state::CachedJobServiceToken;
use treadmill_client::{service, wsproxy};

pub const PROGRAM: &str = "tml";
/// Everything needed to launch a client against one job service.
struct Connection {
    destination: String,
    proxy_command: String,
    ssh_opts: Vec<String>,
}

pub async fn ssh(ctx: &mut Ctx, job_id: Uuid, target: &SshJob, args: &[String]) -> Result<()> {
    let connection = connect(ctx, job_id, target).await?;
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
    wsproxy::run(
        &mut ctx.session,
        job_id,
        tokio::io::stdin(),
        tokio::io::stdout(),
        &service,
        Some(domain),
    )
    .await
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

pub async fn exec_command(
    ctx: &mut Ctx,
    job_id: Uuid,
    target: &SshJob,
    command: &[String],
) -> Result<()> {
    let connection = connect(ctx, job_id, target).await?;
    let mut argv = connection.ssh_options();
    argv.push(connection.destination.clone());
    argv.extend(command.iter().cloned());
    exec(ctx, "ssh", &argv)
}

pub async fn sftp(ctx: &mut Ctx, job_id: Uuid, target: &SshJob, args: &[String]) -> Result<()> {
    let connection = connect(ctx, job_id, target).await?;
    let mut argv = connection.sftp_options();
    argv.push(connection.destination.clone());
    argv.extend(args.iter().cloned());
    exec(ctx, "sftp", &argv)
}

pub async fn upload(
    ctx: &mut Ctx,
    job_id: Uuid,
    target: &SshJob,
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
        job_id,
        target,
        &format!("put {} {}", quote(local), quote(&remote)),
    )
    .await
}

pub async fn download(
    ctx: &mut Ctx,
    job_id: Uuid,
    target: &SshJob,
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
        job_id,
        target,
        &format!("get {} {}", quote(remote), quote(&local)),
    )
    .await
}

/// Drive sftp through a one-line batch script on its stdin, leaving its own
/// stdout and stderr attached.
async fn batch(ctx: &mut Ctx, job_id: Uuid, target: &SshJob, line: &str) -> Result<()> {
    let connection = connect(ctx, job_id, target).await?;
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

async fn connect(ctx: &mut Ctx, job_id: Uuid, target: &SshJob) -> Result<Connection> {
    let client = ctx.authenticated_client()?;

    let service = match &target.service {
        Some(service) => service.clone(),
        None => discover_service(&client, job_id).await?,
    };

    ctx.detail(&format!(
        "Connecting to job {} · service {service}",
        short(job_id)
    ));
    let cached = service::credentials(&mut ctx.session, job_id, &service).await?;

    connection(ctx, target, job_id, service, cached)
}

fn connection(
    ctx: &Ctx,
    target: &SshJob,
    job_id: Uuid,
    service: String,
    cached: CachedJobServiceToken,
) -> Result<Connection> {
    let user = target
        .user
        .clone()
        .unwrap_or_else(|| ctx.session.config.user.clone());

    let endpoint = service::select_endpoint(&cached, None, &[])?;

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
        quote(&ctx.session.profile),
        quote_path(&ctx.session.config_path)?,
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
        "{} --switchboard {} job ws-proxy --job {job_id} {}",
        tml_command(&quote(exe), ctx)?,
        quote(&ctx.session.config.switchboard),
        quote(service),
    );
    if ctx.session.insecure_tls {
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
}

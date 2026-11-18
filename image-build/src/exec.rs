//! Running the external tools a build is made of.
//!
//! An image build is mostly orchestration: `qemu-img`, `qemu-nbd`, `mount`,
//! `systemd-nspawn` and friends do the work. None of them is replaceable by a
//! library call, and most of them need root.
//!
//! Privilege is per operation rather than per process. The build runs as the
//! user and escalates each privileged command through `sudo`, which is what
//! keeps a password prompt working and keeps everything that does *not* need
//! root from running as it. The consequence worth knowing: the process itself
//! is unprivileged, so nothing here can use `mount(2)` or its siblings
//! directly — every privileged step has to be a subprocess.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use anyhow::{Context as _, Result, bail};

/// How to reach root for the operations that need it.
#[derive(Debug, Clone, Copy)]
pub struct Privilege {
    escalate: bool,
}

impl Privilege {
    /// Escalate through `sudo`, unless this process is already root.
    pub fn detect() -> Self {
        Privilege {
            escalate: !nix::unistd::Uid::effective().is_root(),
        }
    }

    /// Never escalate. For tests, and for callers that know they are root.
    pub fn none() -> Self {
        Privilege { escalate: false }
    }

    /// Whether a command built here will be wrapped in `sudo`.
    pub fn escalates(&self) -> bool {
        self.escalate
    }

    /// A command that will run as root.
    ///
    /// Pass an absolute path for anything resolved with `command -v` before
    /// escalating: `sudo`'s `secure_path` is not the caller's `PATH`, and a
    /// tool found in a dev shell will not be found again as root.
    pub fn command(&self, program: impl AsRef<OsStr>) -> Command {
        if self.escalate {
            let mut command = Command::new("sudo");
            command.arg(program);
            command
        } else {
            Command::new(program)
        }
    }
}

/// How a command would be written on a command line, for error messages.
pub fn describe(command: &Command) -> String {
    let mut parts = vec![command.get_program().to_string_lossy().into_owned()];
    parts.extend(command.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

/// Run `command`, letting it inherit stdout and stderr, and fail if it does.
pub fn run(command: &mut Command) -> Result<()> {
    let described = describe(command);
    let status = command
        .status()
        .with_context(|| format!("run `{described}`"))?;
    if !status.success() {
        bail!("`{described}` failed: {status}");
    }
    Ok(())
}

/// Run `command` and return its standard output.
///
/// Standard error is captured and, on failure, becomes the error message —
/// which is the whole point: a bare exit status says nothing a user can act
/// on, and these tools are good at explaining themselves.
pub fn capture(command: &mut Command) -> Result<String> {
    let described = describe(command);
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run `{described}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("`{described}` failed: {}", output.status);
        }
        bail!("`{described}` failed: {stderr}");
    }
    String::from_utf8(output.stdout).with_context(|| format!("`{described}` printed non-UTF-8"))
}

/// Run `command` for its exit status, treating a non-zero one as information
/// rather than failure.
///
/// For the few tools that answer a question by exiting non-zero —
/// `qemu-img compare` says "these differ" with status 1 — where failing would
/// throw the answer away.
pub fn status_code(command: &mut Command) -> Result<i32> {
    let described = describe(command);
    let output = command
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run `{described}`"))?;
    output
        .status
        .code()
        .ok_or_else(|| anyhow::anyhow!("`{described}` was killed by a signal: {}", output.status))
}

/// Whether `tool` can be found on `PATH`.
pub fn have(tool: &str) -> bool {
    which(tool).is_some()
}

/// Where `tool` is, if it is anywhere on `PATH`.
///
/// Needed before escalating: see [`Privilege::command`].
pub fn which(tool: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(tool))
        .find(|candidate| candidate.is_file())
}

/// Fail unless every tool in `tools` is on `PATH`, naming all that are not.
///
/// Run before anything else, so that a missing tool is reported up front
/// rather than halfway through a build that has already attached a device.
pub fn require_tools(tools: &[&str]) -> Result<()> {
    let missing: Vec<&str> = tools.iter().copied().filter(|t| !have(t)).collect();
    if !missing.is_empty() {
        bail!("missing required tool(s): {}", missing.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unprivileged_command_is_not_wrapped() {
        let command = Privilege::none().command("qemu-img");
        assert_eq!(describe(&command), "qemu-img");
    }

    #[test]
    fn a_privileged_command_runs_under_sudo() {
        let privilege = Privilege { escalate: true };
        let mut command = privilege.command("qemu-nbd");
        command.arg("--disconnect");
        assert_eq!(describe(&command), "sudo qemu-nbd --disconnect");
    }

    #[test]
    fn a_failure_carries_what_the_tool_said() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo 'no such file' >&2; exit 1"]);
        let error = capture(&mut command).unwrap_err();
        assert!(
            format!("{error:#}").contains("no such file"),
            "the tool's own words should survive: {error:#}"
        );
    }

    #[test]
    fn a_question_answered_by_exit_status_is_not_a_failure() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 1"]);
        assert_eq!(status_code(&mut command).unwrap(), 1);
    }

    #[test]
    fn missing_tools_are_all_named_at_once() {
        let error = require_tools(&["sh", "tml-no-such-tool", "tml-nor-this"]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("tml-no-such-tool"), "{message}");
        assert!(message.contains("tml-nor-this"), "{message}");
        assert!(
            !message.contains(" sh"),
            "a tool that is present: {message}"
        );
    }
}

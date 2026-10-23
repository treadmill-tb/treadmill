use anyhow::{Context as _, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::SshSetupArgs;
use crate::config;
use crate::ctx::Ctx;
use crate::locked_file;
use crate::ssh::{PROGRAM, ssh_proxy_command};
use crate::tui::{self, ACCENT, BOLD, DIM, Line, OK, plain, span};
use crate::{cancelled, hinted};

const BEGIN: &str = "# BEGIN tml profile ";
const END: &str = "# END tml profile ";

const OPTIONS: [(&str, &str); 7] = [
    ("BatchMode", "yes"),
    ("StrictHostKeyChecking", "no"),
    ("UserKnownHostsFile", "/dev/null"),
    ("GlobalKnownHostsFile", "/dev/null"),
    ("ControlMaster", "no"),
    ("ControlPath", "none"),
    ("LogLevel", "ERROR"),
];

pub async fn setup(ctx: &Ctx, args: &SshSetupArgs) -> Result<()> {
    let domains = ctx.config.ssh_domains(&ctx.profile)?;
    let managed = config::ssh_config_path()?;

    check_path(ctx, &managed);

    let block = block(ctx, domains)?;
    private_dir(&managed)?;
    let contents = locked_file::update(&managed, 0o600, |current| {
        let contents = upsert(current.as_deref().unwrap_or(""), &ctx.profile, &block);
        *current = Some(contents.clone());
        Ok(contents)
    })
    .with_context(|| format!("writing {}", managed.display()))?;
    for (profile, pattern) in overlapping(&contents, &ctx.profile, domains) {
        ctx.warn(&format!(
            "profile {profile} also matches {pattern}; the first matching block in {} applies",
            display_path(&managed)
        ));
    }
    print(vec![
        span(OK, "✓ "),
        plain(format!("Wrote {}", display_path(&managed))),
        span(DIM, format!(" · profile {}", ctx.profile)),
    ]);

    let user_config = user_ssh_config()?;
    let include = format!("Include {}", display_path(&managed));
    let existing = match fs::read_to_string(&user_config) {
        Ok(contents) => Some(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("reading {}", user_config.display())),
    };

    if existing
        .as_deref()
        .is_some_and(|contents| included(contents, &managed))
    {
        print(vec![
            span(OK, "✓ "),
            plain(format!(
                "{} includes {}",
                display_path(&user_config),
                display_path(&managed)
            )),
        ]);
        return connect_hint(ctx, domains);
    }

    anstream::println!();
    anstream::println!(
        "  To support SSH to Treadmill jobs, the following change needs to be applied to the SSH config:"
    );
    anstream::println!();
    for line in patch(&display_path(&user_config), existing.as_deref(), &include) {
        print(line);
    }
    anstream::println!();

    if !args.yes {
        if args.print || !tui::interactive() {
            ctx.hint("Pass --yes to apply this change, or add the line by hand.");
            return Ok(());
        }
        if !tui::ask(&format!(
            "Apply this change to {}?",
            display_path(&user_config)
        ))
        .await?
        {
            return Err(hinted(
                cancelled(format!("{} unchanged", display_path(&user_config))),
                format!(
                    "Until {} includes {}, only `{} job ssh` can connect to jobs.",
                    display_path(&user_config),
                    display_path(&managed),
                    PROGRAM
                ),
            ));
        }
    }

    match prepend(&user_config, &include)? {
        Some(backup) => print(vec![
            span(OK, "✓ "),
            plain(format!("Updated {}", display_path(&user_config))),
            span(DIM, format!(" · backup {}", display_path(&backup))),
        ]),
        None => print(vec![
            span(OK, "✓ "),
            plain(format!("Created {}", display_path(&user_config))),
            span(DIM, format!(" · includes {}", display_path(&managed))),
        ]),
    }
    connect_hint(ctx, domains)
}

/// The change [`prepend`] makes to the file at `path`, as a unified diff.
fn patch(path: &str, existing: Option<&str>, line: &str) -> Vec<Line> {
    let old: Vec<&str> = existing.map_or(Vec::new(), |contents| contents.lines().collect());
    let mut added = vec![line];
    if !old.is_empty() {
        added.push("");
    }
    let range = |start: usize, len: usize| match len {
        1 => format!("{start}"),
        _ => format!("{start},{len}"),
    };
    let mut lines = vec![
        vec![span(
            BOLD,
            format!("  --- {}", existing.map_or("/dev/null", |_| path)),
        )],
        vec![span(BOLD, format!("  +++ {path}"))],
        vec![span(
            ACCENT,
            format!(
                "  @@ -{} +{} @@",
                range(usize::from(!old.is_empty()), old.len()),
                range(1, old.len() + added.len())
            ),
        )],
    ];
    lines.extend(
        added
            .iter()
            .map(|line| vec![span(OK, format!("  +{line}"))]),
    );
    lines.extend(old.iter().map(|line| vec![plain(format!("   {line}"))]));
    lines
}

fn print(line: Line) {
    anstream::println!("{}", tui::render(&line, None, true));
}

fn check_path(ctx: &Ctx, managed: &Path) {
    let Some(found) = on_path(PROGRAM) else {
        ctx.warn(&format!(
            "{PROGRAM} is not on PATH, so ssh cannot run the ProxyCommand in {}",
            display_path(managed)
        ));
        return;
    };

    let running = std::env::current_exe().ok().and_then(resolve);
    if let Some(running) = running
        && running != found
    {
        ctx.warn(&format!(
            "ssh will run {}, not the {PROGRAM} that was invoked ({})",
            display_path(&found),
            display_path(&running)
        ));
    }
}

fn on_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(program))
        .find(|candidate| executable(candidate))
        .and_then(resolve)
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

fn resolve(path: PathBuf) -> Option<PathBuf> {
    fs::canonicalize(path).ok()
}

fn connect_hint(ctx: &Ctx, domains: &[String]) -> Result<()> {
    anstream::println!();
    anstream::println!("  To connect to a Treadmill job, run:");
    anstream::println!();
    print(vec![
        span(DIM, "  $ "),
        plain(format!("ssh {}@<job-uuid>.{}", ctx.config.user, domains[0])),
    ]);
    Ok(())
}

fn block(ctx: &Ctx, domains: &[String]) -> Result<String> {
    let patterns: Vec<String> = domains.iter().map(|domain| format!("*.{domain}")).collect();
    let mut block = format!("{BEGIN}{}\n", ctx.profile);
    block.push_str(&format!("Host {}\n", patterns.join(" ")));
    block.push_str(&format!("    User {}\n", ctx.config.user));
    block.push_str(&format!("    ProxyCommand {}\n", ssh_proxy_command(ctx)?));
    for (keyword, value) in OPTIONS {
        block.push_str(&format!("    {keyword} {value}\n"));
    }
    block.push_str(&format!("{END}{}\n", ctx.profile));
    Ok(block)
}

fn upsert(contents: &str, profile: &str, block: &str) -> String {
    let begin = format!("{BEGIN}{profile}");
    let end = format!("{END}{profile}");

    let mut out = String::new();
    let mut skipping = false;
    let mut replaced = false;
    for line in contents.lines() {
        if line.trim_end() == begin {
            skipping = true;
            out.push_str(block);
            replaced = true;
            continue;
        }
        if skipping {
            skipping = line.trim_end() != end;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if !replaced {
        if !out.is_empty() && !out.ends_with("\n\n") {
            out.push('\n');
        }
        out.push_str(block);
    }
    out
}

/// The other profiles' host patterns in `contents` that match one of
/// `domains`, as `(profile, pattern)`.
fn overlapping(contents: &str, profile: &str, domains: &[String]) -> Vec<(String, String)> {
    let mut current: Option<&str> = None;
    let mut found = Vec::new();
    for line in contents.lines() {
        if let Some(name) = line.trim_end().strip_prefix(BEGIN) {
            current = Some(name);
        } else if line.trim_end().starts_with(END) {
            current = None;
        } else if let Some(patterns) = line.trim().strip_prefix("Host ")
            && let Some(other) = current.filter(|name| *name != profile)
        {
            found.extend(
                patterns
                    .split_whitespace()
                    .filter(|pattern| {
                        domains
                            .iter()
                            .any(|domain| *pattern == format!("*.{domain}"))
                    })
                    .map(|pattern| (other.to_string(), pattern.to_string())),
            );
        }
    }
    found
}

fn included(contents: &str, managed: &Path) -> bool {
    let managed = display_path(managed);
    contents.lines().any(|line| {
        line.trim()
            .strip_prefix("Include ")
            .is_some_and(|paths| paths.split_whitespace().any(|path| path == managed))
    })
}

fn prepend(path: &Path, line: &str) -> Result<Option<PathBuf>> {
    private_dir(path)?;
    locked_file::update(path, 0o600, |current| {
        let backup = match current {
            Some(_) => {
                let backup = backup_path(path);
                fs::copy(path, &backup).with_context(|| {
                    format!("copying {} to {}", path.display(), backup.display())
                })?;
                Some(backup)
            }
            None => None,
        };
        let mut contents = format!("{line}\n");
        if let Some(existing) = current.as_deref().filter(|existing| !existing.is_empty()) {
            contents.push('\n');
            contents.push_str(existing);
        }
        *current = Some(contents);
        Ok(backup)
    })
    .with_context(|| format!("writing {}", path.display()))
}

/// Create the directory of `path`, unless it exists, readable only by the user.
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    let parent = path.parent().context("path has no parent directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("creating {}", parent.display()))
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("tml-backup")
}

fn user_ssh_config() -> Result<PathBuf> {
    Ok(home()?.join(".ssh").join("config"))
}

fn home() -> Result<PathBuf> {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => Ok(PathBuf::from(home)),
        _ => bail!("HOME is not set, so the user's SSH configuration cannot be located"),
    }
}

fn display_path(path: &Path) -> String {
    let Ok(home) = home() else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: Vec<Line>) -> String {
        lines
            .iter()
            .map(|line| tui::render(line, None, false) + "\n")
            .collect()
    }

    #[test]
    fn the_patch_prepends_the_include_line() {
        let include = "Include ~/.config/tml/ssh_config";
        assert_eq!(
            text(patch(
                "~/.ssh/config",
                Some("Host *\n    User ada\n"),
                include
            )),
            "  --- ~/.ssh/config\n  +++ ~/.ssh/config\n  @@ -1,2 +1,4 @@\n  \
             +Include ~/.config/tml/ssh_config\n  +\n   Host *\n       User ada\n"
        );
        assert_eq!(
            text(patch("~/.ssh/config", None, include)),
            "  --- /dev/null\n  +++ ~/.ssh/config\n  @@ -0,0 +1 @@\n  \
             +Include ~/.config/tml/ssh_config\n"
        );
    }
}

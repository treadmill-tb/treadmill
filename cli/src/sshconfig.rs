use anyhow::{Context as _, Result, bail};
use dialoguer::Confirm;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::cli::SshSetupArgs;
use crate::config;
use crate::ctx::Ctx;
use crate::ssh::{PROGRAM, ssh_proxy_command};

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

pub fn setup(ctx: &Ctx, args: &SshSetupArgs) -> Result<()> {
    let domains = ctx.config.ssh_domains(&ctx.profile)?;
    let managed = config::ssh_config_path()?;

    check_path(ctx, &managed);

    let contents = upsert(&read(&managed)?, &ctx.profile, &block(ctx, domains)?);
    for pattern in overlapping(&contents, &ctx.profile, domains) {
        ctx.warn(&format!(
            "{pattern} is also claimed by another profile in {}; the first block wins",
            managed.display()
        ));
    }
    replace(&managed, &contents).with_context(|| format!("writing {}", managed.display()))?;
    anstream::println!(
        "Wrote the SSH configuration for profile {:?} to {}.",
        ctx.profile,
        managed.display()
    );

    let user_config = user_ssh_config()?;
    let include = format!("Include {}", display_path(&managed));
    anstream::println!(
        "\nFor ssh, git, rsync and anything else that speaks SSH to pick it up,\n\
         {} has to contain this line:\n\n    {include}\n",
        user_config.display()
    );

    if included(&read(&user_config)?, &managed) {
        anstream::println!("It already does, so nothing was changed.");
        return hint(ctx, domains);
    }

    if args.print {
        return hint(ctx, domains);
    }

    if !args.yes {
        if !std::io::stdin().is_terminal() {
            return hint(ctx, domains);
        }
        anstream::println!(
            "tml can prepend that one line to {}, keeping a copy of the current file\n\
             at {}. Nothing else in it is changed. Some systems manage this file\n\
             themselves, in which case add the line above by hand instead.\n",
            user_config.display(),
            backup_path(&user_config).display()
        );
        if !Confirm::new()
            .with_prompt(format!("Edit {}?", user_config.display()))
            .default(false)
            .interact()?
        {
            anstream::println!("\nLeft {} alone.", user_config.display());
            return hint(ctx, domains);
        }
    }

    let backup = prepend(&user_config, &include)?;
    match backup {
        Some(backup) => anstream::println!(
            "Added the line to {}; the previous file is at {}.",
            user_config.display(),
            backup.display()
        ),
        None => anstream::println!("Created {} with that line.", user_config.display()),
    }
    hint(ctx, domains)
}

fn check_path(ctx: &Ctx, managed: &Path) {
    let Some(found) = on_path(PROGRAM) else {
        ctx.warn(&format!(
            "{PROGRAM} is not on PATH, so ssh will not be able to start it; put it on PATH, \
             or edit the ProxyCommand in {}",
            managed.display()
        ));
        return;
    };

    let running = std::env::current_exe().ok().and_then(resolve);
    if running.is_some_and(|running| running != found) {
        ctx.warn(&format!(
            "ssh will run {}, which is not the {PROGRAM} you invoked",
            found.display()
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

fn hint(ctx: &Ctx, domains: &[String]) -> Result<()> {
    anstream::println!(
        "\nYou can now reach a job as: ssh {}@<job-id>.{}",
        ctx.config.user,
        domains[0]
    );
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

fn overlapping(contents: &str, profile: &str, domains: &[String]) -> Vec<String> {
    let mut current: Option<&str> = None;
    let mut found = Vec::new();
    for line in contents.lines() {
        if let Some(name) = line.trim_end().strip_prefix(BEGIN) {
            current = Some(name);
        } else if line.trim_end().starts_with(END) {
            current = None;
        } else if let Some(patterns) = line.trim().strip_prefix("Host ")
            && current.is_some_and(|name| name != profile)
        {
            found.extend(
                patterns
                    .split_whitespace()
                    .filter(|pattern| {
                        domains
                            .iter()
                            .any(|domain| *pattern == format!("*.{domain}"))
                    })
                    .map(str::to_string),
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
    let existing = read(path)?;
    let backup = match path.exists() {
        true => {
            let backup = backup_path(path);
            fs::copy(path, &backup)
                .with_context(|| format!("copying {} to {}", path.display(), backup.display()))?;
            Some(backup)
        }
        false => None,
    };

    let mut contents = format!("{line}\n");
    if !existing.is_empty() {
        contents.push('\n');
        contents.push_str(&existing);
    }
    replace(path, &contents).with_context(|| format!("writing {}", path.display()))?;
    Ok(backup)
}

fn backup_path(path: &Path) -> PathBuf {
    path.with_extension("tml-backup")
}

fn read(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn replace(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::DirBuilderExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    use std::os::unix::fs::PermissionsExt as _;

    let parent = path.parent().context("path has no parent directory")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("creating {}", parent.display()))?;

    let mode = match fs::metadata(path) {
        Ok(metadata) => metadata.permissions().mode(),
        Err(_) => 0o600,
    };

    let tmp = path.with_extension("tml-tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(contents.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(())
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

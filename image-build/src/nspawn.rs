//! Running an image's provisioning inside the image.
//!
//! Provisioning is the image's own `provision.sh`, executed against the
//! mounted file system by `systemd-nspawn`. It runs the *guest's* binaries,
//! which is why a cross-architecture build needs `binfmt_misc`
//! ([`crate::spec::require_binfmt`]) and why there is no way to do this
//! without a container.
//!
//! The container is deliberately not isolated from the network: provisioning
//! installs packages, so it shares the host's network namespace.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};

use crate::exec::{self, Privilege};
use crate::spec::{Arch, ImageType};

/// Where the build stages its files inside the guest.
///
/// Under `/var/tmp` rather than `/tmp`, because nspawn overmounts `/tmp` with
/// a fresh tmpfs and anything staged there would be gone before it could run.
pub const GUEST_BUILD_DIR: &str = "/var/tmp/tml-build";

/// The `PATH` provisioning runs with.
///
/// Set explicitly: a container started from a bare mount inherits nothing
/// useful, and a `provision.sh` that cannot find `apt-get` fails confusingly.
const GUEST_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// What runs after provisioning, in every image.
///
/// Package manager caches are large, reproduce from the network, and would
/// otherwise ship in the layer that installed them.
const CLEANUP_SH: &str = "\
#!/bin/sh
set -eu

DEBIAN_FRONTEND=noninteractive apt-get autoremove --purge -y
apt-get clean
rm -rf /var/lib/apt/lists/*
";

/// What a build gives the guest to work with.
pub struct Provisioning<'a> {
    /// The resolved image directory, staged as `$TML_IMAGE_DIR`.
    pub image_dir: &'a Path,
    /// Executables to install into the image, staged as `$TML_PAYLOAD_DIR`.
    pub payloads: &'a [PathBuf],
    /// The `inputs.env` the guest sources.
    pub inputs_env: &'a str,
    pub arch: Arch,
    pub image_type: ImageType,
}

/// The `--bind` arguments that give the container the image's own devices.
///
/// Guest tooling writes boot loaders and initramfs images, which means it
/// needs the block devices — and a `/dev/disk` that resolves to *these*
/// partitions rather than the host's. `device_links` is the farm
/// [`crate::mount::device_links`] built.
pub fn binds(device: &Path, partitioned: bool, device_links: Option<&Path>) -> Result<Vec<String>> {
    let mut binds = vec![format!("--bind={}", device.display())];
    if partitioned {
        for partition in crate::nbd::partitions_of(device)? {
            binds.push(format!("--bind={}", partition.display()));
        }
        if let Some(links) = device_links {
            binds.push(format!("--bind={}:/dev/disk", links.display()));
        }
    }
    Ok(binds)
}

/// Run one command inside the mounted image.
pub fn run(privilege: Privilege, root: &Path, binds: &[String], command: &[&str]) -> Result<()> {
    let mut nspawn = privilege.command("systemd-nspawn");
    nspawn
        .args(["--quiet", "--register=no"])
        .arg(format!("--setenv=PATH={GUEST_PATH}"))
        .args(binds)
        .arg("-D")
        .arg(root)
        .arg("--")
        .args(command);
    exec::run(&mut nspawn).with_context(|| format!("run `{}` in the guest", command.join(" ")))
}

/// Provision the image mounted at `root`.
pub fn provision(
    privilege: Privilege,
    root: &Path,
    binds: &[String],
    work: &Path,
    provisioning: &Provisioning<'_>,
) -> Result<()> {
    stage(privilege, root, work, provisioning)?;

    let resolv = ResolvConf::borrow(privilege, root)?;
    let result = (|| -> Result<()> {
        // Up front and separately, so that a stale package index is one clear
        // failure rather than a confusing one inside provision.sh.
        run(
            privilege,
            root,
            binds,
            &[
                "/bin/sh",
                "-c",
                "DEBIAN_FRONTEND=noninteractive apt-get update",
            ],
        )?;
        run(
            privilege,
            root,
            binds,
            &[&format!("{GUEST_BUILD_DIR}/run.sh")],
        )?;
        run(
            privilege,
            root,
            binds,
            &[&format!("{GUEST_BUILD_DIR}/cleanup.sh")],
        )
    })();

    // Restore before reporting: leaving the host's resolv.conf in the image
    // would ship it, and that must happen whether provisioning worked or not.
    resolv.restore(privilege, root)?;
    result?;

    let staged = guest_path(root);
    exec::run(privilege.command("rm").arg("-rf").arg(&staged))
        .with_context(|| format!("remove {}", staged.display()))
}

/// Where [`GUEST_BUILD_DIR`] is, seen from the host.
fn guest_path(root: &Path) -> PathBuf {
    root.join(GUEST_BUILD_DIR.trim_start_matches('/'))
}

/// Copy everything provisioning needs into the guest.
fn stage(
    privilege: Privilege,
    root: &Path,
    work: &Path,
    provisioning: &Provisioning<'_>,
) -> Result<()> {
    let staged = guest_path(root);
    exec::run(privilege.command("rm").arg("-rf").arg(&staged))?;
    exec::run(privilege.command("mkdir").arg("-p").arg(&staged))
        .with_context(|| format!("create {}", staged.display()))?;

    // -L dereferences: a resolved image directory reaches its components
    // through symlinks that would dangle the moment they were staged.
    exec::run(
        privilege
            .command("cp")
            .arg("-aLT")
            .arg(provisioning.image_dir)
            .arg(staged.join("image")),
    )
    .context("stage the image directory")?;

    let payloads = staged.join("payload");
    exec::run(privilege.command("mkdir").arg("-p").arg(&payloads))?;
    for payload in provisioning.payloads {
        exec::run(
            privilege
                .command("install")
                .args(["-m", "0755"])
                .arg(payload)
                .arg(&payloads),
        )
        .with_context(|| format!("stage payload {}", payload.display()))?;
    }

    // Written locally first: these land in a root-owned tree, and `install`
    // is a cleaner way through that than redirecting a privileged shell.
    let local_env = work.join("inputs.env");
    fs::write(&local_env, provisioning.inputs_env)
        .with_context(|| format!("write {}", local_env.display()))?;
    exec::run(
        privilege
            .command("install")
            .args(["-m", "0644"])
            .arg(&local_env)
            .arg(staged.join("inputs.env")),
    )?;

    let local_cleanup = work.join("cleanup.sh");
    fs::write(&local_cleanup, CLEANUP_SH)
        .with_context(|| format!("write {}", local_cleanup.display()))?;
    exec::run(
        privilege
            .command("install")
            .args(["-m", "0755"])
            .arg(&local_cleanup)
            .arg(staged.join("cleanup.sh")),
    )?;

    let local_runner = work.join("run.sh");
    fs::write(
        &local_runner,
        runner(provisioning.arch, provisioning.image_type),
    )
    .with_context(|| format!("write {}", local_runner.display()))?;
    exec::run(
        privilege
            .command("install")
            .args(["-m", "0755"])
            .arg(&local_runner)
            .arg(staged.join("run.sh")),
    )?;

    // A provision.sh that arrived without its executable bit would fail deep
    // inside the container for no visible reason.
    exec::run(
        privilege
            .command("chmod")
            .arg("0755")
            .arg(staged.join("image/provision.sh")),
    )?;
    Ok(())
}

/// The script that sets provisioning's environment up and hands over to it.
///
/// This is the contract an image's `provision.sh` is written against.
pub fn runner(arch: Arch, image_type: ImageType) -> String {
    format!(
        "\
#!/bin/sh
set -eu
TML_IMAGE_DIR={GUEST_BUILD_DIR}/image
TML_PAYLOAD_DIR={GUEST_BUILD_DIR}/payload
TML_ARCH='{arch}'
TML_TYPE='{image_type}'
DEBIAN_FRONTEND=noninteractive
export TML_IMAGE_DIR TML_PAYLOAD_DIR TML_ARCH TML_TYPE DEBIAN_FRONTEND
set -a
. {GUEST_BUILD_DIR}/inputs.env
set +a
exec \"$TML_IMAGE_DIR/provision.sh\"
"
    )
}

/// The image's own `/etc/resolv.conf`, lent to the host's for the duration.
///
/// A cloud image ships `/etc/resolv.conf` as a symlink into systemd-resolved's
/// runtime directory, which dangles in a container. nspawn's own options do
/// not help: `--resolv-conf=copy-host` cannot write through a dangling
/// symlink, and `replace-host` would bake the *host's* nameservers into the
/// shipped image. So the swap happens here, and is undone afterwards.
struct ResolvConf {
    /// Where the original was moved to, if there was one.
    saved: Option<PathBuf>,
}

impl ResolvConf {
    fn borrow(privilege: Privilege, root: &Path) -> Result<Self> {
        let resolv = root.join("etc/resolv.conf");
        let saved = if resolv.exists() || resolv.symlink_metadata().is_ok() {
            let saved = resolv.with_extension("conf.tml-orig");
            exec::run(privilege.command("mv").arg(&resolv).arg(&saved))
                .context("set the image's resolv.conf aside")?;
            Some(saved)
        } else {
            None
        };
        // -L so the host's own symlink is followed and its contents copied.
        exec::run(
            privilege
                .command("cp")
                .arg("-fL")
                .arg("/etc/resolv.conf")
                .arg(&resolv),
        )
        .context("lend the host's resolv.conf to the image")?;
        Ok(ResolvConf { saved })
    }

    fn restore(self, privilege: Privilege, root: &Path) -> Result<()> {
        let resolv = root.join("etc/resolv.conf");
        exec::run(privilege.command("rm").arg("-f").arg(&resolv))?;
        if let Some(saved) = self.saved {
            exec::run(privilege.command("mv").arg(&saved).arg(&resolv))
                .context("restore the image's resolv.conf")?;
        }
        Ok(())
    }
}

/// Check that `systemd-nspawn` is usable before a build commits to anything.
pub fn require_nspawn() -> Result<()> {
    exec::require_tools(&["systemd-nspawn"])
}

/// The command line [`run`] would use, for tests.
pub fn describe(privilege: Privilege, root: &Path, binds: &[String], command: &[&str]) -> String {
    let mut nspawn = privilege.command("systemd-nspawn");
    nspawn
        .args(["--quiet", "--register=no"])
        .arg(format!("--setenv=PATH={GUEST_PATH}"))
        .args(binds)
        .arg("-D")
        .arg(root)
        .arg("--")
        .args(command);
    exec::describe(&nspawn)
}

/// What runs after every image's own provisioning.
pub fn cleanup_script() -> &'static str {
    CLEANUP_SH
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_runner_sources_inputs_with_export_on() {
        let script = runner(Arch::X86_64, ImageType::Disk);
        // `set -a` is what turns the assignments in inputs.env into exported
        // variables; without it provision.sh sees none of them.
        let set_a = script.find("set -a").expect("set -a");
        let source = script
            .find(". /var/tmp/tml-build/inputs.env")
            .expect("source");
        let set_plus_a = script.find("set +a").expect("set +a");
        assert!(set_a < source && source < set_plus_a);
    }

    #[test]
    fn the_runner_tells_the_guest_what_it_is_building() {
        let script = runner(Arch::Aarch64, ImageType::Sd);
        assert!(script.contains("TML_ARCH='aarch64'"), "{script}");
        assert!(script.contains("TML_TYPE='sd'"), "{script}");
        assert!(
            script.contains("TML_IMAGE_DIR=/var/tmp/tml-build/image"),
            "{script}"
        );
        assert!(
            script.contains("exec \"$TML_IMAGE_DIR/provision.sh\""),
            "{script}"
        );
    }

    #[test]
    fn staging_lives_outside_tmp() {
        // nspawn overmounts /tmp, so anything staged there is gone before it
        // can be run.
        assert!(!GUEST_BUILD_DIR.starts_with("/tmp/"));
        let script = runner(Arch::X86_64, ImageType::Disk);
        assert!(!script.contains("=/tmp/"), "{script}");
    }

    #[test]
    fn the_container_is_given_the_images_own_devices() {
        let binds = binds(Path::new("/dev/nbd0"), false, None).unwrap();
        assert_eq!(binds, ["--bind=/dev/nbd0"]);
    }

    #[test]
    fn nspawn_runs_unregistered_and_with_an_explicit_path() {
        let line = describe(
            Privilege::none(),
            Path::new("/mnt/root"),
            &["--bind=/dev/nbd0".to_string()],
            &["/bin/sh", "-c", "true"],
        );
        assert!(line.contains("--register=no"), "{line}");
        assert!(
            line.contains(&format!("--setenv=PATH={GUEST_PATH}")),
            "{line}"
        );
        assert!(line.contains("--bind=/dev/nbd0"), "{line}");
        assert!(line.ends_with("-D /mnt/root -- /bin/sh -c true"), "{line}");
    }

    #[test]
    fn the_cleanup_script_clears_the_package_caches() {
        let script = cleanup_script();
        assert!(script.contains("apt-get clean"), "{script}");
        assert!(script.contains("rm -rf /var/lib/apt/lists/*"), "{script}");
    }
}

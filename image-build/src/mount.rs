//! Mounting a build's file systems, and finding them again.
//!
//! The build mounts what it is about to provision, and has to be careful
//! about *which* devices it mounts. Everything here needs root.
//!
//! The one rule worth stating up front: every device lookup is scoped to the
//! attached device's own partitions. A build host is often itself a cloud
//! image carrying the same file system labels as the image being built —
//! `cloudimg-rootfs`, say — and udev keeps one `/dev/disk/by-label` entry per
//! label. Resolving a label globally can therefore resolve to the *host's*
//! disk, which a build would then mount and provision. Scoping the lookup is
//! what prevents that.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs as unix_fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use crate::exec::{self, Privilege};
use crate::nbd;

/// File system types in an `fstab` that name no device to mount.
const PSEUDO: &[&str] = &[
    "swap", "proc", "sysfs", "tmpfs", "devpts", "devtmpfs", "none",
];

/// The `blkid` tags a partition can be found by, and the `/dev/disk`
/// directory each populates.
const TAGS: &[(&str, &str)] = &[
    ("UUID", "by-uuid"),
    ("PARTUUID", "by-partuuid"),
    ("LABEL", "by-label"),
    ("PARTLABEL", "by-partlabel"),
];

/// Mount `source` at `target`, creating the mount point.
pub fn mount(privilege: Privilege, source: &Path, target: &Path, options: &[&str]) -> Result<()> {
    exec::run(privilege.command("mkdir").arg("-p").arg(target))
        .with_context(|| format!("create {}", target.display()))?;
    let mut command = privilege.command("mount");
    command.args(options).arg(source).arg(target);
    exec::run(&mut command)
        .with_context(|| format!("mount {} at {}", source.display(), target.display()))
}

/// Unmount `target` and everything under it, then remove the mount point.
///
/// Recursive because a build nests mounts — `/boot/firmware` inside the root,
/// and whatever an `fstab` asks for — and unmounting the outermost alone
/// would leave the rest attached.
pub fn unmount_tree(privilege: Privilege, target: &Path) -> Result<()> {
    if !target.exists() {
        return Ok(());
    }
    exec::run(
        privilege
            .command("umount")
            .arg("-R")
            .arg(target)
            .stderr(std::process::Stdio::null()),
    )
    .with_context(|| format!("unmount {}", target.display()))?;
    // Only the mount point itself; anything else here is not this build's.
    match fs::remove_dir(target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => {
            // It may be root-owned, having been created through sudo.
            let _ = exec::run(privilege.command("rmdir").arg(target));
            Ok(())
        }
    }
}

/// The root file system's device: the first partition of a partitioned image,
/// or the whole device of a bare one.
pub fn root_partition(device: &Path, partitioned: bool) -> PathBuf {
    if partitioned {
        let mut name = device.to_path_buf().into_os_string();
        name.push("p1");
        PathBuf::from(name)
    } else {
        device.to_path_buf()
    }
}

/// Grow the root file system to fill the space the image was grown by.
pub fn resize_root(privilege: Privilege, device: &Path, partitioned: bool) -> Result<()> {
    let target = root_partition(device, partitioned);
    if partitioned {
        exec::run(privilege.command("growpart").arg(device).arg("1"))
            .with_context(|| format!("grow partition 1 of {}", device.display()))?;
        exec::run(privilege.command("partx").arg("-u").arg(device))
            .with_context(|| format!("re-read the partition table of {}", device.display()))?;
    }
    // resize2fs refuses a file system that has not been checked, and a vendor
    // image is routinely in that state. A failure here is not fatal on its
    // own: the resize that follows reports anything that actually matters.
    let _ = exec::run(
        privilege
            .command("e2fsck")
            .arg("-fy")
            .arg(&target)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null()),
    );
    exec::run(privilege.command("resize2fs").arg(&target))
        .with_context(|| format!("resize {}", target.display()))
}

/// Resolve one `fstab` device specification against `device`'s partitions.
///
/// Scoped on purpose — see the module docs.
pub fn resolve_partition(privilege: Privilege, device: &Path, spec: &str) -> Result<PathBuf> {
    if spec.starts_with("/dev/") {
        return Ok(PathBuf::from(spec));
    }
    let (tag, value) = spec
        .split_once('=')
        .filter(|(tag, _)| TAGS.iter().any(|(known, _)| known == tag))
        .with_context(|| format!("cannot resolve fstab specification {spec:?}"))?;

    for partition in nbd::partitions_of(device)? {
        if probe(privilege, &partition, tag).as_deref() == Some(value) {
            return Ok(partition);
        }
    }
    bail!(
        "cannot resolve fstab specification {spec:?} to a partition of {}",
        device.display(),
    )
}

/// What `blkid` reports for one tag of one partition.
fn probe(privilege: Privilege, partition: &Path, tag: &str) -> Option<String> {
    let output = exec::capture(
        privilege
            .command("blkid")
            .args(["-s", tag, "-o", "value"])
            .arg(partition),
    )
    .ok()?;
    let value = output.trim().to_string();
    (!value.is_empty()).then_some(value)
}

/// Mount everything `root`'s own `/etc/fstab` asks for, under `root`.
pub fn mount_fstab(privilege: Privilege, device: &Path, root: &Path) -> Result<Vec<PathBuf>> {
    let fstab = root.join("etc/fstab");
    if !fstab.is_file() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&fstab).with_context(|| format!("read {}", fstab.display()))?;

    let mut mounted = Vec::new();
    for (point, spec) in fstab_entries(&text) {
        let source = resolve_partition(privilege, device, &spec)
            .with_context(|| format!("fstab entry for {point}"))?;
        let target = root.join(point.trim_start_matches('/'));
        mount(privilege, &source, &target, &[])?;
        mounted.push(target);
    }
    Ok(mounted)
}

/// Build a `/dev/disk` of this device's partitions alone, for the container.
///
/// Guest tooling — `update-grub`, `update-initramfs` — looks devices up by
/// label and UUID. Binding the host's real `/dev/disk` would expose the
/// host's own disks to those lookups, so the container gets one made of
/// nothing but the image's partitions.
pub fn device_links(privilege: Privilege, device: &Path, dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for partition in nbd::partitions_of(device)? {
        let Some(name) = partition.file_name() else {
            continue;
        };
        for (tag, directory) in TAGS {
            let Some(value) = probe(privilege, &partition, tag) else {
                continue;
            };
            let by = dir.join(directory);
            fs::create_dir_all(&by).with_context(|| format!("create {}", by.display()))?;
            // Relative, so it resolves to /dev/<name> once bound at /dev/disk.
            let target = Path::new("../..").join(name);
            let link = by.join(&value);
            let _ = fs::remove_file(&link);
            unix_fs::symlink(&target, &link).with_context(|| format!("link {}", link.display()))?;
        }
    }
    Ok(())
}

/// Every mount point at or under `root`, innermost last.
pub fn mount_points(privilege: Privilege, root: &Path) -> Result<Vec<PathBuf>> {
    let output = exec::capture(
        privilege
            .command("findmnt")
            .args(["-R", "-n", "-l", "-o", "TARGET"])
            .arg(root),
    )
    .with_context(|| format!("list the mounts under {}", root.display()))?;
    Ok(output
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .collect())
}

/// Discard every freed block of every file system under `root`.
///
/// The `sync` before each trim is what makes it work. ext4 defers discarding
/// the blocks a deletion freed until the transaction that freed them commits,
/// so a bare `fstrim` skips everything the build just deleted and the shipped
/// blob keeps carrying it.
pub fn trim(privilege: Privilege, root: &Path) -> Result<()> {
    for point in mount_points(privilege, root)? {
        exec::run(privilege.command("sync").arg("-f").arg(&point))
            .with_context(|| format!("sync {}", point.display()))?;
        exec::run(privilege.command("fstrim").arg("-v").arg(&point))
            .with_context(|| format!("trim {}", point.display()))?;
    }
    Ok(())
}

/// The fstab entries a build acts on, mount point and device specification,
/// parents before children.
///
/// Ordering by depth is what keeps a mount from landing on a directory that
/// is about to be covered by its own parent's file system.
pub fn fstab_entries(text: &str) -> Vec<(String, String)> {
    let mut wanted: Vec<(usize, String, String)> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(spec), Some(point), Some(kind)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // The root is already mounted; a relative point is not ours to guess
        // at; a pseudo file system has no device to attach.
        if point == "/" || !point.starts_with('/') || PSEUDO.contains(&kind) {
            continue;
        }
        wanted.push((
            point.matches('/').count(),
            point.to_string(),
            spec.to_string(),
        ));
    }
    wanted.sort();
    wanted
        .into_iter()
        .map(|(_, point, spec)| (point, spec))
        .collect()
}

/// The `/dev/disk` layout [`device_links`] would build, as link path to
/// target. Exposed so the shape can be asserted without a block device.
pub fn link_layout(entries: &[(&str, &str, &str)]) -> BTreeMap<String, String> {
    let mut layout = BTreeMap::new();
    for (partition, tag, value) in entries {
        let Some((_, directory)) = TAGS.iter().find(|(known, _)| known == tag) else {
            continue;
        };
        layout.insert(format!("{directory}/{value}"), format!("../../{partition}"));
    }
    layout
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_partition_depends_on_whether_there_is_a_table() {
        let device = Path::new("/dev/nbd0");
        assert_eq!(root_partition(device, true), Path::new("/dev/nbd0p1"));
        assert_eq!(root_partition(device, false), Path::new("/dev/nbd0"));
    }

    #[test]
    fn fstab_parents_are_mounted_before_their_children() {
        let fstab = "\
LABEL=root / ext4 defaults 0 1
LABEL=deep /var/lib/thing ext4 defaults 0 2
LABEL=boot /boot vfat defaults 0 2
LABEL=mid /var/lib ext4 defaults 0 2
";
        let entries = fstab_entries(fstab);
        let points: Vec<&str> = entries.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(points, ["/boot", "/var/lib", "/var/lib/thing"]);
    }

    #[test]
    fn fstab_skips_what_has_no_device_to_mount() {
        let fstab = "\
# a comment
LABEL=root / ext4 defaults 0 1
/dev/disk/swap none swap sw 0 0
proc /proc proc defaults 0 0
tmpfs /run tmpfs defaults 0 0
LABEL=odd relative/point ext4 defaults 0 0
LABEL=boot /boot vfat defaults 0 2

";
        let entries = fstab_entries(fstab);
        assert_eq!(entries, [("/boot".to_string(), "LABEL=boot".to_string())]);
    }

    #[test]
    fn fstab_keeps_a_nested_mount_point() {
        let entries = fstab_entries("PARTUUID=abc /boot/firmware vfat defaults 0 2\n");
        assert_eq!(
            entries,
            [("/boot/firmware".to_string(), "PARTUUID=abc".to_string())]
        );
    }

    #[test]
    fn a_device_path_resolves_to_itself() {
        let resolved =
            resolve_partition(Privilege::none(), Path::new("/dev/nbd0"), "/dev/sda1").unwrap();
        assert_eq!(resolved, Path::new("/dev/sda1"));
    }

    #[test]
    fn an_unrecognised_specification_is_refused() {
        let error =
            resolve_partition(Privilege::none(), Path::new("/dev/nbd0"), "whatever").unwrap_err();
        assert!(format!("{error:#}").contains("whatever"), "{error:#}");
    }

    #[test]
    fn device_links_are_relative_so_they_resolve_under_dev() {
        // /dev/disk/by-uuid/<uuid> -> ../../nbd0p1 is /dev/nbd0p1 once the
        // farm is bound at /dev/disk.
        let layout = link_layout(&[
            ("nbd0p1", "UUID", "1234-5678"),
            ("nbd0p2", "LABEL", "cloudimg-rootfs"),
        ]);
        assert_eq!(layout["by-uuid/1234-5678"], "../../nbd0p1");
        assert_eq!(layout["by-label/cloudimg-rootfs"], "../../nbd0p2");
    }
}

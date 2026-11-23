//! What a resolved image directory says about itself.
//!
//! One directory with an `image.json`, a `provision.sh` and usually an
//! `inputs.json` is the whole of what a build reads. [`crate::resolve`]
//! produces such a directory from a coordinate, but a hand-written one works
//! just as well — the builder does not care where it came from.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

/// The architectures a Treadmill image can target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

impl Arch {
    /// The name the kernel and `uname -m` use, and the suffix an arch-specific
    /// `inputs.json` key carries.
    pub fn as_str(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }
}

impl std::fmt::Display for Arch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How an image's storage is laid out, which decides what volumes it has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageType {
    /// One partitioned disk, booted as a whole. A single `disk` volume.
    Disk,
    /// An SD card, whose two file systems are netbooted as separate volumes:
    /// `rootfs` and `bootfs`.
    Sd,
}

impl ImageType {
    /// The volumes an image of this type consists of. The first is the one
    /// that gets grown and mounted at `/`.
    pub fn roles(&self) -> &'static [&'static str] {
        match self {
            ImageType::Disk => &["disk"],
            ImageType::Sd => &["rootfs", "bootfs"],
        }
    }

    /// Whether the primary volume carries a partition table.
    pub fn partitioned(&self) -> bool {
        matches!(self, ImageType::Disk)
    }
}

impl std::fmt::Display for ImageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ImageType::Disk => "disk",
            ImageType::Sd => "sd",
        })
    }
}

/// A resolved `image.json`.
///
/// Deliberately lenient about unknown keys: the tree-wide schema check is
/// [`crate::tree::check`]'s job, and a directory may have been produced
/// by something other than this crate.
#[derive(Debug, Clone, Deserialize)]
pub struct ImageSpec {
    pub title: String,
    pub arch: Arch,
    #[serde(rename = "type")]
    pub image_type: ImageType,
    /// What the image calls itself, and what goes in the manifest.
    ///
    /// Required. It used to fall back to `<parent>/<dir>`, which tied an
    /// image's identity to what its checkout directory happened to be
    /// called — rename the clone and the image quietly became a different
    /// one. A resolved directory gets this written for it; a hand-written
    /// one has to say.
    pub name: String,
    /// What this is built on. Only its emptiness matters here: an image with a
    /// base is handed a lower, one without fetches its own vendor image.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// How much to grow the primary volume by, e.g. `6G`.
    #[serde(default)]
    pub grow: Option<String>,
}

/// One pinned download: a version, where to get it, and what it must hash to.
#[derive(Debug, Clone, Deserialize)]
pub struct Input {
    pub version: String,
    pub url: String,
    pub sha256: String,
}

/// An `inputs.json`: pinned downloads, by name.
///
/// A name may carry a `__<arch>` suffix, which restricts it to that
/// architecture and is stripped before provisioning sees it. So
/// `ttyd__x86_64` and `ttyd__aarch64` are one `ttyd` from the guest's point of
/// view, and only the right one is in scope.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    entries: BTreeMap<String, Input>,
}

impl Inputs {
    /// Read `inputs.json` from `dir`, or an empty set if there is none.
    pub fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("inputs.json");
        if !path.is_file() {
            return Ok(Inputs::default());
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        // Entries carry bumper metadata (`source`, `retrieved_at`, …) that is
        // no business of the build's; take the three fields that are.
        let raw: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        let mut entries = BTreeMap::new();
        for (name, value) in raw {
            let input: Input = serde_json::from_value(value)
                .with_context(|| format!("{}: input {name}", path.display()))?;
            entries.insert(name, input);
        }
        Ok(Inputs { entries })
    }

    /// An input by its exact name, suffix and all.
    pub fn get(&self, name: &str) -> Option<&Input> {
        self.entries.get(name)
    }

    /// The inputs that apply to `arch`, under their unsuffixed names.
    pub fn for_arch(&self, arch: Arch) -> BTreeMap<&str, &Input> {
        let mut selected = BTreeMap::new();
        for (key, input) in &self.entries {
            let mut parts = key.splitn(2, "__");
            let name = parts.next().unwrap_or(key);
            match parts.next() {
                None => {
                    selected.insert(name, input);
                }
                Some(suffix) if suffix == arch.as_str() => {
                    selected.insert(name, input);
                }
                Some(_) => {}
            }
        }
        selected
    }

    /// The `inputs.env` a guest's provisioning sources: `<name>_version`,
    /// `<name>_url` and `<name>_sha256` for every input that applies.
    pub fn env_file(&self, arch: Arch) -> String {
        let mut out = String::new();
        for (name, input) in self.for_arch(arch) {
            out.push_str(&format!("{name}_version={}\n", sh_quote(&input.version)));
            out.push_str(&format!("{name}_url={}\n", sh_quote(&input.url)));
            out.push_str(&format!("{name}_sha256={}\n", sh_quote(&input.sha256)));
        }
        out
    }
}

/// Quote a value so that a POSIX shell reads it back unchanged.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// A resolved image directory, opened and checked over.
#[derive(Debug, Clone)]
pub struct ImageDir {
    pub path: PathBuf,
    pub spec: ImageSpec,
    /// What to call this image in messages and in the manifest.
    pub name: String,
    /// Prose for `org.opencontainers.image.description`.
    pub description: Option<String>,
    pub inputs: Inputs,
}

impl ImageDir {
    /// Open the image directory at `path`.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let spec_path = path.join("image.json");
        let bytes =
            fs::read(&spec_path).with_context(|| format!("read {}", spec_path.display()))?;
        let mut deserializer = serde_json::Deserializer::from_slice(&bytes);
        let spec: ImageSpec = serde_path_to_error::deserialize(&mut deserializer)
            .map_err(|e| anyhow!("{}: {e}", spec_path.display()))?;

        ensure!(
            path.join("provision.sh").is_file(),
            "{}: no provision.sh",
            path.display(),
        );

        ensure!(
            !spec.name.is_empty(),
            "{}: name must not be empty",
            spec_path.display(),
        );
        let name = spec.name.clone();

        // A description.txt is the preferred place for prose, and supersedes
        // the inline field.
        let description_path = path.join("description.txt");
        let description = if description_path.is_file() {
            let text = fs::read_to_string(&description_path)
                .with_context(|| format!("read {}", description_path.display()))?
                .trim_end()
                .to_string();
            ensure!(!text.is_empty(), "{} is empty", description_path.display());
            Some(text)
        } else {
            spec.description.clone().filter(|d| !d.is_empty())
        };

        let inputs = Inputs::open(&path)?;

        Ok(ImageDir {
            path,
            spec,
            name,
            description,
            inputs,
        })
    }

    /// Whether this image starts a chain, fetching its own vendor image
    /// rather than being handed a lower.
    pub fn is_root(&self) -> bool {
        self.spec.base.as_deref().unwrap_or("").is_empty()
    }

    /// The volumes this image consists of. The first is the one that gets
    /// grown and mounted at `/`.
    pub fn roles(&self) -> &'static [&'static str] {
        self.spec.image_type.roles()
    }

    /// How many bytes to grow the primary volume by, if any.
    pub fn grow_bytes(&self) -> Result<Option<u64>> {
        self.spec
            .grow
            .as_deref()
            .filter(|g| !g.is_empty())
            .map(parse_size)
            .transpose()
    }

    /// The vendor image a root image starts from.
    pub fn vendor_image(&self) -> Result<&Input> {
        self.inputs.get("vendor_image").with_context(|| {
            format!(
                "{} has no base, so its inputs.json needs a vendor_image",
                self.name,
            )
        })
    }
}

/// Parse a size like `6G`, `512M` or a bare byte count.
pub fn parse_size(size: &str) -> Result<u64> {
    let (digits, multiplier) = match size.as_bytes().last() {
        Some(b'K' | b'k') => (&size[..size.len() - 1], 1024),
        Some(b'M' | b'm') => (&size[..size.len() - 1], 1024 * 1024),
        Some(b'G' | b'g') => (&size[..size.len() - 1], 1024 * 1024 * 1024),
        _ => (size, 1),
    };
    let value: u64 = digits
        .parse()
        .map_err(|_| anyhow!("cannot parse size: {size:?}"))?;
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("size out of range: {size:?}"))
}

/// The build metadata that comes from the environment rather than the image.
///
/// CI knows when the build happened and what commit it came from; the image
/// tree does not, and should not have to.
#[derive(Debug, Clone, Default)]
pub struct Annotations {
    /// Overrides the image's own `version`.
    pub version: Option<String>,
    /// `org.opencontainers.image.created`, an RFC 3339 timestamp.
    pub created: Option<String>,
    /// `org.opencontainers.image.revision`, the source commit.
    pub revision: Option<String>,
    /// `org.opencontainers.image.documentation`, a URL.
    pub documentation: Option<String>,
}

impl Annotations {
    /// Read the `TML_IMAGE_*` variables.
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        Annotations {
            version: var("TML_IMAGE_VERSION"),
            created: var("TML_IMAGE_CREATED"),
            revision: var("TML_IMAGE_REVISION"),
            documentation: var("TML_IMAGE_DOCUMENTATION"),
        }
    }
}

/// The external tools a build shells out to, checked up front.
pub const REQUIRED_TOOLS: &[&str] = &[
    "qemu-img",
    "qemu-nbd",
    "sfdisk",
    "partx",
    "blkid",
    "mount",
    "umount",
    "findmnt",
    "fstrim",
    "growpart",
    "resize2fs",
    "e2fsck",
    "systemd-nspawn",
];

/// Fail unless this host can run `arch` binaries.
///
/// Provisioning runs the guest's own executables, so a foreign-architecture
/// build needs a registered `binfmt_misc` handler. Finding that out now beats
/// finding it out from a container that cannot exec anything.
pub fn require_binfmt(arch: Arch) -> Result<()> {
    let host = nix::sys::utsname::uname()
        .context("uname")?
        .machine()
        .to_string_lossy()
        .into_owned();
    if host == arch.as_str() {
        return Ok(());
    }
    let registered = [
        format!("/proc/sys/fs/binfmt_misc/qemu-{arch}"),
        format!("/proc/sys/fs/binfmt_misc/{arch}-linux"),
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    if registered {
        return Ok(());
    }
    bail!(
        "this image targets {arch} but the host is {host}, and no binfmt_misc handler \
         for qemu-{arch} is registered. Register one, e.g.\n\n    \
         docker run --privileged --rm tonistiigi/binfmt --install all\n\n\
         or on NixOS set boot.binfmt.emulatedSystems, then retry.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(json: &str) -> Inputs {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("inputs.json"), json).unwrap();
        Inputs::open(dir.path()).unwrap()
    }

    #[test]
    fn sizes_carry_a_unit_or_are_bytes() {
        assert_eq!(parse_size("6G").unwrap(), 6 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("512M").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_size("64k").unwrap(), 64 * 1024);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("").is_err());
        assert!(parse_size("6GB").is_err());
        assert!(parse_size("lots").is_err());
    }

    #[test]
    fn an_arch_suffix_selects_and_is_then_stripped() {
        let inputs = inputs(
            r#"{
                "ttyd__x86_64": {"version": "1", "url": "u1", "sha256": "a"},
                "ttyd__aarch64": {"version": "2", "url": "u2", "sha256": "b"},
                "openocd": {"version": "3", "url": "u3", "sha256": "c"}
            }"#,
        );
        let selected = inputs.for_arch(Arch::X86_64);
        // The guest sees one `ttyd`, not two, and the unsuffixed input always.
        assert_eq!(
            selected.keys().copied().collect::<Vec<_>>(),
            ["openocd", "ttyd"]
        );
        assert_eq!(selected["ttyd"].version, "1");

        let selected = inputs.for_arch(Arch::Aarch64);
        assert_eq!(selected["ttyd"].version, "2");
    }

    #[test]
    fn the_env_file_assigns_all_three_fields() {
        let inputs =
            inputs(r#"{"ttyd": {"version": "1.7.7", "url": "https://e/t", "sha256": "ab"}}"#);
        let env = inputs.env_file(Arch::X86_64);
        assert_eq!(
            env,
            "ttyd_version='1.7.7'\nttyd_url='https://e/t'\nttyd_sha256='ab'\n"
        );
    }

    #[test]
    fn a_quote_in_a_value_survives_the_env_file() {
        // Sourced with `set -a`, so a value that escapes its quoting would run
        // as a command.
        let inputs =
            inputs(r#"{"odd": {"version": "1'; touch /tmp/pwned; '", "url": "u", "sha256": "s"}}"#);
        let env = inputs.env_file(Arch::X86_64);
        let line = env.lines().next().unwrap();
        assert_eq!(line, r#"odd_version='1'\''; touch /tmp/pwned; '\'''"#);
    }

    #[test]
    fn an_input_missing_a_pin_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("inputs.json"),
            r#"{"ttyd": {"version": "1.7.7", "url": "https://e/t"}}"#,
        )
        .unwrap();
        let error = Inputs::open(dir.path()).unwrap_err();
        assert!(
            format!("{error:#}").contains("ttyd"),
            "the error should name the input: {error:#}"
        );
    }

    #[test]
    fn bumper_metadata_beside_a_pin_is_ignored() {
        let inputs = inputs(
            r#"{"ttyd": {"version": "1", "url": "u", "sha256": "s",
                         "source": {"datasource": "github-releases"},
                         "retrieved_at": "2026-09-09T00:00:00Z"}}"#,
        );
        assert_eq!(inputs.get("ttyd").unwrap().version, "1");
    }

    fn image_dir(image_json: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("image.json"), image_json).unwrap();
        fs::write(dir.path().join("provision.sh"), "#!/bin/sh\n").unwrap();
        dir
    }

    #[test]
    fn a_resolved_directory_is_named_by_its_file() {
        let dir = image_dir(
            r#"{"name": "ubuntu/webide", "title": "T", "arch": "x86_64", "type": "disk"}"#,
        );
        let image = ImageDir::open(dir.path()).unwrap();
        assert_eq!(image.name, "ubuntu/webide");
    }

    #[test]
    fn an_image_without_a_name_is_refused() {
        // Rather than being named after its directory: that made an image's
        // identity depend on what the checkout was called.
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("image.json"),
            r#"{"title": "T", "arch": "x86_64", "type": "disk"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("provision.sh"), "#!/bin/sh\n").unwrap();
        let error = ImageDir::open(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("name"), "{error:#}");
    }

    #[test]
    fn a_description_file_supersedes_the_inline_field() {
        let dir = image_dir(
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk", "description": "inline"}"#,
        );
        fs::write(dir.path().join("description.txt"), "from the file.\n").unwrap();
        let image = ImageDir::open(dir.path()).unwrap();
        assert_eq!(image.description.as_deref(), Some("from the file."));
    }

    #[test]
    fn an_image_without_a_base_is_a_root() {
        let root = image_dir(
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk", "base": ""}"#,
        );
        assert!(ImageDir::open(root.path()).unwrap().is_root());
        let derived = image_dir(
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk", "base": "ubuntu"}"#,
        );
        assert!(!ImageDir::open(derived.path()).unwrap().is_root());
    }

    #[test]
    fn an_sd_image_has_two_volumes_and_a_disk_one() {
        let sd = image_dir(r#"{"name": "n", "title": "T", "arch": "aarch64", "type": "sd"}"#);
        assert_eq!(
            ImageDir::open(sd.path()).unwrap().roles(),
            ["rootfs", "bootfs"]
        );
        let disk = image_dir(r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk"}"#);
        assert_eq!(ImageDir::open(disk.path()).unwrap().roles(), ["disk"]);
    }

    #[test]
    fn a_directory_without_provisioning_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("image.json"),
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk"}"#,
        )
        .unwrap();
        let error = ImageDir::open(dir.path()).unwrap_err();
        assert!(format!("{error:#}").contains("provision.sh"), "{error:#}");
    }
}

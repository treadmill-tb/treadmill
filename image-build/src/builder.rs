//! Building an image, and the stack beneath it.
//!
//! [`build_link`] builds exactly one image onto a lower that is already
//! there. [`build_stack`] walks a coordinate down to its host kind's root
//! image and builds whatever the cache does not already hold. CI uses the
//! first — it builds one link per job and passes layouts between them — and a
//! person building an image locally wants the second.
//!
//! The shape of a link build is: get a chain to build on (fetch a vendor
//! image, or relink the lower's blobs), put a writable overlay on it, attach
//! it, provision into it, then turn whatever changed into a layer.
//!
//! Two decisions in here are worth knowing about before reading the code.
//! **The host is given back before any layer is finalized** — file systems
//! have to be quiesced and devices detached before the overlay is a sound
//! thing to read. And **a volume that did not change gets no layer at all**,
//! because an empty delta is byte-identical to every other empty delta and
//! the same digest turning up in unrelated images is a miserable thing to
//! debug.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail, ensure};
use sha2::{Digest as _, Sha256};
use treadmill_rs::image::annotations::Role;

use crate::append::{self, AppendParams};
use crate::assemble::{self, AssembleParams};
use crate::cleanup;
use crate::exec::{self, Privilege};
use crate::layer_arg::{ChainArg, LayerArg, LayerFormatArg};
use crate::layout::{self, Layout};
use crate::mount;
use crate::nbd;
use crate::nspawn::{self, Provisioning};
use crate::qemu;
use crate::registry;
use crate::spec::{self, Annotations, ImageDir, ImageType};
use crate::tree;
use crate::verify::{self, DEFAULT_MAX_HEAD_VIRTUAL_SIZE, VerifyParams};

/// What an image is built on top of.
#[derive(Debug, Clone, Default)]
pub enum Lower {
    /// Nothing. The image fetches its own vendor image.
    #[default]
    None,
    /// A layout already on disk.
    Layout(PathBuf),
    /// An image in a registry, pulled first.
    Reference { reference: String, insecure: bool },
}

/// One link to build.
#[derive(Debug, Clone)]
pub struct BuildRequest {
    /// A resolved image directory — see [`crate::resolve`].
    pub image_dir: PathBuf,
    /// Where to leave the finished layout.
    pub out: PathBuf,
    pub lower: Lower,
    /// Executables to install into the image, as `$TML_PAYLOAD_DIR`.
    pub payloads: Vec<PathBuf>,
    /// Where to put the working directory. Defaults to `$RUNNER_TEMP`, then
    /// the system temporary directory.
    pub work_parent: Option<PathBuf>,
    pub annotations: Annotations,
}

/// One volume of the image as it is being built.
struct Volume {
    role: String,
    /// The vendor layer, for a root image. Ships as the chain's base.
    layer0: Option<PathBuf>,
    /// The relinked head this build's overlay sits on.
    head: PathBuf,
    /// How many layers the chain has, before this build adds one.
    chain_len: usize,
    overlay: PathBuf,
    device: Option<PathBuf>,
}

/// Build one image onto `request.lower`.
pub fn build_link(request: &BuildRequest) -> Result<()> {
    let image = ImageDir::open(&request.image_dir)?;

    // Before the environment checks: a mistake in the request is the caller's
    // to fix now, and should not be hidden behind a tool they would only need
    // once the request made sense.
    match (&request.lower, image.is_root()) {
        (Lower::None, false) => bail!(
            "{} has a base, so it needs a lower layout or a registry reference",
            image.name,
        ),
        (Lower::Layout(_) | Lower::Reference { .. }, true) => bail!(
            "{} has no base: it fetches its own vendor image",
            image.name,
        ),
        _ => {}
    }

    exec::require_tools(spec::REQUIRED_TOOLS)?;
    spec::require_binfmt(image.spec.arch)?;

    let privilege = Privilege::detect();

    let work_parent = request
        .work_parent
        .clone()
        .or_else(|| std::env::var_os("RUNNER_TEMP").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir);
    fs::create_dir_all(&work_parent)
        .with_context(|| format!("create {}", work_parent.display()))?;
    // Declared before the teardown guard so that it drops *after* it: the
    // mounts live inside this directory, and removing it first would leave
    // them attached.
    let work_dir = tempfile::Builder::new()
        .prefix("tml-build-")
        .tempdir_in(&work_parent)
        .context("create a working directory")?;
    let work = work_dir.path().to_path_buf();

    let teardown = cleanup::shared(privilege);
    cleanup::tear_down_on_signal(Arc::clone(&teardown))?;
    let _guard = cleanup::Guard::new(Arc::clone(&teardown));
    // Registered as well as owned by `work_dir`, so an interrupted build
    // clears it too. Removing it twice is not an error.
    teardown.lock().expect("teardown lock").created(&work);

    let roles: Vec<String> = image.roles().iter().map(|r| r.to_string()).collect();
    let partitioned = image.spec.image_type.partitioned();

    eprintln!(
        "building {} ({}, {}) -> {}",
        image.name,
        image.spec.image_type,
        image.spec.arch,
        request.out.display(),
    );

    // -- a chain to build on --------------------------------------------

    let mut lower_layout: Option<PathBuf> = None;
    let mut volumes: Vec<Volume> = Vec::new();

    if image.is_root() {
        let vendor = image.vendor_image()?;
        eprintln!("fetching the vendor image...");
        let download = work.join("vendor.download");
        fetch(&vendor.url, &vendor.sha256, &download)?;

        for (role, layer0) in vendor_layers(&image, &work, &download, &roles)? {
            let head = qemu::link_chain(&work, &role, std::slice::from_ref(&layer0))?;
            volumes.push(Volume {
                role,
                layer0: Some(layer0),
                head,
                chain_len: 1,
                overlay: PathBuf::new(),
                device: None,
            });
        }
    } else {
        let layout_dir = match &request.lower {
            Lower::Layout(path) => path.clone(),
            Lower::Reference {
                reference,
                insecure,
            } => {
                eprintln!("copying {reference} into a local layout...");
                let dir = work.join("lower");
                registry::pull(reference, &dir, *insecure)?;
                dir
            }
            Lower::None => unreachable!("checked above"),
        };
        // The lower is what this build's layers will be stacked on and
        // carried through by digest; a bad one poisons everything after.
        verify::verify(&VerifyParams::new(&layout_dir))
            .with_context(|| format!("the lower image of {}", image.name))?;

        let layout = Layout::open(&layout_dir)?;
        let lower_image = layout::read_image(&layout)?;
        for role in &roles {
            let chain = lower_image
                .chain(role)
                .with_context(|| format!("the lower image has no usable {role} chain"))?;
            let blobs: Vec<PathBuf> = chain
                .layers()
                .iter()
                .map(|layer| layout.blob_path(&layer.digest))
                .collect();
            let head = qemu::link_chain(&work, role, &blobs)?;
            volumes.push(Volume {
                role: role.clone(),
                layer0: None,
                head,
                chain_len: blobs.len(),
                overlay: PathBuf::new(),
                device: None,
            });
        }
        lower_layout = Some(layout_dir);
    }

    // -- a writable overlay on each ------------------------------------

    for volume in &mut volumes {
        volume.overlay = work.join(format!("{}.overlay.qcow2", volume.role));
        qemu::create_overlay(&volume.overlay, &volume.head)?;
    }
    if let Some(grow) = image.grow_bytes()? {
        eprintln!("growing {} by {grow} bytes", volumes[0].role);
        qemu::grow_overlay(&volumes[0].overlay, grow)?;
    }

    // -- attach, and make it a file system ------------------------------

    for (index, volume) in volumes.iter_mut().enumerate() {
        // Only the primary carries a partition table worth waiting for.
        let wants_partitions = index == 0 && partitioned;
        let attachment = nbd::attach(privilege, &work, &volume.overlay, wants_partitions)?;
        teardown
            .lock()
            .expect("teardown lock")
            .attached(&attachment.device, &attachment.pid_file);
        volume.device = Some(attachment.device.clone());
    }
    let root_device = volumes[0]
        .device
        .clone()
        .expect("the primary volume was just attached");

    if image.grow_bytes()?.is_some() {
        mount::resize_root(privilege, &root_device, partitioned)?;
    }

    let mount_root = work.join("root");
    fs::create_dir_all(&mount_root).with_context(|| format!("create {}", mount_root.display()))?;
    mount::mount(
        privilege,
        &mount::root_partition(&root_device, partitioned),
        &mount_root,
        &[],
    )?;
    teardown.lock().expect("teardown lock").mounted(&mount_root);

    if partitioned {
        mount::mount_fstab(privilege, &root_device, &mount_root)?;
    } else {
        let bootfs = volumes
            .iter()
            .find(|volume| volume.role == "bootfs")
            .and_then(|volume| volume.device.clone())
            .context("an sd image needs a bootfs volume")?;
        // noatime is load-bearing: without it, reading a file during
        // provisioning dirties the file system and every image ships a
        // spurious boot delta.
        mount::mount(
            privilege,
            &bootfs,
            &mount_root.join("boot/firmware"),
            &["-t", "vfat", "-o", "noatime"],
        )?;
    }

    // -- provision ------------------------------------------------------

    let device_links = if partitioned {
        let dir = work.join("dev-links");
        mount::device_links(privilege, &root_device, &dir)?;
        Some(dir)
    } else {
        None
    };
    let binds = nspawn::binds(&root_device, partitioned, device_links.as_deref())?;
    let inputs_env = image.inputs.env_file(image.spec.arch);
    nspawn::provision(
        privilege,
        &mount_root,
        &binds,
        &work,
        &Provisioning {
            image_dir: &image.path,
            payloads: &request.payloads,
            inputs_env: &inputs_env,
            arch: image.spec.arch,
            image_type: image.spec.image_type,
        },
    )?;

    mount::trim(privilege, &mount_root)?;

    // Give the host back before reading the overlays: a layer finalized from
    // a live file system is a layer finalized from a moving target.
    let failures = teardown.lock().expect("teardown lock").release_host_state();
    if let Some(first) = failures.into_iter().next() {
        return Err(first).context("releasing the image's devices");
    }

    // -- turn what changed into layers ----------------------------------

    let mut layers: Vec<LayerArg> = Vec::new();
    let mut chains: Vec<ChainArg> = Vec::new();
    for volume in &mut volumes {
        let role =
            Role::from_str(&volume.role).map_err(|e| anyhow::anyhow!("{}: {e}", volume.role))?;

        if let Some(layer0) = &volume.layer0 {
            layers.push(LayerArg {
                role: role.clone(),
                format: LayerFormatArg::Qcow2,
                path: layer0.clone(),
            });
        }

        if qemu::volume_unchanged(&volume.overlay, &volume.head)? {
            eprintln!("{} is unchanged, adding no layer to it", volume.role);
        } else {
            let delta = work.join(format!("{}.delta.qcow2", volume.role));
            qemu::finalize_delta(&volume.overlay, &volume.head, &delta)?;
            layers.push(LayerArg {
                role: role.clone(),
                format: LayerFormatArg::Qcow2,
                path: delta,
            });
            volume.chain_len += 1;
        }
        let _ = fs::remove_file(&volume.overlay);
        chains.push(ChainArg {
            role,
            layers: volume.chain_len,
        });
    }

    // -- write the layout -----------------------------------------------

    let version = request
        .annotations
        .version
        .clone()
        .or_else(|| image.spec.version.clone());
    let title = image.spec.title.clone();

    if let Some(lower) = &lower_layout {
        append::append(&AppendParams {
            lower: lower.clone(),
            out: request.out.clone(),
            layers,
            title: Some(title.clone()),
            version,
            description: image.description.clone(),
            created: request.annotations.created.clone(),
            revision: request.annotations.revision.clone(),
            documentation: request.annotations.documentation.clone(),
            base_name: match &request.lower {
                Lower::Reference { reference, .. } => Some(reference.clone()),
                _ => None,
            },
        })?;
    } else {
        assemble::assemble(&AssembleParams {
            out: request.out.clone(),
            title: title.clone(),
            version,
            description: image.description.clone(),
            created: request.annotations.created.clone(),
            revision: request.annotations.revision.clone(),
            documentation: request.annotations.documentation.clone(),
            layers,
        })?;
    }

    verify::verify(&VerifyParams {
        layout: request.out.clone(),
        chains,
        title: Some(title),
        max_head_virtual_size: DEFAULT_MAX_HEAD_VIRTUAL_SIZE,
    })
    .with_context(|| format!("the image built for {}", image.name))?;

    eprintln!("{} -> {} (OK)", image.name, request.out.display());
    Ok(())
}

/// Turn a downloaded vendor image into one base blob per volume.
fn vendor_layers(
    image: &ImageDir,
    work: &Path,
    download: &Path,
    roles: &[String],
) -> Result<Vec<(String, PathBuf)>> {
    match image.spec.image_type {
        ImageType::Disk => {
            let layer0 = work.join("disk.layer0.qcow2");
            qemu::convert_whole(download, &layer0)?;
            let _ = fs::remove_file(download);
            Ok(vec![(roles[0].clone(), layer0)])
        }
        ImageType::Sd => {
            // An SD card image ships as one xz-compressed raw card, whose two
            // file systems become two independently netbooted volumes.
            let raw = work.join("sd.img");
            decompress_xz(download, &raw)?;
            let _ = fs::remove_file(download);

            let bootfs = work.join("bootfs.layer0.qcow2");
            qemu::convert_partition(&raw, 1, &bootfs)?;
            let rootfs = work.join("rootfs.layer0.qcow2");
            qemu::convert_partition(&raw, 2, &rootfs)?;
            let _ = fs::remove_file(&raw);

            // In the order `roles` gives, so the primary stays first.
            let mut layers = Vec::new();
            for role in roles {
                let path = match role.as_str() {
                    "rootfs" => rootfs.clone(),
                    "bootfs" => bootfs.clone(),
                    other => bail!("an sd image has no {other} volume"),
                };
                layers.push((role.clone(), path));
            }
            Ok(layers)
        }
    }
}

/// Download `url` to `dest`, checking it against `sha256` as it arrives.
fn fetch(url: &str, sha256: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .build()
        .context("build an HTTP client")?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;

    let mut file = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = std::io::Read::read(&mut response, &mut buffer)
            .with_context(|| format!("read from {url}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .with_context(|| format!("write {}", dest.display()))?;
    }
    file.flush()
        .with_context(|| format!("write {}", dest.display()))?;

    let actual = hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    ensure!(
        actual == sha256.trim().to_ascii_lowercase(),
        "{url} hashes to {actual} but is pinned at {sha256}",
    );
    Ok(())
}

/// Decompress an xz file.
fn decompress_xz(source: &Path, dest: &Path) -> Result<()> {
    let out = fs::File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    exec::run(
        std::process::Command::new("xz")
            .args(["-dc"])
            .arg(source)
            .stdout(std::process::Stdio::from(out)),
    )
    .with_context(|| format!("decompress {}", source.display()))
}

/// What to build, and what to reuse.
#[derive(Debug, Clone)]
pub struct ChainRequest {
    /// The image to build. Everything its `base` chain names is built first.
    pub image_dir: PathBuf,
    /// Where to leave the finished head layout.
    pub out: PathBuf,
    /// Where built links are kept between runs.
    pub cache: PathBuf,
    /// Rebuild every link, even one the cache already holds.
    pub rebuild: bool,
    pub payloads: Vec<PathBuf>,
    pub work_parent: Option<PathBuf>,
    pub annotations: Annotations,
    /// Talk plain HTTP to a registry the chain's base names.
    pub insecure_lower_ref: bool,
}

/// Build an image and everything under it, reusing what the cache holds.
pub fn build_chain(request: &ChainRequest) -> Result<()> {
    let chain = tree::chain(&request.image_dir)?;
    fs::create_dir_all(&request.cache)
        .with_context(|| format!("create {}", request.cache.display()))?;

    let mut lower: Option<PathBuf> = None;
    for (index, link) in chain.links.iter().enumerate() {
        let layout = request.cache.join(cache_key(&link.name));

        if !request.rebuild && layout.join("index.json").is_file() {
            eprintln!("reusing {}", layout.display());
            lower = Some(layout);
            continue;
        }
        if layout.exists() {
            fs::remove_dir_all(&layout).with_context(|| format!("clear {}", layout.display()))?;
        }

        // Only the first link can be built on a registry reference; every
        // later one is built on what this loop just produced.
        let lower_for_link = match (&lower, index == 0, &chain.lower_ref) {
            (Some(path), _, _) => Lower::Layout(path.clone()),
            (None, true, Some(reference)) => Lower::Reference {
                reference: reference.clone(),
                insecure: request.insecure_lower_ref,
            },
            _ => Lower::None,
        };

        build_link(&BuildRequest {
            image_dir: link.path.clone(),
            out: layout.clone(),
            lower: lower_for_link,
            payloads: request.payloads.clone(),
            work_parent: request.work_parent.clone(),
            annotations: request.annotations.clone(),
        })?;
        lower = Some(layout);
    }

    let head = lower.context("a chain is never empty")?;
    if request.out.exists() {
        fs::remove_dir_all(&request.out)
            .with_context(|| format!("clear {}", request.out.display()))?;
    }
    if let Some(parent) = request.out.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    copy_tree(&head, &request.out)?;
    Ok(())
}

/// Where a built link is kept in the cache.
///
/// Derived from the image's stated name rather than its directory, so moving
/// a definition does not orphan what was built from it.
fn cache_key(name: &str) -> String {
    name.replace(['/', '\\'], "--")
}

/// Copy a layout directory.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for entry in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let entry = entry.with_context(|| format!("read {}", from.display()))?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_tree(&source, &target)?;
        } else {
            fs::copy(&source, &target)
                .with_context(|| format!("copy {} to {}", source.display(), target.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_image_refuses_a_lower() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("image.json"),
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("provision.sh"), "#!/bin/sh\n").unwrap();

        let request = BuildRequest {
            image_dir: dir.path().to_path_buf(),
            out: dir.path().join("out"),
            lower: Lower::Layout(PathBuf::from("/nonexistent")),
            payloads: Vec::new(),
            work_parent: None,
            annotations: Annotations::default(),
        };
        let error = build_link(&request).unwrap_err();
        assert!(
            format!("{error:#}").contains("no base"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn an_image_with_a_base_needs_one() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("image.json"),
            r#"{"name": "n", "title": "T", "arch": "x86_64", "type": "disk", "base": "ubuntu"}"#,
        )
        .unwrap();
        fs::write(dir.path().join("provision.sh"), "#!/bin/sh\n").unwrap();

        let request = BuildRequest {
            image_dir: dir.path().to_path_buf(),
            out: dir.path().join("out"),
            lower: Lower::None,
            payloads: Vec::new(),
            work_parent: None,
            annotations: Annotations::default(),
        };
        let error = build_link(&request).unwrap_err();
        assert!(
            format!("{error:#}").contains("has a base"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn a_download_is_checked_against_its_pin() {
        // No network: an unreachable URL still has to fail rather than
        // silently produce an unverified file.
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download");
        let error = fetch("http://127.0.0.1:1/nothing", &"0".repeat(64), &dest).unwrap_err();
        assert!(format!("{error:#}").contains("127.0.0.1:1"), "{error:#}");
    }

    #[test]
    fn copying_a_layout_reproduces_its_tree() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("from");
        fs::create_dir_all(from.join("blobs/sha256")).unwrap();
        fs::write(from.join("index.json"), "{}").unwrap();
        fs::write(from.join("blobs/sha256/abc"), b"blob").unwrap();

        let to = dir.path().join("to");
        copy_tree(&from, &to).unwrap();
        assert_eq!(fs::read(to.join("blobs/sha256/abc")).unwrap(), b"blob");
        assert_eq!(fs::read_to_string(to.join("index.json")).unwrap(), "{}");
    }
}

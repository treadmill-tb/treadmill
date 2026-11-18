//! `tml image`: building, pushing and publishing images.
//!
//! The work lives in [`treadmill_image_build`] (what an image is, and how to
//! build one) and [`treadmill_client::images`] (what the switchboard has to be
//! told about it). What is here is the command surface over them, and the
//! presentation `tml` gives everything else.
//!
//! A build is long and synchronous, and reaches a registry through a runtime
//! of its own, so it runs on a blocking thread rather than on this one.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use treadmill_client::images;
use treadmill_image_build::builder::{self, BuildRequest, ChainRequest, Lower};
use treadmill_image_build::registry::{self, PushRecord, Target};
use treadmill_image_build::spec::{Annotations, ImageDir};
use treadmill_image_build::tree;
use treadmill_image_build::verify::{self, DEFAULT_MAX_HEAD_VIRTUAL_SIZE, VerifyParams};
use treadmill_rs::api::switchboard::images::GenerationMemberSpec;
use treadmill_rs::image::Digest;

use crate::cli::{
    ImageBuildArgs, ImageCommand, ImagePushArgs, ImageTreeArgs, ImageVerifyArgs, OutputFormat,
};
use crate::ctx::Ctx;
use crate::event::Event;
use crate::tui;

mod publish;

pub async fn run(ctx: &Ctx, command: &ImageCommand) -> Result<()> {
    match command {
        ImageCommand::List(args) => list(ctx, args),
        ImageCommand::Check(args) => check(ctx, args),
        ImageCommand::Build(args) => build(ctx, args).await,
        ImageCommand::Push(args) => push(ctx, args).await,
        ImageCommand::Register(args) => publish::register(ctx, args).await,
        ImageCommand::Publish(args) => publish::publish(ctx, args).await,
        ImageCommand::Verify(args) => check_layout(ctx, args),
    }
}

/// One image, as `-o json` prints it.
#[derive(serde::Serialize)]
struct Listed {
    name: String,
    title: String,
    path: String,
    level: usize,
}

/// Every image under the given directory, with how deep its chain runs.
fn listed(dir: &Path) -> Result<Vec<Listed>> {
    tree::find(dir)?
        .into_iter()
        .map(|path| {
            let image = ImageDir::open(&path)?;
            Ok(Listed {
                name: image.name,
                title: image.spec.title,
                path: path
                    .strip_prefix(dir)
                    .unwrap_or(&path)
                    .display()
                    .to_string(),
                level: tree::level(&path)?,
            })
        })
        .collect()
}

fn list(ctx: &Ctx, args: &ImageTreeArgs) -> Result<()> {
    let images = listed(&args.dir)?;

    match ctx.output {
        // A listing prints as one array under `-o json`, and as a message per
        // image under `-o jsonl`, like every other listing here.
        OutputFormat::Json => {
            anstream::println!("{}", serde_json::to_string_pretty(&images)?);
        }
        OutputFormat::Jsonl => {
            for image in &images {
                ctx.event(Event::Image {
                    name: &image.name,
                    title: &image.title,
                    path: &image.path,
                    level: image.level,
                });
            }
        }
        // Nothing to list is worth saying out loud: an empty tree and a
        // misaimed directory look identical otherwise.
        OutputFormat::Human if images.is_empty() => {
            ctx.note(&format!("No images under {}", args.dir.display()));
        }
        OutputFormat::Human => {
            let lines: Vec<_> = images
                .iter()
                .map(|image| {
                    // Indented by depth, so a chain reads as one.
                    let indent = "  ".repeat(image.level);
                    vec![
                        tui::plain(format!("{indent}{}", image.name)),
                        tui::span(tui::DIM, format!("  {}", image.title)),
                    ]
                })
                .collect();
            crate::listing::print(&lines);
        }
    }
    Ok(())
}

fn check(ctx: &Ctx, args: &ImageTreeArgs) -> Result<()> {
    let dirs = tree::check(&args.dir)?;
    ok(
        ctx,
        &format!(
            "{} image{} OK",
            dirs.len(),
            if dirs.len() == 1 { "" } else { "s" },
        ),
    );
    ctx.result(Event::ImagesChecked { images: dirs.len() });
    Ok(())
}

fn check_layout(ctx: &Ctx, args: &ImageVerifyArgs) -> Result<()> {
    verify::verify(&VerifyParams {
        layout: args.layout.clone(),
        chains: args.chains.clone(),
        title: args.title.clone(),
        max_head_virtual_size: args
            .max_head_virtual_size
            .unwrap_or(DEFAULT_MAX_HEAD_VIRTUAL_SIZE),
    })?;
    ok(ctx, &format!("{} verified", args.layout.display()));
    Ok(())
}

/// The lower to build onto, when the caller names one.
///
/// Without one, the chain's own `base` decides, which is the usual case.
fn requested_lower(args: &ImageBuildArgs) -> Option<Lower> {
    if let Some(path) = &args.lower {
        return Some(Lower::Layout(path.clone()));
    }
    args.lower_ref.as_ref().map(|reference| Lower::Reference {
        reference: reference.clone(),
        insecure: args.insecure_lower_ref,
    })
}

async fn build(ctx: &Ctx, args: &ImageBuildArgs) -> Result<()> {
    // Read it here so a malformed directory is reported before any work, and
    // so the messages can name the image rather than the path.
    let image = ImageDir::open(&args.image)?;
    let name = image.name.clone();
    let title = image.spec.title.clone();
    let out = args.out.clone();
    let annotations = Annotations::from_env();

    let single = requested_lower(args);
    let cache = args
        .cache
        .clone()
        .or_else(|| std::env::var_os("TML_IMAGE_CACHE").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".image-cache"));

    let request = match single {
        // One link, onto what the caller supplied.
        Some(lower) => Build::Link(BuildRequest {
            image_dir: args.image.clone(),
            out: out.clone(),
            lower,
            payloads: args.payloads.clone(),
            work_parent: args.work.clone(),
            annotations,
        }),
        // The whole chain its `base` names.
        None => Build::Chain(ChainRequest {
            image_dir: args.image.clone(),
            out: out.clone(),
            cache,
            rebuild: args.rebuild,
            payloads: args.payloads.clone(),
            work_parent: args.work.clone(),
            annotations,
            insecure_lower_ref: args.insecure_lower_ref,
        }),
    };

    ctx.note(&format!("Building {name}"));

    // The build blocks for a long time and spins up its own runtime for the
    // registry, so it must not run on this one.
    tokio::task::spawn_blocking(move || match request {
        Build::Link(request) => builder::build_link(&request),
        Build::Chain(request) => builder::build_chain(&request),
    })
    .await
    .context("the build thread panicked")??;

    ok(ctx, &format!("Built {name} · {}", out.display()));
    ctx.result(Event::ImageBuilt {
        name: &name,
        title: &title,
        layout: &out.display().to_string(),
    });
    Ok(())
}

/// Which of the two shapes a build takes.
enum Build {
    Link(BuildRequest),
    Chain(ChainRequest),
}

async fn push(ctx: &Ctx, args: &ImagePushArgs) -> Result<()> {
    let (registry_host, repository) = args
        .repository
        .split_once('/')
        .filter(|(host, repo)| !host.is_empty() && !repo.is_empty())
        .with_context(|| {
            format!(
                "expected <registry>/<repository>, got {:?}",
                args.repository,
            )
        })?;

    let target = Target {
        registry: registry_host.to_string(),
        repository: repository.to_string(),
        tags: args.tags.clone(),
        insecure: args.insecure,
    };
    let image = args.image.clone().unwrap_or_else(|| repository.to_string());
    let layout = args.layout.clone();

    ctx.note(&format!(
        "Pushing {} to {}",
        layout.display(),
        args.repository
    ));

    let record = tokio::task::spawn_blocking(move || registry::push(&layout, &image, &target))
        .await
        .context("the push thread panicked")??;

    if let Some(path) = &args.record {
        write_record(path, &record)?;
    }

    ok(
        ctx,
        &format!(
            "Pushed {}@{} · {} tag{}",
            record.repository,
            record.digest,
            args.tags.len(),
            if args.tags.len() == 1 { "" } else { "s" },
        ),
    );
    ctx.result(Event::ImagePushed {
        image: &record.image,
        registry: &record.registry,
        repository: &record.repository,
        digest: record.digest,
        tags: &args.tags,
    });
    Ok(())
}

/// Write a push record where `tml image publish` will find it.
fn write_record(path: &Path, record: &PushRecord) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let json = serde_json::to_vec_pretty(record).context("serialize the push record")?;
    std::fs::write(path, json).with_context(|| format!("write {}", path.display()))
}

/// Every push record under `dir`, which may be a tree of them: downloading CI
/// artifacts puts each in a subdirectory of its own.
pub fn read_records(dir: &Path) -> Result<Vec<PushRecord>> {
    let mut records = Vec::new();
    collect_records(dir, &mut records)?;
    records.sort_by(|a, b| a.image.cmp(&b.image));
    Ok(records)
}

fn collect_records(dir: &Path, into: &mut Vec<PushRecord>) -> Result<()> {
    if !dir.is_dir() {
        bail!("no such directory: {}", dir.display());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_records(&path, into)?;
            continue;
        }
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let bytes = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let record: PushRecord = serde_json::from_slice(&bytes)
            .with_context(|| format!("{} is not a push record", path.display()))?;
        into.push(record);
    }
    Ok(())
}

/// The seconds a `--timeout` asks for, or the policy's own default.
pub fn timeout(seconds: Option<u64>) -> Duration {
    seconds
        .map(Duration::from_secs)
        .unwrap_or(images::DEFAULT_REGISTER_TIMEOUT)
}

/// `✓ <what>`, the shape every `tml` command reports success in.
pub fn ok(ctx: &Ctx, message: &str) {
    if !ctx.human() {
        return;
    }
    let style = tui::OK;
    anstream::eprintln!("{style}✓{style:#} {message}");
}

/// A digest as the catalog spells it.
pub fn parse_digest(text: &str) -> Result<Digest> {
    text.parse()
        .with_context(|| format!("{text:?} is not a sha256 digest"))
}

/// One member a set should have after publishing.
pub struct Member {
    pub image: String,
    pub spec: GenerationMemberSpec,
}

impl Member {
    /// Describe the member for a message, by what it is rather than by a
    /// digest nobody can read.
    pub fn label(&self) -> String {
        format!("{} ({})", self.image, self.spec.platform_profile)
    }
}

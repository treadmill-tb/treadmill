mod append;
mod chain;
mod layer_arg;
mod layout;
mod verify;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Context;
use clap::{Parser, Subcommand};

use treadmill_rs::image::assemble::{self, ImageMeta, LayerSpec};

use crate::append::{AppendArgs, store_layer};
use crate::layer_arg::LayerArg;
use crate::layout::Layout;
use crate::verify::VerifyArgs;

#[derive(Debug, Parser)]
#[command(name = "image-util")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Assemble blob files into a validated Treadmill OCI image layout.
    Assemble(AssembleArgs),
    /// Append layers onto an existing layout, carrying its layers through.
    Append(AppendArgs),
    /// Check a built layout's structural invariants.
    Verify(VerifyArgs),
}

/// Assemble an ordered list of layer blobs into an OCI layout directory.
#[derive(Debug, Parser)]
struct AssembleArgs {
    /// Output OCI layout directory (holds `oci-layout`, `index.json`, `blobs/`;
    /// created if absent).
    #[arg(short = 'o', long)]
    out: PathBuf,

    /// `org.opencontainers.image.title`.
    #[arg(long)]
    title: String,

    /// `org.opencontainers.image.version` (optional).
    #[arg(long)]
    version: Option<String>,

    /// `org.opencontainers.image.description` (optional).
    #[arg(long)]
    description: Option<String>,

    /// A layer blob as `ROLE=PATH` (ROLE is `root` or `boot`). Repeatable, and
    /// ORDER IS SIGNIFICANT — it defines the backing chain (see
    /// `treadmill_rs::image::assemble`).
    #[arg(long = "layer", value_name = "ROLE=PATH", required = true)]
    layers: Vec<LayerArg>,

    /// Human-readable label used in messages (defaults to the title).
    #[arg(long)]
    name: Option<String>,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Assemble(args) => {
            let what = args.name.clone().unwrap_or_else(|| args.title.clone());
            finish(&what, assemble_layout(&args))
        }
        Command::Append(args) => finish(&args.label(), append::append(&args)),
        Command::Verify(args) => finish(&args.label(), verify::verify(&args)),
    }
}

fn finish(what: &str, result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => {
            println!("{what}: OK");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{what}: FAILED: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn assemble_layout(args: &AssembleArgs) -> anyhow::Result<()> {
    let layout = Layout::create(&args.out)?;

    let mut specs: Vec<LayerSpec> = Vec::with_capacity(args.layers.len());
    for layer in &args.layers {
        specs.push(store_layer(&layout, layer.role, &layer.path)?);
    }

    let meta = ImageMeta {
        title: Some(args.title.clone()),
        version: args.version.clone(),
        description: args.description.clone(),
        base_name: None,
    };
    let manifest = assemble::build_manifest(&specs, &meta).context("assemble manifest")?;
    layout.write_manifest(&manifest)?;

    Ok(())
}

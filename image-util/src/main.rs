//! The producer-side CLI over [`treadmill_image_build`].
//!
//! Everything here is argument parsing and reporting: the work is the
//! library's. `tml image` is the user-facing front-end over the same calls.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use treadmill_image_build::append::{self, AppendParams};
use treadmill_image_build::assemble::{self, AssembleParams};
use treadmill_image_build::layer_arg::{ChainArg, LayerArg};
use treadmill_image_build::verify::{self, DEFAULT_MAX_HEAD_VIRTUAL_SIZE, VerifyParams};

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

/// Assemble layer blobs into an OCI layout directory.
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

    /// `org.opencontainers.image.created` (RFC 3339 timestamp).
    #[arg(long)]
    created: Option<String>,

    /// `org.opencontainers.image.revision` (source commit).
    #[arg(long)]
    revision: Option<String>,

    /// `org.opencontainers.image.documentation` (documentation URL).
    #[arg(long)]
    documentation: Option<String>,

    /// A layer blob as `ROLE=FORMAT:PATH`, placed on top of ROLE's chain, where
    /// FORMAT is `qcow2` or `raw`. Repeatable: the first blob of a role is its
    /// chain's base, and each later one backs onto the one before it. Only the
    /// base of a chain may be raw.
    #[arg(long = "layer", value_name = "ROLE=FORMAT:PATH", required = true)]
    layers: Vec<LayerArg>,

    /// Human-readable label used in messages (defaults to the title).
    #[arg(long)]
    name: Option<String>,
}

/// Append layers on top of the chains of an existing image layout.
#[derive(Debug, Parser)]
struct AppendArgs {
    /// Lower OCI image layout whose layers are carried through by digest.
    #[arg(long)]
    lower: PathBuf,

    /// Output OCI layout directory (may be the lower layout, appended in place).
    #[arg(short = 'o', long)]
    out: PathBuf,

    /// A layer blob as `ROLE=FORMAT:PATH`, where FORMAT is `qcow2` or `raw`,
    /// placed on top of ROLE's chain (which only a qcow2 blob can go on), or
    /// starting it if the lower has no such role.
    /// Repeatable: each later blob of a role backs onto the one before it.
    /// Without any, the lower's layers are carried through under the new
    /// metadata.
    #[arg(long = "layer", value_name = "ROLE=FORMAT:PATH")]
    layers: Vec<LayerArg>,

    /// `org.opencontainers.image.title`; inherited from the lower if omitted.
    #[arg(long)]
    title: Option<String>,

    /// `org.opencontainers.image.version`; inherited from the lower if omitted.
    #[arg(long)]
    version: Option<String>,

    /// `org.opencontainers.image.description`; inherited from the lower if
    /// omitted.
    #[arg(long)]
    description: Option<String>,

    /// `org.opencontainers.image.created` (RFC 3339 timestamp).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    created: Option<String>,

    /// `org.opencontainers.image.revision` (source commit).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    revision: Option<String>,

    /// `org.opencontainers.image.documentation` (documentation URL).
    /// Inherited from the lower image on append if omitted.
    #[arg(long)]
    documentation: Option<String>,

    /// `org.opencontainers.image.base.name`; defaults to the lower layout's
    /// manifest digest.
    #[arg(long)]
    base_name: Option<String>,

    /// Human-readable label used in messages (defaults to the title).
    #[arg(long)]
    name: Option<String>,
}

/// Validate a built Treadmill OCI image layout.
#[derive(Debug, Parser)]
struct VerifyArgs {
    /// Path to the OCI image layout directory.
    layout: PathBuf,

    /// An expected chain as `ROLE=LAYERS`. Repeatable; when given, the image
    /// must provide exactly these roles, each with a chain of that many layers.
    #[arg(long = "chain", value_name = "ROLE=LAYERS")]
    chains: Vec<ChainArg>,

    /// Expected `org.opencontainers.image.title`; asserted only when given.
    #[arg(long)]
    title: Option<String>,

    /// Ceiling for the virtual size of every chain's head, in bytes. Must not
    /// exceed the smallest working disk of any supervisor the image may be
    /// scheduled on. Pass 0 to disable the check.
    #[arg(long, default_value_t = DEFAULT_MAX_HEAD_VIRTUAL_SIZE)]
    max_head_virtual_size: u64,

    /// Human-readable label used in messages (defaults to the layout path).
    #[arg(long)]
    name: Option<String>,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Assemble(args) => {
            let what = args.name.clone().unwrap_or_else(|| args.title.clone());
            finish(&what, assemble::assemble(&args.into_params()))
        }
        Command::Append(args) => finish(&args.label(), append::append(&args.into_params())),
        Command::Verify(args) => finish(&args.label(), verify::verify(&args.into_params())),
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

impl AssembleArgs {
    fn into_params(self) -> AssembleParams {
        AssembleParams {
            out: self.out,
            title: self.title,
            version: self.version,
            description: self.description,
            created: self.created,
            revision: self.revision,
            documentation: self.documentation,
            layers: self.layers,
        }
    }
}

impl AppendArgs {
    fn label(&self) -> String {
        self.name
            .clone()
            .or_else(|| self.title.clone())
            .unwrap_or_else(|| self.out.display().to_string())
    }

    fn into_params(self) -> AppendParams {
        AppendParams {
            lower: self.lower,
            out: self.out,
            layers: self.layers,
            title: self.title,
            version: self.version,
            description: self.description,
            created: self.created,
            revision: self.revision,
            documentation: self.documentation,
            base_name: self.base_name,
        }
    }
}

impl VerifyArgs {
    fn label(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| self.layout.display().to_string())
    }

    fn into_params(self) -> VerifyParams {
        VerifyParams {
            layout: self.layout,
            chains: self.chains,
            title: self.title,
            max_head_virtual_size: self.max_head_virtual_size,
        }
    }
}

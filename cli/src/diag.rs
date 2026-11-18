//! Rendering [`treadmill_client`]'s diagnostics as `tml`'s own.

use std::sync::Arc;

use treadmill_client::diagnostics::Diagnostics;

use crate::cli::OutputFormat;
use crate::event::{self, Event};

/// Warnings go to stderr on a styled `warning:` line, and into the `-o jsonl`
/// stream as a `warning` event, exactly as [`crate::ctx::Ctx::warn`] does.
/// Detail is stderr under `-v`, whatever the output format, and is not
/// silenced by `--quiet`.
pub struct Stderr {
    output: OutputFormat,
    verbose: u8,
}

impl Stderr {
    pub fn new(output: OutputFormat, verbose: u8) -> Arc<Self> {
        Arc::new(Self { output, verbose })
    }
}

impl Diagnostics for Stderr {
    fn warn(&self, message: &str) {
        event::emit(self.output, Event::Warning { message });
        let style = anstyle::AnsiColor::Yellow.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}warning:{style:#} {message}");
    }

    fn detail(&self, message: &str) {
        if self.verbose == 0 {
            return;
        }
        anstream::eprintln!("{message}");
    }
}

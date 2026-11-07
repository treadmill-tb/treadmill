use anyhow::Result;
use std::sync::Arc;
use treadmill_client::diagnostics::Diagnostics as _;
use treadmill_client::error::hinted;
// `crate::job` is `tml`'s own job commands; this is the library's.
use treadmill_client::job::{self as client_job, JobRef};
use treadmill_client::session::{Options, Session};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::cli::{ColorChoice, Globals, OutputFormat};
use crate::diag;
use crate::display::{program, short};
use crate::event::{self, Event};

/// A [`Session`] — which switchboard, as whom — together with how this
/// invocation was asked to present itself.
pub struct Ctx {
    pub session: Session,
    pub output: OutputFormat,
    pub quiet: bool,
    pub verbose: u8,
    diagnostics: Arc<diag::Stderr>,
}

impl Ctx {
    pub fn load(globals: &Globals) -> Result<Self> {
        let output = globals.output.unwrap_or(OutputFormat::Human);
        let diagnostics = diag::Stderr::new(output, globals.verbose);

        let session = Session::load(
            &Options {
                profile: globals.profile.clone(),
                config_path: globals.config.clone(),
                switchboard: globals.switchboard.clone(),
                insecure_tls: globals.insecure_tls,
            },
            diagnostics.clone(),
        )?;

        Ok(Self {
            session,
            output,
            quiet: globals.quiet,
            verbose: globals.verbose,
            diagnostics,
        })
    }

    pub fn client(&self) -> Result<SwitchboardClient> {
        self.session.client()
    }

    pub fn anonymous_client(&self) -> Result<SwitchboardClient> {
        self.session.anonymous_client()
    }

    /// As [`Session::authenticated_client`], with the remedy `tml` offers for
    /// having no usable session.
    pub fn authenticated_client(&self) -> Result<SwitchboardClient> {
        self.session
            .authenticated_client()
            .map_err(|error| hinted(error, format!("Run `{} login`.", program())))
    }

    pub fn human(&self) -> bool {
        self.output == OutputFormat::Human
    }

    pub fn interactive(&self) -> bool {
        self.human() && !self.quiet && crate::tui::interactive()
    }

    /// The `--job` flag a suggested command needs for `job_id`, by its short
    /// id: none for the active job.
    pub fn job_flag(&self, job_id: Uuid) -> String {
        if self.session.state.active_job == Some(job_id) {
            String::new()
        } else {
            format!(" --job {}", short(job_id))
        }
    }

    /// The job `reference` names, or the active job if it names none.
    ///
    /// Picking the active job is a selection the user did not make outright,
    /// so it is announced; a `^<hex>` suffix is resolved on the switchboard.
    pub async fn resolve_job(&self, reference: Option<&JobRef>) -> Result<Uuid> {
        if reference.is_none()
            && let Some(job) = self.session.state.active_job
        {
            self.note(&format!("Active job {}", short(job)));
        }
        client_job::resolve(&self.session, reference)
            .await
            .map_err(|error| {
                if reference.is_some() {
                    return error;
                }
                hinted(
                    error,
                    format!("Pass --job, or run `{} context job set <JOB>`.", program()),
                )
            })
    }

    /// A progress or selection notice: stderr, human output only.
    pub fn note(&self, message: &str) {
        if self.quiet || !self.human() {
            return;
        }
        anstream::eprintln!("{message}");
    }

    /// Diagnostic detail asked for with `-v`: stderr, whatever the output
    /// format, and not silenced by `--quiet`.
    pub fn detail(&self, message: &str) {
        self.diagnostics.detail(message);
    }

    pub fn event(&self, event: Event) {
        event::emit(self.output, event);
    }

    /// See [`event::result`].
    pub fn result(&self, event: Event) {
        event::result(self.output, event);
    }

    /// A hint on what to do next: stderr, human output only.
    pub fn hint(&self, message: &str) {
        if !self.human() || self.quiet {
            return;
        }
        let style = anstyle::AnsiColor::Cyan.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}hint:{style:#} {message}");
    }

    pub fn warn(&self, message: &str) {
        self.diagnostics.warn(message);
    }
}

pub fn set_color(choice: Option<ColorChoice>) {
    let choice = match choice {
        Some(ColorChoice::Always) => anstream::ColorChoice::Always,
        Some(ColorChoice::Never) => anstream::ColorChoice::Never,
        Some(ColorChoice::Auto) | None => anstream::ColorChoice::Auto,
    };
    choice.write_global();
}

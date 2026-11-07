use anyhow::{Context as _, Result, bail};
use std::sync::Arc;
use treadmill_client::diagnostics::Diagnostics as _;
use treadmill_client::error::hinted;
use treadmill_client::session::{Options, Session};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::jobs::{JobListQuery, JobListState};
use uuid::Uuid;

use crate::cli::{ColorChoice, Globals, JobRef, OutputFormat};
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

    /// Returns the job's UUID, or the active job's if [None]. A `^<hex>`
    /// suffix is looked up on the switchboard and must match exactly one job.
    pub async fn resolve_job(&self, reference: Option<&JobRef>) -> Result<Uuid> {
        let suffix = match reference {
            Some(JobRef::Id(job)) => return Ok(*job),
            Some(JobRef::Short(suffix)) => suffix,
            None => {
                let job = self
                    .session
                    .state
                    .active_job
                    .with_context(|| format!("no job given and no active job set; pass --job or run `{} context job set <JOB>`", program()))?;
                self.note(&format!("Active job {}", short(job)));
                return Ok(job);
            }
        };

        let client = self.authenticated_client()?;
        let queries = [
            JobListState::Active,
            JobListState::Reclaimable,
            JobListState::Finished,
        ]
        .map(|state| JobListQuery {
            include: "all".to_string(),
            state,
            q: Some(format!("^{suffix}")),
            limit: Some(2),
            cursor: None,
        });
        let pages =
            futures_util::future::try_join_all(queries.iter().map(|query| client.list_jobs(query)))
                .await
                .with_context(|| format!("looking up job ^{suffix}"))?;

        let jobs: Vec<Uuid> = pages
            .into_iter()
            .flat_map(|page| page.jobs)
            .map(|job| job.job_id)
            .collect();
        match jobs[..] {
            [job] => {
                self.detail(&format!("^{suffix} → {job}"));
                Ok(job)
            }
            [] => bail!("no job matches ^{suffix}"),
            _ => bail!(
                "^{suffix} matches several jobs, including {}; give more digits",
                jobs.iter()
                    .map(Uuid::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
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

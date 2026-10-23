use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::jobs::{JobListQuery, JobListState};
use uuid::Uuid;

use crate::cli::{ColorChoice, Globals, JobRef, OutputFormat};
use crate::config::{self, Config, DEFAULT_PROFILE};
use crate::display::{program, short};
use crate::event::{self, Event};
use crate::state::State;

pub struct Ctx {
    pub config: Config,
    pub profile: String,
    pub config_path: PathBuf,
    pub state: State,
    pub state_path: PathBuf,
    pub output: OutputFormat,
    pub quiet: bool,
    pub verbose: u8,
    pub insecure_tls: bool,
}

impl Ctx {
    pub fn load(globals: &Globals) -> Result<Self> {
        let profile = globals
            .profile
            .clone()
            .or_else(|| std::env::var("TML_PROFILE").ok())
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string());

        let config_path = config::config_path(globals.config.as_deref())?;
        let mut config = config::load(&config_path, &profile)?;
        if let Some(switchboard) = &globals.switchboard {
            config.switchboard = switchboard.clone();
        }
        let insecure_tls = globals.insecure_tls || config.insecure_tls;

        let state_path = config::state_path(&profile)?;
        let state = State::load(&state_path)?;

        Ok(Self {
            config,
            profile,
            config_path,
            state,
            state_path,
            output: globals.output.unwrap_or(OutputFormat::Human),
            quiet: globals.quiet,
            verbose: globals.verbose,
            insecure_tls,
        })
    }

    /// The bearer token to present, preferring an explicitly configured one
    /// (`TML_TOKEN` or the profile) over the stored session.
    pub fn token(&self) -> Option<String> {
        if let Some(token) = &self.config.token {
            return Some(token.clone());
        }
        self.state.token.clone()
    }

    pub fn client(&self) -> Result<SwitchboardClient> {
        self.client_with(self.token())
    }

    pub fn anonymous_client(&self) -> Result<SwitchboardClient> {
        self.client_with(None)
    }

    pub fn authenticated_client(&self) -> Result<SwitchboardClient> {
        if self.config.token.is_none() && !self.state.token_valid() {
            if self.state.token.is_some() {
                bail!("the stored session has expired; run `{} login`", program());
            }
            bail!(
                "not logged in to {}; run `{} login`",
                self.config.switchboard,
                program()
            );
        }
        self.client()
    }

    fn client_with(&self, token: Option<String>) -> Result<SwitchboardClient> {
        if self.insecure_tls {
            self.warn("TLS certificate verification is disabled");
            return SwitchboardClient::new_danger_accept_invalid_certs(
                self.config.switchboard.clone(),
                token,
            )
            .context("building an insecure HTTP client");
        }
        Ok(SwitchboardClient::new(
            self.config.switchboard.clone(),
            token,
        ))
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
        if self.state.active_job == Some(job_id) {
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
        if self.verbose == 0 {
            return;
        }
        anstream::eprintln!("{message}");
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
        self.event(Event::Warning { message });
        let style = anstyle::AnsiColor::Yellow.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}warning:{style:#} {message}");
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

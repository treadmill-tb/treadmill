use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::jobs::{JobListQuery, JobListState};
use uuid::Uuid;

use crate::cli::{ColorChoice, Globals, OutputFormat};
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

    pub fn job_flag(&self, job_id: Uuid) -> String {
        if self.state.active_job == Some(job_id) {
            String::new()
        } else {
            format!(" --job {job_id}")
        }
    }

    /// Resolve a `[JOB]` reference: an explicit UUID, a `^<hex>` id suffix
    /// matching exactly one job, `@active`, or — when omitted — the active
    /// job.
    pub async fn resolve_job(
        &self,
        client: &SwitchboardClient,
        reference: Option<&str>,
    ) -> Result<Uuid> {
        match reference {
            None | Some("@active") => {
                let job = self
                    .state
                    .active_job
                    .with_context(|| format!("no job given and no active job set; pass --job or run `{} context job set <UUID>`", program()))?;
                self.note(&format!("Using active job {}", short(job)));
                Ok(job)
            }
            Some(reference) if reference.starts_with('^') => {
                self.resolve_short_job(client, reference).await
            }
            Some(reference) => Uuid::parse_str(reference)
                .with_context(|| format!("{reference:?} is not a job UUID or a `^<hex>` short id")),
        }
    }

    async fn resolve_short_job(&self, client: &SwitchboardClient, reference: &str) -> Result<Uuid> {
        let hex = &reference[1..];
        if hex.is_empty() || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("{reference:?} is not a `^<hex>` short id");
        }
        let mut matches = Vec::new();
        for state in [
            JobListState::Active,
            JobListState::Reclaimable,
            JobListState::Finished,
        ] {
            let query = JobListQuery {
                include: "all".to_string(),
                state,
                q: Some(reference.to_lowercase()),
                limit: Some(2),
                cursor: None,
            };
            let page = client
                .list_jobs(&query)
                .await
                .with_context(|| format!("looking up job {reference}"))?;
            matches.extend(page.jobs.into_iter().map(|job| job.job_id));
        }
        match matches.as_slice() {
            [job_id] => Ok(*job_id),
            [] => bail!("no job matches {reference}"),
            _ => bail!(
                "{reference} matches several jobs ({}); give more digits",
                matches
                    .iter()
                    .map(|id| short(*id))
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

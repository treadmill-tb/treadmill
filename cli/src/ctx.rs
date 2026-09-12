use anyhow::{Context as _, Result, bail};
use std::path::PathBuf;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::cli::{ColorChoice, Globals, OutputFormat};
use crate::config::{self, Config, DEFAULT_PROFILE};
use crate::state::State;

pub struct Ctx {
    pub config: Config,
    pub profile: String,
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
                bail!("the stored session has expired; run `tml login`");
            }
            bail!(
                "not logged in to {}; run `tml login`",
                self.config.switchboard
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

    /// Resolve a `[JOB]` reference: an explicit UUID, `@active`, or — when
    /// omitted — the active job. Never picks a job by any other means.
    pub fn resolve_job(&self, reference: Option<&str>) -> Result<Uuid> {
        match reference {
            None | Some("@active") => {
                let job = self
                    .state
                    .active_job
                    .context("no job given and no active job set; pass --job or run `tml context job set <UUID>`")?;
                self.note(&format!("Using active job {}", short(job)));
                Ok(job)
            }
            Some(reference) => Uuid::parse_str(reference).with_context(|| {
                format!("{reference:?} is not a job UUID (shortened ids are display-only)")
            }),
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

    pub fn warn(&self, message: &str) {
        let style = anstyle::AnsiColor::Yellow.on_default() | anstyle::Effects::BOLD;
        anstream::eprintln!("{style}warning:{style:#} {message}");
    }
}

/// Short display form of a UUID: `^` plus its final eight hexadecimal digits.
pub fn short(id: Uuid) -> String {
    let text = id.simple().to_string();
    format!("^{}", &text[text.len() - 8..])
}

pub fn set_color(choice: Option<ColorChoice>) {
    let choice = match choice {
        Some(ColorChoice::Always) => anstream::ColorChoice::Always,
        Some(ColorChoice::Never) => anstream::ColorChoice::Never,
        Some(ColorChoice::Auto) | None => anstream::ColorChoice::Auto,
    };
    choice.write_global();
}

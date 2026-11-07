//! Who the client is talking to, and as whom.
//!
//! A [`Session`] is one configuration profile, resolved: the switchboard it
//! names, the stored session token, and the paths both live at. It is what
//! turns a profile on disk into an authenticated
//! [`SwitchboardClient`](treadmill_rs::api::switchboard::client::SwitchboardClient).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::LoginResponse;
use treadmill_rs::api::switchboard::client::SwitchboardClient;

use crate::config::{self, Config, DEFAULT_PROFILE};
use crate::diagnostics::Diagnostics;
use crate::state::State;

/// Which profile to load, and the overrides a consumer takes from its own
/// flags or environment. Everything is optional: the default is the `default`
/// profile of the config file at its XDG path.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// The profile to select, overriding `$TML_PROFILE`.
    pub profile: Option<String>,
    /// The config file to read, overriding the XDG path.
    pub config_path: Option<PathBuf>,
    /// A switchboard URL that wins over the profile's.
    pub switchboard: Option<String>,
    /// Skip TLS certificate verification. ORed with the profile's setting, so
    /// that passing `false` never re-enables what the profile turned off.
    pub insecure_tls: bool,
}

/// One configuration profile, resolved.
pub struct Session {
    pub config: Config,
    pub profile: String,
    pub config_path: PathBuf,
    pub state: State,
    pub state_path: PathBuf,
    pub insecure_tls: bool,
    diagnostics: Arc<dyn Diagnostics>,
}

impl Session {
    /// Load the profile `options` selects, layering the config file under
    /// `TML_*` environment variables and then `options`' own overrides.
    ///
    /// `diagnostics` receives what this crate reports along the way; pass
    /// `Arc::new(Silent)` to discard it.
    pub fn load(options: &Options, diagnostics: Arc<dyn Diagnostics>) -> Result<Self> {
        let profile = options
            .profile
            .clone()
            .or_else(|| std::env::var("TML_PROFILE").ok())
            .unwrap_or_else(|| DEFAULT_PROFILE.to_string());

        let config_path = config::config_path(options.config_path.as_deref())?;
        let mut config = config::load(&config_path, &profile, diagnostics.as_ref())?;
        if let Some(switchboard) = &options.switchboard {
            config.switchboard = switchboard.clone();
        }
        let insecure_tls = options.insecure_tls || config.insecure_tls;

        let state_path = config::state_path(&profile)?;
        let state = State::load(&state_path)?;

        Ok(Self {
            config,
            profile,
            config_path,
            state,
            state_path,
            insecure_tls,
            diagnostics,
        })
    }

    /// Where this session reports warnings and detail.
    pub fn diagnostics(&self) -> &dyn Diagnostics {
        self.diagnostics.as_ref()
    }

    /// The bearer token to present, preferring an explicitly configured one
    /// (`TML_TOKEN` or the profile) over the stored session.
    pub fn token(&self) -> Option<String> {
        if let Some(token) = &self.config.token {
            return Some(token.clone());
        }
        self.state.token.clone()
    }

    /// A client carrying whatever token [`Session::token`] found, which may be
    /// none.
    pub fn client(&self) -> Result<SwitchboardClient> {
        self.client_with(self.token())
    }

    /// A client carrying no token, for the calls that take none: listing the
    /// login providers, reading the Terms of Service.
    pub fn anonymous_client(&self) -> Result<SwitchboardClient> {
        self.client_with(None)
    }

    /// A client carrying a token that is present and unexpired, or an error
    /// saying which of the two it is. The remedy — logging in — is the
    /// caller's to suggest.
    pub fn authenticated_client(&self) -> Result<SwitchboardClient> {
        if self.config.token.is_none() && !self.state.token_valid() {
            if self.state.token.is_some() {
                bail!(
                    "the stored session for {} has expired",
                    self.config.switchboard
                );
            }
            bail!("not logged in to {}", self.config.switchboard);
        }
        self.client()
    }

    fn client_with(&self, token: Option<String>) -> Result<SwitchboardClient> {
        if self.insecure_tls {
            self.diagnostics
                .warn("TLS certificate verification is disabled");
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

    /// The SSH gateway domains this profile configures, or an error naming
    /// the profile that configures none.
    pub fn ssh_domains(&self) -> Result<&[String]> {
        self.config.ssh_domains(&self.profile)
    }

    /// Re-read the state file, picking up what another invocation stored.
    pub fn reload_state(&mut self) -> Result<()> {
        self.state = State::load(&self.state_path)?;
        Ok(())
    }

    /// Change the stored state, and keep what was stored.
    pub fn update_state(&mut self, change: impl FnOnce(&mut State)) -> Result<()> {
        self.state = State::update(&self.state_path, self.diagnostics.as_ref(), change)?;
        Ok(())
    }

    /// Store a freshly minted session, as [`crate::login`] completes one.
    pub fn store_session(&mut self, response: &LoginResponse) -> Result<()> {
        self.update_state(|state| {
            state.token = Some(response.token.encode_for_http());
            state.expires_at = Some(response.expires_at);
        })
    }

    /// Forget the stored session. Revoking it at the switchboard is
    /// [`Session::revoke_session`], and is a separate step because it can fail
    /// on its own.
    pub fn clear_session(&mut self) -> Result<()> {
        self.update_state(|state| {
            state.token = None;
            state.expires_at = None;
        })
    }

    /// Revoke the token this session authenticates with, returning whether
    /// there was one to revoke.
    ///
    /// The switchboard flags the presented token in the session listing, so no
    /// token id has to be stored alongside it.
    pub async fn revoke_session(&self) -> Result<bool> {
        let client = self.client()?;
        let Some(session) = client
            .list_my_tokens()
            .await?
            .into_iter()
            .find(|session| session.current)
        else {
            return Ok(false);
        };
        client.revoke_own_token(session.token_id).await?;
        Ok(true)
    }
}

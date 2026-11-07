//! Driving a login to completion.
//!
//! A Treadmill login is staged: the browser flow ends by handing the user a
//! single-use *login code*, which is exchanged for a session token. The
//! exchange can come back asking for another step first — accepting the Terms
//! of Service — and hands out a fresh code each time, because the presented
//! one is consumed either way.
//!
//! That loop is the part worth sharing. The parts around it are not: which
//! provider to use, how to show the user a URL, how to read a code without
//! echoing it, and how to present a ToS for acceptance are all decisions a
//! consumer makes for itself. So [`LoginFlow`] owns the exchange and asks the
//! caller for everything else.

use anyhow::{Context as _, Result};
use treadmill_rs::api::switchboard::client::{LoginCompleteOutcome, SwitchboardClient};
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, LoginCompleteRequest, LoginResponse, MockIdentityInfo, OAuthProviderInfo,
};
use treadmill_rs::util::Secret;

use crate::session::Session;

/// What the switchboard wants next, after a login code was presented.
#[derive(Debug)]
pub enum LoginStep {
    /// The login is done and the session minted. Hand it to
    /// [`Session::store_session`] to keep it.
    Complete(Box<LoginResponse>),
    /// The Terms of Service have to be accepted before the login completes.
    /// Present `text` and, if the user agrees, call
    /// [`LoginFlow::accept_tos`] with `version`.
    NeedsTos { version: i32, text: String },
    /// A step this crate does not implement. The login cannot continue here.
    Unsupported(String),
}

/// One login in progress. Built by [`LoginFlow::begin`], advanced by
/// [`LoginFlow::submit`] and [`LoginFlow::accept_tos`] until it yields
/// [`LoginStep::Complete`].
pub struct LoginFlow {
    client: SwitchboardClient,
    /// The code to present next. Replaced every time the switchboard stages
    /// the login again, since presenting one consumes it.
    login_code: Option<Secret<String>>,
    tos_version: Option<i32>,
}

impl LoginFlow {
    /// Start a login against `session`'s switchboard. No request is made yet.
    pub fn begin(session: &Session) -> Result<Self> {
        Ok(Self {
            client: session.anonymous_client()?,
            login_code: None,
            tos_version: None,
        })
    }

    /// The login methods this switchboard offers, so that a caller can pick
    /// one or present them all.
    pub async fn providers(&self) -> Result<AuthProvidersResponse> {
        self.client
            .auth_providers()
            .await
            .context("listing the switchboard's login providers")
    }

    /// The absolute URL a browser should be sent to, for the `login_path` of
    /// a provider or mock identity from [`LoginFlow::providers`].
    pub fn login_url(&self, login_path: &str) -> String {
        self.client.login_url(login_path)
    }

    /// Present the login code the browser flow handed the user.
    pub async fn submit(&mut self, code: Secret<String>) -> Result<LoginStep> {
        self.login_code = Some(code);
        self.exchange().await
    }

    /// Accept the Terms of Service at `version` and carry on. `version` is the
    /// one [`LoginStep::NeedsTos`] reported; sending a different one is for
    /// the switchboard to refuse.
    pub async fn accept_tos(&mut self, version: i32) -> Result<LoginStep> {
        self.tos_version = Some(version);
        self.exchange().await
    }

    /// The Terms of Service as they stand, whether or not a login needs them.
    pub async fn tos_info(&self) -> Result<(i32, String)> {
        let tos = self
            .client
            .tos_info()
            .await
            .context("fetching the Terms of Service")?;
        Ok((tos.version, tos.text))
    }

    async fn exchange(&mut self) -> Result<LoginStep> {
        let login_code = self
            .login_code
            .clone()
            .context("no login code has been submitted yet")?;
        let request = LoginCompleteRequest {
            login_code,
            tos_version: self.tos_version,
        };

        match self
            .client
            .login_complete(&request)
            .await
            .context("completing the staged login")?
        {
            LoginCompleteOutcome::Complete(response) => Ok(LoginStep::Complete(Box::new(response))),
            LoginCompleteOutcome::Staged(staged) => {
                // The presented code is spent either way; the reply carries
                // its replacement.
                self.login_code = Some(staged.login_code);
                self.tos_version = staged.tos_version;
                if staged.required.iter().any(|step| step == "tos") {
                    let (version, text) = self.tos_info().await?;
                    return Ok(LoginStep::NeedsTos { version, text });
                }
                match staged.required.first() {
                    Some(step) => Ok(LoginStep::Unsupported(step.clone())),
                    // Staged with nothing required is the switchboard asking
                    // for the fresh code to be presented again.
                    None => Box::pin(self.exchange()).await,
                }
            }
        }
    }
}

/// The OAuth provider named `name`, if this switchboard offers it.
pub fn find_provider<'a>(
    providers: &'a AuthProvidersResponse,
    name: &str,
) -> Option<&'a OAuthProviderInfo> {
    providers
        .oauth
        .iter()
        .find(|provider| provider.name == name)
}

/// The mock identity keyed `key`, if this switchboard offers it.
///
/// Mock identities are a development-only auth bypass; a consumer that
/// presents one should say so.
pub fn find_mock_identity<'a>(
    providers: &'a AuthProvidersResponse,
    key: &str,
) -> Option<&'a MockIdentityInfo> {
    providers
        .mock_identities
        .iter()
        .find(|identity| identity.key == key)
}

/// Try to open `url` in the user's browser, reporting whether a browser could
/// be started. Best-effort: the caller should print the URL regardless, since
/// the code still has to come back by hand.
pub fn open_browser(url: &str) -> bool {
    let opener = std::env::var("BROWSER").ok();
    let candidates: Vec<&str> = match &opener {
        Some(browser) => vec![browser.as_str()],
        None => vec!["xdg-open", "open"],
    };

    for candidate in candidates {
        let spawned = std::process::Command::new(candidate)
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return true;
        }
    }
    false
}

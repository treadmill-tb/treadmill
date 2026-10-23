use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::LoginCompleteOutcome;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, LoginCompleteRequest, LoginStagedResponse,
};
use treadmill_rs::util::Secret;

use crate::cli::LoginArgs;
use crate::ctx::Ctx;
use crate::state::State;
use crate::tui::{self, DIM, Item, Terminal, plain, span};

pub async fn login(ctx: &mut Ctx, args: &LoginArgs) -> Result<()> {
    let client = ctx.anonymous_client()?;
    let providers = client
        .auth_providers()
        .await
        .context("listing the switchboard's login providers")?;

    let login_path = select_login(ctx, &providers, args).await?;
    let url = client.login_url(&login_path);

    ctx.note(&format!("Logging in to {}", ctx.config.switchboard));
    open_browser(&url);
    anstream::eprintln!(
        "Open this URL to authenticate, then paste the login code the page shows:\n\n    {url}\n"
    );

    let mut staged = LoginStagedResponse {
        required: Vec::new(),
        login_code: read_login_code().await?,
        tos_version: None,
    };

    loop {
        let mut request = LoginCompleteRequest {
            login_code: staged.login_code.clone(),
            tos_version: staged.tos_version,
        };

        if staged.required.iter().any(|step| step == "tos") {
            accept_tos(ctx, &client, &mut request).await?;
        } else if let Some(step) = staged.required.first() {
            bail!(
                "this switchboard requires the {step:?} login step, which this \
                 version of tml cannot perform; complete the login in a browser"
            );
        }

        match client
            .login_complete(&request)
            .await
            .context("completing the staged login")?
        {
            LoginCompleteOutcome::Complete(response) => {
                ctx.state = State::update(&ctx.state_path, |state| {
                    state.token = Some(response.token.encode_for_http());
                    state.expires_at = Some(response.expires_at);
                })?;

                let whoami = ctx.client()?.whoami().await?;
                anstream::println!("Logged in as {}", whoami.name);
                return Ok(());
            }
            LoginCompleteOutcome::Staged(fresh) => {
                ctx.note("The switchboard requires another step to finish this login");
                staged = fresh;
            }
        }
    }
}

pub async fn logout(ctx: &mut Ctx) -> Result<()> {
    if ctx.state.token.is_none() {
        ctx.note("No stored credentials to remove");
        return Ok(());
    }

    match revoke(ctx).await {
        Ok(true) => ctx.note("Revoked the session token"),
        Ok(false) => ctx.note("The session token was already gone from the switchboard"),
        Err(e) => ctx.warn(&format!(
            "could not revoke the session token ({e:#}); removing it locally anyway"
        )),
    }

    ctx.state = State::update(&ctx.state_path, |state| {
        state.token = None;
        state.expires_at = None;
    })?;
    anstream::println!("Removed the stored credentials");
    Ok(())
}

/// Revoke the token this invocation authenticates with. The switchboard flags
/// it in the session listing, so no token id has to be stored alongside it.
async fn revoke(ctx: &Ctx) -> Result<bool> {
    let client = ctx.client()?;
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

pub async fn whoami(ctx: &Ctx) -> Result<()> {
    let identity = ctx.authenticated_client()?.whoami().await?;

    if !ctx.human() {
        anstream::println!("{}", serde_json::to_string_pretty(&identity)?);
        return Ok(());
    }

    anstream::println!("PROFILE       {}", ctx.profile);
    anstream::println!("SWITCHBOARD   {}", ctx.config.switchboard);
    anstream::println!("SUBJECT       {}", identity.user_id);
    anstream::println!("NAME          {}", identity.name);
    Ok(())
}

async fn select_login(
    ctx: &Ctx,
    providers: &AuthProvidersResponse,
    args: &LoginArgs,
) -> Result<String> {
    if let Some(key) = &args.identity {
        let identity = providers
            .mock_identities
            .iter()
            .find(|id| &id.key == key)
            .with_context(|| format!("no mock identity {key:?} on this switchboard"))?;
        ctx.warn("signing in with a development-only mock identity");
        return Ok(identity.login_path.clone());
    }

    if let Some(name) = &args.provider {
        let provider = providers
            .oauth
            .iter()
            .find(|p| &p.name == name)
            .with_context(|| format!("no login provider {name:?} on this switchboard"))?;
        return Ok(provider.login_path.clone());
    }

    let mut items: Vec<Item> = providers
        .oauth
        .iter()
        .map(|p| Item::new(vec![plain(p.display_name.clone())], &p.display_name))
        .collect();
    let mut paths: Vec<String> = providers
        .oauth
        .iter()
        .map(|p| p.login_path.clone())
        .collect();
    for identity in &providers.mock_identities {
        let line = vec![
            plain(identity.label.clone()),
            span(DIM, " · development only"),
        ];
        items.push(Item::new(line, &identity.label));
        paths.push(identity.login_path.clone());
    }

    match paths.len() {
        0 => bail!("this switchboard advertises no login providers"),
        1 => Ok(paths.remove(0)),
        _ => {
            if !tui::interactive() {
                bail!(
                    "this switchboard offers several login providers; pass --provider or --identity"
                );
            }
            let mut term = Terminal::open()?;
            let chosen = tui::select(&mut term, "Identity provider", &[], &items, 0, true)
                .await?
                .ok_or_else(tui::interrupted)?;
            term.print(&[tui::answered(
                "Identity provider",
                items[chosen].line.clone(),
            )])?;
            Ok(paths.remove(chosen))
        }
    }
}

/// The callback shows the login code when the flow declared no `return_to`,
/// which is the only completion route open to a client that cannot have a
/// loopback URL allowlisted.
async fn read_login_code() -> Result<Secret<String>> {
    let raw: String = if tui::interactive() {
        // The code is a token-minting secret, so it is read like a password:
        // never echoed, and never left in the scrollback.
        let mut term = Terminal::open()?;
        let code = tui::secret(&mut term, "Login code", "paste it here")
            .await?
            .ok_or_else(tui::interrupted)?;
        term.print(&[tui::answered("Login code", vec![span(DIM, "entered")])])?;
        code
    } else {
        use std::io::Read;
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw)?;
        raw
    };
    Ok(Secret::new(raw.trim().to_string()))
}

async fn accept_tos(
    ctx: &Ctx,
    client: &treadmill_rs::api::switchboard::client::SwitchboardClient,
    request: &mut LoginCompleteRequest,
) -> Result<()> {
    let tos = client
        .tos_info()
        .await
        .context("fetching the Terms of Service")?;

    if !tui::interactive() {
        bail!(
            "this login requires accepting the Terms of Service, which needs a terminal; \
             complete the login in a browser"
        );
    }

    ctx.note("This login requires accepting the Terms of Service.");
    anstream::eprintln!("\n{}\n", tos.text);

    if !tui::ask(&format!(
        "Accept version {} of the Terms of Service?",
        tos.version
    ))
    .await?
    {
        bail!("the Terms of Service were not accepted");
    }

    request.tos_version = Some(tos.version);
    Ok(())
}

/// Best-effort: the URL is printed regardless, and the paste flow needs the
/// page open anyway.
fn open_browser(url: &str) {
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
            return;
        }
    }
}

use anyhow::{Context as _, Result, bail};
use dialoguer::{Confirm, Password, Select};
use treadmill_rs::api::switchboard::client::LoginCompleteOutcome;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, LoginCompleteRequest, LoginStagedResponse,
};

use crate::cli::LoginArgs;
use crate::ctx::Ctx;

pub async fn login(ctx: &mut Ctx, args: &LoginArgs) -> Result<()> {
    let client = ctx.anonymous_client()?;
    let providers = client
        .auth_providers()
        .await
        .context("listing the switchboard's login providers")?;

    let login_path = select_login(ctx, &providers, args)?;
    let url = client.login_url(&login_path);

    ctx.note(&format!("Logging in to {}", ctx.config.switchboard));
    open_browser(&url);
    anstream::eprintln!(
        "Open this URL to authenticate, then paste the JSON the page shows:\n\n    {url}\n"
    );

    let mut staged = read_staged()?;

    loop {
        let mut request = LoginCompleteRequest {
            staged_id: staged.staged_id,
            staged_secret: staged.staged_secret.clone(),
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
                ctx.state.token = Some(response.token.encode_for_http());
                ctx.state.expires_at = Some(response.expires_at);
                ctx.state.store(&ctx.state_path)?;

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

    ctx.state.token = None;
    ctx.state.expires_at = None;
    ctx.state.store(&ctx.state_path)?;
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

fn select_login(ctx: &Ctx, providers: &AuthProvidersResponse, args: &LoginArgs) -> Result<String> {
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

    let mut labels: Vec<String> = providers
        .oauth
        .iter()
        .map(|p| p.display_name.clone())
        .collect();
    let mut paths: Vec<String> = providers
        .oauth
        .iter()
        .map(|p| p.login_path.clone())
        .collect();
    for identity in &providers.mock_identities {
        labels.push(format!("{} [development only]", identity.label));
        paths.push(identity.login_path.clone());
    }

    match paths.len() {
        0 => bail!("this switchboard advertises no login providers"),
        1 => Ok(paths.remove(0)),
        _ => {
            if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                bail!(
                    "this switchboard offers several login providers; pass --provider or --identity"
                );
            }
            let chosen = Select::new()
                .with_prompt("Choose an identity provider")
                .items(&labels)
                .default(0)
                .interact()?;
            Ok(paths.remove(chosen))
        }
    }
}

/// The callback renders the staged pair as JSON when the flow declared no
/// `return_to`, which is the only completion route open to a client that
/// cannot have a loopback URL allowlisted.
fn read_staged() -> Result<LoginStagedResponse> {
    let raw: String = if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        // The blob carries a token-minting secret, so it is read like a
        // password: never echoed, and never left in the scrollback.
        Password::new().with_prompt("Paste the JSON").interact()?
    } else {
        use std::io::Read;
        let mut raw = String::new();
        std::io::stdin().read_to_string(&mut raw)?;
        raw
    };

    serde_json::from_str(raw.trim())
        .context("that is not the JSON the login callback shows; paste the whole object")
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

    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!(
            "this login requires accepting the Terms of Service, which needs a terminal; \
             complete the login in a browser"
        );
    }

    ctx.note("This login requires accepting the Terms of Service.");
    anstream::eprintln!("\n{}\n", tos.text);

    if !Confirm::new()
        .with_prompt(format!(
            "Accept version {} of the Terms of Service?",
            tos.version
        ))
        .default(false)
        .interact()?
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

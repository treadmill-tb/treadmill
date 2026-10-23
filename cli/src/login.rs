use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::client::LoginCompleteOutcome;
use treadmill_rs::api::switchboard::{
    AuthProvidersResponse, LoginCompleteRequest, LoginStagedResponse, WhoAmIResponse,
};
use treadmill_rs::util::Secret;

use crate::cli::LoginArgs;
use crate::ctx::Ctx;
use crate::display;
use crate::event::Event;
use crate::state::State;
use crate::tui::{self, DIM, Item, OK, Terminal, plain, span};
use crate::{cancelled, hinted};

pub async fn login(ctx: &mut Ctx, args: &LoginArgs) -> Result<()> {
    let client = ctx.anonymous_client()?;
    let providers = client
        .auth_providers()
        .await
        .context("listing the switchboard's login providers")?;

    let login_path = select_login(ctx, &providers, args).await?;
    let url = client.login_url(&login_path);

    if open_browser(&url) {
        anstream::eprintln!(
            "\n  Opening the login page in your browser. If this does not work, open the \
             following link:\n  {url}\n"
        );
    } else {
        anstream::eprintln!("\n  To log in, open the following link:\n  {url}\n");
    }

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
            accept_tos(&client, &mut request).await?;
        } else if let Some(step) = staged.required.first() {
            return Err(hinted(
                anyhow::anyhow!(
                    "the switchboard requires the login step {step:?}, which this version of \
                     tml does not support"
                ),
                "Log in through the web console instead.",
            ));
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

                let identity = ctx.client()?.whoami().await?;
                if ctx.human() {
                    ctx.note(&tui::render(
                        &vec![
                            span(OK, "✓ "),
                            plain(format!("Logged in as {}", identity.name)),
                            span(DIM, format!(" · {}", ctx.config.switchboard)),
                        ],
                        None,
                        true,
                    ));
                }
                ctx.result(identity_event(ctx, &identity));
                return Ok(());
            }
            LoginCompleteOutcome::Staged(fresh) => staged = fresh,
        }
    }
}

pub async fn logout(ctx: &mut Ctx) -> Result<()> {
    let human = ctx.human();
    let print = |line: tui::Line| {
        if human {
            anstream::println!("{}", tui::render(&line, None, true));
        }
    };
    if ctx.state.token.is_none() {
        print(vec![
            plain("Not logged in"),
            span(DIM, " · nothing removed"),
        ]);
        ctx.result(Event::LoggedOut { revoked: None });
        return Ok(());
    }

    let revoked = match revoke(ctx).await {
        Ok(true) => {
            print(vec![span(OK, "✓ "), plain("Session token revoked")]);
            Some(true)
        }
        Ok(false) => Some(false),
        Err(e) => {
            ctx.warn(&format!("the session token could not be revoked: {e:#}"));
            None
        }
    };

    ctx.state = State::update(&ctx.state_path, |state| {
        state.token = None;
        state.expires_at = None;
    })?;
    let mut removed = vec![span(OK, "✓ "), plain("Credentials removed")];
    if revoked == Some(false) {
        removed.push(span(DIM, " · session token already revoked"));
    }
    print(removed);
    ctx.result(Event::LoggedOut { revoked });
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
        ctx.result(identity_event(ctx, &identity));
        return Ok(());
    }

    let mut user = vec![
        plain(identity.name.clone()),
        span(DIM, format!(" · {}", identity.user_id)),
    ];
    if identity.admin {
        user.push(span(DIM, " · admin"));
    }
    let rows = [
        ("Profile", vec![plain(ctx.profile.clone())]),
        ("Switchboard", vec![plain(ctx.config.switchboard.clone())]),
        ("User", user),
    ];
    for line in display::table(&rows) {
        anstream::println!("{}", tui::render(&line, None, true));
    }
    Ok(())
}

fn identity_event<'a>(ctx: &'a Ctx, identity: &'a WhoAmIResponse) -> Event<'a> {
    Event::Identity {
        profile: &ctx.profile,
        switchboard: &ctx.config.switchboard,
        identity,
    }
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
        ctx.warn(&format!(
            "{} is a mock identity, for development only",
            identity.label
        ));
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
        0 => bail!("{} offers no login providers", ctx.config.switchboard),
        1 => Ok(paths.remove(0)),
        _ => {
            if !tui::interactive() {
                return Err(hinted(
                    anyhow::anyhow!("{} offers several login providers", ctx.config.switchboard),
                    "Pass --provider <NAME> or --identity <KEY> to choose one.",
                ));
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
    client: &treadmill_rs::api::switchboard::client::SwitchboardClient,
    request: &mut LoginCompleteRequest,
) -> Result<()> {
    let tos = client
        .tos_info()
        .await
        .context("fetching the Terms of Service")?;

    if !tui::interactive() {
        return Err(hinted(
            anyhow::anyhow!("accepting the Terms of Service requires a terminal"),
            "Log in through the web console instead.",
        ));
    }

    let bold = anstyle::Style::new().bold();
    let dim = anstyle::Style::new().dimmed();
    anstream::eprintln!(
        "\n  {bold}Terms of Service{bold:#} {dim}· version {}{dim:#}\n",
        tos.version
    );
    for line in tos.text.lines() {
        anstream::eprintln!("  {line}");
    }
    anstream::eprintln!();

    if !tui::ask(&format!("Accept Terms of Service version {}?", tos.version)).await? {
        return Err(cancelled("Terms of Service not accepted, not logged in"));
    }

    request.tos_version = Some(tos.version);
    Ok(())
}

/// Best-effort: the URL is printed regardless, and the paste flow needs the
/// page open anyway. Whether a browser could be started.
fn open_browser(url: &str) -> bool {
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

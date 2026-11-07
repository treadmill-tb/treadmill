use anyhow::{Context as _, Result, bail};
use treadmill_client::login::{self, LoginFlow, LoginStep};
use treadmill_rs::api::switchboard::{AuthProvidersResponse, WhoAmIResponse};
use treadmill_rs::util::Secret;

use crate::cli::LoginArgs;
use crate::ctx::Ctx;
use crate::display;
use crate::event::Event;
use crate::tui::{self, DIM, Item, OK, Terminal, plain, span};
use treadmill_client::error::{cancelled, hinted};

pub async fn login(ctx: &mut Ctx, args: &LoginArgs) -> Result<()> {
    let mut flow = LoginFlow::begin(&ctx.session)?;
    let providers = flow.providers().await?;
    let login_path = select_login(ctx, &providers, args).await?;
    let url = flow.login_url(&login_path);

    if login::open_browser(&url) {
        anstream::eprintln!(
            "\n  Opening the login page in your browser. If this does not work, open the \
             following link:\n  {url}\n"
        );
    } else {
        anstream::eprintln!("\n  To log in, open the following link:\n  {url}\n");
    }

    let mut step = flow.submit(read_login_code().await?).await?;
    loop {
        match step {
            LoginStep::Complete(response) => {
                ctx.session.store_session(&response)?;

                let identity = ctx.client()?.whoami().await?;
                if ctx.human() {
                    ctx.note(&tui::render(
                        &vec![
                            span(OK, "✓ "),
                            plain(format!("Logged in as {}", identity.name)),
                            span(DIM, format!(" · {}", ctx.session.config.switchboard)),
                        ],
                        None,
                        true,
                    ));
                }
                ctx.result(identity_event(ctx, &identity));
                return Ok(());
            }
            LoginStep::NeedsTos { version, text } => {
                accept_tos(version, &text).await?;
                step = flow.accept_tos(version).await?;
            }
            LoginStep::Unsupported(required) => {
                return Err(hinted(
                    anyhow::anyhow!(
                        "the switchboard requires the login step {required:?}, which this \
                         version of tml does not support"
                    ),
                    "Log in through the web console instead.",
                ));
            }
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
    if ctx.session.state.token.is_none() {
        print(vec![
            plain("Not logged in"),
            span(DIM, " · nothing removed"),
        ]);
        ctx.result(Event::LoggedOut { revoked: None });
        return Ok(());
    }

    let revoked = match ctx.session.revoke_session().await {
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

    ctx.session.clear_session()?;
    let mut removed = vec![span(OK, "✓ "), plain("Credentials removed")];
    if revoked == Some(false) {
        removed.push(span(DIM, " · session token already revoked"));
    }
    print(removed);
    ctx.result(Event::LoggedOut { revoked });
    Ok(())
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
        ("Profile", vec![plain(ctx.session.profile.clone())]),
        (
            "Switchboard",
            vec![plain(ctx.session.config.switchboard.clone())],
        ),
        ("User", user),
    ];
    for line in display::table(&rows) {
        anstream::println!("{}", tui::render(&line, None, true));
    }
    Ok(())
}

fn identity_event<'a>(ctx: &'a Ctx, identity: &'a WhoAmIResponse) -> Event<'a> {
    Event::Identity {
        profile: &ctx.session.profile,
        switchboard: &ctx.session.config.switchboard,
        identity,
    }
}

async fn select_login(
    ctx: &Ctx,
    providers: &AuthProvidersResponse,
    args: &LoginArgs,
) -> Result<String> {
    if let Some(key) = &args.identity {
        let identity = login::find_mock_identity(providers, key)
            .with_context(|| format!("no mock identity {key:?} on this switchboard"))?;
        ctx.warn(&format!(
            "{} is a mock identity, for development only",
            identity.label
        ));
        return Ok(identity.login_path.clone());
    }

    if let Some(name) = &args.provider {
        let provider = login::find_provider(providers, name)
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
        0 => bail!(
            "{} offers no login providers",
            ctx.session.config.switchboard
        ),
        1 => Ok(paths.remove(0)),
        _ => {
            if !tui::interactive() {
                return Err(hinted(
                    anyhow::anyhow!(
                        "{} offers several login providers",
                        ctx.session.config.switchboard
                    ),
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

/// Present the Terms of Service and ask for them. The text and version come
/// from the flow, which fetched them.
async fn accept_tos(version: i32, text: &str) -> Result<()> {
    if !tui::interactive() {
        return Err(hinted(
            anyhow::anyhow!("accepting the Terms of Service requires a terminal"),
            "Log in through the web console instead.",
        ));
    }

    let bold = anstyle::Style::new().bold();
    let dim = anstyle::Style::new().dimmed();
    anstream::eprintln!("\n  {bold}Terms of Service{bold:#} {dim}· version {version}{dim:#}\n");
    for line in text.lines() {
        anstream::eprintln!("  {line}");
    }
    anstream::eprintln!();

    if !tui::ask(&format!("Accept Terms of Service version {version}?")).await? {
        return Err(cancelled("Terms of Service not accepted, not logged in"));
    }
    Ok(())
}

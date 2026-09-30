use anyhow::{Context as _, Result};
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::cli::HostCommand;
use crate::ctx::Ctx;
use crate::display::{HostState, boards, snake};

pub async fn run(ctx: &Ctx, command: &HostCommand) -> Result<()> {
    let client = ctx.authenticated_client()?;
    match command {
        HostCommand::List => list(ctx, &client).await,
        HostCommand::Show { host } => show(ctx, &client, host).await,
    }
}

async fn list(ctx: &Ctx, client: &SwitchboardClient) -> Result<()> {
    let hosts = client.list_hosts().await.context("listing hosts")?;
    if !ctx.human() {
        anstream::println!("{}", serde_json::to_string_pretty(&hosts)?);
        return Ok(());
    }

    anstream::println!("{:<20} {:<12} {:<10} DUTS", "NAME", "STATE", "ARCH");
    for host in hosts {
        let arch = host
            .spec
            .as_ref()
            .map_or("—", |spec| spec.platform.arch.as_str());
        anstream::println!(
            "{:<20} {:<12} {arch:<10} {}",
            host.name,
            HostState::of(&host).as_str(),
            boards(&host)
        );
    }
    Ok(())
}

async fn show(ctx: &Ctx, client: &SwitchboardClient, reference: &str) -> Result<()> {
    let host_id = match Uuid::parse_str(reference) {
        Ok(id) => id,
        Err(_) => client
            .list_hosts()
            .await
            .context("listing hosts")?
            .into_iter()
            .find(|host| host.name == reference)
            .map(|host| host.host_id)
            .with_context(|| format!("no host named {reference:?}"))?,
    };
    let host = client
        .get_host(host_id)
        .await
        .with_context(|| format!("reading host {host_id}"))?;

    if !ctx.human() {
        anstream::println!("{}", serde_json::to_string_pretty(&host)?);
        return Ok(());
    }

    anstream::println!("NAME          {}", host.name);
    anstream::println!("ID            {}", host.host_id);
    anstream::println!(
        "STATE         live={} lease={} maintenance={}",
        host.live,
        snake(&host.lease_state),
        host.maintenance
    );
    match &host.spec {
        Some(spec) => anstream::println!("SPEC\n{}", serde_json::to_string_pretty(spec)?),
        None => anstream::println!("SPEC          —"),
    }
    Ok(())
}

use anyhow::Result;
use uuid::Uuid;

use crate::cli::{ContextCommand, ContextJobCommand};
use crate::ctx::{Ctx, short};

pub async fn run(ctx: &mut Ctx, command: &ContextCommand) -> Result<()> {
    match command {
        ContextCommand::Show => show(ctx),
        ContextCommand::Job { command } => match command {
            ContextJobCommand::Set { job } => set_active(ctx, *job),
            ContextJobCommand::Clear => clear(ctx),
        },
    }
}

pub fn set_active(ctx: &mut Ctx, job: Uuid) -> Result<()> {
    ctx.state.active_job = Some(job);
    ctx.state.store(&ctx.state_path)?;

    if ctx.human() {
        anstream::println!("Active job: {}  ({job})", short(job));
    } else {
        anstream::println!("{}", serde_json::json!({ "active_job": job }));
    }
    Ok(())
}

fn clear(ctx: &mut Ctx) -> Result<()> {
    ctx.state.active_job = None;
    ctx.state.store(&ctx.state_path)?;
    anstream::println!("Cleared the active job");
    Ok(())
}

fn show(ctx: &Ctx) -> Result<()> {
    if !ctx.human() {
        anstream::println!(
            "{}",
            serde_json::json!({
                "profile": ctx.profile,
                "switchboard": ctx.config.switchboard,
                "active_job": ctx.state.active_job,
            })
        );
        return Ok(());
    }

    anstream::println!("PROFILE       {}", ctx.profile);
    anstream::println!("SWITCHBOARD   {}", ctx.config.switchboard);
    match ctx.state.active_job {
        Some(job) => anstream::println!("ACTIVE JOB    {}  ({job})", short(job)),
        None => anstream::println!("ACTIVE JOB    —"),
    }
    Ok(())
}

use anyhow::Result;
use uuid::Uuid;

use crate::cli::{ContextCommand, ContextJobCommand};
use crate::ctx::Ctx;
use crate::display::{self, short};
use crate::event::Event;
use crate::state::State;
use crate::tui::{self, DIM, OK, plain, span};

pub async fn run(ctx: &mut Ctx, command: &ContextCommand) -> Result<()> {
    match command {
        ContextCommand::Show => show(ctx),
        ContextCommand::Job { command } => match command {
            ContextJobCommand::Set { job } => {
                let job = ctx.resolve_job(Some(job)).await?;
                set_active(ctx, job)
            }
            ContextJobCommand::Clear => clear(ctx),
        },
    }
}

pub fn set_active(ctx: &mut Ctx, job: Uuid) -> Result<()> {
    ctx.state = State::update(&ctx.state_path, |state| state.active_job = Some(job))?;
    if ctx.human() {
        print(vec![
            span(OK, "✓ "),
            plain(format!("Active job {}", short(job))),
            span(DIM, format!(" · {job}")),
        ]);
    }
    ctx.result(Event::ActiveJob {
        active_job: Some(job),
    });
    Ok(())
}

fn clear(ctx: &mut Ctx) -> Result<()> {
    ctx.state = State::update(&ctx.state_path, |state| state.active_job = None)?;
    if ctx.human() {
        print(vec![span(OK, "✓ "), plain("No active job")]);
    }
    ctx.result(Event::ActiveJob { active_job: None });
    Ok(())
}

fn show(ctx: &Ctx) -> Result<()> {
    if !ctx.human() {
        ctx.result(Event::Context {
            profile: &ctx.profile,
            switchboard: &ctx.config.switchboard,
            active_job: ctx.state.active_job,
        });
        return Ok(());
    }

    let active_job = match ctx.state.active_job {
        Some(job) => vec![plain(short(job)), span(DIM, format!(" · {job}"))],
        None => vec![span(DIM, "—")],
    };
    let rows = [
        ("Profile", vec![plain(ctx.profile.clone())]),
        ("Switchboard", vec![plain(ctx.config.switchboard.clone())]),
        ("Active job", active_job),
    ];
    for line in display::table(&rows) {
        anstream::println!("{}", tui::render(&line, None, true));
    }
    Ok(())
}

fn print(line: tui::Line) {
    anstream::println!("{}", tui::render(&line, None, true));
}

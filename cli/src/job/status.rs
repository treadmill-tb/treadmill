use anyhow::{Context as _, Result};
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::jobs::JobInfo;

use crate::cli::OutputFormat;
use crate::ctx::Ctx;
use crate::display::{self, program, short};
use crate::event::Event;
use crate::tui::{self, Line, plain};

pub async fn run(ctx: &Ctx, job: Option<&str>) -> Result<()> {
    let client = ctx.authenticated_client()?;
    let job_id = ctx.resolve_job(&client, job).await?;
    let job = client
        .get_job(job_id)
        .await
        .with_context(|| format!("reading job {}", short(job_id)))?;
    if !ctx.human() {
        return output(ctx, &job);
    }
    for line in lines(ctx, &client, &job).await {
        anstream::println!("{}", tui::render(&line, None, true));
    }
    Ok(())
}

pub fn output(ctx: &Ctx, job: &JobInfo) -> Result<()> {
    match ctx.output {
        OutputFormat::Json => anstream::println!("{}", serde_json::to_string_pretty(job)?),
        OutputFormat::Jsonl => ctx.event(Event::Job { job }),
        OutputFormat::Human => {}
    }
    Ok(())
}

pub async fn report(ctx: &Ctx, client: &SwitchboardClient, job: &JobInfo) {
    if ctx.quiet || !ctx.human() {
        return;
    }
    ctx.note("");
    for line in lines(ctx, client, job).await {
        ctx.note(&tui::render(&line, None, true));
    }
}

async fn lines(ctx: &Ctx, client: &SwitchboardClient, job: &JobInfo) -> Vec<Line> {
    let rows = display::job_rows(client, job).await;
    let flag = ctx.job_flag(job.job_id);
    let tml = program();
    let commands: Vec<(&str, String)> = match job.state {
        JobState::Ready => vec![
            ("Connect", format!("{tml} job ssh{flag}")),
            ("Upload", format!("{tml} job upload{flag} <local> [remote]")),
            (
                "Download",
                format!("{tml} job download{flag} <remote> [local]"),
            ),
            ("Extend", format!("{tml} job lease{flag} +30m")),
            ("Stop", format!("{tml} job terminate{flag}")),
        ],
        JobState::Queued | JobState::Assigned | JobState::Initializing => vec![
            ("Wait", format!("{tml} job wait{flag}")),
            ("Stop", format!("{tml} job terminate{flag}")),
        ],
        JobState::Terminating | JobState::Finalized => Vec::new(),
    };
    let commands: Vec<(&str, Line)> = commands
        .into_iter()
        .map(|(title, command)| (title, vec![plain(command)]))
        .collect();
    display::table(&rows, &commands)
}

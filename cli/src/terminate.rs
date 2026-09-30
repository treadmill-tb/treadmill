use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::cli::{Globals, OutputFormat};
use crate::display::{self, short};
use crate::event::{self, Event};
use crate::tui;

pub async fn run(
    globals: &Globals,
    client: &SwitchboardClient,
    job_id: Uuid,
    yes: bool,
) -> Result<()> {
    let output = globals.output.unwrap_or(OutputFormat::Human);
    let note = |message: &str| {
        if output == OutputFormat::Human && !globals.quiet {
            anstream::eprintln!("{message}");
        }
    };

    let job = client
        .get_job(job_id)
        .await
        .with_context(|| format!("reading job {job_id}"))?;
    match job.state {
        JobState::Finalized => bail!("job {job_id} has already finished"),
        JobState::Terminating => {
            note(&format!("Job {} is already terminating", short(job_id)));
            return Ok(());
        }
        _ => {}
    }

    if !yes {
        if !tui::interactive() {
            bail!("terminating job {job_id} needs confirmation; pass --yes");
        }
        let rows = display::job_rows(client, &job).await;
        for line in display::table(&rows, &[]) {
            anstream::eprintln!("{}", tui::render(&line, None, true));
        }
        anstream::eprintln!();
        if !tui::ask("Terminate this job?").await? {
            bail!("left job {job_id} running");
        }
    }

    client
        .terminate_job(job_id)
        .await
        .with_context(|| format!("requesting termination of job {job_id}"))?;
    let ok = anstyle::AnsiColor::Green.on_default();
    let dim = anstyle::Style::new().dimmed();
    note(&format!(
        "{ok}✓{ok:#} Termination requested {dim}· {}{dim:#}",
        short(job_id)
    ));
    event::emit(output, Event::TerminationRequested { job_id });
    Ok(())
}

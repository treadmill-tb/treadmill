use anyhow::{Context as _, Result, bail};
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use uuid::Uuid;

use crate::cli::{Globals, OutputFormat};
use crate::display::{self, short};
use crate::event::{self, Event};
use crate::tui;
use crate::{cancelled, hinted};

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
            let dim = anstyle::Style::new().dimmed();
            note(&format!(
                "Job {} {dim}· already terminating{dim:#}",
                short(job_id)
            ));
            return Ok(());
        }
        _ => {}
    }

    if !yes {
        if !tui::interactive() {
            return Err(hinted(
                anyhow::anyhow!("terminating job {job_id} requires confirmation"),
                "Pass --yes to terminate it without a terminal.",
            ));
        }
        let rows = display::job_rows(client, &job).await;
        for line in display::table(&rows) {
            anstream::eprintln!("{}", tui::render(&line, None, true));
        }
        anstream::eprintln!();
        if !tui::ask(&format!("Terminate job {}?", short(job_id))).await? {
            return Err(cancelled(format!("job {} still running", short(job_id))));
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
    event::result(output, Event::TerminationRequested { job_id });
    Ok(())
}

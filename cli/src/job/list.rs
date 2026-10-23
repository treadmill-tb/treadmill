use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::time::Duration;
use treadmill_rs::api::switchboard::JobState;
use treadmill_rs::api::switchboard::client::SwitchboardClient;
use treadmill_rs::api::switchboard::hosts::HostLeaseState;
use treadmill_rs::api::switchboard::jobs::{
    JobImageReference, JobListQuery, JobListState, JobSummary, TaskExitStatus,
};
use uuid::Uuid;

use crate::cli::{JobListArgs, OutputFormat};
use crate::ctx::Ctx;
use crate::display::{self, human_duration, image_name, program, shell_quote, short, snake};
use crate::event::Event;
use crate::listing::{self, Cell, Pager, cell};
use crate::tui::{self, ACCENT, BOLD, DIM, ERR, Line, OK, WARN, plain, span};

const PAGE_MAX: usize = 200;
/// The least time between two requests for further pages of a paged list.
const PAGE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(serde::Serialize)]
struct ListedJob {
    #[serde(flatten)]
    job: JobSummary,
    lease_state: Option<HostLeaseState>,
}

struct Page {
    state: JobListState,
    jobs: Vec<ListedJob>,
    next_cursor: Option<String>,
}

struct Lister<'a> {
    client: &'a SwitchboardClient,
    include: String,
    q: Option<String>,
    limit: usize,
}

impl Lister<'_> {
    async fn page(&self, state: JobListState, mut cursor: Option<String>) -> Result<Page> {
        let mut jobs = Vec::new();
        loop {
            let query = JobListQuery {
                include: self.include.clone(),
                state,
                q: self.q.clone(),
                limit: Some(self.limit.saturating_sub(jobs.len()).clamp(1, PAGE_MAX) as u32),
                cursor,
            };
            let page = self
                .client
                .list_jobs(&query)
                .await
                .context("listing jobs")?;
            jobs.extend(page.jobs.into_iter().map(|job| listed(state, job)));
            cursor = page.next_cursor;
            if cursor.is_none() || jobs.len() >= self.limit {
                break;
            }
        }
        Ok(Page {
            state,
            jobs,
            next_cursor: cursor,
        })
    }

    async fn pages(&self, states: &[JobListState], cursor: Option<&str>) -> Result<Vec<Page>> {
        let mut pages = Vec::new();
        for &state in states {
            pages.push(self.page(state, cursor.map(String::from)).await?);
        }
        Ok(pages)
    }
}

fn listed(state: JobListState, job: JobSummary) -> ListedJob {
    let lease_state = match (state, job.state) {
        (JobListState::Reclaimable, _) => Some(HostLeaseState::Reclaimable),
        (JobListState::Finished, _) | (_, JobState::Queued) => None,
        (JobListState::Active, _) => Some(HostLeaseState::Busy),
    };
    ListedJob { job, lease_state }
}

fn include(args: &JobListArgs) -> String {
    let include: Vec<&str> = [
        (args.mine, "mine"),
        (args.groups, "groups"),
        (args.shared, "shared"),
        (args.global, "global"),
    ]
    .into_iter()
    .filter_map(|(set, name)| set.then_some(name))
    .collect();
    if include.is_empty() {
        "all".to_string()
    } else {
        include.join(",")
    }
}

fn states(args: &JobListArgs) -> Vec<JobListState> {
    if args.finished {
        vec![JobListState::Finished]
    } else if args.reclaimable {
        vec![JobListState::Reclaimable]
    } else if args.active {
        vec![JobListState::Active]
    } else {
        vec![JobListState::Active, JobListState::Reclaimable]
    }
}

pub async fn run(ctx: &Ctx, args: &JobListArgs) -> Result<()> {
    let client = ctx.authenticated_client()?;
    let lister = Lister {
        client: &client,
        include: include(args),
        q: (!args.terms.is_empty()).then(|| args.terms.join(" ")),
        limit: args.limit,
    };
    let states = states(args);
    if args.watch {
        let mut view = View::load(ctx, &lister).await?;
        return listing::watch(
            ctx,
            async || {
                emit(ctx, &lister.pages(&states, None).await?);
                Ok(())
            },
            async || {
                let pages = lister.pages(&states, None).await?;
                view.prepare(&lister.include, &pages);
                Ok(view.sections(&pages))
            },
        )
        .await;
    }

    let pages = lister.pages(&states, args.cursor.as_deref()).await?;
    match ctx.output {
        OutputFormat::Json => {
            let jobs: Vec<&ListedJob> = pages.iter().flat_map(|page| &page.jobs).collect();
            anstream::println!("{}", serde_json::to_string_pretty(&jobs)?);
        }
        OutputFormat::Jsonl => emit(ctx, &pages),
        OutputFormat::Human => human(ctx, &lister, args, pages).await?,
    }
    Ok(())
}

fn emit(ctx: &Ctx, pages: &[Page]) {
    for page in pages {
        for job in &page.jobs {
            ctx.event(Event::ListedJob {
                lease_state: job.lease_state,
                job: &job.job,
            });
        }
        ctx.event(Event::Page {
            state: page.state,
            count: page.jobs.len(),
            next_cursor: page.next_cursor.as_deref(),
        });
    }
}

struct View {
    me: Option<Uuid>,
    sets: HashMap<Uuid, String>,
    active: Option<Uuid>,
    owners: bool,
    widths: Option<Vec<usize>>,
}

impl View {
    async fn load(ctx: &Ctx, lister: &Lister<'_>) -> Result<Self> {
        let sets = lister
            .client
            .list_image_sets()
            .await
            .context("listing images")?
            .iter()
            .map(|set| (set.id, image_name(set)))
            .collect();
        let me = match lister.include.as_str() {
            "mine" => None,
            _ => lister.client.whoami().await.ok().map(|who| who.user_id),
        };
        Ok(Self {
            me,
            sets,
            active: ctx.state.active_job,
            owners: false,
            widths: None,
        })
    }

    fn prepare(&mut self, include: &str, pages: &[Page]) {
        self.owners = include != "mine"
            && pages
                .iter()
                .flat_map(|page| &page.jobs)
                .any(|job| job.job.owner.as_ref().map(|owner| owner.id) != self.me);
        let now = chrono::Utc::now();
        let rows: Vec<Vec<Cell>> = pages
            .iter()
            .flat_map(|page| page.jobs.iter().map(move |job| (page.state, job)))
            .map(|(state, job)| self.cells(state, &job.job, now))
            .collect();
        self.widths = listing::widths(&rows);
    }

    fn cells(
        &self,
        state: JobListState,
        job: &JobSummary,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Vec<Cell> {
        let image = match &job.image.reference {
            JobImageReference::ImageSet { set_id, generation } => {
                let name = self
                    .sets
                    .get(set_id)
                    .cloned()
                    .or_else(|| job.image_name.clone())
                    .unwrap_or_else(|| short(*set_id));
                format!("{name} v{generation}")
            }
            JobImageReference::Image { manifest_digest } => manifest_digest.to_string(),
        };
        let host = match &job.host_name {
            Some(name) => plain(name.clone()),
            None => span(DIM, "—"),
        };

        let mut cells = vec![cell(plain(short(job.job_id)), 9, 9)];
        if state == JobListState::Finished {
            let outcome = match (job.task_exit_status, job.termination_reason) {
                (Some(TaskExitStatus::Success), _) => span(OK, "✓ success"),
                (Some(TaskExitStatus::Failure), _) => span(ERR, "✗ failure"),
                (_, Some(reason)) => plain(snake(&reason)),
                _ => span(DIM, "—"),
            };
            let ended = job.terminated_at.map_or("—".to_string(), |at| {
                format!("{} ago", human_duration(now - at))
            });
            let ran = match (job.started_at, job.terminated_at) {
                (Some(start), Some(end)) => format!("ran {}", human_duration(end - start)),
                _ => "—".to_string(),
            };
            cells.extend([
                cell(outcome, 18, 18),
                cell(plain(image), 0, 28),
                cell(host, 0, 16),
                cell(span(DIM, ended), 11, 11),
                cell(span(DIM, ran), 10, 10),
            ]);
        } else {
            let style = match job.state {
                JobState::Ready => OK,
                JobState::Terminating => DIM,
                _ => WARN,
            };
            cells.extend([
                cell(span(style, snake(&job.state)), 12, 12),
                cell(plain(image), 0, 28),
                cell(host, 0, 16),
                cell(span(DIM, lease(job, now)), 14, 14),
            ]);
        }
        if self.owners {
            let owner = match &job.owner {
                Some(owner) => plain(owner.name.clone().unwrap_or_else(|| snake(&owner.kind))),
                None => span(DIM, "—"),
            };
            cells.push(cell(owner, 0, 14));
        }
        cells.push(cell(
            plain(job.label.clone().unwrap_or_default()),
            0,
            usize::MAX,
        ));
        cells
    }

    fn rows(&mut self, state: JobListState, jobs: &[ListedJob]) -> Vec<Line> {
        let now = chrono::Utc::now();
        let rows: Vec<Vec<Cell>> = jobs
            .iter()
            .map(|job| self.cells(state, &job.job, now))
            .collect();
        if self.widths.is_none() {
            self.widths = listing::widths(&rows);
        }
        let widths = self.widths.clone().unwrap_or_default();
        let mut lines = Vec::new();
        for (job, row) in jobs.iter().zip(rows) {
            let active = Some(job.job.job_id) == self.active;
            let mut line = if active {
                vec![span(ACCENT, "▸"), plain(" ")]
            } else {
                vec![plain("  ")]
            };
            line.extend(listing::line(row, &widths));
            lines.push(line);
            if active {
                lines.push(vec![span(DIM, "    active job")]);
            }
        }
        lines
    }

    fn section(&mut self, page: &Page, always: bool) -> Vec<Line> {
        if page.jobs.is_empty() && !always {
            return Vec::new();
        }
        let title = match page.state {
            JobListState::Active => "Active",
            JobListState::Reclaimable => "Reclaimable",
            JobListState::Finished => "Finished",
        };
        let mut header = vec![span(BOLD, title)];
        if page.next_cursor.is_none() {
            header.push(span(DIM, format!(" · {}", page.jobs.len())));
        }
        let mut lines = vec![header];
        if page.jobs.is_empty() {
            lines.push(vec![span(
                DIM,
                match page.state {
                    JobListState::Active => "  nothing running",
                    _ => "  none",
                },
            )]);
        }
        lines.extend(self.rows(page.state, &page.jobs));
        lines
    }

    fn sections(&mut self, pages: &[Page]) -> Vec<Line> {
        let mut lines = Vec::new();
        for (i, page) in pages.iter().enumerate() {
            let section = self.section(page, i == 0);
            if !section.is_empty() && !lines.is_empty() {
                lines.push(Vec::new());
            }
            lines.extend(section);
        }
        lines
    }
}

fn lease(job: &JobSummary, now: chrono::DateTime<chrono::Utc>) -> String {
    match (job.state, job.lease_expires_at) {
        (JobState::Queued, _) => format!("queued {}", human_duration(now - job.queued_at)),
        (JobState::Terminating, _) => "stopping".to_string(),
        (_, Some(at)) if at > now => format!("{} left", human_duration(at - now)),
        (_, Some(at)) => format!("ended {} ago", human_duration(now - at)),
        (_, None) => "—".to_string(),
    }
}

fn section_flag(state: JobListState) -> &'static str {
    match state {
        JobListState::Active => "--active",
        JobListState::Reclaimable => "--reclaimable",
        JobListState::Finished => "--finished",
    }
}

fn more_command(args: &JobListArgs, state: JobListState, cursor: &str) -> String {
    let mut words = vec![format!("{} job list", program())];
    for (set, flag) in [
        (args.mine, "--mine"),
        (args.groups, "--groups"),
        (args.shared, "--shared"),
        (args.global, "--global"),
    ] {
        if set {
            words.push(flag.to_string());
        }
    }
    words.push(section_flag(state).to_string());
    if args.limit != 50 {
        words.push(format!("--limit {}", args.limit));
    }
    words.push(format!("--cursor {}", shell_quote(cursor)));
    words.extend(args.terms.iter().map(|term| shell_quote(term)));
    words.join(" ")
}

async fn human(ctx: &Ctx, lister: &Lister<'_>, args: &JobListArgs, pages: Vec<Page>) -> Result<()> {
    let mut view = View::load(ctx, lister).await?;
    view.prepare(&lister.include, &pages);

    if let Some(mut pager) = Pager::spawn(args.no_pager) {
        let result = stream(lister, &mut view, pages, &mut pager).await;
        return pager.close(result).await;
    }

    listing::print(&view.sections(&pages));
    let dim = anstyle::Style::new().dimmed();
    for page in &pages {
        if let Some(cursor) = &page.next_cursor {
            ctx.note(&format!(
                "  {dim}… more ·{dim:#} {}",
                more_command(args, page.state, cursor)
            ));
        }
    }
    for line in footer(args) {
        ctx.note(&tui::render(&line, None, true));
    }
    Ok(())
}

fn footer(args: &JobListArgs) -> Vec<Line> {
    if args.finished {
        return Vec::new();
    }
    let tml = program();
    display::commands(
        "Related commands:",
        &[
            (format!("{tml} job status --job <JOB>"), "show a job"),
            (format!("{tml} job list --finished"), "list finished jobs"),
        ],
    )
}

async fn stream(
    lister: &Lister<'_>,
    view: &mut View,
    pages: Vec<Page>,
    pager: &mut Pager,
) -> Result<()> {
    let mut written = false;
    for (i, page) in pages.into_iter().enumerate() {
        let mut lines = view.section(&page, i == 0);
        if lines.is_empty() {
            continue;
        }
        if written {
            lines.insert(0, Vec::new());
        }
        pager.write(&lines).await?;
        written = true;
        let mut cursor = page.next_cursor;
        while let Some(next) = cursor {
            // The pager reads ahead of the reader by a pipe buffer, so pace
            // the requests it would otherwise make back to back.
            tokio::time::sleep(PAGE_INTERVAL).await;
            let next = lister.page(page.state, Some(next)).await?;
            pager.write(&view.rows(next.state, &next.jobs)).await?;
            cursor = next.next_cursor;
        }
    }
    Ok(())
}

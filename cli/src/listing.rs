use anyhow::{Result, bail};
use crossterm::event::KeyCode;
use std::io::IsTerminal;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::cli::OutputFormat;
use crate::ctx::Ctx;
use crate::tui::{self, DIM, Input, Line, Span, Terminal, plain, span};

pub const WATCH_INTERVAL: Duration = Duration::from_secs(3);

pub struct Cell {
    pub span: Span,
    pub min: usize,
    pub max: usize,
}

pub fn cell(span: Span, min: usize, max: usize) -> Cell {
    Cell { span, min, max }
}

pub fn widths(rows: &[Vec<Cell>]) -> Option<Vec<usize>> {
    let first = rows.first()?;
    let mut widths = vec![0; first.len()];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width)
                .max(cell.span.text.chars().count())
                .clamp(cell.min, cell.max);
        }
    }
    Some(widths)
}

pub fn line(row: Vec<Cell>, widths: &[usize]) -> Line {
    let last = row.len().saturating_sub(1);
    let mut line = Vec::new();
    for (i, cell) in row.into_iter().enumerate() {
        if i == last {
            line.push(cell.span);
        } else {
            let width = widths.get(i).copied().unwrap_or(0);
            line.push(span(cell.span.style, tui::fit(&cell.span.text, width)));
            line.push(plain("  "));
        }
    }
    line
}

pub fn print(lines: &[Line]) {
    let width = std::io::stdout()
        .is_terminal()
        .then(|| tui::width().ok())
        .flatten();
    for line in lines {
        anstream::println!("{}", tui::render(line, width, true));
    }
}

pub struct Pager {
    child: Child,
    stdin: ChildStdin,
    color: bool,
    ignore_interrupts: JoinHandle<()>,
}

impl Pager {
    pub fn spawn(no_pager: bool) -> Option<Self> {
        if no_pager || !std::io::stdout().is_terminal() {
            return None;
        }
        let pager = std::env::var("TML_PAGER")
            .or_else(|_| std::env::var("PAGER"))
            .unwrap_or_else(|_| "less".to_string());
        let program = pager.split_whitespace().next()?;
        let found = program.contains('/')
            || std::env::var_os("PATH").is_some_and(|path| {
                std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
            });
        if !found || program == "cat" {
            return None;
        }
        let mut command = Command::new("sh");
        command.arg("-c").arg(&pager).stdin(Stdio::piped());
        if std::env::var_os("LESS").is_none() {
            command.env("LESS", "FRSX");
        }
        let mut child = command.spawn().ok()?;
        let stdin = child.stdin.take()?;
        Some(Self {
            child,
            stdin,
            color: anstream::AutoStream::choice(&std::io::stdout()) != anstream::ColorChoice::Never,
            ignore_interrupts: tokio::spawn(async {
                while tokio::signal::ctrl_c().await.is_ok() {}
            }),
        })
    }

    pub async fn write(&mut self, lines: &[Line]) -> Result<()> {
        let mut text = String::new();
        for line in lines {
            text.push_str(&tui::render(line, None, self.color));
            text.push('\n');
        }
        self.stdin.write_all(text.as_bytes()).await?;
        Ok(())
    }

    pub async fn close(self, result: Result<()>) -> Result<()> {
        let Self {
            mut child,
            stdin,
            ignore_interrupts,
            ..
        } = self;
        drop(stdin);
        let _ = child.wait().await;
        ignore_interrupts.abort();
        match result {
            Err(e)
                if e.downcast_ref::<std::io::Error>().map(std::io::Error::kind)
                    == Some(std::io::ErrorKind::BrokenPipe) =>
            {
                Ok(())
            }
            result => result,
        }
    }
}

pub async fn watch(
    ctx: &Ctx,
    mut emit: impl AsyncFnMut() -> Result<()>,
    mut frame: impl AsyncFnMut() -> Result<Vec<Line>>,
) -> Result<()> {
    match ctx.output {
        OutputFormat::Json => bail!("`--watch` needs human or jsonl output"),
        OutputFormat::Jsonl => loop {
            emit().await?;
            tokio::select! {
                _ = tokio::time::sleep(WATCH_INTERVAL) => {}
                _ = tokio::signal::ctrl_c() => return Ok(()),
            }
        },
        OutputFormat::Human => {}
    }
    if !tui::interactive() {
        bail!("`--watch` needs a terminal; use `-o jsonl` for a stream");
    }

    let mut term = Terminal::open()?;
    loop {
        let mut lines = frame().await?;
        let height = tui::terminal_height()?.saturating_sub(3);
        if lines.len() > height {
            let hidden = lines.len() - height;
            lines.truncate(height.saturating_sub(1));
            lines.push(vec![span(DIM, format!("  … {} more", hidden + 1))]);
        }
        let time = chrono::Local::now().format("%H:%M:%S");
        let mut screen = vec![
            vec![span(
                DIM,
                format!("every {}s · {time} · q: quit", WATCH_INTERVAL.as_secs()),
            )],
            Vec::new(),
        ];
        screen.extend(lines.iter().cloned());

        let deadline = Instant::now() + WATCH_INTERVAL;
        loop {
            term.draw(&screen, None)?;
            tokio::select! {
                input = term.input() => match input? {
                    Input::Interrupt => {
                        term.print(&lines)?;
                        return Ok(());
                    }
                    Input::Key(key) if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) => {
                        term.print(&lines)?;
                        return Ok(());
                    }
                    _ => {}
                },
                _ = tokio::time::sleep_until(deadline) => break,
            }
        }
    }
}

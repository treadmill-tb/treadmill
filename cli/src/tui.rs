use std::io::{self, IsTerminal, Write};

use anstyle::{AnsiColor, Style};
use crossterm::cursor;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::Print;
use crossterm::terminal::{self, ClearType};
use crossterm::{execute, queue};
use futures_util::StreamExt;

pub const DIM: Style = Style::new().dimmed();
pub const BOLD: Style = Style::new().bold();
pub const ACCENT: Style = AnsiColor::Cyan.on_default();
pub const OK: Style = AnsiColor::Green.on_default();
pub const WARN: Style = AnsiColor::Yellow.on_default();
pub const ERR: Style = AnsiColor::Red.on_default();

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone)]
pub struct Span {
    pub style: Style,
    pub text: String,
}

pub type Line = Vec<Span>;

pub fn span(style: Style, text: impl Into<String>) -> Span {
    Span {
        style,
        text: text.into(),
    }
}

pub fn plain(text: impl Into<String>) -> Span {
    span(Style::new(), text)
}

pub enum Input {
    Key(KeyEvent),
    Interrupt,
    Resize,
}

pub struct Terminal {
    events: EventStream,
    color: bool,
    row: u16,
}

impl Terminal {
    pub fn open() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            events: EventStream::new(),
            color: anstream::AutoStream::choice(&io::stderr()) != anstream::ColorChoice::Never,
            row: 0,
        })
    }

    pub async fn input(&mut self) -> io::Result<Input> {
        loop {
            match self.events.next().await {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    if key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c')
                    {
                        return Ok(Input::Interrupt);
                    }
                    return Ok(Input::Key(key));
                }
                Some(Ok(Event::Resize(..))) => return Ok(Input::Resize),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e),
                None => return Err(io::ErrorKind::UnexpectedEof.into()),
            }
        }
    }

    pub fn draw(&mut self, lines: &[Line], cursor: Option<(usize, usize)>) -> io::Result<()> {
        let width = width()?;
        let mut out = io::stderr().lock();
        self.rewind(&mut out)?;
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                queue!(out, Print("\r\n"))?;
            }
            self.write_line(&mut out, line, width)?;
        }
        let last = lines.len().saturating_sub(1);
        match cursor {
            Some((row, col)) => {
                if last > row {
                    queue!(out, cursor::MoveUp((last - row) as u16))?;
                }
                queue!(out, cursor::MoveToColumn(col as u16), cursor::Show)?;
                self.row = row as u16;
            }
            None => {
                queue!(out, cursor::Hide)?;
                self.row = last as u16;
            }
        }
        out.flush()
    }

    pub fn print(&mut self, lines: &[Line]) -> io::Result<()> {
        let mut out = io::stderr().lock();
        self.rewind(&mut out)?;
        for line in lines {
            self.write_line(&mut out, line, usize::MAX)?;
            queue!(out, Print("\r\n"))?;
        }
        self.row = 0;
        out.flush()
    }

    fn rewind(&self, out: &mut impl Write) -> io::Result<()> {
        if self.row > 0 {
            queue!(out, cursor::MoveUp(self.row))?;
        }
        queue!(
            out,
            cursor::MoveToColumn(0),
            terminal::Clear(ClearType::FromCursorDown)
        )
    }

    fn write_line(&self, out: &mut impl Write, line: &Line, width: usize) -> io::Result<()> {
        out.write_all(render(line, Some(width), self.color).as_bytes())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let mut out = io::stderr();
        let _ = self.rewind(&mut out);
        let _ = execute!(out, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

pub fn interactive() -> bool {
    io::stdin().is_terminal() && io::stderr().is_terminal()
}

pub fn width() -> io::Result<usize> {
    Ok(usize::from(terminal::size()?.0).saturating_sub(1))
}

pub fn render(line: &Line, width: Option<usize>, color: bool) -> String {
    let mut out = String::new();
    let mut left = width.unwrap_or(usize::MAX);
    for span in line {
        let first = span.text.lines().next().unwrap_or("");
        let text: String = first.chars().take(left).collect();
        left -= text.chars().count();
        if color {
            out.push_str(&format!(
                "{}{text}{}",
                span.style.render(),
                span.style.render_reset()
            ));
        } else {
            out.push_str(&text);
        }
        if left == 0 || first.len() < span.text.len() {
            break;
        }
    }
    out
}

pub fn interrupted() -> anyhow::Error {
    io::Error::from(io::ErrorKind::Interrupted).into()
}

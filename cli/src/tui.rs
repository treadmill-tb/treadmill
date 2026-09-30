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

const VISIBLE_ITEMS: usize = 8;

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

pub fn answered(title: &str, answer: Line) -> Line {
    let mut line = vec![span(OK, "✓ "), span(BOLD, title), plain("  ")];
    line.extend(answer);
    line
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
    above: Vec<Line>,
}

impl Terminal {
    pub fn open() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self {
            events: EventStream::new(),
            color: anstream::AutoStream::choice(&io::stderr()) != anstream::ColorChoice::Never,
            row: 0,
            above: Vec::new(),
        })
    }

    pub fn set_above(&mut self, lines: Vec<Line>) {
        self.above = lines;
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
        let height = usize::from(terminal::size()?.1).saturating_sub(1);
        let skip = (self.above.len() + lines.len()).saturating_sub(height);
        let above = &self.above[skip.min(self.above.len())..];
        let offset = above.len();
        let lines: Vec<&Line> = above.iter().chain(lines).collect();
        let cursor = cursor.map(|(row, col)| (row + offset, col));
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

pub fn terminal_height() -> io::Result<usize> {
    Ok(usize::from(terminal::size()?.1))
}

pub fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        format!("{text:<width$}")
    } else {
        let mut text: String = text.chars().take(width.saturating_sub(1)).collect();
        text.push('…');
        text
    }
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

#[derive(Default)]
pub struct LineEdit {
    chars: Vec<char>,
    pos: usize,
}

impl LineEdit {
    pub fn new(text: &str) -> Self {
        let chars: Vec<char> = text.chars().collect();
        Self {
            pos: chars.len(),
            chars,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn handle(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.pos = 0,
            KeyCode::Char('e') if ctrl => self.pos = self.chars.len(),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.pos);
                self.pos = 0;
                return true;
            }
            KeyCode::Char('w') if ctrl => {
                let mut start = self.pos;
                while start > 0 && self.chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.chars[start - 1] != ' ' {
                    start -= 1;
                }
                self.chars.drain(start..self.pos);
                self.pos = start;
                return true;
            }
            KeyCode::Char(c) if !ctrl => {
                self.chars.insert(self.pos, c);
                self.pos += 1;
                return true;
            }
            KeyCode::Backspace if self.pos > 0 => {
                self.pos -= 1;
                self.chars.remove(self.pos);
                return true;
            }
            KeyCode::Delete if self.pos < self.chars.len() => {
                self.chars.remove(self.pos);
                return true;
            }
            KeyCode::Left => self.pos = self.pos.saturating_sub(1),
            KeyCode::Right => self.pos = (self.pos + 1).min(self.chars.len()),
            KeyCode::Home => self.pos = 0,
            KeyCode::End => self.pos = self.chars.len(),
            _ => {}
        }
        false
    }

    fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let start = (self.pos + 1).saturating_sub(width);
        let text = self.chars[start..].iter().take(width).collect();
        (text, self.pos - start)
    }
}

pub fn prompt_line(title: &str, edit: &LineEdit, placeholder: &str) -> (Line, usize) {
    let prefix = format!("? {title} › ");
    let prefix_width = prefix.chars().count();
    let (text, col) = edit.view(width().unwrap_or(80).saturating_sub(prefix_width));
    let mut line = vec![span(ACCENT, "? "), span(BOLD, title), span(DIM, " › ")];
    if text.is_empty() {
        line.push(span(DIM, placeholder));
    } else {
        line.push(plain(text));
    }
    (line, prefix_width + col)
}

pub struct Item {
    pub line: Line,
    search: String,
    show: Show,
}

#[derive(PartialEq)]
enum Show {
    Always,
    Browsing,
    Searching,
}

impl Item {
    pub fn new(line: Line, search: impl Into<String>) -> Self {
        Self {
            line,
            search: search.into().to_lowercase(),
            show: Show::Always,
        }
    }

    pub fn action(line: Line) -> Self {
        Self {
            line,
            search: String::new(),
            show: Show::Browsing,
        }
    }

    pub fn hidden(line: Line, search: impl Into<String>) -> Self {
        Self {
            show: Show::Searching,
            ..Self::new(line, search)
        }
    }

    fn matches(&self, terms: &[String]) -> bool {
        match (&self.show, terms.is_empty()) {
            (Show::Browsing, browsing) => browsing,
            (Show::Searching, true) => false,
            _ => terms.iter().all(|term| self.search.contains(term.as_str())),
        }
    }
}

pub async fn select(
    term: &mut Terminal,
    title: &str,
    header: &[Line],
    items: &[Item],
    default: usize,
    filter: bool,
) -> anyhow::Result<Option<usize>> {
    let mut query = LineEdit::default();
    let mut selected = default;
    let mut offset = 0;
    loop {
        let terms: Vec<String> = query
            .text()
            .to_lowercase()
            .split_whitespace()
            .map(String::from)
            .collect();
        let shown: Vec<usize> = (0..items.len())
            .filter(|&i| items[i].matches(&terms))
            .collect();
        let mut position = shown.iter().position(|&i| i == selected).unwrap_or(0);
        if let Some(&i) = shown.get(position) {
            selected = i;
        }
        offset = offset
            .min(position)
            .max((position + 1).saturating_sub(VISIBLE_ITEMS));

        let (first, cursor_col) = if filter {
            prompt_line(title, &query, "type to filter")
        } else {
            (vec![span(ACCENT, "? "), span(BOLD, title)], 0)
        };
        let mut lines = vec![first];
        lines.extend(header.iter().cloned());
        for &i in shown.iter().skip(offset).take(VISIBLE_ITEMS) {
            let mut line = if i == selected {
                vec![span(ACCENT, "❯ ")]
            } else {
                vec![plain("  ")]
            };
            line.extend(items[i].line.iter().map(|s| {
                let style = if i == selected {
                    s.style.fg_color(Some(AnsiColor::Cyan.into()))
                } else {
                    s.style
                };
                span(style, s.text.clone())
            }));
            lines.push(line);
        }
        if shown.is_empty() {
            lines.push(vec![span(DIM, "  no matches")]);
        }
        let below = shown.len().saturating_sub(offset + VISIBLE_ITEMS);
        if below > 0 {
            lines.push(vec![span(DIM, format!("  … {below} more"))]);
        }
        term.draw(&lines, filter.then_some((0, cursor_col)))?;

        match term.input().await? {
            Input::Interrupt => return Err(interrupted()),
            Input::Resize => {}
            Input::Key(key) => match key.code {
                KeyCode::Enter if !shown.is_empty() => return Ok(Some(selected)),
                KeyCode::Esc => return Ok(None),
                KeyCode::Up | KeyCode::BackTab => {
                    position = position
                        .checked_sub(1)
                        .unwrap_or(shown.len().saturating_sub(1));
                    if let Some(&i) = shown.get(position) {
                        selected = i;
                    }
                }
                KeyCode::Down | KeyCode::Tab => {
                    position = if position + 1 >= shown.len() {
                        0
                    } else {
                        position + 1
                    };
                    if let Some(&i) = shown.get(position) {
                        selected = i;
                    }
                }
                _ if filter => {
                    query.handle(&key);
                }
                _ => {}
            },
        }
    }
}

pub async fn input(
    term: &mut Terminal,
    title: &str,
    initial: &str,
    placeholder: &str,
    validate: impl Fn(&str) -> Result<(), String>,
) -> anyhow::Result<Option<String>> {
    let mut edit = LineEdit::new(initial);
    let mut error = None;
    loop {
        let (line, col) = prompt_line(title, &edit, placeholder);
        let mut lines = vec![line];
        if let Some(error) = &error {
            lines.push(vec![span(ERR, format!("  {error}"))]);
        }
        term.draw(&lines, Some((0, col)))?;

        match term.input().await? {
            Input::Interrupt => return Err(interrupted()),
            Input::Resize => {}
            Input::Key(key) => match key.code {
                KeyCode::Esc => return Ok(None),
                KeyCode::Enter => {
                    let text = edit.text().trim().to_string();
                    match validate(&text) {
                        Ok(()) => return Ok(Some(text)),
                        Err(e) => error = Some(e),
                    }
                }
                _ => {
                    if edit.handle(&key) {
                        error = None;
                    }
                }
            },
        }
    }
}

pub async fn ask(title: &str) -> anyhow::Result<bool> {
    let mut term = Terminal::open()?;
    Ok(confirm(&mut term, title, false).await? == Some(true))
}

pub async fn confirm(
    term: &mut Terminal,
    title: &str,
    default: bool,
) -> anyhow::Result<Option<bool>> {
    let hint = if default { " › Y/n" } else { " › y/N" };
    loop {
        let line = vec![span(ACCENT, "? "), span(BOLD, title), span(DIM, hint)];
        term.draw(&[line], None)?;
        match term.input().await? {
            Input::Interrupt => return Err(interrupted()),
            Input::Key(key) => match key.code {
                KeyCode::Enter => return Ok(Some(default)),
                KeyCode::Char('y') | KeyCode::Char('Y') => return Ok(Some(true)),
                KeyCode::Char('n') | KeyCode::Char('N') => return Ok(Some(false)),
                KeyCode::Esc => return Ok(None),
                _ => {}
            },
            Input::Resize => {}
        }
    }
}

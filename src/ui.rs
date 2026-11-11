//! The ratatui front end: streaming transcript, tool cards, status bar, steering input.
//! The turn runs on a worker thread; keys reach it through `Controls`.
//! Slash commands (`/help`, `/tools`, ...) run in the UI thread and never take the session
//! or history lock, so they are refused while a turn is running.

use std::io::{Result as IoResult, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::agent::{Agent, Controls, Sink};
use crate::config::Config;
use crate::llm::Adapter;
use crate::messages::{Message, ToolCall, estimate_tokens};
use crate::plugin::PluginHost;
use crate::session::{list as list_sessions, replay, resolve, Session};
use crate::tools::{Registry, ToolResult};

/// Transcript rows kept in memory. Older rows drop from the top.
const MAX_ROWS: usize = 3000;

#[derive(Clone, Debug)]
pub enum Row {
    User(String),
    Text(String),
    Call(String),
    Result(String),
    Note(String),
    /// Something that went wrong: shown in red, with what to do next.
    Error(String),
}

impl Row {
    fn line(&self) -> String {
        match self {
            Row::User(t) => format!("> {t}"),
            Row::Text(t) => t.clone(),
            Row::Call(t) => format!("-> {t}"),
            Row::Result(t) => format!("<- {t}"),
            Row::Note(t) => format!("# {t}"),
            Row::Error(t) => format!("! {t}"),
        }
    }

    /// The color that tells a row apart at a glance.
    fn color(&self) -> ratatui::style::Color {
        use ratatui::style::Color;
        match self {
            Row::User(_) => Color::Cyan,
            Row::Text(_) => Color::White,
            Row::Call(_) => Color::Yellow,
            Row::Result(_) => Color::Green,
            Row::Note(_) => Color::DarkGray,
            Row::Error(_) => Color::Red,
        }
    }
}

/// What a typed `/command` (or `:command`) asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Help,
    Quit,
    Tools,
    ShowModel,
    SetModel(String),
    Hist,
    Compact,
    Sessions,
    Resume(String),
    New,
    Plugin(String),
    Unknown(String),
}

/// Commands a person can type, in help order. Tab completes against this list.
pub const COMMANDS: &[&str] = &[
    "help",
    "quit",
    "tools",
    "model",
    "hist",
    "compact",
    "sessions",
    "resume",
    "new",
    "plugin",
];

/// Accept `/verb args` and `:verb args`. An empty verb is `help`.
pub fn parse_command(text: &str) -> Command {
    let t = text.trim();
    let body = t.strip_prefix('/').or_else(|| t.strip_prefix(':')).unwrap_or(t);
    let (verb, rest) = match body.split_once(char::is_whitespace) {
        Some((v, r)) => (v, r.trim()),
        None => (body, ""),
    };
    match verb {
        "" | "help" | "?" => Command::Help,
        "quit" | "exit" | "q" => Command::Quit,
        "tools" => Command::Tools,
        "model" if rest.is_empty() => Command::ShowModel,
        "model" => Command::SetModel(rest.to_string()),
        "hist" => Command::Hist,
        "compact" => Command::Compact,
        "sessions" => Command::Sessions,
        "resume" if rest.is_empty() => Command::Resume("last".to_string()),
        "resume" => Command::Resume(rest.to_string()),
        "new" => Command::New,
        "plugin" => Command::Plugin(rest.to_string()),
        other => Command::Unknown(other.to_string()),
    }
}

/// The command a mistyped one was most likely aiming at, e.g. `/hepl` -> `/help`.
pub fn suggested_command(name: &str) -> Option<&'static str> {
    crate::closest(name, COMMANDS.iter().copied())
}

/// The candidates for Tab completion, given the text after `/` or `:`.
pub fn complete(input: &str) -> Vec<String> {
    let body = input
        .trim()
        .strip_prefix('/')
        .or_else(|| input.trim().strip_prefix(':'))
        .unwrap_or("");
    if body.is_empty() {
        return COMMANDS.iter().map(|c| format!("/{c}")).collect();
    }
    COMMANDS
        .iter()
        .filter(|c| c.starts_with(body))
        .map(|c| format!("/{c}"))
        .collect()
}

/// One line of `/help`. Kept short so it fits a phone screen.
pub fn help_lines() -> Vec<String> {
    vec![
        "keys: Enter sends · Enter while running steers · Ctrl+C stops a run, quits when idle".to_string(),
        "keys: Ctrl+D quits · PgUp/PgDown scroll the transcript · Up/Down recall past input".to_string(),
        "keys: Left/Right/Home/End move the cursor · Backspace/Delete edit at it · Tab completes a command".to_string(),
        "commands: /help /quit /tools /model <name> /hist /compact /sessions /resume <id>|last /new".to_string(),
        "commands: /plugin list|load|unload|reload - applies at the next turn, never mid-request".to_string(),
        "setup: api_key lives in ~/.config/neon/config.yaml (mode 0600) · no key: run with --fixture <file>".to_string(),
        "flags: neon --plain for a dumb terminal · neon -p \"task\" for one task · neon --sessions to list sessions".to_string(),
    ]
}

#[derive(Default)]
pub struct Shared {
    rows: Mutex<Vec<Row>>,
    live: Mutex<String>,
    note_status: Mutex<String>,
    running: AtomicBool,
    tokens: AtomicUsize,
    /// Lines of scrollback: 0 means the newest line sits at the bottom.
    scroll: AtomicUsize,
    /// A `/model` change the worker writes to the session log at the start of the next turn.
    pending_model: Mutex<Option<String>>,
    /// When the current turn started, for the spinner.
    started: Mutex<Option<Instant>>,
}

impl Shared {
    fn push(&self, row: Row) {
        let mut rows = self.rows.lock().expect("rows lock");
        rows.push(row);
        let extra = rows.len().saturating_sub(MAX_ROWS);
        if extra > 0 {
            rows.drain(0..extra);
        }
    }

    fn flush_live(&self) {
        let text = self.live.lock().expect("live lock").drain(..).collect::<String>();
        if !text.is_empty() {
            self.push(Row::Text(text));
        }
    }

    fn scroll_up(&self, lines: usize) {
        let cur = self.scroll.load(Ordering::Relaxed);
        self.scroll.store(cur + lines.max(1), Ordering::Relaxed);
    }

    fn scroll_down(&self, lines: usize) {
        let cur = self.scroll.load(Ordering::Relaxed);
        self.scroll.store(cur.saturating_sub(lines.max(1)), Ordering::Relaxed);
    }

    fn reset_scroll(&self) {
        self.scroll.store(0, Ordering::Relaxed);
    }
}

pub struct UiSink {
    shared: Arc<Shared>,
}

impl Sink for UiSink {
    fn text(&self, chunk: &str) {
        self.shared.live.lock().expect("live lock").push_str(chunk);
    }

    fn tool_call(&self, call: &ToolCall) {
        self.shared.flush_live();
        self.shared
            .push(Row::Call(format!("{} {}", call.name, compact(&call.args))));
    }

    fn tool_result(&self, _call: &ToolCall, result: &ToolResult) {
        let head = result
            .content
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string();
        let extra = if result.is_error { " [error]" } else { "" };
        let more = match result.content.lines().count() {
            n if n > 1 => format!(" ({} lines)", n),
            _ => String::new(),
        };
        let row = if result.is_error {
            Row::Error(format!("{head}{extra}{more}"))
        } else {
            Row::Result(format!("{head}{extra}"))
        };
        self.shared.push(row);
    }

    fn note(&self, msg: &str) {
        self.shared.push(Row::Note(msg.to_string()));
    }
}

fn compact(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(120).collect();
        format!("{cut}... {} chars", text.chars().count())
    } else {
        text
    }
}

/// Greedy word wrap: split on newlines, then pack words into width-sized lines.
/// Words longer than the width are hard-broken, so nothing is ever dropped.
fn wrap_to(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out: Vec<String> = Vec::new();
    for seg in text.split('\n') {
        if seg.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in seg.split_whitespace() {
            let mut w = word.to_string();
            while w.chars().count() > width {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                out.push(w.chars().take(width).collect());
                w = w.chars().skip(width).collect();
            }
            if line.is_empty() {
                line = w;
            } else if line.chars().count() + 1 + w.chars().count() <= width {
                line.push(' ');
                line.push_str(&w);
            } else {
                out.push(std::mem::take(&mut line));
                line = w;
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    out
}

/// The typed line with a movable cursor. Indexes are char counts, not bytes.
#[derive(Default)]
pub struct LineInput {
    pub text: String,
    pub cursor: usize,
}

impl LineInput {
    fn char_len(&self) -> usize {
        self.text.chars().count()
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.text.chars().take(chars).map(|c| c.len_utf8()).sum()
    }

    pub fn insert(&mut self, ch: char) {
        let off = self.byte_at(self.cursor);
        self.text.insert(off, ch);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        let off = self.byte_at(self.cursor);
        self.text.replace_range(off..off, s);
        self.cursor += s.chars().count();
    }

    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn right(&mut self) {
        if self.cursor < self.char_len() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_len();
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let from = self.byte_at(self.cursor - 1);
            let to = self.byte_at(self.cursor);
            self.text.replace_range(from..to, "");
            self.cursor -= 1;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.char_len() {
            let from = self.byte_at(self.cursor);
            let to = self.byte_at(self.cursor + 1);
            self.text.replace_range(from..to, "");
        }
    }

    /// The slice that fits in `width` columns, plus the cursor column inside it.
    pub fn visible(&self, width: usize) -> (String, usize) {
        if width == 0 {
            return (String::new(), 0);
        }
        let len = self.char_len();
        if len <= width {
            return (self.text.clone(), self.cursor);
        }
        let mut start = if self.cursor + 1 > width {
            self.cursor + 1 - width
        } else {
            0
        };
        let end = (start + width).min(len);
        if end - start < width {
            start = end.saturating_sub(width);
        }
        let slice: String = self.text.chars().skip(start).take(end - start).collect();
        (slice, self.cursor - start)
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }
}

/// A foreground color as a style.
fn fg(color: ratatui::style::Color) -> ratatui::style::Style {
    ratatui::style::Style::default().fg(color)
}

const SPINNER: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn status_line(cfg: &Config, cwd: &PathBuf, shared: &Shared, registry: &Registry, tick: usize) -> String {
    let tokens = shared.tokens.load(Ordering::Relaxed);
    let budget = cfg.context_budget_tokens.max(1);
    let pct = (tokens * 100) / budget;
    let state = if shared.running.load(Ordering::Relaxed) {
        let secs = shared
            .started
            .lock()
            .expect("started lock")
            .map(|t| t.elapsed().as_secs())
            .unwrap_or(0);
        format!("{} {}s", SPINNER[tick % SPINNER.len()], secs)
    } else {
        "idle".to_string()
    };
    let extra = shared.note_status.lock().expect("status lock").clone();
    let plugins = registry.plugin_rows().len();
    let folder = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.display().to_string());
    let scroll = shared.scroll.load(Ordering::Relaxed);
    let key = if cfg.api_key.trim().is_empty() {
        " · no api_key"
    } else {
        ""
    };
    let near = if pct >= 80 { " · near budget, /compact" } else { "" };
    let scrolled = if scroll > 0 {
        format!(" · ↑{} scrolled (PgDown to latest)", scroll)
    } else {
        String::new()
    };
    format!(
        "{} · {folder} · {state} · {} tok {}%{}{} · {plugins} plugins{} · {extra}",
        cfg.model(),
        tokens,
        pct,
        key,
        near,
        scrolled,
    )
}

fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    shared: &Shared,
    input: &LineInput,
    status: &str,
) -> IoResult<()> {
    terminal.draw(|frame| {
        let areas = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(frame.area());

        let width = areas[1].width.max(1) as usize;
        let height = areas[1].height.max(1) as usize;

        // Wrap each row, keep its color, then take the window the scroll offset asks for.
        let mut lines: Vec<Line<'static>> = Vec::new();
        for row in shared.rows.lock().expect("rows lock").iter() {
            for l in wrap_to(&row.line(), width) {
                lines.push(Line::from(Span::styled(l, fg(row.color()))));
            }
        }
        let live = shared.live.lock().expect("live lock").clone();
        if !live.is_empty() {
            for l in wrap_to(&live, width) {
                lines.push(Line::from(Span::styled(l, fg(ratatui::style::Color::White))));
            }
        }

        let total = lines.len();
        let scroll = shared.scroll.load(Ordering::Relaxed).min(total.saturating_sub(1));
        let bottom = total.saturating_sub(scroll);
        let top = bottom.saturating_sub(height);
        let window: Vec<Line<'static>> = lines[top..bottom].to_vec();

        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                status.to_string(),
                fg(ratatui::style::Color::Blue).bold(),
            ))),
            areas[0],
        );
        frame.render_widget(Paragraph::new(window), areas[1]);

        let input_width = areas[2].width.saturating_sub(2) as usize;
        let (visible, col) = input.visible(input_width);
        let hint = if shared.running.load(Ordering::Relaxed) {
            "Enter steers · Ctrl+C stops the run"
        } else {
            "Enter sends · /help shows keys and commands · Ctrl+C quits"
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(
                    "> ",
                    fg(if shared.running.load(Ordering::Relaxed) {
                        ratatui::style::Color::Yellow
                    } else {
                        ratatui::style::Color::Cyan
                    }),
                ),
                Span::styled(visible, fg(ratatui::style::Color::White)),
                Span::styled(format!("  {hint}"), fg(ratatui::style::Color::DarkGray)),
            ])),
            areas[2],
        );
        frame.set_cursor_position((areas[2].x + 2 + col as u16, areas[2].y));
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{complete, help_lines, parse_command, suggested_command, wrap_to, Command, LineInput, Row, Shared};

    #[test]
    fn cursor_moves_and_edits_at_position() {
        let mut li = LineInput::default();
        li.insert_str("ab cd");
        li.left();
        li.left();
        li.insert('x');
        assert_eq!(li.text, "ab xcd");
        assert_eq!(li.cursor, 4); // cursor sits after the inserted char
        li.backspace();
        assert_eq!(li.text, "ab cd");
        li.home();
        li.delete();
        assert_eq!(li.text, "b cd");
        li.end();
        assert_eq!(li.cursor, 4);
    }

    #[test]
    fn long_input_windows_around_the_cursor() {
        let mut li = LineInput::default();
        li.insert_str(&"abcdefghij".repeat(3));
        li.home();
        let (slice, col) = li.visible(10);
        assert_eq!(slice.chars().count(), 10);
        assert_eq!(col, 0);
        li.cursor = 25;
        let (slice, col) = li.visible(10);
        assert_eq!(slice.chars().count(), 10);
        assert_eq!(col, 9);
        assert!(slice.contains("abcdef"));
    }

    #[test]
    fn long_text_wraps_instead_of_being_cut() {
        let text = "alpha ".repeat(20) + "ENDWORD";
        let lines = wrap_to(&text, 40);
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
        assert!(lines.last().unwrap().contains("ENDWORD")); // nothing past the width is lost
    }

    #[test]
    fn newlines_kept_and_long_words_hard_broken() {
        let lines = wrap_to(&format!("short\nxsaurus\n{}", "z".repeat(95)), 10);
        assert_eq!(lines[0], "short");
        assert_eq!(lines[1], "xsaurus");
        assert_eq!(lines[2..].concat().chars().count(), 95);
        assert!(lines.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn commands_read_with_slash_or_colon() {
        assert_eq!(parse_command("/help"), Command::Help);
        assert_eq!(parse_command(":quit"), Command::Quit);
        assert_eq!(parse_command(""), Command::Help);
        assert_eq!(parse_command("/model gemini-3.5-flash"), Command::SetModel("gemini-3.5-flash".to_string()));
        assert_eq!(parse_command("/model"), Command::ShowModel);
        assert_eq!(parse_command("/resume"), Command::Resume("last".to_string()));
        assert_eq!(parse_command("/resume 7f-2-0"), Command::Resume("7f-2-0".to_string()));
        assert_eq!(parse_command("/plugin list"), Command::Plugin("list".to_string()));
        assert_eq!(parse_command("/nope"), Command::Unknown("nope".to_string()));
    }

    #[test]
    fn tab_completion_lists_the_matches() {
        assert_eq!(complete("/pl"), vec!["/plugin"]);
        assert_eq!(complete(":ne"), vec!["/new"]);
        assert!(complete("/").len() == super::COMMANDS.len());
        let m = complete("/");
        assert!(m.contains(&"/help".to_string()));
        assert!(complete("/zzz").is_empty());
    }

    #[test]
    fn help_names_every_command_once() {
        let text = help_lines().join(" ");
        for c in super::COMMANDS {
            assert!(text.contains(&format!("/{c}")), "/{c} missing from help");
        }
    }

    #[test]
    fn scroll_moves_and_resets() {
        let s = Shared::default();
        s.scroll_up(5);
        assert_eq!(s.scroll.load(std::sync::atomic::Ordering::Relaxed), 5);
        s.scroll_down(2);
        assert_eq!(s.scroll.load(std::sync::atomic::Ordering::Relaxed), 3);
        s.scroll_down(10);
        assert_eq!(s.scroll.load(std::sync::atomic::Ordering::Relaxed), 0);
        s.reset_scroll();
    }

    #[test]
    fn transcript_keeps_the_newest_rows() {
        let s = Shared::default();
        for i in 0..(super::MAX_ROWS + 50) {
            s.push(Row::Text(format!("line {i}")));
        }
        let rows = s.rows.lock().unwrap();
        assert_eq!(rows.len(), super::MAX_ROWS);
        assert!(matches!(&rows[0], Row::Text(t) if t == "line 50")); // oldest 50 dropped
    }

    #[test]
    fn a_mistyped_command_names_the_one_it_meant() {
        assert_eq!(suggested_command("hepl"), Some("help"));
        assert_eq!(suggested_command("modl"), Some("model"));
        assert_eq!(suggested_command("totally-different"), None);
    }

    #[test]
    fn error_rows_read_in_red() {
        assert_eq!(Row::Error("boom".to_string()).line(), "! boom");
        assert_eq!(Row::Error("boom".to_string()).color(), ratatui::style::Color::Red);
    }
}

/// The state the UI thread owns: transcript, config, session handle, and the turn launcher.
struct UiState {
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    history: Arc<Mutex<Vec<Message>>>,
    controls: Arc<Controls>,
    cwd: PathBuf,
    log: Arc<Session>,
    shared: Arc<Shared>,
}

impl UiState {
    /// Run a command in the UI thread. Anything that touches history or the session log
    /// is refused while a turn runs, because the worker owns those for the whole turn.
    fn handle(&mut self, cmd: Command) -> Run {
        let running = self.shared.running.load(Ordering::Relaxed);
        match cmd {
            Command::Help => {
                for l in help_lines() {
                    self.shared.push(Row::Note(l));
                }
                Run::Stay
            }
            Command::Quit => Run::Stop,
            Command::Tools => {
                for s in self.registry.schemas() {
                    let row = if s.plugin_row.is_empty() {
                        "core".to_string()
                    } else {
                        s.plugin_row.clone()
                    };
                    self.shared
                        .push(Row::Note(format!("- {} [{row}] {}", s.name, s.description)));
                }
                Run::Stay
            }
            Command::ShowModel => {
                self.shared.push(Row::Note(format!(
                    "model {} · change it with /model <name> or --model <name>",
                    self.cfg.model()
                )));
                Run::Stay
            }
            Command::SetModel(_) if running => {
                self.shared.push(Row::Error(
                    "a turn is running - Ctrl+C stops it, then /model works".to_string(),
                ));
                Run::Stay
            }
            Command::SetModel(name) => {
                let old = self.cfg.model();
                self.cfg.set_model(&name);
                // The log entry is written by the worker at the next turn, not here.
                *self.shared.pending_model.lock().expect("pending model lock") = Some(name.clone());
                self.shared.push(Row::Note(format!(
                    "model {old} -> {name} · the session log records it at the next turn"
                )));
                Run::Stay
            }
            Command::Hist if running => {
                self.shared.push(Row::Note(
                    "a turn is running - /hist waits for it, Ctrl+C stops it".to_string(),
                ));
                Run::Stay
            }
            Command::Hist => {
                let hist = self.history.lock().expect("history lock");
                let tokens = estimate_tokens(&hist);
                let budget = self.cfg.context_budget_tokens.max(1);
                self.shared.push(Row::Note(format!(
                    "{} messages · {} tok of {} budget ({}%) · logged in {}",
                    hist.len(),
                    tokens,
                    budget,
                    (tokens * 100) / budget,
                    self.log.path.display()
                )));
                self.shared.push(Row::Note(
                    "run /compact to summarize the oldest messages · /sessions opens an older session"
                        .to_string(),
                ));
                Run::Stay
            }
            Command::Compact if running => {
                self.shared.push(Row::Error(
                    "a turn is running - Ctrl+C stops it, then /compact works".to_string(),
                ));
                Run::Stay
            }
            Command::Compact => {
                self.shared.running.store(true, Ordering::Relaxed);
                *self.shared.started.lock().expect("started lock") = Some(Instant::now());
                let worker = CompactWorker {
                    adapter: self.adapter.clone(),
                    registry: self.registry.clone(),
                    plugins: self.plugins.clone(),
                    cfg: self.cfg.clone(),
                    history: self.history.clone(),
                    log: self.log.clone(),
                    controls: self.controls.clone(),
                    shared: self.shared.clone(),
                    cwd: self.cwd.clone(),
                };
                thread::spawn(move || worker.run());
                self.shared.push(Row::Note("compacting...".to_string()));
                Run::Stay
            }
            Command::Sessions => {
                match list_sessions(&self.cfg.sessions_dir, 6) {
                    Ok(rows) => {
                        if rows.is_empty() {
                            self.shared.push(Row::Note(format!(
                                "no sessions in {} yet",
                                self.cfg.sessions_dir.display()
                            )));
                        } else {
                            self.shared.push(Row::Note(format!(
                                "{} sessions in {} · newest first · /resume <id> opens one",
                                rows.len(),
                                self.cfg.sessions_dir.display()
                            )));
                            for r in rows {
                                self.shared.push(Row::Note(format!(
                                    "- {} · {} · {} entries · {} · {}",
                                    r.id,
                                    if r.model.is_empty() { "?" } else { &r.model },
                                    r.entries,
                                    crate::session::human_age(r.secs),
                                    r.cwd
                                )));
                            }
                        }
                    }
                    Err(e) => self.shared.push(Row::Error(format!("sessions: {e}"))),
                }
                Run::Stay
            }
            Command::Resume(_) if running => {
                self.shared.push(Row::Error(
                    "a turn is running - Ctrl+C stops it, then /resume works".to_string(),
                ));
                Run::Stay
            }
            Command::Resume(target) => {
                match resolve(&self.cfg.sessions_dir, &target)
                    .and_then(|id| replay(&self.cfg.sessions_dir.join(format!("{id}.jsonl")))
                        .map(|(msgs, meta)| (id, msgs, meta)))
                {
                    Ok((id, msgs, meta)) => {
                        let model = meta
                            .as_ref()
                            .and_then(|m| m.get("model"))
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        if let Some(m) = &model {
                            self.cfg.set_model(m);
                        }
                        self.log = Arc::new(Session::existing(&id, &self.cfg.sessions_dir));
                        *self.history.lock().expect("history lock") = msgs;
                        self.shared.tokens.store(
                            estimate_tokens(&self.history.lock().expect("history lock")),
                            Ordering::Relaxed,
                        );
                        self.shared.reset_scroll();
                        self.shared.push(Row::Note(format!(
                            "resumed session {id} · {} messages · model {} · {}",
                            self.history.lock().expect("history lock").len(),
                            self.cfg.model(),
                            self.log.path.display()
                        )));
                    }
                    Err(e) => self.shared.push(Row::Error(format!("resume: {e}"))),
                }
                Run::Stay
            }
            Command::New if running => {
                let _ = running;
                self.shared.push(Row::Error(
                    "a turn is running - Ctrl+C stops it, then /new works".to_string(),
                ));
                Run::Stay
            }
            Command::New => {
                match Session::create(&self.cfg.sessions_dir, &self.cfg.model(), &self.cwd) {
                    Ok(sess) => {
                        self.log = Arc::new(sess);
                        *self.history.lock().expect("history lock") = Vec::new();
                        self.shared.tokens.store(0, Ordering::Relaxed);
                        *self.shared.note_status.lock().expect("status lock") = String::new();
                        self.shared.reset_scroll();
                        self.shared.push(Row::Note(format!(
                            "new session {} · the model starts empty · the old log stays on disk",
                            self.log.id
                        )));
                    }
                    Err(e) => self.shared.push(Row::Error(format!("new session: {e}"))),
                }
                Run::Stay
            }
            Command::Plugin(rest) => {
                match self.plugins.lock().map(|mut host| host.handle_command(&rest)) {
                    Ok(Ok(note)) => self.shared.push(Row::Note(note)),
                    Ok(Err(e)) => self.shared.push(Row::Error(format!("plugin: {e}"))),
                    Err(_) => self.shared
                        .push(Row::Error("plugin host is busy - wait for the turn".to_string())),
                }
                Run::Stay
            }
            Command::Unknown(name) => {
                let near = suggested_command(&name);
                self.shared.push(Row::Error(format!(
                    "unknown command: {name}{} · /help lists the commands",
                    match near {
                        Some(c) => format!(" (did you mean /{c}?)"),
                        None => String::new(),
                    }
                )));
                Run::Stay
            }
        }
    }

    fn start_turn(&mut self, input: String) {
        self.shared.push(Row::User(input.clone()));
        self.shared.running.store(true, Ordering::Relaxed);
        self.shared.reset_scroll();
        *self.shared.started.lock().expect("started lock") = Some(Instant::now());
        let worker = Worker {
            adapter: self.adapter.clone(),
            registry: self.registry.clone(),
            plugins: self.plugins.clone(),
            cfg: self.cfg.clone(),
            history: self.history.clone(),
            log: self.log.clone(),
            controls: self.controls.clone(),
            shared: self.shared.clone(),
            cwd: self.cwd.clone(),
            input,
        };
        thread::spawn(move || worker.turn());
    }
}

/// What a command asks the loop to do.
#[derive(PartialEq, Eq)]
enum Run {
    Stay,
    Stop,
}

pub fn run(
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    history: Arc<Mutex<Vec<Message>>>,
    controls: Arc<Controls>,
    cwd: PathBuf,
    log: Arc<Session>,
    resumed: Option<usize>,
) -> IoResult<()> {
    enable_raw_mode()?;
    crossterm::execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout()))?;

    let shared = Arc::new(Shared::default());
    let mut state = UiState {
        adapter,
        registry,
        plugins,
        cfg,
        history,
        controls,
        cwd,
        log,
        shared: shared.clone(),
    };

    shared.push(Row::Note(format!(
        "neon ready · {} · {} tools · {} plugins loaded",
        state.cfg.model(),
        state.registry.schemas().len(),
        state.registry.plugin_rows().len()
    )));
    shared.push(Row::Note(format!(
        "session {} · folder {} · /help shows keys and commands",
        state.log.id,
        state.cwd.display()
    )));
    if let Some(n) = resumed {
        shared.push(Row::Note(format!("resumed {n} messages")));
    }
    if state.cfg.api_key.trim().is_empty() {
        shared.push(Row::Error(format!(
            "no api_key yet · add api_key to {} · or run with --fixture <file>",
            crate::config::default_path().display()
        )));
    }

    let mut input = LineInput::default();
    let mut sent_inputs: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut tick = 0usize;
    loop {
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            KeyCode::Char('c') => {
                                if shared.running.load(Ordering::Relaxed) {
                                    state.controls.abort();
                                } else {
                                    break;
                                }
                            }
                            KeyCode::Char('d') => {
                                if !shared.running.load(Ordering::Relaxed) {
                                    break;
                                }
                            }
                            _ => {}
                        }
                    } else if key.code == KeyCode::Enter {
                        let msg = input.text.clone();
                        input.clear();
                        hist_idx = None;
                        if msg.trim().is_empty() {
                            continue;
                        }
                        let is_command = msg.trim().starts_with('/') || msg.trim().starts_with(':');
                        if !is_command {
                            sent_inputs.push(msg.clone());
                            if sent_inputs.len() > 50 {
                                sent_inputs.remove(0);
                            }
                            if shared.running.load(Ordering::Relaxed) {
                                state.controls.steer(&msg);
                                shared.push(Row::User(msg));
                            } else {
                                state.start_turn(msg);
                            }
                        } else if shared.running.load(Ordering::Relaxed) {
                            // A command while running either queues or refuses; it never
                            // touches the history the worker holds.
                            shared.push(Row::User(msg.clone()));
                            match parse_command(&msg) {
                                Command::Plugin(rest) => {
                                    state.handle(Command::Plugin(rest));
                                }
                                Command::Help | Command::Tools | Command::Sessions => {
                                    state.handle(parse_command(&msg));
                                }
                                Command::Quit => {
                                    state.controls.abort();
                                    break;
                                }
                                other => {
                                    shared.push(Row::Error(format!(
                                        "{other:?} needs an idle turn · Ctrl+C stops the run"
                                    )));
                                }
                            }
                        } else {
                            shared.push(Row::User(msg.clone()));
                            if state.handle(parse_command(&msg)) == Run::Stop {
                                break;
                            }
                        }
                    } else if key.code == KeyCode::Backspace {
                        input.backspace();
                    } else if key.code == KeyCode::Delete {
                        input.delete();
                    } else if key.code == KeyCode::Left {
                        input.left();
                    } else if key.code == KeyCode::Right {
                        input.right();
                    } else if key.code == KeyCode::Home {
                        input.home();
                    } else if key.code == KeyCode::End {
                        input.end();
                    } else if key.code == KeyCode::PageUp {
                        shared.scroll_up(3);
                    } else if key.code == KeyCode::PageDown {
                        shared.scroll_down(3);
                    } else if key.code == KeyCode::Tab {
                        let candidates = complete(&input.text);
                        if candidates.len() == 1 {
                            input.clear();
                            input.insert_str(&candidates[0]);
                            input.insert_str(" ");
                        } else if candidates.is_empty() {
                            shared.push(Row::Error("no command starts with that".to_string()));
                        } else {
                            shared.push(Row::Note(format!("commands: {}", candidates.join(" "))));
                        }
                    } else if key.code == KeyCode::Up {
                        if !sent_inputs.is_empty() {
                            let idx = match hist_idx {
                                Some(i) => i.saturating_sub(1),
                                None => sent_inputs.len() - 1,
                            };
                            hist_idx = Some(idx);
                            input.text = sent_inputs[idx].clone();
                            input.cursor = input.text.chars().count();
                        }
                    } else if key.code == KeyCode::Down {
                        if let Some(i) = hist_idx {
                            if i + 1 < sent_inputs.len() {
                                hist_idx = Some(i + 1);
                                input.text = sent_inputs[i + 1].clone();
                                input.cursor = input.text.chars().count();
                            } else {
                                hist_idx = None;
                                input.clear();
                            }
                        }
                    } else if let KeyCode::Char(ch) = key.code {
                        input.insert(ch);
                    }
                }
                Event::Paste(text) => input.insert_str(&text),
                Event::Mouse(m) => match m.kind {
                    crossterm::event::MouseEventKind::ScrollUp => shared.scroll_up(3),
                    crossterm::event::MouseEventKind::ScrollDown => shared.scroll_down(3),
                    _ => {}
                },
                _ => {}
            }
        }
        tick += 1;
        let status = status_line(&state.cfg, &state.cwd, &shared, &state.registry, tick);
        render(&mut terminal, &shared, &input, &status)?;
    }

    disable_raw_mode()?;
    crossterm::execute!(stdout(), LeaveAlternateScreen)?;
    Ok(())
}

struct Worker {
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    history: Arc<Mutex<Vec<Message>>>,
    log: Arc<Session>,
    controls: Arc<Controls>,
    shared: Arc<Shared>,
    cwd: PathBuf,
    /// The typed message for this turn. The UI thread does not touch the history.
    input: String,
}

impl Worker {
    fn turn(&self) {
        let sink = Arc::new(UiSink {
            shared: self.shared.clone(),
        });
        let agent = Agent {
            adapter: self.adapter.clone(),
            registry: self.registry.clone(),
            plugins: self.plugins.clone(),
            cfg: self.cfg.clone(),
            sink,
            controls: self.controls.clone(),
            cwd: self.cwd.clone(),
            session: Some(self.log.clone()),
        };
        let mut history = self.history.lock().expect("history lock");
        // A /model change is recorded here, at the turn boundary, by the thread that owns
        // the session log.
        if let Some(m) = self.shared.pending_model.lock().expect("pending model lock").take() {
            if let Err(e) = self.log.record_model_change(&m) {
                self.shared.push(Row::Note(format!("session log failed: {e}")));
            }
        }
        // The UI thread only drew the transcript row; the message enters history here.
        let msg = Message::user(&self.input);
        if let Err(e) = self.log.record_message(&msg) {
            self.shared.push(Row::Note(format!("session log failed: {e}")));
        }
        history.push(msg);
        let outcome = agent.run_turn(&mut history);
        self.shared
            .tokens
            .store(estimate_tokens(&history), Ordering::Relaxed);
        self.shared.flush_live();
        match outcome {
            Ok(o) => {
                let mut note = format!("{} steps", o.steps);
                if o.stopped_at_limit {
                    note.push_str(" (step limit, raise max_steps in config.yaml)");
                }
                if o.aborted {
                    note.push_str(" (stopped by Ctrl+C)");
                }
                if let Some(t) = *self.shared.started.lock().expect("started lock") {
                    note.push_str(&format!(" · {}s", t.elapsed().as_secs()));
                }
                *self.shared.note_status.lock().expect("status lock") = note;
            }
            Err(e) => {
                self.shared.push(Row::Error(format!("error: {e}")));
                *self.shared.note_status.lock().expect("status lock") = "error".to_string();
            }
        }
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

/// `/compact` on its own thread: it asks the model for a summary, so it is not the UI thread's
/// job, and it holds the history lock only while it runs.
struct CompactWorker {
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    history: Arc<Mutex<Vec<Message>>>,
    log: Arc<Session>,
    controls: Arc<Controls>,
    shared: Arc<Shared>,
    cwd: PathBuf,
}

impl CompactWorker {
    fn run(&self) {
        let sink = Arc::new(UiSink {
            shared: self.shared.clone(),
        });
        let agent = Agent {
            adapter: self.adapter.clone(),
            registry: self.registry.clone(),
            plugins: self.plugins.clone(),
            cfg: self.cfg.clone(),
            sink,
            controls: self.controls.clone(),
            cwd: self.cwd.clone(),
            session: Some(self.log.clone()),
        };
        let mut history = self.history.lock().expect("history lock");
        match agent.compact_now(&mut history) {
            Ok(replaced) => {
                let tokens = estimate_tokens(&history);
                self.shared.tokens.store(tokens, Ordering::Relaxed);
                self.shared.push(Row::Note(if replaced == 0 {
                    "nothing to compact - the history already fits the budget".to_string()
                } else {
                    format!(
                        "compacted {replaced} messages into a summary · now {} tok · originals stay in {}",
                        tokens,
                        self.log.path.display()
                    )
                }));
                *self.shared.note_status.lock().expect("status lock") = "compacted".to_string();
            }
            Err(e) => {
                self.shared.push(Row::Error(format!("compact failed: {e}")));
                *self.shared.note_status.lock().expect("status lock") = "error".to_string();
            }
        }
        self.controls.reset();
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

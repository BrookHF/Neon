//! The ratatui front end: streaming transcript, tool cards, status bar, steering input.
//! The turn runs on a worker thread; keys reach it through `Controls`.

use std::io::{Result as IoResult, stdout};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::agent::{Agent, Controls, Sink};
use crate::config::Config;
use crate::llm::Adapter;
use crate::messages::{Message, ToolCall, estimate_tokens};
use crate::plugin::PluginHost;
use crate::session::Session;
use crate::tools::{Registry, ToolResult};

#[derive(Clone, Debug)]
pub enum Row {
    User(String),
    Text(String),
    Call(String),
    Result(String),
    Note(String),
}

impl Row {
    fn line(&self) -> String {
        match self {
            Row::User(t) => format!("> {t}"),
            Row::Text(t) => t.clone(),
            Row::Call(t) => format!("-> {t}"),
            Row::Result(t) => format!("<- {t}"),
            Row::Note(t) => format!("# {t}"),
        }
    }
}

#[derive(Default)]
pub struct Shared {
    rows: Mutex<Vec<Row>>,
    live: Mutex<String>,
    note_status: Mutex<String>,
    running: AtomicBool,
    tokens: AtomicUsize,
}

impl Shared {
    fn push(&self, row: Row) {
        self.rows.lock().expect("rows lock").push(row);
    }

    fn flush_live(&self) {
        let text = self.live.lock().expect("live lock").drain(..).collect::<String>();
        if !text.is_empty() {
            self.push(Row::Text(text));
        }
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
        self.shared.push(Row::Result(format!(
            "{head}{}",
            if result.is_error { " [error]" } else { "" }
        )));
    }

    fn note(&self, msg: &str) {
        self.shared.push(Row::Note(msg.to_string()));
    }
}

fn compact(value: &serde_json::Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(120).collect();
        format!("{cut}...")
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

fn status_line(cfg: &Config, cwd: &PathBuf, shared: &Shared, registry: &Registry) -> String {
    let tokens = shared.tokens.load(Ordering::Relaxed);
    let state = if shared.running.load(Ordering::Relaxed) {
        "running"
    } else {
        "idle"
    };
    let extra = shared.note_status.lock().expect("status lock").clone();
    let plugins = registry.plugin_rows().len();
    let folder = cwd.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| cwd.display().to_string());
    format!(
        "{} · {folder} · {state} · {tokens} tok · {plugins} plugins · {extra}",
        cfg.model,
    )
}

fn render(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    shared: &Shared,
    input: &str,
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

        let mut lines: Vec<String> = Vec::new();
        for row in shared.rows.lock().expect("rows lock").iter() {
            lines.extend(wrap_to(&row.line(), width));
        }
        lines.extend(wrap_to(&shared.live.lock().expect("live lock"), width));
        let tail: Vec<String> = lines
            .iter()
            .rev()
            .take(height)
            .rev()
            .cloned()
            .collect();
        let body = tail.join("\n");

        frame.render_widget(Paragraph::new(status), areas[0]);
        frame.render_widget(Paragraph::new(body), areas[1]);
        frame.render_widget(
            Paragraph::new(format!("> {input}\nEnter sends · Ctrl+C abort or quit")),
            areas[2],
        );
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::wrap_to;

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
    shared.push(Row::Note(format!(
        "neon ready · {} · {} tools · /plugin list|load|unload|reload",
        cfg.model,
        registry.schemas().len()
    )));
    shared.push(Row::Note(format!("session {}", log.id)));
    if let Some(n) = resumed {
        shared.push(Row::Note(format!("resumed {n} messages")));
    }

    let mut input = String::new();
    loop {
        if event::poll(Duration::from_millis(60))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        match key.code {
                            KeyCode::Char('c') => {
                                if shared.running.load(Ordering::Relaxed) {
                                    controls.abort();
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
                        let msg = input.clone();
                        input.clear();
                        if msg.trim().is_empty() {
                            continue;
                        }
                        if shared.running.load(Ordering::Relaxed) {
                            controls.steer(&msg);
                            shared.push(Row::User(msg));
                        } else if msg.trim().starts_with("/plugin") {
                            let rest = msg.trim().trim_start_matches("/plugin").to_string();
                            shared.push(Row::User(msg));
                            match plugins.lock().map(|mut host| host.handle_command(&rest)) {
                                Ok(Ok(note)) => shared.push(Row::Note(note)),
                                Ok(Err(e)) => shared.push(Row::Note(format!("plugin: {e}"))),
                                Err(_) => shared.push(Row::Note("plugin host is busy".to_string())),
                            }
                        } else {
                            shared.push(Row::User(msg.clone()));
                            shared.running.store(true, Ordering::Relaxed);
                            let worker = Worker {
                                adapter: adapter.clone(),
                                registry: registry.clone(),
                                plugins: plugins.clone(),
                                cfg: cfg.clone(),
                                history: history.clone(),
                                log: log.clone(),
                                controls: controls.clone(),
                                shared: shared.clone(),
                                cwd: cwd.clone(),
                                input: msg,
                            };
                            thread::spawn(move || worker.turn());
                        }
                    } else if key.code == KeyCode::Backspace {
                        input.pop();
                    } else if let KeyCode::Char(ch) = key.code {
                        input.push(ch);
                    }
                }
                Event::Paste(text) => input.push_str(&text),
                _ => {}
            }
        }
        let status = status_line(&cfg, &cwd, &shared, &registry);
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
                    note.push_str(" (step limit)");
                }
                if o.aborted {
                    note.push_str(" (aborted)");
                }
                *self.shared.note_status.lock().expect("status lock") = note;
            }
            Err(e) => {
                self.shared.push(Row::Note(format!("error: {e}")));
                *self.shared.note_status.lock().expect("status lock") = "error".to_string();
            }
        }
        self.shared.running.store(false, Ordering::Relaxed);
    }
}

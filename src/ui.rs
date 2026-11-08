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
use ratatui::widgets::{Paragraph, Wrap};
use ratatui::Terminal;

use crate::agent::{Agent, Controls, Sink};
use crate::config::Config;
use crate::llm::Adapter;
use crate::messages::{Message, ToolCall, estimate_tokens};
use crate::plugin::PluginHost;
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

fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let cut: String = text.chars().take(width.saturating_sub(1)).collect();
        format!("{cut}\u{2026}")
    }
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
            lines.push(row.line());
        }
        for line in shared.live.lock().expect("live lock").split('\n') {
            lines.push(line.to_string());
        }
        let tail: Vec<String> = lines
            .iter()
            .rev()
            .take(height)
            .rev()
            .map(|l| clip(l, width))
            .collect();
        let body = tail.join("\n");

        frame.render_widget(Paragraph::new(status), areas[0]);
        frame.render_widget(
            Paragraph::new(body).wrap(Wrap { trim: true }),
            areas[1],
        );
        frame.render_widget(
            Paragraph::new(format!("> {input}\nEnter sends · Ctrl+C abort or quit")),
            areas[2],
        );
    })?;
    Ok(())
}

pub fn run(
    adapter: Arc<dyn Adapter>,
    registry: Arc<Registry>,
    plugins: Arc<Mutex<PluginHost>>,
    cfg: Arc<Config>,
    session: Arc<Mutex<Vec<Message>>>,
    controls: Arc<Controls>,
    cwd: PathBuf,
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
                                session: session.clone(),
                                controls: controls.clone(),
                                shared: shared.clone(),
                                cwd: cwd.clone(),
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
    session: Arc<Mutex<Vec<Message>>>,
    controls: Arc<Controls>,
    shared: Arc<Shared>,
    cwd: PathBuf,
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
        };
        let mut history = self.session.lock().expect("session lock");
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

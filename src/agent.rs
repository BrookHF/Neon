//! The turn/step driver, the sink it writes to, and the steering/abort controls.
//! A turn is zero or more steps; a step is one request plus the tools it calls.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::llm::Adapter;
use crate::messages::{Event, Message, Part, Request, Role, ToolCall, estimate_tokens};
use crate::plugin::PluginHost;
use crate::prompt::build_system;
use crate::session::{Session, summary_message};
use crate::tools::{Registry, ToolCtx, ToolResult};

/// Where the loop reports progress. Plain stdio, the TUI, or a test buffer.
pub trait Sink: Send + Sync {
    fn text(&self, chunk: &str);
    fn tool_call(&self, call: &ToolCall);
    fn tool_result(&self, call: &ToolCall, result: &ToolResult);
    fn note(&self, msg: &str);
}

pub struct StdioSink;

impl Sink for StdioSink {
    fn text(&self, chunk: &str) {
        use std::io::Write;
        let out = std::io::stdout();
        let _ = out.lock().write_all(chunk.as_bytes());
        let _ = std::io::stdout().flush();
    }

    fn tool_call(&self, call: &ToolCall) {
        println!("\n-> {} {}", call.name, compact(&call.args));
    }

    fn tool_result(&self, call: &ToolCall, result: &ToolResult) {
        println!(
            "<- {}{}",
            first_line(&result.content),
            if result.is_error { " [error]" } else { "" }
        );
        let _ = call;
    }

    fn note(&self, msg: &str) {
        println!("[{msg}]");
    }
}

#[derive(Default)]
pub struct VecSink {
    lines: Mutex<Vec<String>>,
}

impl VecSink {
    pub fn dump(&self) -> String {
        self.lines.lock().expect("sink lock").join("\n")
    }
}

impl Sink for VecSink {
    fn text(&self, chunk: &str) {
        self.lines
            .lock()
            .expect("sink lock")
            .push(chunk.to_string());
    }

    fn tool_call(&self, call: &ToolCall) {
        self.lines.lock().expect("sink lock").push(format!(
            "-> {} {}",
            call.name,
            compact(&call.args)
        ));
    }

    fn tool_result(&self, _call: &ToolCall, result: &ToolResult) {
        self.lines.lock().expect("sink lock").push(format!(
            "<- {}{}",
            first_line(&result.content),
            if result.is_error { " [error]" } else { "" }
        ));
    }

    fn note(&self, msg: &str) {
        self.lines
            .lock()
            .expect("sink lock")
            .push(format!("[{msg}]"));
    }
}

/// Steering messages typed while a turn runs, plus the abort switch.
#[derive(Default)]
pub struct Controls {
    inbox: Mutex<Vec<String>>,
    /// Shared with ToolCtx so a waiting tool can kill its child on Ctrl+C.
    abort: Arc<AtomicBool>,
}

impl Controls {
    pub fn steer(&self, msg: &str) {
        self.inbox.lock().expect("inbox lock").push(msg.to_string());
    }

    pub fn take_steering(&self) -> Option<String> {
        self.inbox.lock().expect("inbox lock").pop()
    }

    pub fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// The abort switch, shared with `ToolCtx` so a waiting tool kills its child.
    pub fn abort_flag(&self) -> Arc<AtomicBool> {
        self.abort.clone()
    }

    pub fn reset(&self) {
        self.abort.store(false, Ordering::Relaxed);
        self.inbox.lock().expect("inbox lock").clear();
    }
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub steps: usize,
    pub text: String,
    /// True when the loop hit `max_steps` instead of finishing.
    pub stopped_at_limit: bool,
    /// True when Ctrl+C stopped the turn.
    pub aborted: bool,
}

pub struct Agent {
    pub adapter: Arc<dyn Adapter>,
    pub registry: Arc<Registry>,
    pub plugins: Arc<Mutex<PluginHost>>,
    pub cfg: Arc<Config>,
    pub sink: Arc<dyn Sink>,
    pub controls: Arc<Controls>,
    pub cwd: PathBuf,
    /// The append-only session log this turn appends to. None for bare loop tests.
    pub session: Option<Arc<Session>>,
}

impl Agent {
    pub fn run_turn(&self, history: &mut Vec<Message>) -> Result<TurnOutcome> {
        // Plugin swaps take effect at the turn boundary, never mid-request.
        if let Ok(mut host) = self.plugins.lock()
            && let Err(e) = host.apply(&self.registry)
        {
            self.sink.note(&format!("plugin change failed: {e}"));
        }
        let outcome = self.run_inner(history);
        // Leftover steering and the abort switch belong to the turn that just ended.
        self.controls.reset();
        outcome
    }

    /// Push a message to the history and append its entries to the session log.
    fn push(&self, history: &mut Vec<Message>, msg: Message) {
        if let Some(s) = &self.session
            && let Err(e) = s.record_message(&msg)
        {
            self.sink.note(&format!("session log failed: {e}"));
        }
        history.push(msg);
    }

    /// Ask the model to summarize the oldest span so the history fits the budget again.
    /// The summary replaces the span in the model-facing history; originals stay in the log.
    /// Returns the number of entries replaced, 0 when there was nothing worth summarizing.
    /// Public so `/compact` can run on demand, not only when the budget is crossed.
    pub fn compact_now(&self, history: &mut Vec<Message>) -> Result<usize> {
        let keep = self.cfg.context_budget_tokens * 3 / 5;

        // Keep the newest whole messages that fit; a call and its results stay together.
        let mut split = history.len();
        let mut tail_tokens = 0usize;
        while split > 0 {
            let size = estimate_tokens(&history[split - 1..split]);
            if tail_tokens + size > keep {
                break;
            }
            tail_tokens += size;
            split -= 1;
        }
        if split == 0 {
            return Ok(0); // everything fits; nothing worth summarizing
        }

        let mut tail = history.split_off(split);
        // A result whose call fell into the summarized span is invalid on the wire;
        // the summary covers it, so strip those parts from the tail's first message.
        if let Some(first) = tail.first_mut() {
            first.parts.retain(|p| !matches!(p, Part::Result { .. }));
            if first.parts.is_empty() {
                tail.remove(0);
            }
        }

        let request = Request {
            system: COMPACT_SYSTEM.to_string(),
            messages: history.clone(),
            tools: Vec::new(),
            model: self.cfg.model(),
            temperature: None,
            max_tokens: None,
        };
        let mut summary = String::new();
        self.adapter.stream(&request, &mut |event| {
            if let Event::Delta(chunk) = event {
                summary.push_str(&chunk);
            }
        })?;
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return Err(AgentError::Adapter(
                "compaction returned no summary".to_string(),
            ));
        }

        let replaced = history.len();
        history.clear();
        history.push(summary_message(&summary));
        history.extend(tail);
        if let Some(s) = &self.session {
            s.record_compaction(&summary, replaced)?;
        }
        Ok(replaced)
    }

    fn run_inner(&self, history: &mut Vec<Message>) -> Result<TurnOutcome> {
        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            timeout_secs: self.cfg.bash_timeout_secs,
            excerpt_bytes: self.cfg.bash_excerpt_bytes,
            spill_dir: self.cfg.spill_dir.clone(),
            abort: self.controls.abort_flag(),
        };

        let mut steps = 0usize;
        let mut final_text = String::new();

        for _ in 0..self.cfg.max_steps {
            if self.controls.aborted() {
                self.sink.note("aborted");
                return Ok(TurnOutcome {
                    steps,
                    text: final_text,
                    stopped_at_limit: false,
                    aborted: true,
                });
            }
            if estimate_tokens(history) > self.cfg.context_budget_tokens {
                if self.compact_now(history)? > 0 {
                    self.sink.note("context compacted");
                }
                if estimate_tokens(history) > self.cfg.context_budget_tokens {
                    return Err(AgentError::Adapter(format!(
                        "context is still over the {} token budget \u{2014} run /compact \u{2014} \
                         or raise context_budget_tokens in config.yaml",
                        self.cfg.context_budget_tokens
                    )));
                }
            }
            steps += 1;

            let request = Request {
                system: build_system(&self.cwd),
                messages: history.clone(),
                tools: self.registry.schemas(),
                model: self.cfg.model(),
                temperature: None,
                max_tokens: Some(self.cfg.max_tokens),
            };

            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            self.adapter.stream(&request, &mut |event| match event {
                Event::Delta(chunk) => {
                    text.push_str(&chunk);
                    self.sink.text(&chunk);
                }
                Event::Call(call) => calls.push(call),
                Event::Done { finish_reason } => {
                    if finish_reason.as_deref() == Some("MAX_TOKENS") {
                        self.sink.note(
                            "output hit max_output_tokens \u{2014} raise max_tokens in config.yaml \u{2014} \
                             or split the write into smaller steps",
                        );
                    }
                }
                Event::Error(msg) => {
                    text.push_str(&format!("\n[adapter error: {msg}]\n"));
                    self.sink.text(&text);
                }
            })?;

            if calls.is_empty() {
                self.push(
                    history,
                    Message {
                        role: Role::Assistant,
                        parts: vec![Part::Text(text.clone())],
                    },
                );
                final_text = text;
                return Ok(TurnOutcome {
                    steps,
                    text: final_text,
                    stopped_at_limit: false,
                    aborted: false,
                });
            }

            // Run calls in order. Steering lands after the tool that was in flight, and
            // the calls still owed are dropped, so no call goes unanswered.
            let mut executed: Vec<(ToolCall, ToolResult)> = Vec::new();
            let mut steering: Option<String> = None;
            let mut aborted = false;
            for call in &calls {
                if self.controls.aborted() {
                    aborted = true;
                    break;
                }
                self.sink.tool_call(call);
                let result = self.registry.execute(call, &ctx);
                self.sink.tool_result(call, &result);
                executed.push((call.clone(), result));
                if self.controls.aborted() {
                    aborted = true;
                    break;
                }
                steering = self.controls.take_steering();
                if steering.is_some() {
                    break;
                }
            }

            let mut parts: Vec<Part> = Vec::new();
            if !text.is_empty() {
                parts.push(Part::Text(text));
            }
            for (call, _) in &executed {
                parts.push(Part::Call(call.clone()));
            }
            self.push(
                history,
                Message {
                    role: Role::Assistant,
                    parts,
                },
            );

            let mut results: Vec<Part> = Vec::new();
            for (call, result) in &executed {
                results.push(Part::Result {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: result.content.clone(),
                    is_error: result.is_error,
                });
            }
            if !results.is_empty() || steering.is_some() {
                if let Some(msg) = &steering {
                    results.push(Part::Text(msg.clone()));
                }
                self.push(
                    history,
                    Message {
                        role: Role::User,
                        parts: results,
                    },
                );
            }
            if aborted {
                self.sink.note("aborted");
                return Ok(TurnOutcome {
                    steps,
                    text: final_text,
                    stopped_at_limit: false,
                    aborted: true,
                });
            }
        }

        Ok(TurnOutcome {
            steps,
            text: final_text,
            stopped_at_limit: true,
            aborted: false,
        })
    }
}

/// Instruction for the compaction request.
const COMPACT_SYSTEM: &str = "You compact an agent conversation. Summarize the messages above in plain text: what the user asked, what was done, tool results that matter, decisions made, and where the work stands. Keep it under 200 words.";

fn compact(value: &Value) -> String {
    let text = value.to_string();
    if text.chars().count() > 120 {
        let cut: String = text.chars().take(120).collect();
        format!("{cut}...")
    } else {
        text
    }
}

fn first_line(text: &str) -> String {
    text.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_string()
}

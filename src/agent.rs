//! The turn/step driver. A turn is zero or more steps; a step is one request plus its tools.

use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::llm::Adapter;
use crate::messages::{Event, Message, Part, Request, Role, ToolCall, estimate_tokens};
use crate::prompt::build_system;
use crate::tools::{Registry, ToolCtx};

#[derive(Debug)]
pub struct TurnOutcome {
    pub steps: usize,
    pub text: String,
    /// True when the loop hit `max_steps` instead of finishing.
    pub stopped_at_limit: bool,
}

pub struct Agent<'a> {
    pub adapter: &'a dyn Adapter,
    pub registry: &'a Registry,
    pub cfg: &'a Config,
    pub cwd: PathBuf,
    pub out: &'a mut dyn Write,
}

impl<'a> Agent<'a> {
    pub fn run_turn(&mut self, history: &mut Vec<Message>) -> Result<TurnOutcome> {
        let ctx = ToolCtx {
            cwd: self.cwd.clone(),
            timeout_secs: self.cfg.bash_timeout_secs,
            excerpt_bytes: self.cfg.bash_excerpt_bytes,
            spill_dir: self.cfg.spill_dir.clone(),
        };

        let mut steps = 0usize;
        let mut final_text = String::new();

        for _ in 0..self.cfg.max_steps {
            if estimate_tokens(history) > self.cfg.context_budget_tokens {
                return Err(AgentError::Adapter(format!(
                    "context budget of {} tokens exceeded. Compaction arrives in milestone 4.",
                    self.cfg.context_budget_tokens
                )));
            }
            steps += 1;

            let request = Request {
                system: build_system(&self.cwd),
                messages: history.clone(),
                tools: self.registry.schemas(),
                model: self.cfg.model.clone(),
                temperature: None,
                max_tokens: None,
            };

            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let out = &mut self.out;
            self.adapter.stream(&request, &mut |event| match event {
                Event::Delta(chunk) => {
                    text.push_str(&chunk);
                    let _ = out.write_all(chunk.as_bytes());
                    let _ = out.flush();
                }
                Event::Call(call) => calls.push(call),
                Event::Done { .. } => {}
                Event::Error(msg) => {
                    text.push_str(&format!("\n[adapter error: {msg}]\n"));
                    let _ = out.write_all(text.as_bytes());
                }
            })?;
            if !text.is_empty() {
                let _ = out.write_all(b"\n");
                let _ = out.flush();
            }

            if calls.is_empty() {
                history.push(Message {
                    role: Role::Assistant,
                    parts: vec![Part::Text(text.clone())],
                });
                final_text = text;
                return Ok(TurnOutcome {
                    steps,
                    text: final_text,
                    stopped_at_limit: false,
                });
            }

            let mut parts: Vec<Part> = Vec::new();
            if !text.is_empty() {
                parts.push(Part::Text(text));
            }
            for call in &calls {
                parts.push(Part::Call(call.clone()));
            }
            history.push(Message {
                role: Role::Assistant,
                parts,
            });

            let mut results: Vec<Part> = Vec::new();
            for call in &calls {
                let _ = writeln!(out, "\n-> {} {}", call.name, compact(&call.args));
                let result = self.registry.execute(call, &ctx);
                let _ = writeln!(
                    out,
                    "<- {}{}",
                    first_line(&result.content),
                    if result.is_error { " [error]" } else { "" }
                );
                let _ = out.flush();
                results.push(Part::Result {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    content: result.content,
                    is_error: result.is_error,
                });
            }
            history.push(Message {
                role: Role::User,
                parts: results,
            });
        }

        Ok(TurnOutcome {
            steps,
            text: final_text,
            stopped_at_limit: true,
        })
    }
}

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
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").to_string()
}

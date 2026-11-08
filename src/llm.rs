//! The LLM seam. One Gemini adapter now, one fake adapter for tests.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::http;
use crate::messages::{Event, Message, Part, Request, ToolCall};

pub trait Adapter {
    /// Drive one model request. Events arrive in stream order.
    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Event)) -> Result<()>;
}

pub struct GeminiAdapter {
    cfg: Config,
}

impl GeminiAdapter {
    pub fn new(cfg: Config) -> Self {
        GeminiAdapter { cfg }
    }
}

impl Adapter for GeminiAdapter {
    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Event)) -> Result<()> {
        let key = self.cfg.require_key()?;
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.cfg.base_url, req.model
        );
        let body = serde_json::to_vec(&gemini_body(req))
            .map_err(|e| AgentError::Adapter(format!("encode request: {e}")))?;

        let mut resp = http::post(
            &url,
            &[
                ("x-goog-api-key", &key),
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
            ],
            &body,
        )?;

        if resp.status != 200 {
            let bytes = http::read_all(&mut resp.body);
            return Err(AgentError::Adapter(format!(
                "HTTP {}: {}",
                resp.status,
                String::from_utf8_lossy(&bytes)
            )));
        }

        let mut call_index = 0usize;
        let mut failure: Option<String> = None;
        let mut saw_any = false;
        let io_error = http::each_line(&mut resp.body, |line| {
            let payload = match line.trim().strip_prefix("data:") {
                Some(p) => p.trim(),
                None => return,
            };
            if payload == "[DONE]" {
                return;
            }
            let value: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(e) => {
                    failure.get_or_insert_with(|| format!("bad stream chunk: {e}"));
                    return;
                }
            };
            if let Some(err) = value.get("error") {
                failure.get_or_insert_with(|| err.to_string());
                return;
            }
            if let Some(msg) = parse_chunk(&value, sink, &mut call_index) {
                failure.get_or_insert_with(|| msg);
            }
            saw_any = true;
        });

        if let Some(msg) = failure {
            return Err(AgentError::Adapter(msg));
        }
        if !saw_any {
            return Err(AgentError::Adapter(
                io_error.unwrap_or_else(|| "the stream returned no candidates".to_string()),
            ));
        }
        Ok(())
    }
}

/// Replay a recorded JSONL fixture, one chunk per request. No key, no network.
pub struct FixtureAdapter {
    lines: Mutex<VecDeque<String>>,
}

impl FixtureAdapter {
    pub fn from_file(path: &std::path::Path) -> Result<FixtureAdapter> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
        Ok(FixtureAdapter {
            lines: Mutex::new(
                text
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_string)
                    .collect(),
            ),
        })
    }
}

impl Adapter for FixtureAdapter {
    fn stream(&self, _req: &Request, sink: &mut dyn FnMut(Event)) -> Result<()> {
        let line = self
            .lines
            .lock()
            .expect("fixture lock")
            .pop_front()
            .ok_or_else(|| AgentError::Adapter("fixture exhausted".to_string()))?;
        let value: Value = serde_json::from_str(&line)
            .map_err(|e| AgentError::Adapter(format!("bad fixture line: {e}")))?;
        let mut call_index = 0usize;
        if let Some(msg) = parse_chunk(&value, sink, &mut call_index) {
            return Err(AgentError::Adapter(msg));
        }
        Ok(())
    }
}

/// Turn one stream chunk into events. Returns an error message if the chunk is malformed.
fn parse_chunk(value: &Value, sink: &mut dyn FnMut(Event), call_index: &mut usize) -> Option<String> {
    let candidates = match value.get("candidates").and_then(|c| c.as_array()) {
        Some(c) => c,
        None => return Some("chunk has no candidates".to_string()),
    };
    if candidates.is_empty() {
        sink(Event::Done {
            finish_reason: None,
        });
        return None;
    }
    let cand = &candidates[0];
    let parts = cand
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array());
    if let Some(parts) = parts {
        for part in parts {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    sink(Event::Delta(text.to_string()));
                }
            } else if let Some(call) = part.get("functionCall") {
                let name = match call.get("name").and_then(|n| n.as_str()) {
                    Some(n) => n,
                    None => return Some("functionCall has no name".to_string()),
                };
                let args = call.get("args").cloned().unwrap_or(json!({}));
                let id = format!("call_{call_index}");
                *call_index += 1;
                sink(Event::Call(ToolCall {
                    id,
                    name: name.to_string(),
                    args,
                }));
            }
        }
    }
    let finish = cand
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .map(str::to_string);
    sink(Event::Done {
        finish_reason: finish,
    });
    None
}

fn gemini_body(req: &Request) -> Value {
    let contents: Vec<Value> = req
        .messages
        .iter()
        .map(|m| json!({ "role": m.role.wire(), "parts": parts_wire(m) }))
        .collect();

    let declarations: Vec<Value> = req
        .tools
        .iter()
        .map(|t| {
            let mut params = t.parameters.clone();
            normalize_schema(&mut params);
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": params,
            })
        })
        .collect();

    let mut body = json!({
        "systemInstruction": { "parts": [{ "text": req.system }] },
        "contents": contents,
    });
    if !declarations.is_empty() {
        body["tools"] = json!([{ "function_declarations": declarations }]);
    }
    let mut gen_config = json!({});
    if let Some(t) = req.temperature {
        gen_config["temperature"] = json!(t);
    }
    if let Some(m) = req.max_tokens {
        gen_config["maxOutputTokens"] = json!(m);
    }
    if gen_config.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
        body["generationConfig"] = gen_config;
    }
    body
}

fn parts_wire(m: &Message) -> Vec<Value> {
    m.parts
        .iter()
        .map(|p| match p {
            Part::Text(t) => json!({ "text": t }),
            Part::Call(c) => json!({ "functionCall": { "name": c.name, "args": c.args } }),
            Part::Result {
                name,
                content,
                is_error,
                ..
            } => json!({
                "functionResponse": {
                    "name": name,
                    "response": { "content": content, "is_error": is_error }
                }
            }),
        })
        .collect()
}

/// Gemini wants schema `type` values in upper case and rejects `additionalProperties`.
fn normalize_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(t) = map.get("type").and_then(|v| v.as_str()) {
                map.insert("type".to_string(), Value::String(t.to_uppercase()));
            }
            map.remove("additionalProperties");
            for v in map.values_mut() {
                normalize_schema(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                normalize_schema(v);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{Role, ToolSchema};

    fn req() -> Request {
        Request {
            system: "You are Neon.".to_string(),
            messages: vec![
                Message::user("list files"),
                Message {
                    role: Role::Assistant,
                    parts: vec![
                        Part::Text("listing".to_string()),
                        Part::Call(ToolCall {
                            id: "call_0".to_string(),
                            name: "bash".to_string(),
                            args: json!({ "command": "ls" }),
                        }),
                    ],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::Result {
                        call_id: "call_0".to_string(),
                        name: "bash".to_string(),
                        content: "a.txt".to_string(),
                        is_error: false,
                    }],
                },
            ],
            tools: vec![ToolSchema {
                name: "bash".to_string(),
                description: "run a shell command".to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"],
                }),
            }],
            model: "gemini-3.5-flash-lite".to_string(),
            temperature: None,
            max_tokens: Some(2048),
        }
    }

    #[test]
    fn body_shape() {
        let body = gemini_body(&req());
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "You are Neon.");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(body["contents"][1]["parts"][1]["functionCall"]["name"], "bash");
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "bash"
        );
        assert_eq!(body["tools"][0]["function_declarations"][0]["name"], "bash");
        assert_eq!(body["tools"][0]["function_declarations"][0]["parameters"]["type"], "OBJECT");
        assert!(body["tools"][0]["function_declarations"][0]["parameters"]
            .get("additionalProperties")
            .is_none());
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
    }

    #[test]
    fn chunk_emits_text_and_call() {
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "running" },
                    { "functionCall": { "name": "bash", "args": { "command": "ls" } } }
                ] },
                "finish_reason": "STOP"
            }]
        });
        let mut events = Vec::new();
        let err = parse_chunk(&chunk, &mut |e| events.push(e), &mut 0);
        assert!(err.is_none());
        assert_eq!(events.len(), 3);
        match &events[1] {
            Event::Call(c) => assert_eq!(c.name, "bash"),
            other => panic!("expected a call, got {other:?}"),
        }
    }
}

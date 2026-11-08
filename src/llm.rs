//! The LLM seam. One Gemini adapter now, one fake adapter for tests.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Value, json};

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::http;
use crate::messages::{Event, Message, Part, Request, ToolCall};

pub trait Adapter: Send + Sync {
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

    /// One request attempt. Errors say whether waiting and retrying could help.
    fn stream_once(&self, key: &str, req: &Request, sink: &mut dyn FnMut(Event)) -> Result<()> {
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            self.cfg.base_url, req.model
        );
        let body = serde_json::to_vec(&gemini_body(req))
            .map_err(|e| AgentError::Adapter(format!("encode request: {e}")))?;

        let mut resp = http::post(
            &url,
            &[
                ("x-goog-api-key", key),
                ("content-type", "application/json"),
                ("accept", "text/event-stream"),
            ],
            &body,
        )?;

        if resp.status != 200 {
            let bytes = http::read_all(&mut resp.body);
            let text = String::from_utf8_lossy(&bytes);
            let retryable = matches!(resp.status, 429 | 500 | 502 | 503 | 504);
            let msg = format!("HTTP {}: {}", resp.status, text);
            if retryable {
                return Err(AgentError::Retryable(msg));
            }
            return Err(AgentError::Adapter(msg));
        }

        let mut call_index = 0usize;
        let mut failure: Option<String> = None;
        let mut saw_any = false;
        // Debug aid: NEON_WIRE_DUMP=/path appends each SSE payload line (never the key).
        let dump = std::env::var("NEON_WIRE_DUMP").ok();
        // Idle guard: SSE keepalive bytes keep the socket warm but carry no data, so a
        // silent stream would hang forever. Track time since the last data line.
        let mut last_data = std::time::Instant::now();
        let idle = std::time::Duration::from_secs(self.cfg.stream_idle_secs);
        let io_error = http::each_line(&mut resp.body, |line| {
            // Idle guard runs on every line, including keepalive comments and blanks.
            if last_data.elapsed() > idle {
                failure.get_or_insert_with(|| format!("stream stalled: no data for {idle:?}"));
                return false;
            }
            let payload = match line.trim().strip_prefix("data:") {
                Some(p) => p.trim(),
                None => return true,
            };
            if payload == "[DONE]" {
                return false;
            }
            last_data = std::time::Instant::now();
            if let Some(path) = &dump {
                if let Err(e) = crate::append_line(std::path::Path::new(path), payload) {
                    failure.get_or_insert_with(|| format!("wire dump: {e}"));
                    return false;
                }
            }
            let value: Value = match serde_json::from_str(payload) {
                Ok(v) => v,
                Err(e) => {
                    failure.get_or_insert_with(|| format!("bad stream chunk: {e}"));
                    return false;
                }
            };
            if let Some(err) = value.get("error") {
                failure.get_or_insert_with(|| err.to_string());
                return false;
            }
            if let Some(msg) = parse_chunk(&value, sink, &mut call_index) {
                failure.get_or_insert_with(|| msg);
                return false;
            }
            saw_any = true;
            true
        });

        if let Some(msg) = failure {
            // A stall before any data is worth retrying; after data, surface it.
            if !saw_any && msg.starts_with("stream stalled") {
                return Err(AgentError::Retryable(msg));
            }
            return Err(AgentError::Adapter(msg));
        }
        if !saw_any {
            // An empty stream (bad close, server hiccup) is worth waiting for.
            return Err(AgentError::Retryable(
                io_error.unwrap_or_else(|| "the stream returned no candidates".to_string()),
            ));
        }
        if let Some(e) = io_error {
            // A read timeout mid-stream is not an end of body; say so loudly.
            return Err(AgentError::Adapter(format!("stream ended abnormally: {e}")));
        }
        Ok(())
    }
}

impl Adapter for GeminiAdapter {
    /// Retries transient failures with progressive waits: 1s, 2s, 4s, capped at 30s.
    /// A retry only happens if no event was emitted yet, so text is never duplicated.
    fn stream(&self, req: &Request, sink: &mut dyn FnMut(Event)) -> Result<()> {
        let key = self.cfg.require_key()?;
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=self.cfg.llm_retries {
            let mut emitted = false;
            let result = self.stream_once(&key, req, &mut |event| {
                emitted = true;
                sink(event);
            });
            match result {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let transient = matches!(&e, AgentError::Retryable(_) | AgentError::Http(_));
                    if attempt == self.cfg.llm_retries || emitted || !transient {
                        return Err(e);
                    }
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(Duration::from_secs(30));
                }
            }
        }
        Err(AgentError::Adapter("retry loop ended".to_string()))
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
            // Thinking-model reasoning summaries are not conversation text; drop them.
            if part.get("thought").and_then(|t| t.as_bool()).unwrap_or(false) {
                continue;
            }
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
                // A sibling of functionCall on the part; both spellings appear in the wild.
                let sig = part
                    .get("thought_signature")
                    .or_else(|| part.get("thoughtSignature"))
                    .and_then(|s| s.as_str())
                    .map(str::to_string);
                let id = format!("call_{call_index}");
                *call_index += 1;
                sink(Event::Call(ToolCall {
                    id,
                    name: name.to_string(),
                    args,
                    thought_sig: sig,
                }));
            }
        }
    }
    let finish = cand
        .get("finishReason")
        .or_else(|| cand.get("finish_reason"))
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
            Part::Call(c) => {
                let mut part = json!({ "functionCall": { "name": c.name, "args": c.args } });
                if let Some(sig) = &c.thought_sig {
                    part["thought_signature"] = json!(sig);
                }
                part
            }
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
                            thought_sig: Some("sig-1".to_string()),
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
                plugin_row: String::new(),
                index: 0,
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
        assert_eq!(body["contents"][1]["parts"][1]["thought_signature"], "sig-1");
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
    fn chunk_captures_thought_signature_and_skips_thought_text() {
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [
                    { "text": "hidden reasoning", "thought": true },
                    { "functionCall": { "name": "bash", "args": {} }, "thoughtSignature": "sig-abc" }
                ] },
                "finish_reason": "STOP"
            }]
        });
        let mut events = Vec::new();
        let err = parse_chunk(&chunk, &mut |e| events.push(e), &mut 0);
        assert!(err.is_none());
        assert_eq!(events.len(), 2); // the thought text is not a delta
        match &events[0] {
            Event::Call(c) => assert_eq!(c.thought_sig.as_deref(), Some("sig-abc")),
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn camel_case_finish_reason_reports_truncation() {
        // The real wire sends camelCase finishReason; MAX_TOKENS must reach the loop.
        let chunk = json!({
            "candidates": [{
                "content": { "parts": [{ "text": "I will write the file now" }] },
                "finishReason": "MAX_TOKENS"
            }]
        });
        let mut events = Vec::new();
        let err = parse_chunk(&chunk, &mut |e| events.push(e), &mut 0);
        assert!(err.is_none());
        assert_eq!(events.len(), 2);
        match &events[1] {
            Event::Done { finish_reason } => {
                assert_eq!(finish_reason.as_deref(), Some("MAX_TOKENS"))
            }
            other => panic!("expected done, got {other:?}"),
        }
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

//! Append-only JSONL session log: `~/.local/state/neon/sessions/<id>.jsonl`.
//! Entry kinds: `meta`, `user`, `assistant`, `tool_result`, `model_change`, `compaction`.
//! Replay rebuilds the model-facing history. A `compaction` entry replaces the entries
//! before it with one summary message; the originals stay in the log.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::error::{AgentError, Result};
use crate::messages::{Message, Part, Role, ToolCall};

static NONCE: AtomicU64 = AtomicU64::new(0);

pub struct Session {
    pub id: String,
    pub path: PathBuf,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Session {
    /// New session: writes the `meta` line and returns the handle.
    pub fn create(dir: &Path, model: &str, cwd: &Path) -> Result<Session> {
        std::fs::create_dir_all(dir)
            .map_err(|e| AgentError::Config(format!("{}: {e}", dir.display())))?;
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let id = format!("{:x}-{}-{nonce}", now_secs(), std::process::id());
        let session = Session {
            id: id.clone(),
            path: dir.join(format!("{id}.jsonl")),
        };
        session.append(&json!({
            "k": "meta",
            "id": id,
            "model": model,
            "cwd": cwd.display().to_string(),
            "ts": now_secs(),
        }))?;
        Ok(session)
    }

    /// Handle for a session that already exists on disk; appends continue it.
    pub fn existing(id: &str, dir: &Path) -> Session {
        Session {
            id: id.to_string(),
            path: dir.join(format!("{id}.jsonl")),
        }
    }

    pub fn append(&self, entry: &Value) -> Result<()> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| AgentError::Config(format!("{}: {e}", self.path.display())))?;
        let mut line = entry.to_string();
        line.push('\n');
        file.write_all(line.as_bytes())
            .map_err(|e| AgentError::Config(format!("{}: {e}", self.path.display())))?;
        Ok(())
    }

    /// Append the entries for one history message, in part order.
    pub fn record_message(&self, m: &Message) -> Result<()> {
        match m.role {
            Role::Assistant => {
                let calls: Vec<Value> = m
                    .parts
                    .iter()
                    .filter_map(|p| match p {
                        Part::Call(c) => Some(json!({
                            "id": c.id,
                            "name": c.name,
                            "args": c.args,
                            "sig": c.thought_sig,
                        })),
                        _ => None,
                    })
                    .collect();
                self.append(&json!({ "k": "assistant", "text": m.text(), "calls": calls }))
            }
            Role::User => {
                for part in &m.parts {
                    match part {
                        Part::Text(t) => self.append(&json!({ "k": "user", "text": t }))?,
                        Part::Result {
                            call_id,
                            name,
                            content,
                            is_error,
                        } => self.append(&json!({
                            "k": "tool_result",
                            "id": call_id,
                            "name": name,
                            "content": content,
                            "err": *is_error,
                        }))?,
                        Part::Call(c) => {
                            return Err(AgentError::Config(format!(
                                "user message holds a call: {}",
                                c.name
                            )));
                        }
                    }
                }
                Ok(())
            }
        }
    }

    pub fn record_compaction(&self, summary: &str, replaced: usize) -> Result<()> {
        self.append(&json!({ "k": "compaction", "summary": summary, "covers": replaced }))
    }

    pub fn record_model_change(&self, model: &str) -> Result<()> {
        self.append(&json!({ "k": "model_change", "model": model }))
    }
}

/// Turn `--resume <id>|last` into a session id. Ids are hex-safe: no dots or slashes.
pub fn resolve(dir: &Path, id_or_last: &str) -> Result<String> {
    if id_or_last == "last" {
        let mut ids: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(dir)
            .map_err(|e| AgentError::Config(format!("{}: {e}", dir.display())))?
        {
            let entry = entry
                .map_err(|e| AgentError::Config(format!("{}: {e}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().to_string();
            if let Some(id) = name.strip_suffix(".jsonl") {
                ids.push(id.to_string());
            }
        }
        ids.sort();
        ids.last().cloned().ok_or_else(|| {
            AgentError::Config(format!("{} has no sessions yet", dir.display()))
        })
    } else {
        if id_or_last.is_empty()
            || !id_or_last
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(AgentError::Config(format!("bad session id: {id_or_last}")));
        }
        let path = dir.join(format!("{id_or_last}.jsonl"));
        if !path.is_file() {
            return Err(AgentError::Config(format!(
                "no session {id_or_last} in {}",
                dir.display()
            )));
        }
        Ok(id_or_last.to_string())
    }
}

pub fn summary_message(summary: &str) -> Message {
    Message::user(&format!("Earlier conversation was compacted. Summary:\n{summary}"))
}

/// Replay a session file into model-facing messages, plus the `meta` line if present.
pub fn replay(path: &Path) -> Result<(Vec<Message>, Option<Value>)> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
    let mut out: Vec<Message> = Vec::new();
    let mut pending: Vec<Part> = Vec::new();
    let mut meta: Option<Value> = None;

    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let bad = |msg: String| AgentError::Config(format!("{}:{}: {msg}", path.display(), n + 1));
        let v: Value = serde_json::from_str(line).map_err(|e| bad(format!("{e}")))?;
        let k = v
            .get("k")
            .and_then(|k| k.as_str())
            .ok_or_else(|| bad("entry has no k".to_string()))?;
        match k {
            "meta" => meta = Some(v),
            "model_change" => {}
            "user" => pending.push(Part::Text(
                v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string(),
            )),
            "tool_result" => pending.push(Part::Result {
                call_id: v.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                name: v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                content: v.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string(),
                is_error: v.get("err").and_then(|e| e.as_bool()).unwrap_or(false),
            }),
            "assistant" => {
                if !pending.is_empty() {
                    out.push(Message {
                        role: Role::User,
                        parts: std::mem::take(&mut pending),
                    });
                }
                let mut parts: Vec<Part> = Vec::new();
                let text_field = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text_field.is_empty() {
                    parts.push(Part::Text(text_field.to_string()));
                }
                if let Some(calls) = v.get("calls").and_then(|c| c.as_array()) {
                    for c in calls {
                        parts.push(Part::Call(ToolCall {
                            id: c.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                            name: c.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                            args: c.get("args").cloned().unwrap_or(json!({})),
                            thought_sig: c
                                .get("sig")
                                .and_then(|s| s.as_str())
                                .map(str::to_string),
                        }));
                    }
                }
                out.push(Message {
                    role: Role::Assistant,
                    parts,
                });
            }
            "compaction" => {
                pending.clear();
                out.clear();
                let summary = v.get("summary").and_then(|s| s.as_str()).unwrap_or("");
                out.push(summary_message(summary));
            }
            other => return Err(bad(format!("unknown entry kind {other}"))),
        }
    }
    if !pending.is_empty() {
        out.push(Message {
            role: Role::User,
            parts: pending,
        });
    }
    Ok((out, meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dir(name: &str) -> PathBuf {
        let d = PathBuf::from("/tmp").join(format!("neon-sess-{name}"));
        std::fs::remove_dir_all(&d).ok();
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn call(id: &str, cmd: &str) -> Part {
        Part::Call(ToolCall {
            id: id.to_string(),
            name: "bash".to_string(),
            args: json!({ "command": cmd }),
            thought_sig: None,
        })
    }

    fn result(id: &str) -> Part {
        Part::Result {
            call_id: id.to_string(),
            name: "bash".to_string(),
            content: "ok".to_string(),
            is_error: false,
        }
    }

    #[test]
    fn round_trip_rebuilds_the_same_messages() {
        let d = dir("roundtrip");
        let s = Session::create(&d, "gemini-3.5-flash-lite", &d).unwrap();
        s.record_message(&Message::user("run ls")).unwrap();
        s.record_message(&Message {
            role: Role::Assistant,
            parts: vec![Part::Text("listing".to_string()), call("call_0", "ls")],
        })
        .unwrap();
        s.record_message(&Message {
            role: Role::User,
            parts: vec![result("call_0"), Part::Text("steer text".to_string())],
        })
        .unwrap();

        let (msgs, meta) = replay(&s.path).unwrap();
        assert_eq!(meta.as_ref().and_then(|m| m["model"].as_str()), Some("gemini-3.5-flash-lite"));
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text(), "run ls");
        assert_eq!(msgs[1].role, Role::Assistant);
        assert!(matches!(msgs[1].parts[1], Part::Call(_)));
        assert!(matches!(msgs[2].parts[0], Part::Result { .. }));
        assert!(matches!(msgs[2].parts[1], Part::Text(_)));
        assert_eq!(msgs[2].text(), "steer text");
    }

    #[test]
    fn thought_signature_survives_the_session_log() {
        let d = dir("sig");
        let s = Session::create(&d, "m", &d).unwrap();
        s.record_message(&Message::user("go")).unwrap();
        s.record_message(&Message {
            role: Role::Assistant,
            parts: vec![Part::Call(ToolCall {
                id: "c0".to_string(),
                name: "bash".to_string(),
                args: json!({ "command": "ls" }),
                thought_sig: Some("sig-9".to_string()),
            })],
        })
        .unwrap();
        let (msgs, _) = replay(&s.path).unwrap();
        assert_eq!(msgs.len(), 2);
        match &msgs[1].parts[0] {
            Part::Call(c) => assert_eq!(c.thought_sig.as_deref(), Some("sig-9")),
            other => panic!("expected a call, got {other:?}"),
        }
    }

    #[test]
    fn compaction_entry_replaces_the_earlier_messages() {
        let d = dir("compacted");
        let s = Session::create(&d, "m", &d).unwrap();
        s.record_message(&Message::user("old task")).unwrap();
        s.record_message(&Message {
            role: Role::Assistant,
            parts: vec![Part::Text("old answer".to_string())],
        })
        .unwrap();
        s.record_compaction("the old task finished", 2).unwrap();
        s.record_message(&Message::user("next task")).unwrap();

        let (msgs, _meta) = replay(&s.path).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].text().contains("the old task finished"));
        assert!(!msgs[0].text().contains("old answer")); // originals stay in the log, not the context
        assert_eq!(msgs[1].text(), "next task");

        let raw = std::fs::read_to_string(&s.path).unwrap();
        assert!(raw.contains("old answer"));
        assert!(raw.contains("\"k\":\"compaction\""));
    }

    #[test]
    fn resolve_last_picks_the_newest_id() {
        let d = dir("resolve");
        std::fs::write(d.join("aaaa.jsonl"), "{\"k\":\"meta\"}\n").unwrap();
        std::fs::write(d.join("bbbb.jsonl"), "{\"k\":\"meta\"}\n").unwrap();
        assert_eq!(resolve(&d, "last").unwrap(), "bbbb");
        assert_eq!(resolve(&d, "aaaa").unwrap(), "aaaa");
        assert!(resolve(&d, "../evil").is_err());
        assert!(resolve(&d, "missing").is_err());
    }

    #[test]
    fn unknown_entry_kind_is_reported() {
        let d = dir("bad");
        std::fs::write(d.join("x.jsonl"), "{\"k\":\"banana\"}\n").unwrap();
        let err = replay(&d.join("x.jsonl")).unwrap_err();
        assert!(err.to_string().contains("unknown entry kind banana"));
    }
}

//! Neon: a minimal coding agent. One bash tool, Gemini over SSE, plugins as WASM later.

pub mod agent;
pub mod config;
pub mod error;
pub mod http;
pub mod llm;
pub mod messages;
pub mod plugin;
pub mod profile;
pub mod prompt;
pub mod session;
pub mod tools;
pub mod ui;

use std::path::PathBuf;

/// Raw tool output spills here: `~/.cache/neon/spill`.
pub fn spill_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".cache/neon/spill")
}

/// Session logs live here: `~/.local/state/neon/sessions`.
pub fn sessions_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".local/state/neon/sessions")
}

/// Append one line to a file, creating it if needed. Used by NEON_WIRE_DUMP.
pub fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

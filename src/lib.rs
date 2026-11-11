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

/// Edit distance between two short strings. Used to suggest a flag or a command after a typo.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let mut prev: Vec<usize> = (0..=b.chars().count()).collect();
    let mut cur = vec![0usize; b.chars().count() + 1];
    for (i, ca) in a.chars().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.chars().enumerate() {
            cur[j + 1] = prev[j] + if ca == cb { 0 } else { 1 };
            cur[j + 1] = cur[j + 1].min(prev[j + 1] + 1);
            cur[j + 1] = cur[j + 1].min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.chars().count()]
}

/// The closest candidate to a mistyped word, or None. The allowed distance grows with the
/// typed word, so a short typo needs a near match.
pub fn closest<'a>(typed: &str, candidates: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let limit = typed.chars().count().saturating_sub(2).min(3);
    candidates
        .map(|c| (edit_distance(typed, c), c))
        .filter(|(d, _)| *d <= limit)
        .min_by_key(|(d, _)| *d)
        .map(|(_, c)| c)
}

/// Append one line to a file, creating it if needed. Used by NEON_WIRE_DUMP.
pub fn append_line(path: &std::path::Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{line}")
}

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

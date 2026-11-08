//! System prompt assembly: one short built-in section plus AGENTS.md discovery.

use std::path::{Path, PathBuf};

/// About 30 lines. Keep it short: context engineering happens in files, not here.
pub const SYSTEM: &str = r#"You are Neon, a small coding agent working in one folder on a Linux machine.

Work one step at a time. Your only tool right now is bash.
Inspect before you change. Read a file before you edit it.
Prefer short commands. cat, sed, grep, patch, and python are all available.
Write files with a heredoc or with python, and quote the heredoc marker.
Say what you are about to do before you do it.
Never delete data you did not create in this session.
If a command fails, read the error, then try a narrower command.
Track your work in a plan file when a task takes more than three steps.
Report what you changed by path, and give the command output that proves it.
Keep answers short. The person reads this on a small screen.
Stop when the task is done. Do not repeat work you already did."#;

const CAP: usize = 8192;

/// AGENTS.md from the cwd upward to the home directory. Parents first, cwd last.
pub fn project_instructions(cwd: &Path) -> String {
    let home = std::env::var("HOME").map(PathBuf::from).ok();
    let mut found: Vec<PathBuf> = Vec::new();
    let mut dir = cwd.to_path_buf();
    loop {
        let file = dir.join("AGENTS.md");
        if file.is_file() {
            found.push(file);
        }
        if home.as_ref().map(|h| dir == *h).unwrap_or(false) {
            break;
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p.to_path_buf(),
            _ => break,
        }
    }
    found.reverse();

    let mut out = String::new();
    for path in found {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if out.len() + text.len() > CAP {
                break;
            }
            out.push_str(&format!("\n--- {} ---\n{}\n", path.display(), text));
        }
    }
    out
}

pub fn build_system(cwd: &Path) -> String {
    let extra = project_instructions(cwd);
    if extra.is_empty() {
        SYSTEM.to_string()
    } else {
        format!("{SYSTEM}\n\n## Project instructions{extra}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_agents_md_upward() {
        let dir = PathBuf::from("/tmp/neon-test-prompt/deep");
        std::fs::remove_dir_all("/tmp/neon-test-prompt").ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write("/tmp/neon-test-prompt/AGENTS.md", "parent rule").unwrap();
        std::fs::write(dir.join("AGENTS.md"), "local rule").unwrap();
        let text = build_system(&dir);
        assert!(text.contains("parent rule"));
        assert!(text.contains("local rule"));
        assert!(text.contains("You are Neon"));
        // Parent comes before the local file.
        assert!(text.find("parent rule") < text.find("local rule"));
    }
}

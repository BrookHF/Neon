//! Config from `~/.config/neon/config.yaml`. The Gemini key lives here, never in the environment.

use std::path::{Path, PathBuf};

use yaml_rust2::YamlLoader;

use crate::error::{AgentError, Result};
use crate::{sessions_dir, spill_dir};

#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    pub model: String,
    pub base_url: String,
    pub context_budget_tokens: usize,
    pub bash_timeout_secs: u64,
    pub bash_excerpt_bytes: usize,
    pub max_steps: usize,
    pub llm_retries: usize,
    pub max_tokens: u32,
    pub profile: String,
    pub spill_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_key: String::new(),
            model: "gemini-3.5-flash-lite".to_string(),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            context_budget_tokens: 200_000,
            bash_timeout_secs: 120,
            bash_excerpt_bytes: 16_384,
            max_steps: 24,
            llm_retries: 3,
            max_tokens: 16_000,
            profile: "default".to_string(),
            spill_dir: spill_dir(),
            sessions_dir: sessions_dir(),
        }
    }
}

pub fn default_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/neon/config.yaml")
}

/// Turn a leading `~/` into the home directory.
fn expand_home(value: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    match value.strip_prefix("~/") {
        Some(rest) => PathBuf::from(home).join(rest),
        None => PathBuf::from(value),
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
        let docs = YamlLoader::load_from_str(&text)
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
        let doc = docs
            .first()
            .ok_or_else(|| AgentError::Config("empty config file".to_string()))?;

        let mut cfg = Config::default();
        if let Some(v) = doc["api_key"].as_str() {
            cfg.api_key = v.to_string();
        }
        if let Some(v) = doc["model"].as_str() {
            cfg.model = v.to_string();
        }
        if let Some(v) = doc["base_url"].as_str() {
            cfg.base_url = v.trim_end_matches('/').to_string();
        }
        if let Some(v) = doc["context_budget_tokens"].as_i64() {
            cfg.context_budget_tokens = v.max(1) as usize;
        }
        if let Some(v) = doc["bash_timeout_secs"].as_i64() {
            cfg.bash_timeout_secs = v.max(1) as u64;
        }
        if let Some(v) = doc["bash_excerpt_bytes"].as_i64() {
            cfg.bash_excerpt_bytes = v.max(256) as usize;
        }
        if let Some(v) = doc["max_steps"].as_i64() {
            cfg.max_steps = v.max(1) as usize;
        }
        if let Some(v) = doc["llm_retries"].as_i64() {
            cfg.llm_retries = v.max(1) as usize;
        }
        if let Some(v) = doc["max_tokens"].as_i64() {
            cfg.max_tokens = v.max(256) as u32;
        }
        if let Some(v) = doc["profile"].as_str() {
            cfg.profile = v.to_string();
        }
        if let Some(v) = doc["spill_dir"].as_str() {
            cfg.spill_dir = expand_home(v);
        }
        if let Some(v) = doc["sessions_dir"].as_str() {
            cfg.sessions_dir = expand_home(v);
        }
        Ok(cfg)
    }

    pub fn require_key(&self) -> Result<String> {
        if self.api_key.trim().is_empty() {
            return Err(AgentError::Config(
                "api_key is empty. Fill it in ~/.config/neon/config.yaml, or run with --fixture <file>."
                    .to_string(),
            ));
        }
        Ok(self.api_key.clone())
    }
}

//! Config from `~/.config/neon/config.yaml`. The Gemini key lives here, never in the environment.
//! The model lives behind a lock, so `/model <name>` changes it without rebuilding the agent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use yaml_rust2::YamlLoader;

use crate::error::{AgentError, Result};
use crate::{sessions_dir, spill_dir};

#[derive(Clone, Debug)]
pub struct Config {
    pub api_key: String,
    /// Behind a lock so a `/model` change is seen by every request that follows.
    pub model_name: Arc<Mutex<String>>,
    pub base_url: String,
    pub context_budget_tokens: usize,
    pub bash_timeout_secs: u64,
    pub bash_excerpt_bytes: usize,
    pub max_steps: usize,
    pub llm_retries: usize,
    pub max_tokens: u32,
    pub stream_idle_secs: u64,
    pub profile: String,
    pub spill_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            api_key: String::new(),
            model_name: Arc::new(Mutex::new("gemini-3.5-flash-lite".to_string())),
            base_url: "https://generativelanguage.googleapis.com".to_string(),
            context_budget_tokens: 200_000,
            bash_timeout_secs: 120,
            bash_excerpt_bytes: 16_384,
            max_steps: 24,
            llm_retries: 3,
            max_tokens: 16_000,
            stream_idle_secs: 180,
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

/// The starter file neon writes on first run: two keys set, every other one documented.
const TEMPLATE: &str = r#"# neon config. Your Gemini key goes here, never in the environment.
# Get a key at https://aistudio.google.com/apikey. Leave api_key empty to run with --fixture.
api_key: ""
model: gemini-3.5-flash-lite
# gemini-3.5-flash is steadier for one large file write; flash-lite stalls mid-stream on those.
# context_budget_tokens: 200000   # tokens before neon summarizes the oldest messages
# max_tokens: 16000               # output cap per step; raise it for a big single-file write
# max_steps: 24                   # model steps per turn
# llm_retries: 3                  # retries for a transient failure (429, 5xx, dropped stream)
# stream_idle_secs: 180           # give up when a stream goes silent this long
# bash_timeout_secs: 120
# bash_excerpt_bytes: 16384       # output the model sees; the full raw output is spilled
# profile: default                # plugin rows from ~/.config/neon/profiles/<profile>.yaml
# spill_dir: ~/.cache/neon/spill
# sessions_dir: ~/.local/state/neon/sessions
"#;

/// Keys the config file accepts, named in every config error so the fix is obvious.
const KEYS: &str = "api_key, model, base_url, context_budget_tokens, max_tokens, max_steps, \
    llm_retries, stream_idle_secs, bash_timeout_secs, bash_excerpt_bytes, profile, spill_dir, \
    sessions_dir";

/// Write the starter config file. Mode 0600, because the key lives in it.
pub fn ensure_template(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AgentError::Config(format!("{}: {e}", parent.display())))?;
    }
    std::fs::write(path, TEMPLATE)
        .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
    Ok(())
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
        let docs = YamlLoader::load_from_str(&text).map_err(|e| {
            AgentError::Config(format!(
                "{}: {e} - config takes plain keys: {KEYS}",
                path.display()
            ))
        })?;
        let doc = docs
            .first()
            .ok_or_else(|| AgentError::Config(format!("{}: empty config file", path.display())))?;

        let mut cfg = Config::default();
        if let Some(v) = doc["api_key"].as_str() {
            cfg.api_key = v.to_string();
        }
        if let Some(v) = doc["model"].as_str() {
            cfg.set_model(v);
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
        if let Some(v) = doc["stream_idle_secs"].as_i64() {
            cfg.stream_idle_secs = v.max(10) as u64;
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

    /// The model in use right now.
    pub fn model(&self) -> String {
        self.model_name.lock().expect("model lock").clone()
    }

    pub fn set_model(&self, model: &str) {
        *self.model_name.lock().expect("model lock") = model.to_string();
    }

    pub fn require_key(&self) -> Result<String> {
        if self.api_key.trim().is_empty() {
            return Err(AgentError::Config(format!(
                "no api_key yet - add api_key: \"YOUR_KEY\" to {} \
                 (get a key at https://aistudio.google.com/apikey), \
                 or run with --fixture <file> to work without a key",
                default_path().display()
            )));
        }
        Ok(self.api_key.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        PathBuf::from("/tmp").join(format!("neon-cfg-{name}"))
    }

    #[test]
    fn template_is_written_with_mode_0600_and_loads() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp("template").join("nested").join("config.yaml");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
        ensure_template(&path).unwrap();
        assert!(path.is_file());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let cfg = Config::load(&path).unwrap();
        assert_eq!(cfg.model(), "gemini-3.5-flash-lite");
        assert!(cfg.api_key.trim().is_empty());
        // The hint names the file and the offline path, so nobody has to guess.
        let err = cfg.require_key().unwrap_err().to_string();
        assert!(err.contains("api_key"));
        assert!(err.contains("--fixture"));
        assert!(err.contains("config.yaml"));
    }

    #[test]
    fn model_changes_in_place_and_a_clone_sees_it() {
        let cfg = Config::default();
        cfg.set_model("gemini-3.5-flash");
        assert_eq!(cfg.model(), "gemini-3.5-flash");
        let shared = cfg.clone();
        cfg.set_model("gemini-3.5-pro");
        assert_eq!(shared.model(), "gemini-3.5-pro");
    }

    #[test]
    fn a_bad_config_file_names_the_valid_keys() {
        let dir = tmp("bad");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config.yaml");
        std::fs::write(&file, "api_key: [unclosed\n").unwrap();
        let err = Config::load(&file).unwrap_err().to_string();
        assert!(err.contains("config takes plain keys"));
        assert!(err.contains("max_tokens"));
    }
}

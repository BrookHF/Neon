//! Plugin rows: profiles and patches stack ordered layers onto an empty row list.
//!
//! Layer order: profile bundle, profile patch, home patch, `--patch` overlays.
//! A patch targets a row by `id`: it replaces the row, inserts a new one, or sets
//! `disabled`. Later layers win.

use std::path::{Path, PathBuf};

use yaml_rust2::YamlLoader;

use crate::error::{AgentError, Result};
use crate::plugin::Row;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()))
}

/// Where plugin directories live: `~/.config/neon/plugins`.
pub fn plugin_dir() -> PathBuf {
    home().join(".config/neon/plugins")
}

/// Where profile files live: `~/.config/neon/profiles`.
pub fn profile_dir() -> PathBuf {
    home().join(".config/neon/profiles")
}

/// The home patch: `~/.config/neon/patch.yaml`.
pub fn home_patch() -> PathBuf {
    home().join(".config/neon/patch.yaml")
}

/// Turn a leading `~/` into the home directory.
pub fn expand_home(value: &str) -> PathBuf {
    match value.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(value),
    }
}

fn rows_from_file(path: &Path) -> Result<Vec<Row>> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
    let docs = YamlLoader::load_from_str(&text)
        .map_err(|e| AgentError::Config(format!("{}: {e}", path.display())))?;
    let doc = docs
        .first()
        .ok_or_else(|| AgentError::Config(format!("{}: empty file", path.display())))?;
    let items = doc["rows"]
        .as_vec()
        .ok_or_else(|| AgentError::Config(format!("{}: rows list is required", path.display())))?;

    let mut rows = Vec::new();
    for item in items {
        let id = item["id"]
            .as_str()
            .ok_or_else(|| AgentError::Config(format!("{}: row needs an id", path.display())))?
            .to_string();
        let path = match item["plugin"].as_str() {
            Some(p) => expand_home(p),
            None => plugin_dir().join(&id),
        };
        let path_set = item["plugin"].as_str().is_some();
        let disabled = item["disabled"].as_bool().unwrap_or(false);
        let grants = match item["grants"].as_vec() {
            Some(v) => v
                .iter()
                .filter_map(|g| g.as_str().map(|s| s.to_string()))
                .collect(),
            None => Vec::new(),
        };
        rows.push(Row {
            id,
            path,
            path_set,
            disabled,
            grants,
        });
    }
    Ok(rows)
}

/// Apply layers onto an empty row list. Missing files are skipped.
pub fn layers(profile_dir: &Path, profile: &str, overlays: &[PathBuf]) -> Result<Vec<Row>> {
    let mut files = vec![
        profile_dir.join(format!("{profile}.yaml")),
        profile_dir.join(format!("{profile}.patch.yaml")),
        home_patch(),
    ];
    files.extend(overlays.iter().cloned());

    let mut rows: Vec<Row> = Vec::new();
    for file in files {
        if !file.is_file() {
            continue;
        }
        for row in rows_from_file(&file)? {
            match rows.iter_mut().find(|r| r.id == row.id) {
                Some(existing) => {
                    // A patch overrides only the fields it names.
                    if row.disabled {
                        existing.disabled = true;
                    }
                    if row.path_set {
                        existing.path = row.path;
                    }
                    if !row.grants.is_empty() {
                        existing.grants = row.grants;
                    }
                }
                None => rows.push(row),
            }
        }
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = PathBuf::from(format!("/tmp/neon-test-profile-{name}"));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        dir
    }

    #[test]
    fn layers_stack_and_patches_replace() {
        let dir = temp_dir("layers");
        let profiles = dir.join("profiles");
        std::fs::write(
            profiles.join("test.yaml"),
            "rows:\n  - id: a\n    plugin: ~/p/a\n    grants: [log]\n  - id: b\n    plugin: ~/p/b\n",
        )
        .unwrap();
        std::fs::write(
            profiles.join("test.patch.yaml"),
            "rows:\n  - id: a\n    disabled: true\n  - id: c\n    plugin: ~/p/c\n",
        )
        .unwrap();
        let overlay = dir.join("overlay.yaml");
        std::fs::write(&overlay, "rows:\n  - id: b\n    grants: [clock.now]\n").unwrap();

        let rows = layers(&profiles, "test", &[overlay]).expect("layers");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].id, "a");
        assert!(rows[0].disabled, "profile patch disables a");
        assert_eq!(rows[0].grants, vec!["log".to_string()], "disable keeps grants");
        assert_eq!(rows[1].id, "b");
        assert_eq!(rows[1].grants, vec!["clock.now".to_string()], "overlay replaces b");
        assert_eq!(rows[2].id, "c");
        assert_eq!(rows[2].id, "c");
        assert_eq!(rows[1].path, expand_home("~/p/b"));
    }

    #[test]
    fn missing_files_are_skipped() {
        let dir = temp_dir("empty");
        let rows = layers(&dir.join("profiles"), "nope", &[]).expect("empty stack");
        assert!(rows.is_empty());
    }
}

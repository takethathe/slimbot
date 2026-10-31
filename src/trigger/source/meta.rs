//! A data source on disk: one directory, `meta.json` plus `collect.js`.
//!
//! Sources are plain files, not config entries: the config schema drives the
//! WebUI editor's field list, and thirty sources would turn that list into
//! noise. Editing the directory by hand is a supported workflow, so loading
//! tolerates a half-finished source (missing script, broken JSON) and reports
//! it as a broken source instead of failing the whole tick.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// `snapshot` replaces the current value, `append` keeps a deduplicated window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceMode {
    #[default]
    Snapshot,
    Append,
}

impl SourceMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SourceMode::Snapshot => "snapshot",
            SourceMode::Append => "append",
        }
    }
}

pub const META_FILE: &str = "meta.json";
pub const SCRIPT_FILE: &str = "collect.js";
pub const ID_PREFIX: &str = "src-";
pub const DEFAULT_EVERY_N_TICKS: u32 = 1;
pub const DEFAULT_TIMEOUT_S: u64 = 10;
pub const DEFAULT_MAX_ITEMS: usize = 200;

/// What a source declares about itself. `id` is assigned by the host and never
/// changes; `name` is the model's choice and may repeat.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mode: SourceMode,
    #[serde(default = "default_every_n_ticks")]
    pub every_n_ticks: u32,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    #[serde(default = "default_max_items")]
    pub max_items: usize,
    #[serde(default)]
    pub allow_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: i64,
}

fn default_every_n_ticks() -> u32 {
    DEFAULT_EVERY_N_TICKS
}

fn default_timeout_s() -> u64 {
    DEFAULT_TIMEOUT_S
}

fn default_max_items() -> usize {
    DEFAULT_MAX_ITEMS
}

impl SourceMeta {
    /// Minimal, valid defaults for a new source.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            mode: SourceMode::Snapshot,
            every_n_ticks: DEFAULT_EVERY_N_TICKS,
            timeout_s: DEFAULT_TIMEOUT_S,
            max_items: DEFAULT_MAX_ITEMS,
            allow_paths: Vec::new(),
            output_schema: None,
            created_at: now_ms(),
        }
    }

    /// Clamp user-supplied values into the ranges the host can honour. Called
    /// on every save so a hand-edited `meta.json` cannot smuggle in a 0-tick
    /// cadence or an unbounded window.
    pub fn normalize(&mut self) {
        self.every_n_ticks = self.every_n_ticks.clamp(1, 1440);
        self.timeout_s = self.timeout_s.clamp(1, 120);
        self.max_items = self.max_items.clamp(1, 10_000);
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// One source directory: where its files live and whether they parse.
#[derive(Debug, Clone)]
pub struct LoadedSource {
    pub meta: SourceMeta,
    pub script: Option<String>,
    /// Set when the directory exists but cannot be used as-is.
    pub problem: Option<String>,
}

/// Scan the sources root. Directories without a readable `meta.json` become
/// entries with `problem` set, so `list` can show them instead of hiding them.
pub fn load_all(root: &Path) -> Vec<LoadedSource> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if dir_name.starts_with('.') {
            continue;
        }
        out.push(load_one(&path, &dir_name));
    }
    out.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    out
}

fn load_one(dir: &Path, dir_name: &str) -> LoadedSource {
    let meta_path = dir.join(META_FILE);
    let fallback = SourceMeta::new(dir_name.to_string(), dir_name.to_string());
    let (mut meta, problem) = match std::fs::read_to_string(&meta_path) {
        Ok(text) => match serde_json::from_str::<SourceMeta>(&text) {
            Ok(mut meta) => {
                // The directory name is the identity: a hand-edited id cannot
                // detach a source from the path other things reference.
                if meta.id.trim().is_empty() || meta.id != dir_name {
                    meta.id = dir_name.to_string();
                }
                (meta, None)
            }
            Err(e) => (
                fallback.clone(),
                Some(format!("meta.json is not valid JSON: {e}")),
            ),
        },
        Err(_) => (fallback.clone(), Some("meta.json is missing".to_string())),
    };
    meta.normalize();

    let script_path = dir.join(SCRIPT_FILE);
    let script = std::fs::read_to_string(&script_path).ok();
    let problem = problem.or_else(|| {
        if script.is_none() {
            Some("collect.js is missing".to_string())
        } else {
            None
        }
    });

    LoadedSource {
        meta,
        script,
        problem,
    }
}

/// Create (or overwrite) a source directory with its meta file.
pub fn save_meta(root: &Path, meta: &SourceMeta) -> Result<PathBuf> {
    let dir = root.join(&meta.id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let text = serde_json::to_string_pretty(meta)?;
    write_atomic(&dir.join(META_FILE), text.as_bytes())?;
    Ok(dir)
}

/// Write the collector atomically: a half-written script must never be
/// observable, because the next tick would compile garbage.
pub fn save_script(root: &Path, id: &str, script: &str) -> Result<()> {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    write_atomic(&dir.join(SCRIPT_FILE), script.as_bytes())
}

pub fn remove_source(root: &Path, id: &str) -> Result<bool> {
    let dir = root.join(id);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    Ok(true)
}

/// Write through a sibling temporary file and rename, so readers see either
/// the old content or the new content and never a partial write.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// Generate an unused `src-<4hex>` id.
pub fn generate_id(root: &Path) -> String {
    for _ in 0..64 {
        let candidate = format!("{ID_PREFIX}{:04x}", rand::random::<u16>());
        if !root.join(&candidate).exists() {
            return candidate;
        }
    }
    // 65536 ids in use is not a real scenario; fall back to a longer id so
    // creation still succeeds instead of looping forever.
    format!("{ID_PREFIX}{:08x}", rand::random::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn test_meta_defaults_when_fields_omitted() {
        let meta: SourceMeta = serde_json::from_str(r#"{"id":"src-1","name":"n"}"#).unwrap();
        assert_eq!(meta.mode, SourceMode::Snapshot);
        assert_eq!(meta.every_n_ticks, 1);
        assert_eq!(meta.timeout_s, 10);
        assert_eq!(meta.max_items, 200);
        assert!(meta.allow_paths.is_empty());
    }

    #[test]
    fn test_meta_normalize_clamps_out_of_range_values() {
        let mut meta = SourceMeta::new("src-1", "n");
        meta.every_n_ticks = 0;
        meta.timeout_s = 10_000;
        meta.max_items = 0;
        meta.normalize();
        assert_eq!(meta.every_n_ticks, 1);
        assert_eq!(meta.timeout_s, 120);
        assert_eq!(meta.max_items, 1);
    }

    #[test]
    fn test_load_all_reports_missing_script() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let dir = root.join("src-abcd");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, r#"{"name":"half"}"#);

        let loaded = load_all(root);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.id, "src-abcd");
        assert!(loaded[0].script.is_none());
        assert!(loaded[0].problem.as_ref().unwrap().contains("collect.js"));
    }

    #[test]
    fn test_load_all_reports_broken_meta_but_keeps_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("src-baad");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, "not json");
        write(&dir, SCRIPT_FILE, "async function collect() {}");

        let loaded = load_all(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.id, "src-baad");
        assert!(
            loaded[0]
                .problem
                .as_ref()
                .unwrap()
                .contains("not valid JSON")
        );
    }

    #[test]
    fn test_directory_name_wins_over_meta_id() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("src-real");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, r#"{"id":"src-other","name":"n"}"#);
        write(&dir, SCRIPT_FILE, "async function collect() {}");

        let loaded = load_all(tmp.path());
        assert_eq!(loaded[0].meta.id, "src-real");
    }

    #[test]
    fn test_generated_id_is_unused_and_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let id = generate_id(tmp.path());
        assert!(id.starts_with(ID_PREFIX));
        assert_eq!(id.len(), ID_PREFIX.len() + 4);
        // A generated id is not on disk yet.
        assert!(!tmp.path().join(&id).exists());
    }

    #[test]
    fn test_write_atomic_leaves_no_temp_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        assert!(!tmp.path().join("x.tmp").exists());
    }

    #[test]
    fn test_save_and_reload_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = SourceMeta::new("src-1234", "todo watcher");
        meta.mode = SourceMode::Append;
        meta.every_n_ticks = 3;
        save_meta(tmp.path(), &meta).unwrap();
        save_script(tmp.path(), "src-1234", "async function collect() {}").unwrap();

        let loaded = load_all(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.name, "todo watcher");
        assert_eq!(loaded[0].meta.mode, SourceMode::Append);
        assert_eq!(loaded[0].meta.every_n_ticks, 3);
        assert!(loaded[0].problem.is_none());
    }

    #[test]
    fn test_remove_source_deletes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = SourceMeta::new("src-9999", "gone");
        save_meta(tmp.path(), &meta).unwrap();
        assert!(remove_source(tmp.path(), "src-9999").unwrap());
        assert!(!remove_source(tmp.path(), "src-9999").unwrap());
    }
}

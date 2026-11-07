//! An executor on disk: one directory, `meta.json` plus `act.js`.
//!
//! The shape follows a data source (`trigger/source/meta.rs`) field for field,
//! because the two are operation mirrors and a maintainer should only have to
//! learn one layout. Scheduling is what does not carry over: an executor has no
//! tick, so there is no in-memory runtime table and every read scans the root.
//!
//! Files are the only truth and are edited by hand on purpose, so loading
//! tolerates a half-built executor (missing `act.js`, broken JSON) and reports
//! it as `broken` instead of failing the whole listing.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// Shared with the collector instead of copied: a third `write_atomic` / clock
// is a fourth place to drift. Moving both into `utils` is a separate refactor.
use crate::trigger::source::meta::{now_ms, write_atomic};

pub const META_FILE: &str = "meta.json";
pub const SCRIPT_FILE: &str = "act.js";
pub const ID_PREFIX: &str = "exec-";
pub const DEFAULT_TIMEOUT_S: u64 = 30;
pub const MIN_TIMEOUT_S: u64 = 1;
pub const MAX_TIMEOUT_S: u64 = 120;

/// What an executor declares about itself. `id` is assigned by the host and
/// never changes; `name` is the model's choice and may repeat.
///
/// `args_schema` is a JSON Schema subset validating the `args` a caller passes
/// to `run` — the reverse of a source's output schema (see `docs/executor.md`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutorMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_timeout_s")]
    pub timeout_s: u64,
    /// MCP server names whose write tools this executor may call. Empty means
    /// no connector writes at all, which is the safe default.
    #[serde(default)]
    pub allow_connectors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args_schema: Option<serde_json::Value>,
    #[serde(default)]
    pub created_at: i64,
}

fn default_timeout_s() -> u64 {
    DEFAULT_TIMEOUT_S
}

impl ExecutorMeta {
    /// Minimal, valid defaults for a new executor.
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            timeout_s: DEFAULT_TIMEOUT_S,
            allow_connectors: Vec::new(),
            args_schema: None,
            created_at: now_ms(),
        }
    }

    /// Clamp user-supplied values into the ranges the host can honour. Called
    /// on every load and every save, so a hand-edited `meta.json` cannot
    /// smuggle in a deadline the runner will not enforce.
    pub fn normalize(&mut self) {
        self.timeout_s = self.timeout_s.clamp(MIN_TIMEOUT_S, MAX_TIMEOUT_S);
        let mut servers: Vec<String> = Vec::new();
        for name in self.allow_connectors.drain(..) {
            let name = name.trim().to_string();
            if !name.is_empty() && !servers.contains(&name) {
                servers.push(name);
            }
        }
        self.allow_connectors = servers;
    }
}

/// One executor directory: what it declares, its script and whether it parses.
#[derive(Debug, Clone)]
pub struct LoadedExecutor {
    pub meta: ExecutorMeta,
    pub script: Option<String>,
    /// Set when the directory exists but cannot be used as-is.
    pub problem: Option<String>,
}

impl LoadedExecutor {
    /// Only two states: an executor either has everything it needs or it does
    /// not. There is no `pending` — nothing collects in the background.
    pub fn status(&self) -> &'static str {
        if self.problem.is_some() {
            "broken"
        } else {
            "ready"
        }
    }
}

/// Scan the executors root. Directories without a readable `meta.json` become
/// entries with `problem` set, so `list` can show them instead of hiding them.
///
/// This *is* the storage layer: there is no cache to warm and no reload hook to
/// call, because nothing here runs on a schedule.
pub fn load_all(root: &Path) -> Vec<LoadedExecutor> {
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

fn load_one(dir: &Path, dir_name: &str) -> LoadedExecutor {
    let meta_path = dir.join(META_FILE);
    let fallback = ExecutorMeta::new(dir_name.to_string(), dir_name.to_string());
    let (mut meta, problem) = match std::fs::read_to_string(&meta_path) {
        Ok(text) => match serde_json::from_str::<ExecutorMeta>(&text) {
            Ok(mut meta) => {
                // The directory name is the identity: a hand-edited id cannot
                // detach an executor from the path other things reference.
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

    let script = std::fs::read_to_string(dir.join(SCRIPT_FILE)).ok();
    let problem = problem.or_else(|| {
        if script.is_none() {
            Some(format!("{SCRIPT_FILE} is missing"))
        } else {
            None
        }
    });

    LoadedExecutor {
        meta,
        script,
        problem,
    }
}

/// The directory for `id`, which is one path component and nothing else: the
/// id comes from the model, so `../` must never reach outside the executors
/// root.
pub fn executor_dir(root: &Path, id: &str) -> Result<PathBuf> {
    if !is_single_component(id) {
        anyhow::bail!("'{id}' is not a single directory name");
    }
    Ok(root.join(id))
}

pub fn is_single_component(id: &str) -> bool {
    !id.is_empty() && id != "." && id != ".." && !id.contains(['/', '\\'])
}

/// Create (or overwrite) an executor directory with its meta file.
pub fn save_meta(root: &Path, meta: &ExecutorMeta) -> Result<PathBuf> {
    let dir = executor_dir(root, &meta.id)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let text = serde_json::to_string_pretty(meta)?;
    // Same atomic writer as the collector: a half-written `meta.json` would be
    // a `broken` executor that never existed as such.
    write_atomic(&dir.join(META_FILE), text.as_bytes())?;
    Ok(dir)
}

/// Write the act script atomically, so a reader never sees half a script.
pub fn save_script(root: &Path, id: &str, script: &str) -> Result<()> {
    let dir = executor_dir(root, id)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    write_atomic(&dir.join(SCRIPT_FILE), script.as_bytes())
}

pub fn remove_executor(root: &Path, id: &str) -> Result<bool> {
    let dir = executor_dir(root, id)?;
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    Ok(true)
}

/// Generate an unused `exec-<4hex>` id.
pub fn generate_id(root: &Path) -> String {
    for _ in 0..64 {
        let candidate = random_four_hex_id();
        if !root.join(&candidate).exists() {
            return candidate;
        }
    }
    // 64 collisions in a row means the four-hex space is nearly full, not that
    // one draw was unlucky, so look for the free slot instead of drawing again:
    // the id keeps the documented `exec-<4hex>` shape.
    free_four_hex_id(root)
        // Every one of the 65536 four-hex ids is taken, so no id of that shape
        // is left to hand out. The tail grows rather than looping forever; a
        // store that size is built by hand, never by this tool.
        .unwrap_or_else(|| format!("{ID_PREFIX}{:08x}", rand::random::<u32>()))
}

fn random_four_hex_id() -> String {
    format!("{ID_PREFIX}{:04x}", rand::random::<u16>())
}

/// The first unused `exec-<4hex>` id in id order. `None` only when all 65536
/// are in use.
fn free_four_hex_id(root: &Path) -> Option<String> {
    (0..=u16::MAX)
        .map(|n| format!("{ID_PREFIX}{n:04x}"))
        .find(|candidate| !root.join(candidate).exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn test_meta_defaults_when_fields_omitted() {
        let meta: ExecutorMeta = serde_json::from_str(r#"{"id":"exec-1","name":"n"}"#).unwrap();
        assert_eq!(meta.timeout_s, 30);
        assert!(meta.allow_connectors.is_empty());
        assert!(meta.args_schema.is_none());
    }

    #[test]
    fn test_meta_normalize_clamps_timeout_at_both_bounds() {
        // The four values spec A pins: below range, both edges, above range.
        for (given, expected) in [(0, 1), (1, 1), (120, 120), (121, 120)] {
            let mut meta = ExecutorMeta::new("exec-1", "n");
            meta.timeout_s = given;
            meta.normalize();
            assert_eq!(
                meta.timeout_s, expected,
                "timeout_s {given} normalizes to {expected}"
            );
        }
    }

    #[test]
    fn test_meta_normalize_trims_and_dedupes_connector_names() {
        let mut meta = ExecutorMeta::new("exec-1", "n");
        meta.allow_connectors = vec![" github ".into(), "github".into(), "".into()];
        meta.normalize();
        assert_eq!(meta.allow_connectors, vec!["github".to_string()]);
    }

    #[test]
    fn test_load_all_reports_missing_script() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("exec-abcd");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, r#"{"name":"half"}"#);

        let loaded = load_all(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.id, "exec-abcd");
        assert!(loaded[0].script.is_none());
        assert!(loaded[0].problem.as_ref().unwrap().contains("act.js"));
    }

    #[test]
    fn test_load_all_reports_broken_meta_but_keeps_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("exec-baad");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, "not json");
        write(&dir, SCRIPT_FILE, "async function act(ctx) {}");

        let loaded = load_all(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.id, "exec-baad");
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
        let dir = tmp.path().join("exec-real");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, r#"{"id":"exec-other","name":"n"}"#);
        write(&dir, SCRIPT_FILE, "async function act(ctx) {}");

        let loaded = load_all(tmp.path());
        assert_eq!(loaded[0].meta.id, "exec-real");
    }

    #[test]
    fn test_load_all_clamps_a_hand_edited_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("exec-9999");
        std::fs::create_dir_all(&dir).unwrap();
        write(&dir, META_FILE, r#"{"name":"n","timeout_s":10000}"#);
        write(&dir, SCRIPT_FILE, "async function act(ctx) {}");

        let loaded = load_all(tmp.path());
        assert_eq!(loaded[0].meta.timeout_s, 120);
    }

    #[test]
    fn test_is_single_component_accepts_one_plain_directory_name() {
        // The id becomes a path component, so every escape hatch is a refusal:
        // the empty string, the two relative names, and both separators.
        for id in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            "/abs",
            "exec-1234/",
        ] {
            assert!(!is_single_component(id), "{id:?} must be refused");
        }
        for id in ["exec-1234", "exec-abcd", "a", "...", ".hidden"] {
            assert!(is_single_component(id), "{id:?} must be accepted");
        }
    }

    #[test]
    fn test_generated_id_is_unused_and_prefixed() {
        let tmp = tempfile::tempdir().unwrap();
        let id = generate_id(tmp.path());
        assert!(id.starts_with("exec-"));
        assert_eq!(id.len(), ID_PREFIX.len() + 4);
        assert!(!tmp.path().join(&id).exists());
    }

    #[test]
    fn test_free_id_lookup_skips_the_ids_in_use() {
        // The sweep is what keeps an id at four hex digits when the random draws
        // keep colliding, so it has to skip what is already on disk.
        let tmp = tempfile::tempdir().unwrap();
        for id in ["exec-0000", "exec-0001"] {
            std::fs::create_dir_all(tmp.path().join(id)).unwrap();
        }
        assert_eq!(free_four_hex_id(tmp.path()).as_deref(), Some("exec-0002"));
    }

    #[test]
    fn test_save_and_reload_round_trip() {
        let tmp = tempfile::tempdir().unwrap();
        let mut meta = ExecutorMeta::new("exec-1234", "file the issue");
        meta.description = "opens a GitHub issue".into();
        meta.allow_connectors = vec!["github".into()];
        meta.args_schema = Some(serde_json::json!({
            "type": "object",
            "properties": {"title": {"type": "string"}}
        }));
        save_meta(tmp.path(), &meta).unwrap();
        save_script(tmp.path(), "exec-1234", "async function act(ctx) {}").unwrap();

        let loaded = load_all(tmp.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].meta.name, "file the issue");
        assert_eq!(loaded[0].meta.allow_connectors, vec!["github".to_string()]);
        assert!(loaded[0].meta.args_schema.is_some());
        assert!(loaded[0].problem.is_none());
        assert!(!tmp.path().join("exec-1234/meta.tmp").exists());
        assert!(!tmp.path().join("exec-1234/act.tmp").exists());
    }

    #[test]
    fn test_remove_executor_deletes_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = ExecutorMeta::new("exec-9999", "gone");
        save_meta(tmp.path(), &meta).unwrap();
        assert!(remove_executor(tmp.path(), "exec-9999").unwrap());
        assert!(!remove_executor(tmp.path(), "exec-9999").unwrap());
    }

    #[test]
    fn test_remove_executor_rejects_a_traversal_id() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let root = tmp.path().join("executors");
        std::fs::create_dir_all(&root).unwrap();

        assert!(remove_executor(&root, "../outside").is_err());
        assert!(outside.exists());
    }
}

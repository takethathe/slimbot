//! The `trigger_source` tool: the trigger agent's only way to manage sources.
//!
//! It is a multi-action tool rather than four tools with long names — fewer
//! names means fewer ways to pick the wrong one. Every action is host-owned
//! file work (ids, atomic writes, sanitising), so the model never has to be
//! trusted with paths.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::Tool;

use super::source::Origin;
use super::source::SourceLayer;
use super::source::meta::{self, LoadedSource, SourceMeta, SourceMode};
use super::source::{normalize, trial_collect};

/// Fallback for the cap when a caller passes none: the same number
/// `triggers.max_active_sources` defaults to.
pub const DEFAULT_MAX_ACTIVE_SOURCES: usize = 8;

pub struct TriggerSourceTool {
    sources: Arc<SourceLayer>,
    /// `triggers.max_active_sources`: how many sources the enabled rules may
    /// read before `create` refuses. `0` disables the cap, the same convention
    /// as `executors.max_count`.
    max_active_sources: usize,
}

impl TriggerSourceTool {
    /// `max_active_sources` is passed in rather than read from the global
    /// config so the cap is testable without a live config singleton — the
    /// same reason `ExecutorTool::new` takes `max_count`.
    pub fn new(sources: Arc<SourceLayer>, max_active_sources: usize) -> Self {
        Self {
            sources,
            max_active_sources,
        }
    }

    /// The sources the cap counts: what the enabled rules read, or — before
    /// anything has pushed a reference set — every source, which is the same
    /// rule the tick applies when deciding what is active.
    fn active_ids(&self) -> HashSet<String> {
        self.sources.active_source_ids().unwrap_or_else(|| {
            self.sources
                .status()
                .into_iter()
                .map(|status| status.meta.id)
                .collect()
        })
    }

    /// The report for a `create` refused by the cap. It names every counted
    /// source, because the caller's next move is to decide which one to delete
    /// and it can only decide by comparing them.
    fn over_cap(&self) -> Value {
        let active = self.active_ids();
        let rows: Vec<Value> = self
            .sources
            .status()
            .into_iter()
            .filter(|status| active.contains(&status.meta.id))
            .map(|status| {
                json!({
                    "id": status.meta.id,
                    "name": status.meta.name,
                    "description": status.meta.description,
                    // A source a rule reads is why the cap is full; an orphan
                    // that is merely listed is the safest one to delete.
                    "referenced": active.contains(&status.meta.id),
                    "origin": status.origin.as_str(),
                    "status": status.status,
                })
            })
            .collect();
        json!({
            "created": false,
            "error": format!(
                "not created: {} source(s) are active and `triggers.max_active_sources` is {}. Compare the sources below with what you want to watch, `delete` (or edit the rule that reads) the one that duplicates it or no longer runs, then `create` again.",
                rows.len(),
                self.max_active_sources
            ),
            "count": rows.len(),
            "max_active_sources": self.max_active_sources,
            "sources": rows,
        })
    }

    fn list(&self) -> Value {
        let rows: Vec<Value> = self
            .sources
            .status()
            .into_iter()
            .map(|status| {
                json!({
                    "id": status.meta.id,
                    // How the source came to exist: a directory an agent may
                    // edit, or an MCP subscription it can only read.
                    "origin": status.origin.as_str(),
                    "name": status.meta.name,
                    "description": status.meta.description,
                    "mode": status.meta.mode.as_str(),
                    "every_n_ticks": status.meta.every_n_ticks,
                    "status": status.status,
                    "last_collected_at": iso(status.last_collected_at),
                    "items_count": status.items_count,
                    "shape": status.shape,
                    "last_error": status.last_error,
                })
            })
            .collect();
        json!({ "sources": rows })
    }

    async fn create(&self, args: &Value) -> Result<Value> {
        let name = text(args, "name");
        if name.is_empty() {
            return Ok(json!({"error": "`name` is required"}));
        }
        // The cap is checked before the trial, not after: a refused create must
        // cost no script run and consume no id.
        if self.max_active_sources > 0 && self.active_ids().len() >= self.max_active_sources {
            return Ok(self.over_cap());
        }
        let id = meta::generate_id(self.sources.root());
        let mut source = SourceMeta::new(id.clone(), name);
        source.description = text(args, "description");
        source.mode = mode(args);
        source.every_n_ticks =
            int(args, "every_n_ticks").unwrap_or(source.every_n_ticks as i64) as u32;
        source.timeout_s = int(args, "timeout_s").unwrap_or(source.timeout_s as i64) as u64;
        source.max_items = int(args, "max_items").unwrap_or(source.max_items as i64) as usize;
        source.allow_paths = strings(args, "allow_paths");
        source.output_schema = args.get("output_schema").cloned();
        source.normalize();

        let script = text(args, "script");
        // A source without a script is a half-built one an agent fills in with
        // `update`, so there is nothing to try yet. With a script, the trial
        // decides whether anything is written at all: a collector that fails
        // here would fail every tick, and a broken definition on disk is worse
        // than a refused call.
        let test = if script.is_empty() {
            None
        } else {
            let started = std::time::Instant::now();
            let outcome = trial_collect(
                self.sources.workspace().to_path_buf(),
                self.sources.secrets(),
                self.sources.connectors(),
                &source,
                &script,
                &[],
            )
            .await;
            Some((outcome, started.elapsed()))
        };
        if let Some((Err(error), elapsed)) = &test {
            return Ok(json!({
                "created": false,
                "error": format!("not created: the collector failed its trial run, so nothing was written. Fix `script` and call `create` again. Cause: {error}"),
                "test": trial_failure(error, *elapsed),
            }));
        }

        meta::save_meta(self.sources.root(), &source)?;
        if !script.is_empty() {
            meta::save_script(self.sources.root(), &id, &script)?;
        }
        self.sources.add(LoadedSource {
            problem: if script.is_empty() {
                Some(format!("{} is missing", meta::SCRIPT_FILE))
            } else {
                None
            },
            script: (!script.is_empty()).then_some(script),
            meta: source.clone(),
        });

        let mut out = json!({
            "created": true,
            "id": source.id,
            "name": source.name,
            "mode": source.mode.as_str(),
            "every_n_ticks": source.every_n_ticks,
            "has_script": !args.get("script").and_then(|v| v.as_str()).unwrap_or("").is_empty(),
            "next_collection_at": iso(self.sources.next_collection_at(&source.id)),
            "note": "The snapshot stays empty until a tick collects it."
        });
        if let Some((Ok(normalized), elapsed)) = test {
            out["test"] = trial_success(&normalized, elapsed);
            out["note"] = json!(
                "The trial output is not kept: the snapshot stays empty until a tick collects it."
            );
        }
        Ok(out)
    }

    /// The origin a virtual source was declared with, or `None` when `id` names
    /// a source an agent may write. A virtual source has no directory, so
    /// neither `update` nor `delete` can reach it: saying "updated" would
    /// report a change nothing made, and the caller would stop looking for the
    /// real fix. Naming the config path is the whole point — the edit the
    /// caller wants is one file over.
    fn virtual_origin(&self, id: &str) -> Option<Origin> {
        self.sources
            .status()
            .into_iter()
            .find(|status| status.meta.id == id)
            .map(|status| status.origin)
            .filter(|origin| origin.is_virtual())
    }

    async fn update(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        if let Some(origin) = self.virtual_origin(&id) {
            return Ok(json!({
                "updated": false,
                "id": id,
                "origin": origin.as_str(),
                "error": format!(
                    "not updated: source '{id}' is a virtual source declared in mcp.servers[].triggers; edit the MCP config instead"
                ),
            }));
        }
        let Some(mut source) = self.sources.get(&id) else {
            return Ok(json!({"error": format!("no source with id '{id}'"), "id": id}));
        };

        if args.get("name").is_some() {
            source.name = text(args, "name");
        }
        if args.get("description").is_some() {
            source.description = text(args, "description");
        }
        if args.get("mode").is_some() {
            source.mode = mode(args);
        }
        if let Some(value) = int(args, "every_n_ticks") {
            source.every_n_ticks = value as u32;
        }
        if let Some(value) = int(args, "timeout_s") {
            source.timeout_s = value as u64;
        }
        if let Some(value) = int(args, "max_items") {
            source.max_items = value as usize;
        }
        if args.get("allow_paths").is_some() {
            source.allow_paths = strings(args, "allow_paths");
        }
        if args.get("output_schema").is_some() {
            source.output_schema = args.get("output_schema").cloned();
        }
        source.normalize();

        // The trial runs the merged meta against the incoming script, or — when
        // this update carries no script — against the one already on disk. A
        // settings-only edit is tested too on purpose: `timeout_s` and
        // `allow_paths` are part of what decides whether the collector can run,
        // so tightening them onto a script that cannot keep up has to be caught
        // here, where the old version is still one copy away. A source with no
        // collector at all has nothing to run, and stays a half-built one.
        let new_script = text(args, "script");
        let candidate = if !new_script.is_empty() {
            Some(new_script.clone())
        } else {
            std::fs::read_to_string(self.sources.root().join(&id).join(meta::SCRIPT_FILE)).ok()
        };
        let test = match candidate {
            Some(script) => {
                let started = std::time::Instant::now();
                let outcome = trial_collect(
                    self.sources.workspace().to_path_buf(),
                    self.sources.secrets(),
                    self.sources.connectors(),
                    &source,
                    &script,
                    &[],
                )
                .await;
                Some((outcome, started.elapsed()))
            }
            None => None,
        };
        if let Some((Err(error), elapsed)) = &test {
            return Ok(json!({
                "updated": false,
                "id": source.id,
                "error": format!("not updated: the collector failed its trial run, so the source keeps its previous version. Fix `script` (or the settings the trial runs under) and call `update` again. Cause: {error}"),
                "test": trial_failure(error, *elapsed),
            }));
        }

        meta::save_meta(self.sources.root(), &source)?;
        if !new_script.is_empty() {
            meta::save_script(self.sources.root(), &id, &new_script)?;
        }
        let script_on_disk = !new_script.is_empty()
            || self
                .sources
                .root()
                .join(&id)
                .join(meta::SCRIPT_FILE)
                .exists();
        self.sources.add(LoadedSource {
            problem: (!script_on_disk).then(|| format!("{} is missing", meta::SCRIPT_FILE)),
            script: std::fs::read_to_string(self.sources.root().join(&id).join(meta::SCRIPT_FILE))
                .ok(),
            meta: source.clone(),
        });

        let mut out = json!({
            "updated": true,
            "id": source.id,
            "has_script": script_on_disk,
            "next_collection_at": iso(self.sources.next_collection_at(&source.id)),
        });
        if let Some((Ok(normalized), elapsed)) = test {
            out["test"] = trial_success(&normalized, elapsed);
        }
        Ok(out)
    }

    fn delete(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        // A virtual source is not on disk, so `remove_source` would report
        // `deleted: false` for a source that exists; refusing names the config
        // as the real owner instead.
        if let Some(origin) = self.virtual_origin(&id) {
            return Ok(json!({
                "deleted": false,
                "id": id,
                "origin": origin.as_str(),
                "error": format!(
                    "not deleted: source '{id}' is a virtual source declared in mcp.servers[].triggers; edit the MCP config instead"
                ),
            }));
        }
        let removed = meta::remove_source(self.sources.root(), &id)?;
        self.sources.forget(&id);
        Ok(json!({ "deleted": removed, "id": id }))
    }
}

#[async_trait]
impl Tool for TriggerSourceTool {
    fn name(&self) -> &str {
        "trigger_source"
    }

    /// Risk class (ADR 0011): Writes trigger source definitions inside the workspace.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
    }

    fn description(&self) -> &str {
        "Manage data sources for triggers. A source is a collector script that turns a channel (a file, an endpoint, a mailbox) into `{items: [...]}`. Actions: list, create, update, delete. `create` and `update` run the collector once before writing anything, under the same environment and timeout the tick uses, and report the trial as `test: {ok, items, elapsed_ms}` (or `{ok: false, error}`): a failed trial writes nothing — `create` returns `created: false` and `update` keeps the previous version. `create` is refused once the number of sources the enabled triggers read reaches `triggers.max_active_sources`, and the refusal lists those sources so you can delete or retarget one (0 disables the cap). `list` reports field names and types only, never sample values; its `origin` says whether a source is `disk` (editable here), `notification` (pushed by an MCP server) or `tool` (one connector tool called each tick) — virtual sources come from the MCP config, so `update` and `delete` refuse them and point at the config."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "create", "update", "delete"],
                    "description": "Action to perform"
                },
                "id": { "type": "string", "description": "Source id (required for update/delete)" },
                "name": { "type": "string", "description": "Human name, your choice, may repeat" },
                "description": { "type": "string", "description": "One line: what this source watches" },
                "mode": {
                    "type": "string",
                    "enum": ["snapshot", "append"],
                    "description": "snapshot replaces the value each collection, append keeps a deduplicated window (default snapshot)"
                },
                "script": {
                    "type": "string",
                    "description": "Body of `async function collect(ctx) { return {items: [...]}; }`. Available: ctx.http.get/post(url, opts), ctx.fs.read/stat/list(path), ctx.secret(name), ctx.log(msg), ctx.now(). No shell, no sleep, no writes. Every item must be an object; the script is run once when you pass it (create/update) and a failure is reported instead of written."
                },
                "every_n_ticks": { "type": "integer", "description": "Collect once every N ticks (default 1)" },
                "timeout_s": { "type": "integer", "description": "Per-collection timeout in seconds (default 10, max 120); also the budget the trial run gets" },
                "max_items": { "type": "integer", "description": "Keep at most this many items (default 200)" },
                "allow_paths": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Absolute directories outside the workspace that the script may read"
                },
                "output_schema": {
                    "type": "object",
                    "description": "Optional expected item shape, echoed back by list as `declared`"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let action = text(&args, "action");
        let result = match action.as_str() {
            "list" => Ok(self.list()),
            "create" => self.create(&args).await,
            "update" => self.update(&args).await,
            "delete" => self.delete(&args),
            _ => Ok(json!({"error": format!("unknown action '{action}'")})),
        };
        Ok(result?.to_string())
    }
}

fn text(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn int(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(|v| v.as_i64())
}

fn strings(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn mode(args: &Value) -> SourceMode {
    match text(args, "mode").as_str() {
        "append" => SourceMode::Append,
        _ => SourceMode::Snapshot,
    }
}

fn iso(at: Option<i64>) -> Value {
    match at {
        Some(ms) => json!(
            chrono::DateTime::from_timestamp_millis(ms)
                .map(|dt| dt.with_timezone(&chrono::Local).to_rfc3339())
                .unwrap_or_else(|| ms.to_string())
        ),
        None => Value::Null,
    }
}

/// What a passed trial reports: how much it collected and how long it took.
/// Counts and timings only — the items themselves are external content and
/// never travel back through a tool result (ADR 0009).
fn trial_success(normalized: &normalize::Normalized, elapsed: std::time::Duration) -> Value {
    let mut report = json!({
        "ok": true,
        "items": normalized.entries.len(),
        "elapsed_ms": elapsed.as_millis() as u64,
    });
    if !normalized.warnings.is_empty() {
        report["warnings"] = json!(normalized.warnings);
    }
    report
}

/// What a failed trial reports: one readable line, and the same message in the
/// response's `error` so a caller that reads only one field still sees why.
fn trial_failure(error: &str, elapsed: std::time::Duration) -> Value {
    json!({
        "ok": false,
        "error": error,
        "elapsed_ms": elapsed.as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer() -> (tempfile::TempDir, Arc<SourceLayer>) {
        let tmp = tempfile::tempdir().unwrap();
        let layer = Arc::new(SourceLayer::new(
            tmp.path().join("triggers/sources"),
            tmp.path().to_path_buf(),
            Arc::new(std::collections::BTreeMap::new()),
            std::time::Duration::from_secs(60),
            super::super::source::DEFAULT_TTL,
        ));
        (tmp, layer)
    }

    async fn run(layer: &Arc<SourceLayer>, args: Value) -> Value {
        run_capped(layer, DEFAULT_MAX_ACTIVE_SOURCES, args).await
    }

    async fn run_capped(layer: &Arc<SourceLayer>, max: usize, args: Value) -> Value {
        let tool = TriggerSourceTool::new(layer.clone(), max);
        let text = tool.execute(args).await.unwrap();
        serde_json::from_str(&text).unwrap()
    }

    const GOOD_SCRIPT: &str =
        "async function collect(ctx) { return {items: [{title: 'a'}, {title: 'b'}]}; }";

    /// Everything under the sources root. A refused `create` must leave this
    /// empty: not the directory, and not a stray `.tmp` sibling either.
    fn dirs_under(root: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(root)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn test_create_runs_the_script_before_committing() {
        // The whole point of the embedded test: a source whose collector is
        // broken is refused by the call that defined it, not discovered at the
        // next tick. The response says what the trial saw, and says it without
        // the collected values (ADR 0009).
        let (_tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "n", "script": GOOD_SCRIPT}),
        )
        .await;
        assert_eq!(out["created"], true, "{out}");
        assert_eq!(out["test"]["ok"], true, "{out}");
        assert_eq!(out["test"]["items"], 2, "{out}");
        assert!(out["test"]["elapsed_ms"].is_u64(), "{out}");
        assert!(!out.to_string().contains("\"title\""), "no values: {out}");
    }

    #[tokio::test]
    async fn test_create_with_a_broken_script_writes_no_directory() {
        // `generate_id` is consumed before the trial; a failed create must not
        // leave even the empty directory behind, or the next `list` shows a
        // source nobody meant to keep.
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "broken",
                "script": "async function collect(ctx) { return {items: []} ",
            }),
        )
        .await;
        assert_eq!(out["created"], false, "{out}");
        assert!(out["error"].is_string(), "{out}");
        assert_eq!(out["test"]["ok"], false, "{out}");
        let dirs: Vec<String> = std::fs::read_dir(tmp.path().join("triggers/sources"))
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        assert!(dirs.is_empty(), "nothing may be written: {dirs:?}");
        assert!(out["id"].is_null(), "no id when nothing was created: {out}");
        assert!(layer.status().is_empty());
    }

    #[tokio::test]
    async fn test_create_with_a_script_that_returns_no_items_writes_no_directory() {
        // Not a syntax error — a shape error, which only the trial can catch:
        // the tick would otherwise book this source as failing every round.
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "wrong shape",
                "script": "async function collect(ctx) { return [1, 2, 3]; }",
            }),
        )
        .await;
        assert_eq!(out["created"], false, "{out}");
        assert_eq!(out["test"]["ok"], false, "{out}");
        assert!(
            out["test"]["error"].as_str().unwrap().contains("items"),
            "the reason names the shape: {out}"
        );
        assert!(out["id"].is_null(), "{out}");
        let root = tmp.path().join("triggers/sources");
        assert!(dirs_under(&root).is_empty(), "{out}");
        assert!(layer.status().is_empty());
    }

    #[tokio::test]
    async fn test_create_with_a_script_that_throws_writes_no_directory() {
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "throws",
                "script": "async function collect(ctx) { throw new Error('upstream is down'); }",
            }),
        )
        .await;
        assert_eq!(out["created"], false, "{out}");
        assert!(
            out["test"]["error"]
                .as_str()
                .unwrap()
                .contains("upstream is down"),
            "the thrown message reaches the caller: {out}"
        );
        assert!(
            dirs_under(&tmp.path().join("triggers/sources")).is_empty(),
            "{out}"
        );
        assert!(layer.status().is_empty());
    }

    #[tokio::test]
    async fn test_create_at_the_limit_is_refused_with_the_current_sources() {
        // The cap exists to force a comparison: a caller at the limit has to be
        // told what is already there to delete, or an unattended run has nowhere
        // to go.
        let (tmp, layer) = layer();
        let first = run(
            &layer,
            json!({"action": "create", "name": "one", "description": "watches A"}),
        )
        .await;
        let first_id = first["id"].as_str().unwrap().to_string();
        layer.set_referenced_sources(std::collections::HashSet::from([first_id.clone()]));

        let out = run_capped(&layer, 1, json!({"action": "create", "name": "two"})).await;
        assert_eq!(out["created"], false, "{out}");
        assert!(out["max_active_sources"].is_u64(), "{out}");
        let rows = out["sources"].as_array().unwrap_or_else(|| panic!("{out}"));
        assert_eq!(rows.len(), 1, "{out}");
        assert_eq!(rows[0]["id"], first_id.as_str());
        assert_eq!(rows[0]["name"], "one");
        assert_eq!(rows[0]["description"], "watches A");
        assert_eq!(rows[0]["referenced"], true);
        assert_eq!(
            dirs_under(&tmp.path().join("triggers/sources")).len(),
            1,
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_create_at_the_limit_never_runs_the_script() {
        // The refusal comes before the trial, so a blocked create costs no
        // network call and leaves nothing behind.
        let (tmp, layer) = layer();
        let _ = run(&layer, json!({"action": "create", "name": "one"})).await;
        let out = run_capped(
            &layer,
            1,
            json!({
                "action": "create",
                "name": "blocked",
                "script": "async function collect(ctx) { throw new Error('should never run'); }",
            }),
        )
        .await;
        assert_eq!(out["created"], false, "{out}");
        assert!(
            !out.to_string().contains("should never run"),
            "the trial never ran: {out}"
        );
        assert!(
            dirs_under(&tmp.path().join("triggers/sources")).len() == 1,
            "{out}"
        );
    }

    #[tokio::test]
    async fn test_create_below_the_limit_still_creates() {
        let (_tmp, layer) = layer();
        let _ = run_capped(&layer, 2, json!({"action": "create", "name": "one"})).await;
        let out = run_capped(&layer, 2, json!({"action": "create", "name": "two"})).await;
        assert_eq!(out["created"], true, "{out}");
    }

    #[tokio::test]
    async fn test_create_only_counts_sources_a_rule_reads() {
        // A cap on *active* sources must not be spent by an orphan collection:
        // a source no enabled rule names is not active, so it does not block a
        // new one. Both the counted and the uncounted source exist on disk.
        let tmp = tempfile::tempdir().unwrap();
        let layer = Arc::new(SourceLayer::new(
            tmp.path().join("triggers/sources"),
            tmp.path().to_path_buf(),
            Arc::new(std::collections::BTreeMap::new()),
            std::time::Duration::from_secs(60),
            super::super::source::DEFAULT_TTL,
        ));
        let used = run(&layer, json!({"action": "create", "name": "used"})).await;
        let used_id = used["id"].as_str().unwrap().to_string();
        let _orphan = run(&layer, json!({"action": "create", "name": "orphan"})).await;
        layer.set_referenced_sources(std::collections::HashSet::from([used_id.clone()]));

        let blocked = run_capped(&layer, 1, json!({"action": "create", "name": "next"})).await;
        assert_eq!(blocked["created"], false, "{blocked}");
        let rows = blocked["sources"]
            .as_array()
            .unwrap_or_else(|| panic!("{blocked}"));
        assert_eq!(rows.len(), 1, "only the active one: {blocked}");
        assert_eq!(rows[0]["id"], used_id.as_str());
        assert_eq!(rows[0]["referenced"], true);
    }

    #[tokio::test]
    async fn test_create_with_a_zero_cap_is_never_refused() {
        // `0` disables the cap, the same convention as `executors.max_count`.
        let (_tmp, layer) = layer();
        for index in 0..5 {
            let out = run_capped(
                &layer,
                0,
                json!({"action": "create", "name": format!("n{index}")}),
            )
            .await;
            assert_eq!(out["created"], true, "{out}");
        }
    }

    #[tokio::test]
    async fn test_create_with_a_script_that_outlives_its_timeout_writes_no_directory() {
        // The trial runs under the meta's own `timeout_s`, so a source that would
        // burn the tick's budget is refused at creation instead of degrading on
        // its first round. A runaway loop is the cheapest way to show that the
        // trial is cut at the budget it was told: the same 10s default would
        // take ten times as long.
        let (tmp, layer) = layer();
        let started = std::time::Instant::now();
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "runaway",
                "timeout_s": 1,
                "script": "async function collect(ctx) { while (true) {} }",
            }),
        )
        .await;
        assert_eq!(out["created"], false, "{out}");
        assert_eq!(out["test"]["ok"], false, "{out}");
        assert!(
            out["test"]["error"].as_str().unwrap().contains("budget"),
            "the reason is the deadline: {out}"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "it gave up on the meta's 1s budget: {:?}",
            started.elapsed()
        );
        assert!(
            dirs_under(&tmp.path().join("triggers/sources")).is_empty(),
            "{out}"
        );
        assert!(layer.status().is_empty());
    }

    #[tokio::test]
    async fn test_create_trial_rolls_the_collect_service_side_of_the_script() {
        // The trial has to see exactly what the tick sees: `ctx.secret` resolves
        // against `triggers.secrets` and `ctx.http` is installed, so a script can
        // be refused for the reason the tick would refuse it.
        let tmp = tempfile::tempdir().unwrap();
        let secrets = Arc::new(std::collections::BTreeMap::from([(
            "token".to_string(),
            "s3cr3t".to_string(),
        )]));
        let layer = Arc::new(SourceLayer::new(
            tmp.path().join("triggers/sources"),
            tmp.path().to_path_buf(),
            secrets,
            std::time::Duration::from_secs(60),
            super::super::source::DEFAULT_TTL,
        ));
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "needs a secret",
                "script": "async function collect(ctx) { return {items: [{token: ctx.secret('token')}]}; }",
            }),
        )
        .await;
        assert_eq!(out["created"], true, "{out}");
        assert_eq!(out["test"]["ok"], true, "{out}");
        assert!(
            !out.to_string().contains("s3cr3t"),
            "the value stays out: {out}"
        );

        // An unknown secret is the failure the tick would book, reported as such.
        let missing = run(
            &layer,
            json!({
                "action": "create",
                "name": "no such secret",
                "script": "async function collect(ctx) { return {items: [{t: ctx.secret('nope')}]}; }",
            }),
        )
        .await;
        assert_eq!(missing["created"], false, "{missing}");
        assert!(
            missing["test"]["error"].as_str().unwrap().contains("nope"),
            "{missing}"
        );

        // `ctx.http` is the same shared surface; a private target is refused.
        let http = run(
            &layer,
            json!({
                "action": "create",
                "name": "http",
                "script": "async function collect(ctx) { return {items: [{r: await ctx.http.get('http://127.0.0.1:8080/x')}]}; }",
            }),
        )
        .await;
        assert_eq!(http["created"], false, "{http}");
        assert!(
            http["test"]["error"]
                .as_str()
                .unwrap()
                .to_lowercase()
                .contains("private"),
            "the refusal is the shared HTTP guard's own: {http}"
        );
    }

    #[tokio::test]
    async fn test_create_returns_an_id_and_next_tick() {
        let (_tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "todo watcher", "description": "watches TODO.md"}),
        )
        .await;
        assert_eq!(out["created"], true);
        assert!(out["id"].as_str().unwrap().starts_with("src-"));
        assert!(out["next_collection_at"].is_string());
        assert_eq!(out["has_script"], false);
    }

    #[tokio::test]
    async fn test_create_requires_a_name() {
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "create"})).await;
        assert!(out["error"].as_str().unwrap().contains("name"));
    }

    #[tokio::test]
    async fn test_create_without_a_script_is_a_half_built_source() {
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "create", "name": "half"})).await;
        let id = out["id"].as_str().unwrap().to_string();
        let listed = run(&layer, json!({"action": "list"})).await;
        let row = &listed["sources"][0];
        assert_eq!(row["id"], id);
        assert_eq!(row["status"], "degraded");
        assert!(row["last_error"].as_str().unwrap().contains("collect.js"));
    }

    #[tokio::test]
    async fn test_create_writes_script_and_meta_to_disk() {
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({
                "action": "create",
                "name": "n",
                "mode": "append",
                "every_n_ticks": 3,
                "script": "async function collect(ctx) { return {items: []}; }"
            }),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();
        let dir = tmp.path().join("triggers/sources").join(&id);
        assert!(dir.join("meta.json").exists());
        assert!(dir.join("collect.js").exists());
        assert_eq!(out["mode"], "append");
        assert_eq!(out["every_n_ticks"], 3);
        assert_eq!(out["has_script"], true);
    }

    #[tokio::test]
    async fn test_list_reports_shape_without_values() {
        let (_tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "n", "script": "async function collect(ctx) { return {items: []}; }"}),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();
        // Give the layer a collected value with real content.
        layer.add(LoadedSource {
            meta: layer.get(&id).unwrap(),
            script: Some("async function collect(ctx) { return {items: []}; }".into()),
            problem: None,
        });
        let report = layer.tick(1).await;
        assert_eq!(report.succeeded, vec![id.clone()]);

        let listed = run(&layer, json!({"action": "list"})).await;
        let text = listed.to_string();
        assert!(!text.contains("collect.js"));
        assert_eq!(listed["sources"][0]["items_count"], 0);
        assert_eq!(listed["sources"][0]["status"], "ready");
    }

    #[tokio::test]
    async fn test_update_replaces_only_given_fields() {
        let (_tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "first", "description": "keep me"}),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();
        let updated = run(
            &layer,
            json!({"action": "update", "id": id, "name": "second"}),
        )
        .await;
        assert_eq!(updated["updated"], true);
        let meta = layer.get(&id).unwrap();
        assert_eq!(meta.name, "second");
        assert_eq!(meta.description, "keep me");
    }

    #[tokio::test]
    async fn test_update_runs_a_new_script_before_overwriting() {
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "n", "script": GOOD_SCRIPT}),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();
        let updated = run(
            &layer,
            json!({"action": "update", "id": id.clone(), "script": GOOD_SCRIPT}),
        )
        .await;
        assert_eq!(updated["updated"], true, "{updated}");
        assert_eq!(updated["test"]["ok"], true, "{updated}");
        assert_eq!(updated["test"]["items"], 2, "{updated}");
        assert!(
            tmp.path()
                .join("triggers/sources")
                .join(&id)
                .join("collect.js")
                .exists()
        );
    }

    #[tokio::test]
    async fn test_update_with_a_broken_script_keeps_the_old_version() {
        // The whole reason update tests first: an edit that cannot collect must
        // leave the source exactly as it was, on disk and in memory, rather than
        // overwrite a working collector with one that fails every tick.
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "keep", "script": GOOD_SCRIPT}),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();
        let dir = tmp.path().join("triggers/sources").join(&id);
        let meta_before = std::fs::read_to_string(dir.join("meta.json")).unwrap();

        let updated = run(
            &layer,
            json!({"action": "update", "id": id.clone(), "name": "renamed", "script": "async function collect(ctx) { throw new Error('nope'); }"}),
        )
        .await;
        assert_eq!(updated["updated"], false, "{updated}");
        assert!(updated["error"].is_string(), "{updated}");
        assert_eq!(updated["test"]["ok"], false, "{updated}");
        assert_eq!(
            std::fs::read_to_string(dir.join("collect.js")).unwrap(),
            GOOD_SCRIPT,
            "the old collector is untouched"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("meta.json")).unwrap(),
            meta_before,
            "a failed update writes no meta either"
        );
        let meta = layer.get(&id).unwrap();
        assert_eq!(meta.name, "keep", "the name was not taken either");
        assert_eq!(layer.status().len(), 1, "memory still holds the source");
        // And it is the collector that kept working: the next tick still runs it.
        layer.tick(4).await;
        assert_eq!(
            layer.status()[0].status,
            "ready",
            "the surviving collector still collects"
        );
    }

    #[tokio::test]
    async fn test_update_settings_only_re_tests_the_script_on_disk() {
        // A settings-only update is still tested: `timeout_s` is part of the
        // trial's budget, so tightening it onto a slower script must be caught
        // at the update, not on the next tick.
        let (tmp, layer) = layer();
        let out = run(
            &layer,
            json!({"action": "create", "name": "n", "script": "async function collect(ctx) { while (true) {} }"}),
        )
        .await;
        assert_eq!(out["created"], false, "the runaway collector never lands");

        // A source created without a script is the normal half-built case: it
        // has nothing to run yet, so a rename must not be refused for it.
        let half = run(&layer, json!({"action": "create", "name": "half"})).await;
        let half_id = half["id"].as_str().unwrap().to_string();
        let renamed = run(
            &layer,
            json!({"action": "update", "id": half_id.clone(), "name": "half renamed"}),
        )
        .await;
        assert_eq!(renamed["updated"], true, "{renamed}");
        assert!(
            renamed["test"].is_null(),
            "no script, nothing to test: {renamed}"
        );

        // Add a script that only the generous default budget lets finish, then
        // tighten the timeout under it: the update is refused and the source
        // keeps both the script and the timeout it was created with.
        let slow = "async function collect(ctx) { const t = Date.now(); while (Date.now() - t < 2500) {} return {items: []}; }";
        let added = run(
            &layer,
            json!({"action": "update", "id": half_id.clone(), "script": slow}),
        )
        .await;
        assert_eq!(added["updated"], true, "{added}");
        let dir = tmp.path().join("triggers/sources").join(&half_id);
        let before = std::fs::read_to_string(dir.join("collect.js")).unwrap();
        let tightened = run(
            &layer,
            json!({"action": "update", "id": half_id.clone(), "timeout_s": 1}),
        )
        .await;
        assert_eq!(tightened["updated"], false, "{tightened}");
        assert_eq!(tightened["test"]["ok"], false, "{tightened}");
        assert_eq!(
            std::fs::read_to_string(dir.join("collect.js")).unwrap(),
            before
        );
        assert_eq!(layer.get(&half_id).unwrap().timeout_s, 10);
    }

    #[tokio::test]
    async fn test_update_unknown_id_is_an_error_not_a_create() {
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "update", "id": "src-0000"})).await;
        assert!(out["error"].as_str().unwrap().contains("no source"));
    }

    #[tokio::test]
    async fn test_delete_removes_directory_and_runtime() {
        let (tmp, layer) = layer();
        let out = run(&layer, json!({"action": "create", "name": "gone"})).await;
        let id = out["id"].as_str().unwrap().to_string();
        let deleted = run(&layer, json!({"action": "delete", "id": id.clone()})).await;
        assert_eq!(deleted["deleted"], true);
        assert!(!tmp.path().join("triggers/sources").join(&id).exists());
        assert!(layer.get(&id).is_none());
    }

    #[tokio::test]
    async fn test_list_shows_virtual_sources_with_their_origin() {
        // An agent has to be able to discover what a connector feeds the tick;
        // a virtual source is listed like any other, labelled so it is clear it
        // cannot be edited as a directory.
        let (_tmp, layer) = layer();
        let _ = run(&layer, json!({"action": "create", "name": "on disk"})).await;
        let subscriptions = crate::config::McpServerConfig {
            name: "ci".to_string(),
            triggers: vec![
                crate::config::McpTriggerSubscription {
                    r#type: "notification".to_string(),
                    uri: "notifications/builds".to_string(),
                    source_id: "builds".to_string(),
                    description: "last build".to_string(),
                    ..Default::default()
                },
                crate::config::McpTriggerSubscription {
                    r#type: "tool".to_string(),
                    tool: "list_runs".to_string(),
                    source_id: "ci-runs".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        layer.attach_mcp_servers(&[subscriptions]);

        let listed = run(&layer, json!({"action": "list"})).await;
        let rows = listed["sources"].as_array().unwrap();
        let row = |id: &str| {
            rows.iter()
                .find(|r| r["id"] == id)
                .unwrap_or_else(|| panic!("{id} must be listed: {rows:?}"))
                .clone()
        };
        assert_eq!(row("builds")["origin"], "notification");
        assert_eq!(row("builds")["name"], "ci");
        assert_eq!(row("builds")["description"], "last build");
        assert_eq!(row("ci-runs")["origin"], "tool");
        assert_eq!(row("ci-runs")["description"], "mcp ci tool list_runs");
        assert_eq!(
            rows.iter().filter(|r| r["origin"] == "disk").count(),
            1,
            "the directory source keeps its own origin: {rows:?}"
        );
        // Still a shape report: no collected value is echoed back.
        assert_eq!(row("builds")["shape"]["observed"]["items"], 0);
    }

    #[tokio::test]
    async fn test_update_of_a_virtual_source_is_refused_and_changes_nothing() {
        // An `update` aimed at a subscription must not answer `updated: true`:
        // there is no directory to write, so the only truthful answer is a
        // refusal that names where the source really lives.
        let (_tmp, layer) = layer();
        layer.attach_mcp_servers(&[crate::config::McpServerConfig {
            name: "ci".to_string(),
            triggers: vec![crate::config::McpTriggerSubscription {
                r#type: "notification".to_string(),
                uri: "notifications/builds".to_string(),
                source_id: "builds".to_string(),
                description: "last build".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }]);
        let before = layer.status();
        let out = run(
            &layer,
            json!({"action": "update", "id": "builds", "name": "renamed"}),
        )
        .await;
        assert_eq!(out["updated"], false, "{out}");
        assert_eq!(out["origin"], "notification", "{out}");
        let error = out["error"].as_str().unwrap_or_default();
        assert!(error.contains("builds"), "{out}");
        assert!(error.contains("mcp.servers[].triggers"), "{out}");
        // Nothing moved: not the synthesized meta in memory, and not a
        // directory that the write path might have invented for it.
        let meta = layer.get("builds").expect("the subscription still exists");
        assert_eq!(meta.name, "ci", "the meta still names its server: {meta:?}");
        assert_eq!(meta.description, "last build");
        assert_eq!(layer.status().len(), before.len());
        assert!(dirs_under(layer.root()).is_empty(), "{out}");
    }

    #[tokio::test]
    async fn test_delete_of_a_virtual_source_is_refused_and_changes_nothing() {
        // `delete` would otherwise report `deleted: false` for a source that
        // plainly exists and leave the caller guessing; the refusal says the
        // config owns it and the source survives.
        let (_tmp, layer) = layer();
        layer.attach_mcp_servers(&[crate::config::McpServerConfig {
            name: "ci".to_string(),
            triggers: vec![crate::config::McpTriggerSubscription {
                r#type: "tool".to_string(),
                tool: "list_runs".to_string(),
                source_id: "ci-runs".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        }]);
        let out = run(&layer, json!({"action": "delete", "id": "ci-runs"})).await;
        assert_eq!(out["deleted"], false, "{out}");
        assert_eq!(out["origin"], "tool", "{out}");
        let error = out["error"].as_str().unwrap_or_default();
        assert!(error.contains("ci-runs"), "{out}");
        assert!(error.contains("mcp.servers[].triggers"), "{out}");
        assert!(
            layer.get("ci-runs").is_some(),
            "the subscription survives a refused delete"
        );
    }

    #[tokio::test]
    async fn test_delete_unknown_id_is_not_called_a_virtual_source() {
        // The virtual-source refusal is for sources that exist; an id nobody
        // knows keeps the plain "nothing was removed" answer.
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "delete", "id": "src-0000"})).await;
        assert_eq!(out["deleted"], false, "{out}");
        assert!(out["error"].is_null(), "{out}");
    }

    #[tokio::test]
    async fn test_unknown_action_reports_an_error() {
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "frobnicate"})).await;
        assert!(out["error"].as_str().unwrap().contains("frobnicate"));
    }
}

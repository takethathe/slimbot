//! The `trigger_source` tool: the trigger agent's only way to manage sources.
//!
//! It is a multi-action tool rather than four tools with long names — fewer
//! names means fewer ways to pick the wrong one. Every action is host-owned
//! file work (ids, atomic writes, sanitising), so the model never has to be
//! trusted with paths.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::tool::Tool;

use super::source::SourceLayer;
use super::source::meta::{self, LoadedSource, SourceMeta, SourceMode};

pub struct TriggerSourceTool {
    sources: Arc<SourceLayer>,
}

impl TriggerSourceTool {
    pub fn new(sources: Arc<SourceLayer>) -> Self {
        Self { sources }
    }

    fn list(&self) -> Value {
        let rows: Vec<Value> = self
            .sources
            .status()
            .into_iter()
            .map(|status| {
                json!({
                    "id": status.meta.id,
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

    fn create(&self, args: &Value) -> Result<Value> {
        let name = text(args, "name");
        if name.is_empty() {
            return Ok(json!({"error": "`name` is required"}));
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

        Ok(json!({
            "created": true,
            "id": source.id,
            "name": source.name,
            "mode": source.mode.as_str(),
            "every_n_ticks": source.every_n_ticks,
            "has_script": !args.get("script").and_then(|v| v.as_str()).unwrap_or("").is_empty(),
            "next_collection_at": iso(self.sources.next_collection_at(&source.id)),
            "note": "The snapshot stays empty until a tick collects it."
        }))
    }

    fn update(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
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

        meta::save_meta(self.sources.root(), &source)?;
        let script = text(args, "script");
        if !script.is_empty() {
            meta::save_script(self.sources.root(), &id, &script)?;
        }
        let script_on_disk = !script.is_empty()
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

        Ok(json!({
            "updated": true,
            "id": source.id,
            "has_script": script_on_disk,
            "next_collection_at": iso(self.sources.next_collection_at(&source.id)),
        }))
    }

    fn delete(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
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
        "Manage data sources for triggers. A source is a collector script that turns a channel (a file, an endpoint, a mailbox) into `{items: [...]}`. Actions: list, create, update, delete. `list` reports field names and types only, never sample values."
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
                    "description": "Body of `async function collect(ctx) { return {items: [...]}; }`. Available: ctx.http.get/post(url, opts), ctx.fs.read/stat/list(path), ctx.secret(name), ctx.log(msg), ctx.now(). No shell, no sleep, no writes."
                },
                "every_n_ticks": { "type": "integer", "description": "Collect once every N ticks (default 1)" },
                "timeout_s": { "type": "integer", "description": "Per-collection timeout in seconds (default 10, max 120)" },
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
            "create" => self.create(&args),
            "update" => self.update(&args),
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
        let tool = TriggerSourceTool::new(layer.clone());
        let text = tool.execute(args).await.unwrap();
        serde_json::from_str(&text).unwrap()
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
    async fn test_unknown_action_reports_an_error() {
        let (_tmp, layer) = layer();
        let out = run(&layer, json!({"action": "frobnicate"})).await;
        assert!(out["error"].as_str().unwrap().contains("frobnicate"));
    }
}

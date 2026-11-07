//! The `executor` tool: the executor agent's only way to manage executors.
//!
//! One multi-action tool rather than four long names, mirroring
//! `trigger_source`: fewer names means fewer ways to pick the wrong one. The
//! store half is host-owned file work (ids, paths, atomic writes, clamping), so
//! the model never has to be trusted with a path; the one action that executes
//! a stored script lives in [`super::run`]. Nothing is cached — each action
//! scans the directory, so a hand-edited meta is what the next call reads.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};

use crate::tool::Tool;

use super::args as call_args_schema;
use super::meta::{self, ExecutorMeta};
use super::run::run_stored;

/// Largest number of schema fields echoed by `list`. A pathological schema
/// must not turn one listing into a wall of text.
///
/// A bound on one tool result, not a context budget: it does not move with the
/// provider's window and does not belong in `pressure.rs` (ADR 0013), the same
/// way `docs/codemode.md` describes the 16KB output ceiling.
const MAX_SCHEMA_FIELDS: usize = 64;

/// How many stored executors a cap refusal names.
///
/// `executors.max_count` may be as high as 10000, so listing the whole store
/// would turn one refusal into a wall of text — the same reason
/// [`MAX_SCHEMA_FIELDS`] exists. A caller that needs the rest calls `list`.
const MAX_CAP_CANDIDATES: usize = 10;

pub struct ExecutorTool {
    root: PathBuf,
    /// `triggers.secrets`, the same map the collect entry reads: an action's
    /// `ctx.secret(name)` resolves against one source, so there is one place to
    /// put a credential. The values also feed the run's [`Redactor`].
    secrets: Arc<BTreeMap<String, String>>,
    /// `executors.max_count`: the soft cap on stored executors. `0` disables it,
    /// so the cap costs nothing until a deployment asks for it.
    max_count: usize,
}

impl ExecutorTool {
    /// `root` is `workspace_dir/executors`; it need not exist yet, since the
    /// first save creates it. `max_count` is passed in rather than read from the
    /// global config so the cap is testable without a live config singleton.
    pub fn new(root: PathBuf, secrets: Arc<BTreeMap<String, String>>, max_count: usize) -> Self {
        Self {
            root,
            secrets,
            max_count,
        }
    }

    fn list(&self) -> Value {
        let rows: Vec<Value> = meta::load_all(&self.root)
            .into_iter()
            .map(|loaded| {
                json!({
                    "id": loaded.meta.id,
                    "name": loaded.meta.name,
                    "description": loaded.meta.description,
                    "timeout_s": loaded.meta.timeout_s,
                    "allow_connectors": loaded.meta.allow_connectors,
                    "args_schema_fields": schema_fields(loaded.meta.args_schema.as_ref()),
                    "status": loaded.status(),
                    "last_error": loaded.problem,
                })
            })
            .collect();
        json!({
            "executors": rows,
            "note": "`args_schema_fields` gives names and types only. There is no tick: an executor runs only when something asks for it."
        })
    }

    fn create(&self, args: &Value) -> Result<Value> {
        let name = text(args, "name");
        if name.is_empty() {
            return Ok(json!({"error": "`name` is required"}));
        }
        // Sprawl guard (spec M): the cap is the only mechanism, and it acts here
        // alone. `update` changes an entry that already exists, so it adds
        // nothing for the cap to count.
        let stored = meta::load_all(&self.root);
        if self.max_count > 0 && stored.len() >= self.max_count {
            return Ok(over_cap(self.max_count, &stored));
        }
        let id = meta::generate_id(&self.root);
        let mut executor = ExecutorMeta::new(id.clone(), name);
        replace_given(args, &mut executor);
        executor.normalize();
        // Refuse a schema the host cannot enforce before anything is written, so
        // a strong-looking declaration never lands as a silent no-op.
        if let Err(problem) = call_args_schema::validate(executor.args_schema.as_ref()) {
            return Ok(json!({"error": problem}));
        }

        // Meta first: it creates the directory and pins the identity, so a
        // half-built executor is a real, listable `broken` entry rather than a
        // failed call. The script follows in its own atomic write.
        meta::save_meta(&self.root, &executor)?;
        let script = text(args, "script");
        if !script.is_empty() {
            meta::save_script(&self.root, &id, &script)?;
        }

        Ok(json!({
            "created": true,
            "id": executor.id,
            "name": executor.name,
            "timeout_s": executor.timeout_s,
            "allow_connectors": executor.allow_connectors,
            "has_act_script": !script.is_empty(),
            "has_args_schema": executor.args_schema.is_some(),
            "status": if script.is_empty() { "broken" } else { "ready" },
            "note": if script.is_empty() {
                "Half-built: no `act.js` yet, so this executor stays broken until `update` supplies `script`."
            } else {
                "Stored on disk. Nothing else runs it on a schedule."
            }
        }))
    }

    fn update(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        // Read before writing: the directory is the truth, so the update builds
        // on whatever is on disk right now, not on what a previous call left
        // behind in memory.
        let Some(mut executor) = meta::load_all(&self.root)
            .into_iter()
            .find(|loaded| loaded.meta.id == id)
        else {
            return Ok(json!({"error": format!("no executor with id '{id}'"), "id": id}));
        };
        replace_given(args, &mut executor.meta);
        executor.meta.normalize();
        // Same gate as `create`: the stored schema has to stay enforceable, and
        // the old one survives a refusal because nothing is written yet.
        if let Err(problem) = call_args_schema::validate(executor.meta.args_schema.as_ref()) {
            return Ok(json!({"error": problem, "id": id}));
        }

        let had_script = executor.script.is_some();
        meta::save_meta(&self.root, &executor.meta)?;
        let script = text(args, "script");
        if !script.is_empty() {
            meta::save_script(&self.root, &id, &script)?;
        }
        let has_act_script = !script.is_empty() || had_script;

        Ok(json!({
            "updated": true,
            "id": executor.meta.id,
            "has_act_script": has_act_script,
            "status": if has_act_script { "ready" } else { "broken" },
        }))
    }

    fn delete(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        // The id comes from the model and becomes a path, so only a single
        // directory name is accepted; `../` can never leave the root.
        if !meta::is_single_component(&id) {
            return Ok(json!({"error": format!("'{id}' is not an executor id"), "id": id}));
        }
        let removed = meta::remove_executor(&self.root, &id)?;
        Ok(json!({
            "deleted": removed,
            "id": id,
            "note": if removed { "Nothing else references it: there is no history file to clean up." } else { "No executor with that id." }
        }))
    }
}

#[async_trait]
impl Tool for ExecutorTool {
    fn name(&self) -> &str {
        "executor"
    }

    /// Risk class (ADR 0011): writes executor definitions inside the workspace.
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::WorkspaceWrite
    }

    fn description(&self) -> &str {
        "Manage executors. An executor is a stored `act.js` codemode script that performs one action on demand — no tick, no schedule. Actions: list, create, update, delete, run. `run` executes the stored script and reports `{ok, summary, evidence?, failure?, ran, external_calls, logs?}` in-band; `list` reports field names and types only, never script source or sample values."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "create", "update", "delete", "run"],
                    "description": "Action to perform"
                },
                "id": { "type": "string", "description": "Executor id (required for update/delete/run)" },
                "name": { "type": "string", "description": "Human name, your choice, may repeat" },
                "description": { "type": "string", "description": "One line: what this executor does" },
                "script": {
                    "type": "string",
                    "description": "Body of `async function act(ctx) { ... }`. Available: ctx.args, ctx.http.get/post, ctx.secret(name), ctx.log(msg), ctx.now(), plus connector tools via tools.mcp__<server>__<tool>() and searchTools(query). No ctx.fs, no shell, no http writes."
                },
                "timeout_s": { "type": "integer", "description": "Per-run timeout in seconds (default 30, clamped to 1..120)" },
                "allow_connectors": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "MCP server names whose write tools this executor may call (default empty: no connector writes at all)"
                },
                "args_schema": {
                    "type": "object",
                    "description": "Optional JSON Schema subset (object with `properties`) validating the `args` passed to `run`; a call whose args do not match is refused before the script starts. `list` echoes field names and types only."
                },
                "args": {
                    "type": "object",
                    "description": "Values for `run`: passed to the script as `ctx.args`, after being checked against the executor's `args_schema`."
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
            "run" => Ok(run_stored(&self.root, &self.secrets, &args).await),
            _ => Ok(json!({"error": format!("unknown action '{action}'")})),
        };
        Ok(result?.to_string())
    }
}

/// The report for a `create` refused by `executors.max_count`. It names the
/// entries already stored so the caller can compare and delete in one round
/// instead of guessing, and it says what to do — a bare "over the limit" would
/// leave an unattended run with nowhere to go. The listing is bounded by
/// [`MAX_CAP_CANDIDATES`]; `omitted` says how many it left out.
fn over_cap(max_count: usize, stored: &[meta::LoadedExecutor]) -> Value {
    let shown = &stored[..stored.len().min(MAX_CAP_CANDIDATES)];
    let candidates: Vec<Value> = shown
        .iter()
        .map(|loaded| {
            json!({
                "id": loaded.meta.id,
                "name": loaded.meta.name,
                "description": loaded.meta.description,
                "status": loaded.status(),
            })
        })
        .collect();
    let omitted = stored.len() - shown.len();
    let note = if omitted == 0 {
        "The cap exists to force that comparison: a near-duplicate action script forks behaviour and can land the same effect twice.".to_string()
    } else {
        format!(
            "`candidates` names the first {MAX_CAP_CANDIDATES} of {} stored executors and {omitted} more are not listed here — call `list` for all of them. The cap exists to force that comparison: a near-duplicate action script forks behaviour and can land the same effect twice.",
            stored.len()
        )
    };
    json!({
        "error": format!(
            "not created: the workspace already holds {} executors and `executors.max_count` is {max_count}. Clean up first: compare the candidates below (or `list` for the rest) with the action you want, `delete` the ones that duplicate it or no longer run, then `create` again.",
            stored.len()
        ),
        "count": stored.len(),
        "omitted": omitted,
        "max_count": max_count,
        "candidates": candidates,
        "note": note
    })
}

fn replace_given(args: &Value, executor: &mut ExecutorMeta) {
    if args.get("name").is_some() {
        executor.name = text(args, "name");
    }
    if args.get("description").is_some() {
        executor.description = text(args, "description");
    }
    if let Some(value) = int(args, "timeout_s") {
        // Negative values would wrap into a huge `u64` and then clamp to the
        // maximum, which is the opposite of what a caller asked for.
        executor.timeout_s = value.clamp(0, i64::MAX) as u64;
    }
    if args.get("allow_connectors").is_some() {
        // Server names are stored as given, never checked against the live MCP
        // pool: this tool is built before the pool exists (it is assembled per
        // run in `agent_loop`), and a server that is offline at save time may be
        // online when the executor runs, so refusing an unmatched name here
        // would turn a temporary outage into "cannot save". The failure
        // direction stays closed — nothing is authorized that the store does not
        // name — and a name that never matches is reported at run time by the
        // write-side ceiling, which stops the run and quotes the server name.
        executor.allow_connectors = strings(args, "allow_connectors");
    }
    if let Some(value) = args.get("args_schema") {
        // An explicit `null` clears the schema; anything else replaces it.
        executor.args_schema = (!value.is_null()).then(|| value.clone());
    }
}

/// Field names and declared types from the args schema. A schema is a
/// declaration, not content, but it may carry `examples` / `default` values, so
/// only names and types are echoed — the listing never becomes a content
/// channel (mirrors `trigger/source/shape.rs`).
fn schema_fields(schema: Option<&Value>) -> Map<String, Value> {
    let mut out = Map::new();
    let Some(Value::Object(properties)) = schema.and_then(|s| s.get("properties")) else {
        return out;
    };
    for (key, spec) in properties.iter().take(MAX_SCHEMA_FIELDS) {
        out.insert(key.clone(), Value::String(declared_type(spec)));
    }
    out
}

fn declared_type(spec: &Value) -> String {
    match spec.get("type") {
        Some(Value::String(name)) => name.clone(),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(|name| name.as_str())
            .collect::<Vec<_>>()
            .join("|"),
        _ => "any".to_string(),
    }
}

pub(super) fn text(args: &Value, key: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::executor::meta::ExecutorMeta;

    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("executors");
        (tmp, root)
    }

    fn tool(root: &Path) -> ExecutorTool {
        tool_capped(root, 0)
    }

    /// An uncapped tool for the tests that are about something else.
    fn tool_capped(root: &Path, max_count: usize) -> ExecutorTool {
        ExecutorTool::new(root.to_path_buf(), Arc::new(BTreeMap::new()), max_count)
    }

    async fn run(root: &Path, args: Value) -> Value {
        let text = tool(root).execute(args).await.unwrap();
        serde_json::from_str(&text).unwrap()
    }

    async fn run_capped(root: &Path, max_count: usize, args: Value) -> Value {
        let text = tool_capped(root, max_count).execute(args).await.unwrap();
        serde_json::from_str(&text).unwrap()
    }

    fn on_disk(root: &Path, id: &str) -> ExecutorMeta {
        let text = std::fs::read_to_string(root.join(id).join("meta.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    #[tokio::test]
    async fn test_create_returns_an_exec_id_and_writes_meta() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create", "name": "file the issue"})).await;
        assert_eq!(out["created"], true);
        let id = out["id"].as_str().unwrap();
        assert!(id.starts_with("exec-"));
        assert!(root.join(id).join("meta.json").exists());
        assert!(!root.join(id).join("act.js").exists());
        assert_eq!(out["has_act_script"], false);
        assert_eq!(on_disk(&root, id).name, "file the issue");
    }

    #[tokio::test]
    async fn test_create_requires_a_name() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create"})).await;
        assert!(out["error"].as_str().unwrap().contains("name"));
    }

    #[tokio::test]
    async fn test_create_refuses_a_schema_keyword_it_cannot_enforce_and_writes_nothing() {
        let (_tmp, root) = workspace();
        let out = run(
            &root,
            json!({
                "action": "create",
                "name": "n",
                "args_schema": {
                    "type": "object",
                    "properties": {"title": {"anyOf": [{"type": "string"}]}}
                }
            }),
        )
        .await;

        let error = out["error"].as_str().unwrap();
        assert!(error.contains("anyOf"), "{error}");
        assert!(error.contains("args_schema.properties.title"), "{error}");
        assert!(out.get("created").is_none(), "{out}");
        // A refused schema must not leave a half-built executor behind.
        let listed = run(&root, json!({"action": "list"})).await;
        assert_eq!(listed["executors"].as_array().unwrap().len(), 0, "{listed}");
    }

    #[tokio::test]
    async fn test_update_refuses_a_schema_keyword_it_cannot_enforce_and_keeps_the_old_one() {
        let (_tmp, root) = workspace();
        let id = run(
            &root,
            json!({
                "action": "create",
                "name": "n",
                "args_schema": {"type": "object", "properties": {"title": {"type": "string"}}}
            }),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string();

        let out = run(
            &root,
            json!({
                "action": "update",
                "id": id,
                "args_schema": {"type": "object", "oneOf": [{"required": ["x"]}]}
            }),
        )
        .await;

        let error = out["error"].as_str().unwrap();
        assert!(error.contains("oneOf"), "{error}");
        assert_eq!(
            on_disk(&root, &id).args_schema.unwrap()["properties"]["title"]["type"],
            "string"
        );
    }

    #[tokio::test]
    async fn test_an_array_of_items_is_accepted_and_enforced_at_run_time() {
        let (_tmp, root) = workspace();
        let id = run(
            &root,
            json!({
                "action": "create",
                "name": "n",
                "args_schema": {
                    "type": "object",
                    "properties": {"labels": {"type": "array", "items": {"type": "string"}}}
                }
            }),
        )
        .await["id"]
            .as_str()
            .unwrap()
            .to_string();
        run(
            &root,
            json!({
                "action": "update",
                "id": id,
                "script": "async function act(ctx) { return {ok: true, summary: 'ok'}; }"
            }),
        )
        .await;

        let out = run(
            &root,
            json!({"action": "run", "id": id, "args": {"labels": ["bug", 7]}}),
        )
        .await;
        assert_eq!(out["failure"], "rejected");
        assert!(
            out["summary"].as_str().unwrap().contains("args.labels[1]"),
            "{out}"
        );
        assert_eq!(out["ran"], false);
    }

    #[tokio::test]
    async fn test_create_at_the_cap_is_refused_and_points_at_the_listing() {
        let (_tmp, root) = workspace();
        for name in ["first", "second"] {
            run(&root, json!({"action": "create", "name": name})).await;
        }

        let out = run_capped(&root, 2, json!({"action": "create", "name": "third"})).await;

        assert!(out.get("created").is_none(), "{out}");
        let error = out["error"].as_str().unwrap();
        assert!(error.contains("executors.max_count"), "{error}");
        assert!(error.contains("delete"), "{error}");
        // The candidates are named so the cleanup does not need a second call.
        // Order is by id, so compare the set rather than the sequence.
        let candidates = out["candidates"].as_array().unwrap();
        let names: Vec<&str> = candidates
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.contains(&"first") && names.contains(&"second"),
            "{names:?}"
        );
        assert!(candidates.iter().all(|c| c["id"].is_string()));
        // Nothing was written: the store still holds exactly the two.
        let listed = run(&root, json!({"action": "list"})).await;
        assert_eq!(listed["executors"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_create_at_the_cap_names_a_bounded_number_of_candidates() {
        let (_tmp, root) = workspace();
        // `executors.max_count` allows up to 10000, so a refusal must not grow
        // with the store: one tool result is read by a model, not by `grep`.
        for n in 0..40u16 {
            let meta = ExecutorMeta::new(format!("exec-{n:04x}"), format!("executor {n}"));
            meta::save_meta(&root, &meta).unwrap();
        }

        let out = run_capped(&root, 40, json!({"action": "create", "name": "one more"})).await;

        assert!(out.get("created").is_none(), "{out}");
        let candidates = out["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), MAX_CAP_CANDIDATES, "{candidates:?}");
        // The full count stays visible, and the gap is named, so the caller
        // knows to `list` instead of assuming the store is that small.
        assert_eq!(out["count"], 40);
        assert_eq!(out["omitted"], 40 - MAX_CAP_CANDIDATES);
        let note = out["note"].as_str().unwrap();
        assert!(note.contains("list"), "{note}");
        assert!(
            note.contains(&(40 - MAX_CAP_CANDIDATES).to_string()),
            "{note}"
        );
    }

    #[tokio::test]
    async fn test_create_below_the_cap_still_creates() {
        let (_tmp, root) = workspace();
        run(&root, json!({"action": "create", "name": "first"})).await;

        let out = run_capped(&root, 2, json!({"action": "create", "name": "second"})).await;

        assert_eq!(out["created"], true);
        assert_eq!(out["name"], "second");
    }

    #[tokio::test]
    async fn test_create_is_uncapped_when_max_count_is_zero() {
        let (_tmp, root) = workspace();
        for name in ["one", "two", "three"] {
            let out = run_capped(&root, 0, json!({"action": "create", "name": name})).await;
            assert_eq!(out["created"], true, "{out}");
        }
    }

    #[tokio::test]
    async fn test_update_at_the_cap_is_not_refused() {
        let (_tmp, root) = workspace();
        let first = run(&root, json!({"action": "create", "name": "first"})).await;
        run(&root, json!({"action": "create", "name": "second"})).await;
        let id = first["id"].as_str().unwrap().to_string();

        // Changing an executor that already exists adds no entry, so the cap
        // has nothing to protect here.
        let out = run_capped(
            &root,
            2,
            json!({"action": "update", "id": id, "description": "narrowed"}),
        )
        .await;

        assert_eq!(out["updated"], true, "{out}");
    }

    #[tokio::test]
    async fn test_create_without_a_script_is_a_half_built_executor() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create", "name": "half"})).await;
        let id = out["id"].as_str().unwrap().to_string();
        let listed = run(&root, json!({"action": "list"})).await;
        let row = &listed["executors"][0];
        assert_eq!(row["id"], id);
        assert_eq!(row["status"], "broken");
        assert!(row["last_error"].as_str().unwrap().contains("act.js"));
    }

    #[tokio::test]
    async fn test_create_clamps_timeout_before_writing() {
        let (_tmp, root) = workspace();
        let out = run(
            &root,
            json!({
                "action": "create",
                "name": "n",
                "timeout_s": 9999,
                "script": "async function act(ctx) {}"
            }),
        )
        .await;
        let id = out["id"].as_str().unwrap();
        assert_eq!(out["timeout_s"], 120);
        assert_eq!(on_disk(&root, id).timeout_s, 120);
    }

    #[tokio::test]
    async fn test_list_reports_field_names_and_types_not_values() {
        let (_tmp, root) = workspace();
        run(
            &root,
            json!({
                "action": "create",
                "name": "n",
                "script": "async function act(ctx) { return {ok: true}; }",
                "args_schema": {
                    "type": "object",
                    "properties": {
                        "title": {"type": "string", "examples": ["top secret headline"]},
                        "labels": {"type": ["array", "null"]}
                    }
                }
            }),
        )
        .await;

        let listed = run(&root, json!({"action": "list"})).await;
        let row = &listed["executors"][0];
        assert_eq!(row["args_schema_fields"]["title"], "string");
        assert_eq!(row["args_schema_fields"]["labels"], "array|null");
        assert_eq!(row["status"], "ready");

        // The listing is a structural view: neither the script nor a schema's
        // example values may travel through it into a model context.
        let text = listed.to_string();
        assert!(!text.contains("top secret headline"));
        assert!(!text.contains("async function act"));
    }

    #[tokio::test]
    async fn test_list_sees_a_hand_edited_meta_on_the_next_call() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create", "name": "before"})).await;
        let id = out["id"].as_str().unwrap().to_string();

        let mut meta = on_disk(&root, &id);
        meta.name = "after".into();
        crate::executor::meta::save_meta(&root, &meta).unwrap();

        let listed = run(&root, json!({"action": "list"})).await;
        assert_eq!(listed["executors"][0]["name"], "after");
    }

    #[tokio::test]
    async fn test_update_replaces_only_given_fields() {
        let (_tmp, root) = workspace();
        let out = run(
            &root,
            json!({
                "action": "create",
                "name": "first",
                "description": "keep me",
                "allow_connectors": ["github"],
                "timeout_s": 45
            }),
        )
        .await;
        let id = out["id"].as_str().unwrap().to_string();

        let updated = run(
            &root,
            json!({"action": "update", "id": id, "name": "second"}),
        )
        .await;
        assert_eq!(updated["updated"], true);

        let meta = on_disk(&root, &id);
        assert_eq!(meta.name, "second");
        assert_eq!(meta.description, "keep me");
        assert_eq!(meta.allow_connectors, vec!["github".to_string()]);
        assert_eq!(meta.timeout_s, 45);
    }

    #[tokio::test]
    async fn test_update_unknown_id_is_an_error_not_a_create() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "update", "id": "exec-0000"})).await;
        assert!(out["error"].as_str().unwrap().contains("no executor"));
        assert!(!root.join("exec-0000").exists());
    }

    #[tokio::test]
    async fn test_update_completes_a_half_built_executor() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create", "name": "half"})).await;
        let id = out["id"].as_str().unwrap().to_string();

        let updated = run(
            &root,
            json!({
                "action": "update",
                "id": id,
                "script": "async function act(ctx) { return {ok: true}; }"
            }),
        )
        .await;
        assert_eq!(updated["updated"], true);
        assert_eq!(updated["has_act_script"], true);
        assert_eq!(updated["status"], "ready");
        assert!(root.join(&id).join("act.js").exists());
    }

    #[tokio::test]
    async fn test_delete_removes_directory() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "create", "name": "gone"})).await;
        let id = out["id"].as_str().unwrap().to_string();

        let deleted = run(&root, json!({"action": "delete", "id": id.clone()})).await;
        assert_eq!(deleted["deleted"], true);
        assert!(!root.join(&id).exists());

        let listed = run(&root, json!({"action": "list"})).await;
        assert_eq!(listed["executors"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_delete_rejects_a_traversal_id() {
        let (tmp, root) = workspace();
        std::fs::create_dir_all(tmp.path().join("outside")).unwrap();
        let out = run(&root, json!({"action": "delete", "id": "../outside"})).await;
        assert!(
            out["error"]
                .as_str()
                .unwrap()
                .contains("not an executor id")
        );
        assert!(tmp.path().join("outside").exists());
    }

    #[tokio::test]
    async fn test_unknown_action_reports_an_error() {
        let (_tmp, root) = workspace();
        let out = run(&root, json!({"action": "frobnicate"})).await;
        assert!(out["error"].as_str().unwrap().contains("frobnicate"));
    }

    #[test]
    fn test_tool_risk_level_is_workspace_write() {
        // ADR 0011: the risk class is a fact the tool declares. This tool only
        // writes executor definitions inside the workspace; the outward write
        // happens on a nested connector call (ticket 15), which carries its own
        // class. Mirrors `TriggerTool`.
        let (_tmp, root) = workspace();
        assert_eq!(
            tool(&root).risk_level(),
            crate::approval::RiskLevel::WorkspaceWrite
        );
    }
}

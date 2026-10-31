//! The `trigger` tool: the trigger agent's way to manage rules.
//!
//! Same shape as `trigger_source`: one tool, five actions, all file work done
//! by the host. The agent never names a path and never supplies an `origin` —
//! the origin is read from the conversation that is calling, so a rule created
//! inside a subagent run still reports back to the human conversation that
//! asked for it.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::agent::Origin;
use crate::tool::{Tool, current_turn_target};
use crate::trigger::source::SourceLayer;

use super::SubscriptionLayer;
use super::meta::{self, TriggerMeta};

pub struct TriggerTool {
    rules: Arc<SubscriptionLayer>,
    sources: Arc<SourceLayer>,
    workspace: PathBuf,
    /// Secrets for the script the same way the tick does it.
    secrets: Arc<std::collections::BTreeMap<String, String>>,
}

impl TriggerTool {
    pub fn new(
        rules: Arc<SubscriptionLayer>,
        sources: Arc<SourceLayer>,
        workspace: PathBuf,
        secrets: Arc<std::collections::BTreeMap<String, String>>,
    ) -> Self {
        Self {
            rules,
            sources,
            workspace,
            secrets,
        }
    }

    fn list(&self, args: &Value) -> Value {
        let enabled_only = args
            .get("enabled_only")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let rows: Vec<Value> = self
            .rules
            .status()
            .into_iter()
            .filter(|status| !enabled_only || status.meta.enabled)
            .map(|status| {
                json!({
                    "id": status.meta.id,
                    "name": status.meta.name,
                    "description": status.meta.description,
                    "enabled": status.meta.enabled,
                    "status": status.status,
                    "sources": status.meta.sources,
                    "schedule": status.meta.schedule,
                    "cooldown_ms": status.meta.cooldown_ms,
                    "action": {
                        "kind": status.meta.action.kind.as_str(),
                        "agent": status.meta.action.agent,
                        "deliver": status.meta.action.deliver,
                    },
                    "has_condition": self.rules.script(&status.meta.id).is_some(),
                    "next_run_at": iso(status.state.next_run_at),
                    "last_fired_at": iso(status.state.last_fired_at),
                    "fire_count": status.state.fire_count,
                    "consecutive_failures": status.state.consecutive_failures,
                    "last_error": status.state.last_error.or(status.problem.clone()),
                })
            })
            .collect();
        json!({ "triggers": rows })
    }

    fn create(&self, args: &Value) -> Result<Value> {
        let name = text(args, "name");
        if name.is_empty() {
            return Ok(json!({"error": "`name` is required"}));
        }
        let id = meta::generate_id(self.rules.root());
        let mut rule = TriggerMeta::new(id.clone(), name);
        rule.description = text(args, "description");
        rule.sources = strings(args, "sources");
        rule.cooldown_ms = int(args, "cooldown_ms").unwrap_or(rule.cooldown_ms);
        match meta::schedule_from_args(args) {
            Ok(schedule) => rule.schedule = schedule,
            Err(e) => return Ok(rejected(&e)),
        }
        match meta::action_from_args(args) {
            Ok(action) => rule.action = action,
            Err(e) => return Ok(rejected(&e)),
        }
        rule.origin = self.current_origin();
        rule.normalize();

        let script = text(args, "script");
        let has_script = !script.is_empty();
        if let Err(e) = self.validate(&rule, has_script) {
            return Ok(rejected(&e));
        }

        meta::save_meta(self.rules.root(), &rule)?;
        if has_script {
            meta::save_script(self.rules.root(), &id, &script)?;
        }
        self.rules.put(rule.clone(), has_script.then_some(script));

        Ok(json!({
            "created": true,
            "id": rule.id,
            "name": rule.name,
            "enabled": rule.enabled,
            "sources": rule.sources,
            "schedule": rule.schedule,
            "cooldown_ms": rule.cooldown_ms,
            "action_kind": rule.action.kind.as_str(),
            "has_condition": has_script,
            "origin": rule.origin,
            "next_run_at": iso(self.rules.next_run_at(&id)),
            "note": "run `test` before you tell the user it works.",
        }))
    }

    fn update(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        let Some(mut rule) = self.rules.get(&id) else {
            return Ok(json!({"error": format!("no trigger with id '{id}'"), "id": id}));
        };

        if let Some(value) = args.get("name").and_then(|v| v.as_str()) {
            rule.name = value.trim().to_string();
        }
        if let Some(value) = args.get("description").and_then(|v| v.as_str()) {
            rule.description = value.to_string();
        }
        if args.get("sources").is_some() {
            rule.sources = strings(args, "sources");
        }
        if let Some(value) = int(args, "cooldown_ms") {
            rule.cooldown_ms = value;
        }
        if args.get("schedule").is_some() {
            match meta::schedule_from_args(args) {
                Ok(schedule) => rule.schedule = schedule,
                Err(e) => return Ok(rejected(&e)),
            }
        }
        if args.get("on_fire").is_some() {
            match meta::action_from_args(args) {
                Ok(action) => rule.action = action,
                Err(e) => return Ok(rejected(&e)),
            }
        }
        if let Some(enabled) = args.get("enabled").and_then(|v| v.as_bool()) {
            rule.enabled = enabled;
        }
        rule.normalize();
        rule.updated_at = crate::trigger::source::meta::now_ms();

        let script = match args.get("script").and_then(|v| v.as_str()) {
            Some(text) if !text.trim().is_empty() => Some(text.to_string()),
            // Absent means "leave the script alone"; an explicit empty string
            // clears it, which is how a rule becomes "time only".
            Some(_) => None,
            None => self.rules.script(&id),
        };
        if let Err(e) = self.validate(&rule, script.is_some()) {
            return Ok(rejected(&e));
        }

        meta::save_meta(self.rules.root(), &rule)?;
        match &script {
            Some(text) => meta::save_script(self.rules.root(), &id, text)?,
            None => {
                // Clearing a condition must actually remove the file, or the
                // old rule would keep judging from disk.
                let path = self.rules.root().join(&id).join(meta::SCRIPT_FILE);
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        self.rules.put(rule.clone(), script.clone());
        // `enabled` needs the edge reset that only the layer knows how to do.
        self.rules.set_enabled(&id, rule.enabled);

        Ok(json!({
            "updated": true,
            "id": rule.id,
            "name": rule.name,
            "enabled": rule.enabled,
            "sources": rule.sources,
            "schedule": rule.schedule,
            "cooldown_ms": rule.cooldown_ms,
            "action_kind": rule.action.kind.as_str(),
            "has_condition": script.is_some(),
            "next_run_at": iso(self.rules.next_run_at(&id)),
        }))
    }

    fn delete(&self, args: &Value) -> Result<Value> {
        let id = text(args, "id");
        if id.is_empty() {
            return Ok(json!({"error": "`id` is required"}));
        }
        let removed = meta::remove_trigger(self.rules.root(), &id)?;
        self.rules.forget(&id);
        Ok(json!({
            "deleted": removed,
            "id": id,
            "note": if removed { "the rule and its state are gone" } else { "no such directory" },
        }))
    }

    async fn test(&self, args: &Value) -> Value {
        let id = text(args, "id");
        if id.is_empty() {
            return json!({"error": "`id` is required"});
        }
        // The current snapshot, frozen the same way a tick freezes one.
        let frame = self.sources.freeze();
        self.rules.test(&id, &frame, &self.secrets).await
    }

    /// Everything `create` and `update` must agree on before a file is written.
    /// `has_script` is the caller's answer, because a create carries the script
    /// in the same call that asks for the check.
    fn validate(&self, rule: &TriggerMeta, has_script: bool) -> std::result::Result<(), String> {
        let registry = crate::agent::load_workspace_registry(&self.workspace);
        let known_agent = rule
            .action
            .agent
            .as_deref()
            .is_some_and(|name| registry.get(name).is_some());
        rule.validate(has_script, known_agent)
    }

    /// The conversation this tool is being called from. A subagent inherits its
    /// origin from the caller, so this stays the human-facing chat.
    fn current_origin(&self) -> Option<Origin> {
        let (channel, chat_id) = current_turn_target();
        if channel.is_empty() || chat_id.is_empty() {
            None
        } else {
            Some(Origin::new(channel, chat_id))
        }
    }
}

const HINT: &str = "a rule needs a schedule or a condition script, an `on_fire` action \
                    whose template uses only {{evidence}} / {{detail}} / {{now}}, and \
                    sources that exist";

/// A rejected argument is a tool result, not a failure: the agent reads the
/// message and fixes the call.
fn rejected(error: &str) -> Value {
    json!({ "error": error, "hint": HINT })
}

#[async_trait]
impl Tool for TriggerTool {
    fn name(&self) -> &str {
        "trigger"
    }

    fn description(&self) -> &str {
        "Manage durable rules: list, create, update, delete and test a trigger. A trigger watches data sources, judges a condition script, and acts on a new fire."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["list", "create", "update", "delete", "test"],
                    "description": "What to do"
                },
                "id": {"type": "string", "description": "Rule id (trg-xxxx) for update / delete / test"},
                "name": {"type": "string", "description": "Human name for the rule; may repeat across rules"},
                "description": {"type": "string", "description": "One line: what it watches and what it does"},
                "sources": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Source ids (src-xxxx) the condition may read"
                },
                "schedule": {
                    "type": "object",
                    "description": "Optional gate: {kind:'cron',expr:'0 0 9 * * *'} | {kind:'every',every_ms:3600000} | {kind:'at',at_ms:<epoch ms>}. Local time for cron.",
                    "additionalProperties": true
                },
                "cooldown_ms": {"type": "integer", "description": "Minimum gap between fires; default 0"},
                "script": {"type": "string", "description": "condition.js: function judge(ctx) returning {fire, evidence?, detail?, keys?}"},
                "on_fire": {
                    "type": "object",
                    "description": "What a fire does: {kind:'record'} | {kind:'notify',text_template} | {kind:'agent',agent,task_template,deliver}. Templates may use {{evidence}}, {{detail}}, {{now}}."
                },
                "enabled": {"type": "boolean", "description": "update only: start or stop the rule"},
                "enabled_only": {"type": "boolean", "description": "list only: hide disabled rules"}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, args: Value) -> Result<String> {
        let action = text(&args, "action");
        // The discriminator and the rule's action cannot both be called
        // `action`, so a rule action arrives as `on_fire`.
        let outcome = match action.as_str() {
            "list" => Ok(self.list(&args)),
            "create" => self.create(&args),
            "update" => self.update(&args),
            "delete" => self.delete(&args),
            "test" => Ok(self.test(&args).await),
            other => Ok(json!({
                "error": format!("unknown action '{other}'"),
                "actions": ["list", "create", "update", "delete", "test"],
            })),
        };
        match outcome {
            Ok(value) => Ok(value.to_string()),
            Err(e) => Ok(json!({"error": format!("{e:#}")}).to_string()),
        }
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

fn iso(epoch_ms: Option<i64>) -> Option<String> {
    epoch_ms.map(|ms| {
        chrono::DateTime::<chrono::Local>::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms.max(0) as u64),
        )
        .to_rfc3339()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Origin, load_workspace_registry};
    use crate::tool::{TurnContext, with_turn_context};
    use crate::trigger::source::{DEFAULT_TTL, SourceLayer, meta as source_file};
    use crate::trigger::subscription::state::TriggerState;
    use std::collections::BTreeMap;
    use std::time::Duration;

    const FIRE: &str = "function judge() { return {fire: true, detail: 'fire', \
                    evidence: [{source_id: 'src-0001', text: 'two open'}]}; }";

    fn create_args(name: &str) -> Value {
        json!({"action": "create", "name": name, "script": FIRE, "on_fire": "record"})
    }

    struct Harness {
        _tmp: tempfile::TempDir,
        tool: TriggerTool,
        rules: Arc<SubscriptionLayer>,
        sources: Arc<SourceLayer>,
        workspace: PathBuf,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().to_path_buf();
            let secrets: Arc<BTreeMap<String, String>> = Arc::new(BTreeMap::new());
            let sources = Arc::new(SourceLayer::new(
                workspace.join("triggers").join("sources"),
                workspace.clone(),
                secrets.clone(),
                Duration::from_secs(60),
                DEFAULT_TTL,
            ));
            let rules = SubscriptionLayer::new(workspace.join("triggers").join("triggers"));
            let tool = TriggerTool::new(rules.clone(), sources.clone(), workspace.clone(), secrets);
            Self {
                _tmp: tmp,
                tool,
                rules,
                sources,
                workspace,
            }
        }

        async fn call(&self, args: Value) -> Value {
            let text = self.tool.execute(args).await.unwrap();
            serde_json::from_str(&text).expect("the tool must answer with JSON")
        }

        fn source(&self, id: &str) {
            let root = self.sources.root().to_path_buf();
            let mut meta = source_file::SourceMeta::new(id, id);
            meta.timeout_s = 1;
            source_file::save_meta(&root, &meta).unwrap();
            source_file::save_script(
                &root,
                id,
                "function collect() { return {items: [{title: 'a'}]}; }",
            )
            .unwrap();
            self.sources.reload();
        }

        fn rule_dir(&self, id: &str) -> PathBuf {
            self.workspace.join("triggers").join("triggers").join(id)
        }

        fn state(&self, id: &str) -> TriggerState {
            TriggerState::load(&self.rule_dir(id)).0
        }

        /// Install the shipped agent guide into this harness' workspace, which
        /// outlives the test, and hand back the workspace path.
        fn with_agent_template(&self) -> PathBuf {
            let agents = self.workspace.join("agents");
            std::fs::create_dir_all(&agents).unwrap();
            let template = include_str!("../../../resources/templates/agents/trigger.md");
            std::fs::write(agents.join("trigger.md"), template).unwrap();
            self.workspace.clone()
        }
    }

    /// Create one rule and hand back its id, failing the test if the tool
    /// refused.
    async fn create(h: &Harness, args: Value) -> String {
        let response = h.call(args).await;
        assert_eq!(response["created"], json!(true), "{response}");
        let id = response["id"].as_str().expect("id").to_string();
        assert!(id.starts_with("trg-"), "{id}");
        id
    }

    #[tokio::test]
    async fn test_create_writes_the_definition_and_answers_with_the_id() {
        let h = Harness::new();
        let mut args = create_args("watch the todo");
        args["sources"] = json!(["src-0001"]);
        args["cooldown_ms"] = json!(60_000);
        let id = create(&h, args).await;

        let dir = h.rule_dir(&id);
        assert!(
            dir.join(meta::META_FILE).exists(),
            "meta.json must be on disk"
        );
        assert!(
            dir.join(meta::SCRIPT_FILE).exists(),
            "condition.js must be on disk"
        );
        assert_eq!(h.state(&id).fire_count, 0, "a fresh rule has not fired");

        let stored = h.rules.get(&id).expect("the layer must know the rule");
        assert_eq!(stored.name, "watch the todo");
        assert_eq!(stored.cooldown_ms, 60_000);
        assert_eq!(stored.sources, vec!["src-0001".to_string()]);
    }

    #[tokio::test]
    async fn test_create_rejects_a_rule_with_neither_schedule_nor_script() {
        let h = Harness::new();
        let response = h
            .call(json!({"action": "create", "name": "nothing to judge", "on_fire": "record"}))
            .await;
        assert!(response["error"].is_string(), "{response}");
        assert!(
            response["error"].as_str().unwrap().contains("schedule"),
            "{response}"
        );
        assert!(
            response["hint"].is_string(),
            "the reply must say how to fix it"
        );
        assert!(
            h.rules.status().is_empty(),
            "a rejected rule must not exist"
        );
        assert!(
            meta::load_all(&h.workspace.join("triggers").join("triggers")).is_empty(),
            "and nothing must be on disk"
        );
    }

    #[tokio::test]
    async fn test_create_rejects_an_unknown_placeholder() {
        let h = Harness::new();
        let response = h
            .call(json!({
                "action": "create",
                "name": "bad template",
                "script": FIRE,
                "on_fire": {"kind": "notify", "text_template": "{{forever}}"},
            }))
            .await;
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains("unknown placeholder"),
            "{response}"
        );
        assert!(h.rules.status().is_empty());
    }

    #[tokio::test]
    async fn test_create_rejects_an_unknown_agent() {
        let h = Harness::new();
        let response = h
            .call(json!({
                "action": "create",
                "name": "ghost agent",
                "script": FIRE,
                "on_fire": {
                    "kind": "agent",
                    "agent": "nope",
                    "task_template": "look at {{detail}}",
                },
            }))
            .await;
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains("nope"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_create_rejects_a_half_given_destination() {
        let h = Harness::new();
        let response = h
            .call(json!({
                "action": "create",
                "name": "where do I send it",
                "script": FIRE,
                "on_fire": {"kind": "notify", "text_template": "{{detail}}", "channel": "webui"},
            }))
            .await;
        assert!(
            response["error"]
                .as_str()
                .unwrap_or_default()
                .contains("to"),
            "{response}"
        );
    }

    #[tokio::test]
    async fn test_create_accepts_a_time_only_rule_and_reports_the_horizon() {
        let h = Harness::new();
        let response = h
            .call(json!({
                "action": "create",
                "name": "daily standup",
                "schedule": {"kind": "cron", "expr": "0 0 9 * * *"},
                "on_fire": "record",
            }))
            .await;
        assert_eq!(response["created"], json!(true), "{response}");
        assert_eq!(response["has_condition"], json!(false));
        assert!(
            response["next_run_at"].is_string(),
            "a scheduled rule must answer with its next time: {response}"
        );
        // A time-only rule is exactly what a later ticket runs without a script.
        assert!(
            !h.rule_dir(response["id"].as_str().unwrap())
                .join(meta::SCRIPT_FILE)
                .exists()
        );
    }

    #[tokio::test]
    async fn test_update_overrides_only_the_fields_it_is_given() {
        let h = Harness::new();
        let mut args = create_args("original");
        args["sources"] = json!(["src-0001"]);
        let id = create(&h, args).await;

        let response = h
            .call(json!({"action": "update", "id": id, "cooldown_ms": 3_600_000}))
            .await;
        assert_eq!(response["updated"], json!(true), "{response}");
        let stored = h.rules.get(&id).unwrap();
        assert_eq!(stored.cooldown_ms, 3_600_000, "the named field changes");
        assert_eq!(stored.name, "original", "an unnamed field is left alone");
        assert_eq!(stored.sources, vec!["src-0001".to_string()]);
        assert_eq!(
            response["has_condition"],
            json!(true),
            "the script survives"
        );
    }

    #[tokio::test]
    async fn test_update_can_drop_the_condition_when_a_schedule_remains() {
        let h = Harness::new();
        let mut args = create_args("both");
        args["schedule"] = json!({"kind": "every", "every_ms": 60_000});
        let id = create(&h, args).await;

        let response = h
            .call(json!({"action": "update", "id": id, "script": ""}))
            .await;
        assert_eq!(response["updated"], json!(true), "{response}");
        assert_eq!(response["has_condition"], json!(false));
        assert!(
            !h.rule_dir(&id).join(meta::SCRIPT_FILE).exists(),
            "clearing a condition must remove the file, or the old rule keeps judging"
        );
    }

    #[tokio::test]
    async fn test_update_cannot_drop_the_last_reason_to_judge() {
        let h = Harness::new();
        let id = create(&h, create_args("script only")).await;

        let response = h
            .call(json!({"action": "update", "id": id, "script": ""}))
            .await;
        assert!(response["error"].is_string(), "{response}");
        assert!(
            h.rule_dir(&id).join(meta::SCRIPT_FILE).exists(),
            "a rejected update must not touch the files"
        );
        assert_eq!(h.rules.get(&id).unwrap().name, "script only");
    }

    #[tokio::test]
    async fn test_update_reports_an_unknown_id() {
        let h = Harness::new();
        let response = h
            .call(json!({"action": "update", "id": "trg-ffff", "cooldown_ms": 1}))
            .await;
        assert!(response["error"].as_str().unwrap().contains("trg-ffff"));
    }

    #[tokio::test]
    async fn test_delete_removes_the_rule_and_its_state() {
        let h = Harness::new();
        let id = create(&h, create_args("doomed")).await;
        assert!(h.rule_dir(&id).exists());

        let response = h.call(json!({"action": "delete", "id": id})).await;
        assert_eq!(response["deleted"], json!(true), "{response}");
        assert!(!h.rule_dir(&id).exists(), "the directory must be gone");
        assert!(!h.rules.exists(&id), "and the layer must forget it");

        let again = h.call(json!({"action": "delete", "id": id})).await;
        assert_eq!(again["deleted"], json!(false), "deleting twice says so");
    }

    #[tokio::test]
    async fn test_list_shows_each_rule_with_its_health() {
        let h = Harness::new();
        let id = create(&h, create_args("listed")).await;
        h.source("src-0001");

        let response = h.call(json!({"action": "list"})).await;
        let rows = response["triggers"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row["id"], json!(id));
        assert_eq!(row["name"], json!("listed"));
        assert_eq!(row["status"], json!("ready"));
        assert_eq!(row["has_condition"], json!(true));
        assert_eq!(row["action"]["kind"], json!("record"));
        assert_eq!(row["fire_count"], json!(0));

        h.call(json!({"action": "update", "id": id, "enabled": false}))
            .await;
        let enabled_only = h
            .call(json!({"action": "list", "enabled_only": true}))
            .await;
        assert!(enabled_only["triggers"].as_array().unwrap().is_empty());
        let all = h.call(json!({"action": "list"})).await;
        assert_eq!(
            all["triggers"].as_array().unwrap()[0]["status"],
            json!("disabled")
        );
    }

    #[tokio::test]
    async fn test_a_rule_referencing_a_missing_source_shows_as_degraded() {
        let h = Harness::new();
        let mut args = create_args("dangling");
        args["sources"] = json!(["src-9999"]);
        let id = create(&h, args).await;

        // One tick's worth of judging, driven the way the service drives it.
        let frame = h.sources.freeze();
        let secrets = Arc::new(BTreeMap::new());
        h.rules.judge(1, &frame, &secrets).await;

        let response = h.call(json!({"action": "list"})).await;
        let row = &response["triggers"].as_array().unwrap()[0];
        assert_eq!(row["id"], json!(id));
        assert_eq!(row["status"], json!("degraded"), "{response}");
        assert!(
            row["last_error"]
                .as_str()
                .unwrap_or_default()
                .contains("src-9999"),
            "the list must say which id is missing: {response}"
        );
    }

    #[tokio::test]
    async fn test_an_unknown_action_answers_with_the_action_list() {
        let h = Harness::new();
        let response = h.call(json!({"action": "fire"})).await;
        assert!(response["error"].as_str().unwrap().contains("fire"));
        assert_eq!(
            response["actions"].as_array().unwrap().len(),
            5,
            "the reply must teach the caller what to call instead"
        );
    }

    #[tokio::test]
    async fn test_origin_comes_from_the_conversation_not_the_arguments() {
        let h = Harness::new();
        // Outside any turn there is nobody to report back to.
        let lonely = h
            .call(json!({
                "action": "create",
                "name": "no origin",
                "script": FIRE,
                "on_fire": "record",
            }))
            .await;
        assert!(lonely["created"].is_boolean(), "{lonely}");
        assert_eq!(lonely["origin"], json!(null));

        // Inside a turn the origin is read from it, and an `origin` argument is
        // simply not a thing the model can supply.
        let mut args = create_args("with origin");
        args["origin"] = json!({"channel": "evil", "chat_id": "evil"});
        let response = with_turn_context(
            Arc::new(TurnContext::new("webui".to_string(), "chat-7".to_string())),
            h.call(args),
        )
        .await;
        assert_eq!(
            response["origin"],
            json!({"channel": "webui", "chat_id": "chat-7"}),
            "{response}"
        );
        let id = response["id"].as_str().unwrap();
        assert_eq!(
            h.rules.get(id).unwrap().origin,
            Some(Origin::new("webui", "chat-7"))
        );
    }

    #[tokio::test]
    async fn test_the_test_action_judges_now_without_firing_or_writing() {
        let h = Harness::new();
        h.source("src-0001");
        let mut args = create_args("try me");
        args["sources"] = json!(["src-0001"]);
        let id = create(&h, args).await;
        // Sample the source the way a tick would, so the rule is ready.
        h.sources.collect(1, 2).await;

        let response = h.call(json!({"action": "test", "id": id})).await;
        assert_eq!(response["ready"], json!(true), "{response}");
        assert_eq!(response["fire"], json!(true));
        assert_eq!(response["would_fire"], json!(true));
        assert_eq!(response["evidence"].as_array().unwrap().len(), 1);
        assert!(response["elapsed_ms"].as_u64().is_some());
        assert!(
            response["note"].as_str().unwrap().contains("nothing fired"),
            "{response}"
        );

        let state = h.state(&id);
        assert!(!state.prev);
        assert_eq!(state.fire_count, 0);
        assert!(
            state.last_fired_at.is_none(),
            "a test must not start a cooldown"
        );
    }

    #[tokio::test]
    async fn test_the_agent_definition_knows_the_five_actions() {
        // The bundled agent guide is what teaches a model to use this tool, so
        // its action list has to match the schema — and it has to be allowed to
        // call the tool at all.
        let h = Harness::new();
        let registry = load_workspace_registry(&h.with_agent_template());
        let trigger = registry
            .get("trigger")
            .expect("the bundled trigger agent must load from its template");
        assert!(
            trigger.tools.iter().any(|t| t == "trigger"),
            "the guide must declare the tool it describes: {:?}",
            trigger.tools
        );
        for action in ["list", "create", "update", "delete", "test"] {
            assert!(
                trigger.system_prompt.contains(action),
                "the agent guide must cover `{action}`"
            );
        }
        assert!(
            trigger.system_prompt.contains("on_fire"),
            "the guide must name the argument the schema expects"
        );
    }
}

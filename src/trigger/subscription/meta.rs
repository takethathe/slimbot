//! One trigger definition: what to watch, when it counts, what to do.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::agent::Origin;
use crate::cron::CronSchedule;

pub const META_FILE: &str = "meta.json";
pub const SCRIPT_FILE: &str = "condition.js";
pub const STATE_FILE: &str = "state.json";
pub const ID_PREFIX: &str = "trg-";
pub const DEFAULT_COOLDOWN_MS: i64 = 0;

/// The placeholders a template may use. Anything else is a definition error,
/// caught at create time rather than at 3am on the first fire.
pub const KNOWN_PLACEHOLDERS: &[&str] = &["evidence", "detail", "now"];

/// What happens on a fire. v1 has exactly one action per rule; several actions
/// are several rules.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// Run a subagent with a rendered task.
    #[default]
    Agent,
    /// Send text to a channel.
    Notify,
    /// Record the fire and say nothing to the user.
    Record,
}

impl ActionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionKind::Agent => "agent",
            ActionKind::Notify => "notify",
            ActionKind::Record => "record",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "agent" => Some(ActionKind::Agent),
            "notify" => Some(ActionKind::Notify),
            "record" => Some(ActionKind::Record),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TriggerAction {
    #[serde(default)]
    pub kind: ActionKind,
    /// `agent`: which agent definition to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_template: Option<String>,
    /// `agent`: overrides the agent definition's own `deliver`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver: Option<bool>,
    /// `notify`: explicit destination, else the rule's origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_template: Option<String>,
}

impl TriggerAction {
    pub fn record() -> Self {
        Self {
            kind: ActionKind::Record,
            ..Default::default()
        }
    }

    /// Every field of the action that carries a template, for validation.
    pub fn templates(&self) -> Vec<&str> {
        [self.task_template.as_deref(), self.text_template.as_deref()]
            .into_iter()
            .flatten()
            .collect()
    }

    /// Reject an action that cannot do anything.
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self.kind {
            ActionKind::Agent => {
                let Some(agent) = self.agent.as_deref().map(str::trim) else {
                    return Err("action.kind=agent needs action.agent".to_string());
                };
                if agent.is_empty() {
                    return Err("action.agent is empty".to_string());
                }
                let template = self.task_template.as_deref().unwrap_or("").trim();
                if template.is_empty() {
                    return Err("action.kind=agent needs action.task_template".to_string());
                }
                Ok(())
            }
            ActionKind::Notify => {
                let template = self.text_template.as_deref().unwrap_or("").trim();
                if template.is_empty() {
                    return Err("action.kind=notify needs action.text_template".to_string());
                }
                let has_channel = self
                    .channel
                    .as_deref()
                    .is_some_and(|c| !c.trim().is_empty());
                let has_to = self.to.as_deref().is_some_and(|c| !c.trim().is_empty());
                if has_channel != has_to {
                    return Err("action.channel and action.to must be given together".to_string());
                }
                Ok(())
            }
            ActionKind::Record => Ok(()),
        }
    }
}

/// A rule as written on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<CronSchedule>,
    #[serde(default)]
    pub cooldown_ms: i64,
    #[serde(default)]
    pub action: TriggerAction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
}

fn default_true() -> bool {
    true
}

impl TriggerMeta {
    pub fn new(id: impl Into<String>, name: impl Into<String>) -> Self {
        let now = crate::trigger::source::meta::now_ms();
        Self {
            id: id.into(),
            name: name.into(),
            description: String::new(),
            enabled: true,
            sources: Vec::new(),
            schedule: None,
            cooldown_ms: DEFAULT_COOLDOWN_MS,
            action: TriggerAction::record(),
            origin: None,
            created_at: now,
            updated_at: now,
        }
    }

    /// Clamp values the host has to honour, and drop whitespace noise. Ids are
    /// never re-derived here: the directory name is the identity.
    pub fn normalize(&mut self) {
        self.cooldown_ms = self.cooldown_ms.clamp(0, 365 * 24 * 3600 * 1000);
        self.sources.retain(|s| !s.trim().is_empty());
        self.sources.sort();
        self.sources.dedup();
    }

    /// A one-shot rule: its schedule has exactly one occurrence.
    pub fn is_once(&self) -> bool {
        matches!(self.schedule, Some(CronSchedule::At { .. }))
    }

    /// A one-shot rule whose moment has passed. It stays on disk as `disabled`.
    pub fn once_expired(&self, now_ms: i64) -> bool {
        matches!(
            self.schedule,
            Some(CronSchedule::At { at_ms }) if at_ms < now_ms
        )
    }

    /// Whether the user is supposed to hear about this rule. `record` rules and
    /// rules with `action.deliver: false` are silent, so they never alert.
    pub fn delivers_to_user(&self) -> bool {
        self.action.kind != ActionKind::Record && self.action.deliver.unwrap_or(true)
    }

    /// Full definition check, including the halves that only make sense
    /// together: a rule must be able to decide something *and* do something.
    pub fn validate(&self, has_script: bool, known_agent: bool) -> std::result::Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("`name` is required".to_string());
        }
        if !has_script && self.schedule.is_none() {
            return Err("a rule needs either a condition script or a schedule; \
                 with neither it would never judge"
                .to_string());
        }
        if let Some(schedule) = &self.schedule {
            validate_schedule(schedule)?;
        }
        self.action.validate()?;
        if self.action.kind == ActionKind::Agent && !known_agent {
            return Err(format!(
                "action.agent '{}' is not a known agent definition; \
                 start it as `record` until the agent exists",
                self.action.agent.as_deref().unwrap_or("")
            ));
        }
        for template in self.action.templates() {
            validate_template(template)?;
        }
        Ok(())
    }
}

/// A cron expression must parse and a one-shot must be in the future-ish
/// range; both are checked at create time so a bad rule never silently skips.
fn validate_schedule(schedule: &CronSchedule) -> std::result::Result<(), String> {
    match schedule {
        CronSchedule::At { at_ms } => {
            if *at_ms <= 0 {
                return Err("schedule.at_ms must be an epoch-millisecond value".to_string());
            }
            Ok(())
        }
        CronSchedule::Every { every_ms } => {
            if *every_ms < 60_000 {
                return Err("schedule.every_ms must be at least a minute (60000)".to_string());
            }
            Ok(())
        }
        CronSchedule::Cron { expr, .. } => {
            crate::cron::compute_next_run(schedule, crate::trigger::source::meta::now_ms())
                .ok_or_else(|| format!("schedule.expr '{expr}' does not parse"))
                .map(|_| ())
        }
    }
}

/// Unknown `{{...}}` is a definition error, not a silent leftover.
pub fn validate_template(template: &str) -> std::result::Result<(), String> {
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let tail = &rest[start + 2..];
        let Some(end) = tail.find("}}") else {
            return Err(format!(
                "template has an unclosed `{{{{` near '{}'",
                &template[start..template.len().min(start + 40)]
            ));
        };
        let name = tail[..end].trim();
        if !KNOWN_PLACEHOLDERS.contains(&name) {
            return Err(format!(
                "unknown placeholder `{{{{{name}}}}}`; allowed: {}",
                KNOWN_PLACEHOLDERS.join(", ")
            ));
        }
        rest = &tail[end + 2..];
    }
    Ok(())
}

/// What a rule directory holds, and whether it is usable.
#[derive(Debug, Clone)]
pub struct LoadedTrigger {
    pub meta: TriggerMeta,
    pub script: Option<String>,
    pub problem: Option<String>,
    pub dir: PathBuf,
}

/// Scan the rules root. A directory with unreadable JSON becomes a degraded
/// row rather than a vanished rule — the user can still see and fix it.
pub fn load_all(root: &Path) -> Vec<LoadedTrigger> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return out,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        if dir_name.starts_with('.') {
            continue;
        }
        out.push(load_one(&dir, &dir_name));
    }
    out.sort_by(|a, b| a.meta.id.cmp(&b.meta.id));
    out
}

fn load_one(dir: &Path, dir_name: &str) -> LoadedTrigger {
    let meta_path = dir.join(META_FILE);
    let mut problem = None;
    let mut meta = match std::fs::read_to_string(&meta_path) {
        Ok(text) => match serde_json::from_str::<TriggerMeta>(&text) {
            Ok(mut parsed) => {
                // The directory name owns the identity, same as a source.
                if parsed.id.trim().is_empty() || parsed.id != dir_name {
                    parsed.id = dir_name.to_string();
                }
                parsed
            }
            Err(e) => {
                problem = Some(format!("meta.json is not a valid trigger: {e}"));
                TriggerMeta::new(dir_name, dir_name)
            }
        },
        Err(_) => {
            problem = Some("meta.json is missing".to_string());
            TriggerMeta::new(dir_name, dir_name)
        }
    };
    meta.normalize();

    let script = std::fs::read_to_string(dir.join(SCRIPT_FILE)).ok();
    if problem.is_none() && script.is_none() && meta.schedule.is_none() {
        problem = Some(format!("{SCRIPT_FILE} is missing and no schedule was set"));
    }
    if problem.is_none()
        && let Err(e) = meta.validate(script.is_some(), true)
    {
        problem = Some(e);
    }

    LoadedTrigger {
        meta,
        script,
        problem,
        dir: dir.to_path_buf(),
    }
}

/// Write the definition file (atomic: the next tick must not read a half file).
pub fn save_meta(root: &Path, meta: &TriggerMeta) -> Result<PathBuf> {
    let dir = root.join(&meta.id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let text = serde_json::to_string_pretty(meta)?;
    crate::trigger::source::meta::write_atomic(&dir.join(META_FILE), text.as_bytes())?;
    Ok(dir)
}

pub fn save_script(root: &Path, id: &str, script: &str) -> Result<()> {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    crate::trigger::source::meta::write_atomic(&dir.join(SCRIPT_FILE), script.as_bytes())
}

pub fn remove_trigger(root: &Path, id: &str) -> Result<bool> {
    let dir = root.join(id);
    if !dir.exists() {
        return Ok(false);
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("cannot remove {}", dir.display()))?;
    Ok(true)
}

/// Generate an unused `trg-<4hex>` id.
pub fn generate_id(root: &Path) -> String {
    for _ in 0..64 {
        let candidate = format!("{ID_PREFIX}{:04x}", rand::random::<u16>());
        if !root.join(&candidate).exists() {
            return candidate;
        }
    }
    format!("{ID_PREFIX}{:08x}", rand::random::<u32>())
}

/// Parse the rule's action out of tool arguments.
///
/// The tool's own discriminator is also called `action`, so a rule action
/// arrives under `on_fire`.
pub fn action_from_args(args: &Value) -> std::result::Result<TriggerAction, String> {
    let Some(action) = args.get("on_fire") else {
        return Err("`on_fire` is required: {kind: 'record'|'notify'|'agent'}".to_string());
    };
    if let Some(text) = action.as_str() {
        // `"record"` as a shorthand for the whole action.
        let kind = ActionKind::parse(text).ok_or_else(|| {
            format!("unknown action kind '{text}'; expected agent, notify or record")
        })?;
        return Ok(TriggerAction {
            kind,
            ..Default::default()
        });
    }
    let kind_text = action
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("record");
    let kind = ActionKind::parse(kind_text).ok_or_else(|| {
        format!("unknown action kind '{kind_text}'; expected agent, notify or record")
    })?;
    let string_field = |name: &str| -> Option<String> {
        action
            .get(name)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty())
    };
    Ok(TriggerAction {
        kind,
        agent: string_field("agent"),
        task_template: string_field("task_template"),
        deliver: action.get("deliver").and_then(|v| v.as_bool()),
        channel: string_field("channel"),
        to: string_field("to"),
        text_template: string_field("text_template"),
    })
}

/// Parse a schedule out of tool arguments.
pub fn schedule_from_args(args: &Value) -> std::result::Result<Option<CronSchedule>, String> {
    let Some(value) = args.get("schedule") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let schedule: CronSchedule = serde_json::from_value(value.clone())
        .map_err(|e| format!("schedule must be {{kind:'at',at_ms}} / {{kind:'every',every_ms}} / {{kind:'cron',expr}}: {e}"))?;
    validate_schedule(&schedule)?;
    Ok(Some(schedule))
}

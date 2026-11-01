//! Tool risk levels and the approval policy/lifecycle (ADR 0011).
//!
//! Three separated layers live here:
//!
//! 1. [`RiskLevel`] — a fact declared by every built-in tool (`Tool::risk_level`).
//! 2. [`ApprovalConfig`] — the owner's control knobs (`tools.approval`).
//! 3. [`ApprovalStore`] — the in-memory pending list + the decisions that need
//!    no store (allow/deny, unattended turns).
//!
//! The single enforcement point is `runner::hooks::ApprovalHook`; this module
//! only owns the data and the "what does the owner see" text. In particular it
//! never persists the pending list: a restart is meant to fail closed.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::session::{Message, TurnKind};

/// The five risk classes (ADR 0011). Declared by the tool itself and not
/// configurable: configuration only picks a control level per class or tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// Reads state the agent already owns; no side effects.
    ReadOnly,
    /// Writes inside the workspace (files, memory, sessions).
    WorkspaceWrite,
    /// Runs arbitrary code/commands on the host.
    Exec,
    /// Leaves the machine to fetch data.
    NetworkEgress,
    /// Sends something to a third party (message, external write).
    ExternalSend,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::ReadOnly => "read_only",
            RiskLevel::WorkspaceWrite => "workspace_write",
            RiskLevel::Exec => "exec",
            RiskLevel::NetworkEgress => "network_egress",
            RiskLevel::ExternalSend => "external_send",
        }
    }
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Three control levels the owner chooses from (ADR 0011).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlLevel {
    /// Execute without asking.
    Allow,
    /// Execute only after the owner confirms on the next turn.
    Ask,
    /// Never execute.
    Deny,
}

impl ControlLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            ControlLevel::Allow => "allow",
            ControlLevel::Ask => "ask",
            ControlLevel::Deny => "deny",
        }
    }
}

impl std::fmt::Display for ControlLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Default approval TTL: 30 minutes.
pub const DEFAULT_APPROVAL_TIMEOUT_SECONDS: u64 = 30 * 60;
/// Hard upper bound on the TTL; configuration is clamped to it.
pub const MAX_APPROVAL_TIMEOUT_SECONDS: u64 = 24 * 60 * 60;

fn allow() -> ControlLevel {
    ControlLevel::Allow
}

fn ask() -> ControlLevel {
    ControlLevel::Ask
}

/// Per-risk-level defaults. These are fallbacks; `ApprovalConfig::tools` wins
/// over them for the named tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LevelControls {
    pub read_only: ControlLevel,
    pub workspace_write: ControlLevel,
    pub exec: ControlLevel,
    pub network_egress: ControlLevel,
    pub external_send: ControlLevel,
}

impl Default for LevelControls {
    fn default() -> Self {
        Self {
            read_only: allow(),
            workspace_write: allow(),
            exec: ask(),
            network_egress: allow(),
            external_send: ask(),
        }
    }
}

impl LevelControls {
    pub fn control(&self, risk: RiskLevel) -> ControlLevel {
        match risk {
            RiskLevel::ReadOnly => self.read_only,
            RiskLevel::WorkspaceWrite => self.workspace_write,
            RiskLevel::Exec => self.exec,
            RiskLevel::NetworkEgress => self.network_egress,
            RiskLevel::ExternalSend => self.external_send,
        }
    }
}

/// `tools.approval` in `config.json`.
///
/// It is read once at startup and snapshotted into [`ApprovalStore`]; editing
/// the file while the gateway runs does not change the in-effect policy
/// (restart to apply).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApprovalConfig {
    /// Level fallbacks.
    pub levels: LevelControls,
    /// Per-tool overrides, keyed by tool name.
    pub tools: HashMap<String, ControlLevel>,
    /// How long a pending record stays redeemable.
    pub timeout_seconds: u64,
}

impl Default for ApprovalConfig {
    fn default() -> Self {
        Self {
            levels: LevelControls::default(),
            tools: HashMap::new(),
            timeout_seconds: DEFAULT_APPROVAL_TIMEOUT_SECONDS,
        }
    }
}

impl ApprovalConfig {
    /// Clamp the TTL into the code-enforced bound, so a config mistake cannot
    /// make a pending record live forever.
    pub fn clamp(&mut self) {
        if self.timeout_seconds == 0 {
            self.timeout_seconds = DEFAULT_APPROVAL_TIMEOUT_SECONDS;
        }
        self.timeout_seconds = self.timeout_seconds.min(MAX_APPROVAL_TIMEOUT_SECONDS);
    }

    /// The control level in effect for `tool` at `risk`: a per-tool override
    /// beats the level default.
    pub fn control_for(&self, tool: &str, risk: RiskLevel) -> ControlLevel {
        self.tools
            .get(tool)
            .copied()
            .unwrap_or_else(|| self.levels.control(risk))
    }

    pub fn ttl_ms(&self) -> u64 {
        self.timeout_seconds.saturating_mul(1000)
    }

    /// Flatten the policy into the `tools.approval` schema field names, so the
    /// WebUI editor can render one form control per knob without knowing the
    /// nested shape.
    pub fn schema_values(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut values = serde_json::Map::new();
        values.insert(
            "levels.read_only".into(),
            serde_json::json!(self.levels.read_only),
        );
        values.insert(
            "levels.workspace_write".into(),
            serde_json::json!(self.levels.workspace_write),
        );
        values.insert("levels.exec".into(), serde_json::json!(self.levels.exec));
        values.insert(
            "levels.network_egress".into(),
            serde_json::json!(self.levels.network_egress),
        );
        values.insert(
            "levels.external_send".into(),
            serde_json::json!(self.levels.external_send),
        );
        values.insert(
            "timeout_seconds".into(),
            serde_json::json!(self.timeout_seconds),
        );
        values.insert(
            "tools".into(),
            serde_json::to_value(&self.tools).unwrap_or_else(|_| serde_json::json!({})),
        );
        values
    }
}

/// One pending approval record. It lives only in memory and dies with the
/// process (restart fails closed), so it is never serialized into the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingApproval {
    pub approval_id: String,
    pub owner_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_kind: TurnKind,
    pub tool: String,
    pub args: serde_json::Value,
    pub args_hash: String,
    pub risk: RiskLevel,
    pub created_ms: u64,
}

impl PendingApproval {
    /// One-line args rendering for the owner-facing list.
    pub fn args_text(&self) -> String {
        serde_json::to_string(&self.args).unwrap_or_else(|_| "{}".to_string())
    }

    /// Last-resort truncation for the trace payload only.
    pub fn trace_args(&self) -> String {
        let mut text = self.args_text();
        if text.len() > 2000 {
            text.truncate(2000);
            text.push('…');
        }
        text
    }
}

/// Hash of the canonical argument JSON. Pending records never outlive the
/// process, so a process-local stable hash is enough to bind a replay to the
/// proposal.
pub fn args_hash(args: &serde_json::Value) -> String {
    let canonical = serde_json::to_string(args).unwrap_or_default();
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

type Clock = dyn Fn() -> u64 + Send + Sync;

#[derive(Default)]
struct SessionState {
    pending: HashMap<String, PendingApproval>,
    /// Ids that were already pending when this turn started. Only these may be
    /// redeemed now, and any that survive the turn are cleared at its end.
    carried: HashSet<String>,
}

impl SessionState {
    fn drop_if_empty(&self) -> bool {
        self.pending.is_empty() && self.carried.is_empty()
    }
}

/// In-memory pending approvals, keyed by session.
///
/// It is deliberately not persisted: a restart loses the records, which turns
/// every `Ask` back into "propose again" (fail closed).
pub struct ApprovalStore {
    config: ApprovalConfig,
    clock: Arc<Clock>,
    inner: RwLock<HashMap<String, SessionState>>,
}

impl std::fmt::Debug for ApprovalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApprovalStore")
            .field("config", &self.config)
            .field("sessions", &self.inner.read().len())
            .finish()
    }
}

impl Default for ApprovalStore {
    fn default() -> Self {
        Self::new(ApprovalConfig::default())
    }
}

impl ApprovalStore {
    pub fn new(mut config: ApprovalConfig) -> Self {
        config.clamp();
        Self::with_clock(config, Arc::new(now_ms))
    }

    /// Test seam: run the lifecycle against a controllable clock.
    pub fn with_clock(config: ApprovalConfig, clock: Arc<Clock>) -> Self {
        Self {
            config,
            clock,
            inner: RwLock::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &ApprovalConfig {
        &self.config
    }

    pub fn control_for(&self, tool: &str, risk: RiskLevel) -> ControlLevel {
        self.config.control_for(tool, risk)
    }

    pub fn now_ms(&self) -> u64 {
        (self.clock)()
    }

    /// Start a policy turn: expire records past the TTL, then mark the rest as
    /// carried (the only records redeemable during this turn).
    pub fn begin_turn(&self, session_id: &str) -> Vec<PendingApproval> {
        let now = self.now_ms();
        let ttl_ms = self.config.ttl_ms();
        let mut inner = self.inner.write();
        let state = inner.entry(session_id.to_string()).or_default();
        let expired: Vec<PendingApproval> = state
            .pending
            .values()
            .filter(|record| now.saturating_sub(record.created_ms) > ttl_ms)
            .cloned()
            .collect();
        for record in &expired {
            state.pending.remove(&record.approval_id);
        }
        state.carried = state.pending.keys().cloned().collect();
        expired
    }

    /// End a policy turn: every record that was still pending from earlier
    /// turns is dropped (one turn to confirm; otherwise it rolls off). Records
    /// proposed during this turn are the *next* turn's business.
    pub fn end_turn(&self, session_id: &str) -> Vec<PendingApproval> {
        let mut inner = self.inner.write();
        let Some(state) = inner.get_mut(session_id) else {
            return Vec::new();
        };
        let carried = std::mem::take(&mut state.carried);
        let cleared: Vec<PendingApproval> = carried
            .iter()
            .filter_map(|id| state.pending.remove(id))
            .collect();
        if state.drop_if_empty() {
            inner.remove(session_id);
        }
        cleared
    }

    /// Record a proposal. Duplicate (tool, args_hash) proposals in a session are
    /// collapsed onto the first record so the model replaying a call twice does
    /// not create two entries.
    pub fn propose(&self, record: PendingApproval) -> PendingApproval {
        let mut inner = self.inner.write();
        let state = inner.entry(record.session_id.clone()).or_default();
        if let Some(existing) = state
            .pending
            .values()
            .find(|existing| existing.tool == record.tool && existing.args_hash == record.args_hash)
        {
            return existing.clone();
        }
        state
            .pending
            .insert(record.approval_id.clone(), record.clone());
        record
    }

    /// The record a replay redeems: same tool + same args hash, still pending.
    pub fn matching(&self, session_id: &str, tool: &str, hash: &str) -> Option<PendingApproval> {
        let inner = self.inner.read();
        inner.get(session_id)?.pending.values().find_map(|record| {
            (record.tool == tool && record.args_hash == hash).then(|| record.clone())
        })
    }

    /// Remove one record (redemption or veto).
    pub fn take(&self, session_id: &str, approval_id: &str) -> Option<PendingApproval> {
        let mut inner = self.inner.write();
        let state = inner.get_mut(session_id)?;
        let taken = state.pending.remove(approval_id);
        state.carried.remove(approval_id);
        if state.drop_if_empty() {
            inner.remove(session_id);
        }
        taken
    }

    /// Records that were pending when this turn began (the only ones a replay
    /// can redeem) for `tool`, with a different args hash: a replay that does
    /// not match the proposal, i.e. a veto.
    pub fn mismatching(&self, session_id: &str, tool: &str, hash: &str) -> Vec<PendingApproval> {
        let inner = self.inner.read();
        let Some(state) = inner.get(session_id) else {
            return Vec::new();
        };
        state
            .pending
            .values()
            .filter(|record| {
                state.carried.contains(&record.approval_id)
                    && record.tool == tool
                    && record.args_hash != hash
            })
            .cloned()
            .collect()
    }

    /// Pending records for a session, oldest first.
    pub fn pending_for(&self, session_id: &str) -> Vec<PendingApproval> {
        let inner = self.inner.read();
        let mut records: Vec<PendingApproval> = inner
            .get(session_id)
            .map(|state| state.pending.values().cloned().collect())
            .unwrap_or_default();
        records.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.approval_id.cmp(&b.approval_id))
        });
        records
    }

    /// Ids still present; used to decide which guidance results may be merged.
    pub fn live_ids(&self, session_id: &str) -> HashSet<String> {
        let inner = self.inner.read();
        inner
            .get(session_id)
            .map(|state| state.pending.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn has_pending(&self, session_id: &str) -> bool {
        let inner = self.inner.read();
        inner
            .get(session_id)
            .map(|state| !state.pending.is_empty())
            .unwrap_or(false)
    }

    /// The plain-text pending list shown to the owner at turn end.
    pub fn pending_list_text(&self, session_id: &str) -> String {
        format_pending_list(&self.pending_for(session_id))
    }
}

/// Owner-facing pending list. Plain text on purpose: no buttons, no card
/// protocol, one line per action plus the exact args.
pub fn format_pending_list(records: &[PendingApproval]) -> String {
    let mut text = String::from("Pending approval — nothing was executed:\n");
    for (index, record) in records.iter().enumerate() {
        text.push_str(&format!(
            "\n{}. {} (risk: {}, approval: {})\n   args: {}\n",
            index + 1,
            record.tool,
            record.risk,
            record.approval_id,
            record.args_text()
        ));
    }
    text.push_str(
        "\nReply to approve: the same call is replayed next turn and executed. \
         Approvals expire; nothing runs in the background.",
    );
    text
}

/// Text for a guidance tool result (same turn). The model must not treat the
/// call as executed and must not rephrase it away.
pub fn pending_guidance_text(record: &PendingApproval) -> String {
    format!(
        "NOT EXECUTED: {tool} needs an owner approval before it can run. \
         Proposed args: {args}. Approval id: {id}. \
         The owner sees this as a pending item; the same call replays next turn. \
         Do not try to route around the approval with other tools.",
        tool = record.tool,
        args = record.args_text(),
        id = record.approval_id
    )
}

/// Text an unredeemed guidance result is rewritten to at run end, so the
/// history never claims the call is still waiting and no `tool_call` dangles.
pub fn unredeemed_text(approval_id: &str, tool: &str) -> String {
    format!(
        "NOT EXECUTED: the approval for `{tool}` ({approval_id}) was not redeemed; \
         it expired or was cleared. Nothing ran. Propose the action again if it is still needed."
    )
}

/// Fixed refusal text for a `Deny` control level (or an unattended `Ask`).
pub fn denied_text(tool: &str, risk: RiskLevel) -> String {
    format!(
        "DENIED: `{tool}` (risk: {risk}) is not allowed by the current approval policy. \
         Do not retry it with different phrasing or another tool; continue without it and \
         tell the owner which action was refused."
    )
}

/// Unattended variant of [`denied_text`]: nobody can approve right now.
pub fn unattended_denied_text(tool: &str, risk: RiskLevel, kind: TurnKind) -> String {
    format!(
        "DENIED: `{tool}` (risk: {risk}) needs owner authorization, and this run is \
         unattended ({}); nothing was executed and no confirmation was queued. \
         Return `needs owner authorization` as the result instead of retrying.",
        kind.as_str()
    )
}

/// Fixed text for a replay that does not match the proposal (ADR 0011 veto):
/// the approval covered a different call, so this one never runs.
pub fn vetoed_text(tool: &str, approval_id: &str) -> String {
    format!(
        "DENIED: `{tool}` does not match the pending approval {approval_id}; that approval was \
         withdrawn and nothing was executed. Do not retry this call — if the action is still \
         needed, propose it again so the owner sees the exact arguments."
    )
}

/// A build-time transform over the provider-bound message list (ADR 0011).
///
/// All approval state is read from the messages plus `live` (the ids still
/// pending). The append-only JSONL is never rewritten; this runs on the copy
/// handed to the provider, so it is idempotent and restart-safe.
///
/// - A guidance result whose record is still `live` stays: the owner is being
///   asked right now and the model must see what it is waiting for.
/// - A guidance result whose record is gone, but whose call was replayed with
///   the same `(tool, args_hash)` and produced a real result, is folded: the
///   real output takes the original slot (so the model sees one completed
///   round) and the confirmation message, replay call and replay result are
///   dropped.
/// - A guidance result whose record is gone and was never redeemed is
///   *rewritten* to a failure text, never deleted: deleting it would leave a
///   `tool_call` without a result, which the model reads as a truncated call
///   worth retrying.
pub fn merge_approval_rounds(messages: &[Message], live: &HashSet<String>) -> Vec<Message> {
    let results = result_indices(messages);
    let mut remove: HashSet<usize> = HashSet::new();
    let mut replaced: HashMap<usize, String> = HashMap::new();
    // Replay calls that were folded away, keyed by their owning assistant index.
    let mut dropped_calls: HashMap<usize, HashSet<String>> = HashMap::new();

    for (index, message) in messages.iter().enumerate() {
        let Message::Tool {
            meta,
            name,
            tool_call_id,
            ..
        } = message
        else {
            continue;
        };
        let Some(approval_id) = meta.approval_id.as_deref() else {
            continue;
        };
        if live.contains(approval_id) {
            continue;
        }
        let tool_name = name
            .clone()
            .or_else(|| original_call(messages, tool_call_id).map(|(_, call)| call.name.clone()))
            .unwrap_or_else(|| "tool".to_string());

        let Some((anchor, replay)) = find_replay(messages, &results, index, tool_call_id) else {
            // Unredeemed: keep the call paired with a failure result.
            replaced.insert(index, unredeemed_text(approval_id, &tool_name));
            continue;
        };

        let Message::Tool { content: real, .. } = &messages[replay.result] else {
            unreachable!("result index points at a tool message")
        };
        replaced.insert(index, real.clone());
        remove.insert(anchor);
        remove.insert(replay.result);
        dropped_calls
            .entry(replay.assistant)
            .or_default()
            .insert(replay.call_id);
    }

    let mut merged = Vec::with_capacity(messages.len());
    for (index, message) in messages.iter().enumerate() {
        if remove.contains(&index) {
            continue;
        }
        let mut message = message.clone();
        if let Some(content) = replaced.get(&index)
            && let Message::Tool {
                meta,
                content: slot,
                ..
            } = &mut message
        {
            *slot = content.clone();
            meta.approval_id = None;
        }
        if let Some(dropped) = dropped_calls.get(&index)
            && let Message::Assistant { tool_calls, .. } = &mut message
            && let Some(calls) = tool_calls.as_mut()
        {
            calls.retain(|call| !dropped.contains(&call.id));
            if calls.is_empty() {
                // The replay call was the message's only call.
                continue;
            }
        }
        merged.push(message);
    }
    merged
}

/// The index of the first message that must survive trimming while an approval
/// is still pending (ADR 0011: marked messages are pinned to the tail).
///
/// The pin starts at the user message opening the turn that produced the
/// guidance, so the pinned tail always starts on a legal boundary.
pub fn protect_start(messages: &[Message], live: &HashSet<String>) -> Option<usize> {
    let mut start: Option<usize> = None;
    for (index, message) in messages.iter().enumerate() {
        let Some(approval_id) = message.approval_id() else {
            continue;
        };
        if !live.contains(approval_id) {
            continue;
        }
        let from = messages[..=index]
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(index);
        start = Some(start.map_or(from, |current: usize| current.min(from)));
    }
    start
}

/// Tool result index by tool call id (first occurrence wins).
fn result_indices(messages: &[Message]) -> HashMap<&str, usize> {
    let mut results = HashMap::new();
    for (index, message) in messages.iter().enumerate() {
        if let Message::Tool { tool_call_id, .. } = message {
            results.entry(tool_call_id.as_str()).or_insert(index);
        }
    }
    results
}

/// The assistant message + call that originally declared `call_id`.
fn original_call<'a>(
    messages: &'a [Message],
    call_id: &str,
) -> Option<(usize, &'a crate::tool::ToolCall)> {
    messages.iter().enumerate().find_map(|(index, message)| {
        let Message::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        else {
            return None;
        };
        calls
            .iter()
            .find(|call| call.id == call_id)
            .map(|call| (index, call))
    })
}

struct Replay {
    assistant: usize,
    result: usize,
    call_id: String,
}

/// Find the replay of the proposal at `guidance`: the first assistant turn
/// after the owner's next message that calls the same tool with the same
/// arguments and got a real (non-guidance) result.
fn find_replay(
    messages: &[Message],
    results: &HashMap<&str, usize>,
    guidance: usize,
    original_call_id: &str,
) -> Option<(usize, Replay)> {
    let (_, original) = original_call(messages, original_call_id)?;
    let wanted = args_hash(&original.args);
    let anchor = messages
        .iter()
        .enumerate()
        .skip(guidance + 1)
        .find_map(|(index, message)| matches!(message, Message::User { .. }).then_some(index))?;
    for (index, message) in messages.iter().enumerate().skip(anchor + 1) {
        let Message::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        else {
            if matches!(message, Message::User { .. }) {
                // The owner moved on: no replay happened.
                break;
            }
            continue;
        };
        for call in calls {
            if call.name != original.name || args_hash(&call.args) != wanted {
                continue;
            }
            let Some(&result) = results.get(call.id.as_str()) else {
                continue;
            };
            // A guidance result means the call was proposed again rather than
            // redeemed, so this is a new round, not a replay.
            if messages[result].approval_id().is_some() {
                continue;
            }
            return Some((
                anchor,
                Replay {
                    assistant: index,
                    result,
                    call_id: call.id.clone(),
                },
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests;

//! The approval lifecycle driven only through the crate's public surface.
//!
//! `src/runner/hooks/approval.rs` tests the same behaviour with a `pub(crate)`
//! harness that can reach `RunState` and the hook traits. These tests are the
//! embedder's view: a `Provider`, a `Tool`, a `TaskHook` and an `ApprovalStore`
//! are enough to walk a proposal, a redemption and an abandoned round, and the
//! session JSONL is read as raw text so "append-only" is asserted on the file
//! rather than on the in-memory session view.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::Mutex;

use slimbot::{
    AgentConfig, AgentRunner, ApprovalStore, FinishReason, LLMResponse, MemoryService, Message,
    Provider, ProviderError, ProviderErrorKind, RiskLevel, SessionManager, SharedSessionManager,
    TaskHook, Tool, ToolCall, ToolDefinition, ToolManager, TurnKind, Usage,
};

const SESSION: &str = "appr-session";
const SHELL: &str = "shell";

// ── Scripted provider ──

struct ScriptedProvider {
    script: std::sync::Mutex<std::collections::VecDeque<LLMResponse>>,
    /// Every message list handed to the model, in call order.
    seen: std::sync::Mutex<Vec<Vec<Message>>>,
}

impl ScriptedProvider {
    fn new(script: Vec<LLMResponse>) -> Self {
        Self {
            script: std::sync::Mutex::new(script.into()),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn last_messages(&self) -> Vec<Message> {
        self.seen
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("the model must have been called")
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    async fn chat(
        &self,
        messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
        _recorder: Option<&dyn slimbot::trace::SpanRecorder>,
    ) -> slimbot::ProviderResult<LLMResponse> {
        self.seen
            .lock()
            .unwrap()
            .push(messages.iter().map(|m| (*m).clone()).collect());
        self.script.lock().unwrap().pop_front().ok_or_else(|| {
            ProviderError::new(ProviderErrorKind::Other, "the test script is exhausted")
        })
    }
}

// ── Counting tool ──

struct CounterTool {
    name: String,
    risk: RiskLevel,
    runs: Arc<AtomicUsize>,
}

impl CounterTool {
    fn new(name: &str, risk: RiskLevel, runs: Arc<AtomicUsize>) -> Self {
        Self {
            name: name.to_string(),
            risk,
            runs,
        }
    }
}

#[async_trait::async_trait]
impl Tool for CounterTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn risk_level(&self) -> RiskLevel {
        self.risk
    }

    fn description(&self) -> &str {
        "Counts how often the gate let it run"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": [],
        })
    }

    async fn execute(&self, _args: Value) -> anyhow::Result<String> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Ok("ran".to_string())
    }
}

// ── Script helpers ──

fn tool_call_response(id: &str, name: &str, args: Value) -> LLMResponse {
    LLMResponse {
        content: Some("calling a tool".to_string()),
        tool_calls: Some(vec![ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args,
        }]),
        finish_reason: FinishReason::ToolCalls,
        usage: Usage::default(),
    }
}

fn final_response(content: &str) -> LLMResponse {
    LLMResponse {
        content: Some(content.to_string()),
        tool_calls: None,
        finish_reason: FinishReason::Stop,
        usage: Usage::default(),
    }
}

fn test_config() -> AgentConfig {
    AgentConfig {
        provider: "test".to_string(),
        max_iterations: 40,
        timeout_seconds: 120,
        persist_tool_results: false,
        unknown: Default::default(),
    }
}

// ── Harness ──

/// What a caller can observe from `AgentRunner::run` without naming
/// `AgentResult` (it is not re-exported).
struct Outcome {
    success: bool,
    content: String,
    iterations: u32,
}

struct Harness {
    /// Held only so the temp directory outlives the session file.
    _tmp: TempDir,
    runner: AgentRunner,
    provider: Arc<ScriptedProvider>,
    store: Arc<ApprovalStore>,
    runs: Arc<AtomicUsize>,
    session_dir: PathBuf,
}

impl Harness {
    async fn new(script: Vec<LLMResponse>) -> Self {
        let tmp = TempDir::new().unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let session_dir = tmp.path().join("sessions");

        let runs = Arc::new(AtomicUsize::new(0));
        let mut tools = ToolManager::new(workspace.clone());
        tools.register(Box::new(CounterTool::new(
            SHELL,
            RiskLevel::Exec,
            runs.clone(),
        )));

        let sessions: SharedSessionManager = Arc::new(Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));
        sessions.lock().await.get_or_create(SESSION).await.unwrap();

        let provider = Arc::new(ScriptedProvider::new(script));
        let store = Arc::new(ApprovalStore::default());
        let memory = Arc::new(Mutex::new(MemoryService::new(&workspace)));
        memory.lock().await.init().unwrap();

        let runner = AgentRunner::new(
            Arc::new(tools),
            provider.clone(),
            sessions,
            test_config(),
            workspace,
            memory,
            None,
            4096,
            None,
        )
        .with_approvals(store.clone());

        Self {
            _tmp: tmp,
            runner,
            provider,
            store,
            runs,
            session_dir,
        }
    }

    async fn run(&self, content: &str, kind: TurnKind) -> Outcome {
        let hook = TaskHook::new(SESSION).with_turn_kind(kind);
        let result = self
            .runner
            .run(content.to_string(), hook, SESSION, None, None, None, None)
            .await;
        Outcome {
            success: result.success,
            content: result.content,
            iterations: result.iterations,
        }
    }

    /// The session file as written to disk, verbatim.
    fn session_file(&self) -> String {
        std::fs::read_to_string(self.session_dir.join(format!("{SESSION}.jsonl")))
            .unwrap_or_default()
    }

    /// The session file parsed back into messages. A line that does not parse
    /// is a failure, not something to skip: the point of these tests is that
    /// what landed on disk is exactly what the session model describes.
    fn persisted(&self) -> Vec<Message> {
        self.session_file()
            .lines()
            .map(|line| {
                serde_json::from_str::<Message>(line)
                    .unwrap_or_else(|e| panic!("unparseable session line {line:?}: {e}"))
            })
            .collect()
    }

    fn executed(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }
}

fn tool_results(messages: &[Message]) -> Vec<&Message> {
    messages
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect()
}

// ── Tests ──

/// The mark is written in PreTool and read in AfterTool: the persisted line
/// carries the approval id, so a restart can still key the build-time
/// transform on it.
#[tokio::test]
async fn test_approval_mark_across_phases_is_persisted_with_id() {
    let h = Harness::new(vec![
        tool_call_response("tc-1", SHELL, json!({"command": "deploy"})),
        final_response("unused"),
    ])
    .await;

    let outcome = h.run("deploy it", TurnKind::User).await;
    assert!(outcome.success, "{}", outcome.content);
    assert!(
        outcome.content.starts_with("Pending approval"),
        "the turn ends on the pending list: {}",
        outcome.content
    );

    let pending = h.store.pending_for(SESSION);
    assert_eq!(pending.len(), 1, "one pending record");
    let approval_id = pending[0].approval_id.clone();

    let lines: Vec<Value> = h
        .session_file()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let marked: Vec<&Value> = lines
        .iter()
        .filter(|v| v.get("approval_id").is_some())
        .collect();
    assert_eq!(
        marked.len(),
        1,
        "exactly one persisted line carries the mark: {lines:#?}"
    );
    assert_eq!(
        marked[0]["approval_id"],
        json!(approval_id),
        "the mark is the identifier of the pending record"
    );
    let content = marked[0]["content"].as_str().unwrap();
    assert!(content.starts_with("NOT EXECUTED"), "{content}");
    assert!(content.contains(&approval_id), "{content}");

    assert_eq!(h.executed(), 0, "a pending call never runs");
}

/// The JSONL is append-only: two rounds leave two tool results on disk, the
/// first round's guidance text is never rewritten, and everything written in
/// round one is still a byte-for-byte prefix of the file afterwards.
#[tokio::test]
async fn test_redeemed_round_on_disk_keeps_jsonl_append_only() {
    let h = Harness::new(vec![
        tool_call_response("tc-1", SHELL, json!({"command": "deploy"})),
        tool_call_response("tc-2", SHELL, json!({"command": "deploy"})),
        final_response("deployed"),
    ])
    .await;

    let first = h.run("deploy it", TurnKind::User).await;
    assert!(
        first.content.starts_with("Pending approval"),
        "{}",
        first.content
    );
    let after_first = h.session_file();
    assert!(!after_first.is_empty(), "round one is persisted");

    let second = h.run("yes, go ahead", TurnKind::User).await;
    assert!(second.success, "{}", second.content);

    let after_second = h.session_file();
    assert!(
        after_second.starts_with(&after_first),
        "round two must append, not rewrite: was\n{after_first}\nnow\n{after_second}"
    );

    let persisted = h.persisted();
    assert_eq!(
        tool_results(&persisted).len(),
        2,
        "one guidance result and one real result: {persisted:#?}"
    );
    assert!(
        after_second.contains("NOT EXECUTED"),
        "the guidance line stays on disk as appended: {after_second}"
    );
    assert!(
        after_second.contains("ran"),
        "the real result is appended: {after_second}"
    );

    // Folding happens on the provider-bound copy only.
    let seen = h.provider.last_messages();
    assert!(
        !seen.iter().any(|m| m.approval_id().is_some()),
        "no marked message survives a redeemed round: {seen:#?}"
    );
}

/// An unattended turn has nobody to redeem a proposal, so `Ask` is a refusal
/// with the unattended wording: the run continues, nothing is executed and no
/// pending record is written.
#[tokio::test]
async fn test_unattended_ask_on_any_turn_kind_writes_no_pending() {
    for kind in [
        TurnKind::Cron,
        TurnKind::Heartbeat,
        TurnKind::Trigger,
        TurnKind::Subagent,
        TurnKind::System,
    ] {
        let h = Harness::new(vec![
            tool_call_response("tc-1", SHELL, json!({"command": "deploy"})),
            final_response("needs owner authorization"),
        ])
        .await;

        let outcome = h.run("deploy it", kind).await;
        assert!(outcome.success, "{kind:?}: {}", outcome.content);
        assert_eq!(
            outcome.content, "needs owner authorization",
            "{kind:?}: the model gets to answer"
        );
        assert_eq!(outcome.iterations, 1, "{kind:?}: the run continues");
        assert!(
            h.store.pending_for(SESSION).is_empty(),
            "{kind:?}: no record may be queued"
        );
        assert_eq!(h.executed(), 0, "{kind:?}: nothing ran");
        assert!(
            !h.session_file().contains("approval_id"),
            "{kind:?}: the refusal is not an approval message"
        );

        // The refusal must be the unattended one, not the plain-policy denial:
        // the model has to be told that no confirmation was queued.
        let refusal = h
            .provider
            .last_messages()
            .into_iter()
            .find_map(|m| match m {
                Message::Tool { content, .. } => Some(content),
                _ => None,
            })
            .expect("the refusal is answered as a tool result");
        assert!(refusal.starts_with("DENIED"), "{kind:?}: {refusal}");
        assert!(
            refusal.contains("unattended"),
            "{kind:?}: the message must say why: {refusal}"
        );
        assert!(
            refusal.contains("no confirmation was queued"),
            "{kind:?}: {refusal}"
        );
    }
}

/// An abandoned round must not leave a `tool_call` without a result: the
/// guidance is rewritten to a failure text at the next build, never deleted,
/// and the rewrite is not written back to disk.
#[tokio::test]
async fn test_unredeemed_round_in_context_rewrites_guidance_to_failure() {
    let h = Harness::new(vec![
        tool_call_response("tc-1", SHELL, json!({"command": "deploy"})),
        final_response("ok, dropping it"),
        final_response("still here"),
    ])
    .await;

    h.run("deploy it", TurnKind::User).await;
    h.run("never mind", TurnKind::User).await;
    assert!(
        !h.store.has_pending(SESSION),
        "the unredeemed record rolled off"
    );

    h.run("hello again", TurnKind::User).await;

    let seen = h.provider.last_messages();
    let mut answers: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for message in &seen {
        if let Message::Tool {
            tool_call_id,
            content,
            ..
        } = message
        {
            answers.insert(tool_call_id.clone(), content.clone());
        }
    }
    let mut declared = Vec::new();
    for message in &seen {
        if let Message::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        {
            declared.extend(calls.iter().map(|call| call.id.clone()));
        }
    }
    assert!(!declared.is_empty(), "the proposal round is visible");
    for id in &declared {
        assert!(
            answers.contains_key(id),
            "tool_call {id} has no result: {seen:#?}"
        );
    }
    assert!(
        answers["tc-1"].contains("not redeemed"),
        "the guidance became a failure result: {}",
        answers["tc-1"]
    );

    // On disk the rewritten text is not what was appended: the failure text
    // exists only in the provider-bound copy.
    assert!(
        !h.session_file().contains("not redeemed"),
        "the transform never writes back: {}",
        h.session_file()
    );
}

/// The whole redemption round is reachable from the public traits alone: a
/// scripted provider replays the same call, the gate redeems it, the tool runs
/// once, and the third model call sees one folded, completed round.
#[tokio::test]
async fn test_redeemed_round_in_context_folds_to_one_result() {
    let h = Harness::new(vec![
        tool_call_response("tc-1", SHELL, json!({"command": "deploy"})),
        tool_call_response("tc-2", SHELL, json!({"command": "deploy"})),
        final_response("deployed"),
    ])
    .await;

    let first = h.run("deploy it", TurnKind::User).await;
    assert!(first.success, "{}", first.content);
    assert_eq!(first.iterations, 0, "no second model call on a proposal");
    assert_eq!(h.executed(), 0);

    let second = h.run("yes, go ahead", TurnKind::User).await;
    assert!(second.success, "{}", second.content);
    assert_eq!(h.executed(), 1, "the replay executed exactly once");
    assert!(!h.store.has_pending(SESSION), "the record was redeemed");

    let seen = h.provider.last_messages();

    // One result, and it sits in the *first* call's slot: the replay call and
    // its result were folded away rather than kept alongside.
    let results = tool_results(&seen);
    assert_eq!(results.len(), 1, "one result for one call: {seen:#?}");
    assert!(
        matches!(results[0], Message::Tool { tool_call_id, content, .. }
            if tool_call_id == "tc-1" && content.contains("ran")),
        "the real result takes the original call's slot: {seen:#?}"
    );

    let mut declared = Vec::new();
    for message in &seen {
        if let Message::Assistant {
            tool_calls: Some(calls),
            ..
        } = message
        {
            declared.extend(calls.iter().map(|call| call.id.clone()));
        }
    }
    assert_eq!(
        declared,
        vec!["tc-1".to_string()],
        "the replay call is folded out of the assistant turn: {seen:#?}"
    );
    assert!(
        !seen.iter().any(|m| matches!(
            m,
            Message::User { content: slimbot::Content::Plain(text), .. } if text == "yes, go ahead"
        )),
        "the confirmation line is folded away: {seen:#?}"
    );
}

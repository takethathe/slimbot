use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::memory::SharedMemoryStore;
use crate::message_bus::BusResult;
use crate::provider::Provider;
use crate::runner::AgentRunner;
use crate::tool::{ToolCall, ToolManager};
use crate::utils::write_file_atomic;
use crate::worker::BoxFuture;
use crate::worker::WorkerPool;

/// Shared SessionManager type alias
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;

/// Per-session execution coordinator.
/// Holds `running` flag and task queue, shared between SessionManager and task closures.
#[derive(Clone)]
pub struct SessionRunner {
    running: Arc<AtomicBool>,
    task_queue: Arc<std::sync::Mutex<VecDeque<Box<dyn FnOnce() -> BoxFuture + Send>>>>,
    cancel_token: Arc<std::sync::Mutex<CancellationToken>>,
    /// Notified by on_task_complete so shutdown can wait for the running task.
    task_done: Arc<tokio::sync::Notify>,
}

impl SessionRunner {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            task_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
            cancel_token: Arc::new(std::sync::Mutex::new(CancellationToken::new())),
            task_done: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// Push a task to the queue. If not currently running, submit it immediately.
    fn submit(&self, wrapped: Box<dyn FnOnce() -> BoxFuture + Send>) {
        self.task_queue.lock().unwrap().push_back(wrapped);
        self.maybe_start_next();
    }

    /// If idle and queue non-empty, pop the next task and submit to WorkerPool.
    fn maybe_start_next(&self) {
        if self.running.swap(true, Ordering::Relaxed) {
            return; // already running
        }
        let Some(f) = self.task_queue.lock().unwrap().pop_front() else {
            self.running.store(false, Ordering::Relaxed);
            return;
        };
        WorkerPool::global().submit(f);
    }

    /// Called by a task closure when it finishes: mark idle and trigger next.
    fn on_task_complete(&self) {
        self.running.store(false, Ordering::Relaxed);
        self.maybe_start_next();
        self.task_done.notify_waiters();
    }

    /// Cancel the current token and replace it with a fresh one.
    /// Returns a clone of the new (non-cancelled) token for the sentinel task.
    /// Tasks already queued hold a clone of the old token and will observe cancellation.
    fn cancel_and_reset(&self) -> CancellationToken {
        let mut guard = self.cancel_token.lock().unwrap();
        guard.cancel();
        let new_token = CancellationToken::new();
        let old = std::mem::replace(&mut *guard, new_token.clone());
        drop(old);
        new_token
    }

    /// Return a clone of the session's current cancellation token.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel_token.lock().unwrap().clone()
    }

    /// Whether the runner is currently idle.
    fn is_idle(&self) -> bool {
        !self.running.load(Ordering::Relaxed)
    }

    /// Shutdown: clear pending queue and cancel running tasks.
    /// Does NOT wait for the running task — callers use `wait_idle()` for that.
    fn shutdown(&self) {
        self.cancel_token.lock().unwrap().cancel();
        self.task_queue.lock().unwrap().clear();
    }

    /// Wait until the currently running task (if any) finishes.
    async fn wait_idle(&self) {
        if self.is_idle() {
            return;
        }
        // Poll in a loop: the running task calls on_task_complete() which
        // stores running=false AND notifies, so we'll see idle after the notify.
        loop {
            if self.is_idle() {
                return;
            }
            self.task_done.notified().await;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageMeta {
    #[serde(default)]
    id: usize,
    #[serde(default)]
    timestamp: String,
}

impl Default for MessageMeta {
    fn default() -> Self {
        Self { id: 0, timestamp: String::new() }
    }
}

/// Content block for multi-modal messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image { mime_type: String, base64_data: String },
}

/// User message content — plain text or multi-modal blocks.
/// `#[serde(untagged)]` allows deserializing plain strings from existing JSONL.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Content {
    Plain(String),
    Multi(Vec<ContentBlock>),
}

impl Content {
    /// Returns true if the content is empty.
    pub fn is_empty(&self) -> bool {
        match self {
            Content::Plain(s) => s.is_empty(),
            Content::Multi(blocks) => blocks.is_empty(),
        }
    }

    /// Get plain text representation.
    pub fn as_text(&self) -> &str {
        match self {
            Content::Plain(s) => s,
            Content::Multi(blocks) => {
                for block in blocks {
                    if let ContentBlock::Text { text } = block {
                        return text;
                    }
                }
                ""
            }
        }
    }

    /// Serialize content into a JSON value compatible with OpenAI's message format.
    pub fn to_openai_value(&self) -> serde_json::Value {
        match self {
            Content::Plain(s) => serde_json::Value::String(s.clone()),
            Content::Multi(blocks) => {
                let arr: Vec<_> = blocks.iter().map(|b| match b {
                    ContentBlock::Text { text } => {
                        serde_json::json!({"type": "text", "text": text})
                    }
                    ContentBlock::Image { mime_type, base64_data } => {
                        serde_json::json!({
                            "type": "image_url",
                            "image_url": {"url": format!("data:{};base64,{}", mime_type, base64_data)}
                        })
                    }
                }).collect();
                serde_json::Value::Array(arr)
            }
        }
    }
}

/// Convenience conversion: a String becomes Content::Plain.
impl From<String> for Content {
    fn from(s: String) -> Self {
        Content::Plain(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        #[serde(flatten)]
        meta: MessageMeta,
        content: String,
    },
    User {
        #[serde(flatten)]
        meta: MessageMeta,
        content: Content,
    },
    Assistant {
        #[serde(flatten)]
        meta: MessageMeta,
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking_blocks: Option<Vec<serde_json::Value>>,
    },
    Tool {
        #[serde(flatten)]
        meta: MessageMeta,
        content: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl Message {
    pub fn id(&self) -> usize {
        match self {
            Message::System { meta, .. } => meta.id,
            Message::User { meta, .. } => meta.id,
            Message::Assistant { meta, .. } => meta.id,
            Message::Tool { meta, .. } => meta.id,
        }
    }

    pub fn timestamp(&self) -> &str {
        match self {
            Message::System { meta, .. } | Message::User { meta, .. }
            | Message::Assistant { meta, .. } | Message::Tool { meta, .. } => &meta.timestamp,
        }
    }

    pub fn user(content: String) -> Self {
        Message::User {
            meta: MessageMeta { id: 0, timestamp: String::new() },
            content: Content::Plain(content),
        }
    }

    pub fn assistant(
        content: Option<String>,
        tool_calls: Option<Vec<ToolCall>>,
        reasoning_content: Option<String>,
        thinking_blocks: Option<Vec<serde_json::Value>>,
    ) -> Self {
        Message::Assistant {
            meta: MessageMeta { id: 0, timestamp: String::new() },
            content,
            tool_calls,
            reasoning_content,
            thinking_blocks,
        }
    }

    pub fn system(content: String) -> Self {
        Message::System { meta: MessageMeta { id: 0, timestamp: String::new() }, content }
    }

    pub fn tool(content: String, tool_call_id: String, name: Option<String>) -> Self {
        Message::Tool { meta: MessageMeta { id: 0, timestamp: String::new() }, content, tool_call_id, name }
    }
}

/// Snapshot of session data for consolidation analysis.
pub struct SessionData {
    pub messages: Vec<Message>,
    pub char_per_token_ratio: f64,
    pub last_consolidated_id: usize,
}

#[derive(Debug, Clone)]
pub enum TaskState {
    Pending,
    Running { current_iteration: u32 },
    Completed { result: String },
    Failed { error: String },
}

#[allow(dead_code)]
pub struct SessionTask {
    pub id: String,
    /// Session ID, used by SessionManager to find the correct SessionRunner.
    pub session_id: String,
    pub content: String,
    pub hook: TaskHook,
    pub state: TaskState,
    /// Optional execution closure. None for direct invocation via run_task.
    pub closure: Option<Box<dyn FnOnce() -> BoxFuture + Send>>,
}

#[derive(Clone)]
pub struct TaskHook {
    status_tx: Option<tokio::sync::mpsc::Sender<(String, TaskState)>>,
    session_id: String,
}

impl TaskHook {
    pub fn new(session_id: &str) -> Self {
        Self {
            status_tx: None,
            session_id: session_id.to_string(),
        }
    }

    pub fn with_status_channel(self, tx: tokio::sync::mpsc::Sender<(String, TaskState)>) -> Self {
        Self {
            status_tx: Some(tx),
            session_id: self.session_id,
        }
    }

    pub fn notify_status_change(&self, state: &TaskState) {
        if let Some(ref tx) = self.status_tx {
            let _ = tx.try_send((self.session_id.clone(), state.clone()));
        }
    }
}

/// Builder for constructing a `SessionTask` with its execution closure attached.
/// Encapsulates the full ReAct loop + outbound routing logic.
pub struct SessionTaskBuilder {
    session_id: String,
    content: String,
    hook: TaskHook,
    session_manager: Option<SharedSessionManager>,
    tool_manager: Option<Arc<ToolManager>>,
    provider: Option<Arc<dyn Provider>>,
    config: Option<AgentConfig>,
    workspace_dir: Option<PathBuf>,
    memory_store: Option<SharedMemoryStore>,
    outbound_tx: Option<tokio::sync::mpsc::Sender<BusResult>>,
    channel_inject: Option<String>,
    consolidator: Option<Arc<Consolidator>>,
    cancel_token: Option<CancellationToken>,
}

impl SessionTaskBuilder {
    pub fn new(session_id: String, content: String, hook: TaskHook) -> Self {
        Self {
            session_id,
            content,
            hook,
            session_manager: None,
            tool_manager: None,
            provider: None,
            config: None,
            workspace_dir: None,
            memory_store: None,
            outbound_tx: None,
            channel_inject: None,
            consolidator: None,
            cancel_token: None,
        }
    }

    pub fn session_manager(mut self, sm: SharedSessionManager) -> Self {
        self.session_manager = Some(sm);
        self
    }

    pub fn tool_manager(mut self, tm: Arc<ToolManager>) -> Self {
        self.tool_manager = Some(tm);
        self
    }

    pub fn provider(mut self, pv: Arc<dyn Provider>) -> Self {
        self.provider = Some(pv);
        self
    }

    pub fn config(mut self, cfg: AgentConfig) -> Self {
        self.config = Some(cfg);
        self
    }

    pub fn workspace_dir(mut self, wd: PathBuf) -> Self {
        self.workspace_dir = Some(wd);
        self
    }

    pub fn memory_store(mut self, ms: SharedMemoryStore) -> Self {
        self.memory_store = Some(ms);
        self
    }

    pub fn outbound_tx(mut self, ob: tokio::sync::mpsc::Sender<BusResult>) -> Self {
        self.outbound_tx = Some(ob);
        self
    }

    pub fn channel_inject(mut self, ci: Option<String>) -> Self {
        self.channel_inject = ci;
        self
    }

    pub fn consolidator(mut self, c: Option<Arc<Consolidator>>) -> Self {
        self.consolidator = c;
        self
    }

    pub fn cancel_token(mut self, ct: Option<CancellationToken>) -> Self {
        self.cancel_token = ct;
        self
    }

    pub fn build(self) -> SessionTask {
        let session_manager = self.session_manager.expect("session_manager required");
        let tool_manager = self.tool_manager.expect("tool_manager required");
        let provider = self.provider.expect("provider required");
        let config = self.config.expect("config required");
        let workspace_dir = self.workspace_dir.expect("workspace_dir required");
        let memory_store = self.memory_store.expect("memory_store required");
        let outbound_tx = self.outbound_tx.expect("outbound_tx required");

        let sid1 = self.session_id.clone();
        let sid2 = self.session_id.clone();
        let sid_for_task = self.session_id;
        let content = self.content;
        let hook = self.hook;

        let consolidator = self.consolidator;
        let cancel_token = self.cancel_token;

        let exec_closure: Box<dyn FnOnce() -> BoxFuture + Send> =
            Box::new(move || {
                Box::pin(async move {
                    let runner = AgentRunner::builder()
                        .session_manager(session_manager)
                        .tool_manager(tool_manager)
                        .provider(provider)
                        .config(config)
                        .workspace_dir(workspace_dir)
                        .memory_store(memory_store)
                        .channel_inject(self.channel_inject)
                        .consolidator(consolidator)
                        .cancel_token(cancel_token)
                        .build();
                    let result = runner.run(content, hook, &sid1).await;

                    // Send result outbound
                    let content = if result.success {
                        result.content.clone()
                    } else {
                        format!("Error: {}", result.content)
                    };
                    let _ = outbound_tx
                        .send(BusResult {
                            session_id: sid2,
                            task_id: String::new(),
                            content,
                        })
                        .await;
                })
            });

        SessionTask {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: sid_for_task,
            content: String::new(),
            hook: TaskHook::new(""),
            state: TaskState::Running { current_iteration: 0 },
            closure: Some(exec_closure),
        }
    }
}

impl SessionTask {
    /// Consume self and wrap the closure with auto-trigger logic.
    /// The returned closure executes the task, then calls `SessionRunner::on_task_complete`
    /// to mark idle and submit the next pending task.
    pub fn wrap(self, runner: SessionRunner)
        -> Box<dyn FnOnce() -> BoxFuture + Send>
    {
        Box::new(move || {
            let closure = self.closure.unwrap();
            Box::pin(async move {
                closure().await;
                runner.on_task_complete();
            })
        })
    }
}

fn default_char_per_token_ratio() -> f64 {
    4.0
}

/// Session metadata persisted alongside messages in the JSONL file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Messages with id <= this value have been consolidated and are excluded from context.
    pub last_consolidated_id: usize,
    /// Next auto-increment message ID.
    pub next_message_id: usize,
    /// Average characters per token observed from the last LLM call.
    /// Used to estimate per-message token contribution without re-calling the API.
    #[serde(default = "default_char_per_token_ratio")]
    pub char_per_token_ratio: f64,
    /// Summary text from the last consolidation round, injected into system prompt.
    #[serde(default)]
    pub last_summary: Option<String>,
}

#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    meta: SessionMeta,
    /// Index into messages: messages before this index have been flushed to disk.
    /// After consolidation, this is set to remaining message count (already on disk).
    last_persisted_idx: usize,
}

impl Session {
    pub fn last_consolidated_id(&self) -> usize {
        self.meta.last_consolidated_id
    }
    pub fn next_message_id(&self) -> usize {
        self.meta.next_message_id
    }
    pub fn char_per_token_ratio(&self) -> f64 {
        self.meta.char_per_token_ratio
    }
    pub fn last_summary(&self) -> Option<&str> {
        self.meta.last_summary.as_deref()
    }
    pub fn set_last_summary(&mut self, summary: &str) {
        self.meta.last_summary = if summary.is_empty() || summary == "(nothing)" {
            None
        } else {
            Some(summary.to_string())
        };
    }
    fn update_consolidated_id(&mut self, id: usize) {
        self.meta.last_consolidated_id = id;
    }
    /// Update the chars-per-token ratio based on the last LLM prompt tokens.
    /// Keeps the previous ratio if there are no messages to measure against.
    pub fn update_token_ratio(&mut self, prompt_tokens: u32) {
        let total_chars: usize = self
            .messages
            .iter()
            .map(message_content_chars)
            .sum();
        if total_chars > 0 && prompt_tokens > 0 {
            self.meta.char_per_token_ratio = total_chars as f64 / prompt_tokens as f64;
        }
    }
}

/// Count visible text characters in a message.
pub fn message_content_chars(msg: &Message) -> usize {
    match msg {
        Message::System { content, .. } => content.len(),
        Message::User { content, .. } => content.as_text().len(),
        Message::Assistant { content, tool_calls, .. } => {
            let text_len = content.as_deref().map(|c| c.len()).unwrap_or(0);
            let tc_len = tool_calls
                .as_ref()
                .map(|calls| {
                    calls
                        .iter()
                        .map(|tc| tc.id.len() + tc.name.len() + tc.args.to_string().len())
                        .sum::<usize>()
                })
                .unwrap_or(0);
            text_len + tc_len
        }
        Message::Tool { content, .. } => content.len(),
    }
}

/// Extract the content string from a message.
pub fn message_content_str(msg: &Message) -> &str {
    match msg {
        Message::System { content, .. } => content,
        Message::User { content, .. } => content.as_text(),
        Message::Assistant { content, .. } => content.as_deref().unwrap_or(""),
        Message::Tool { content, .. } => content,
    }
}

/// Simplified message for frontend display.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum FrontendMessage {
    User { content: String },
    Assistant { content: String },
}

impl FrontendMessage {
    pub fn from_message(msg: &Message) -> Option<Self> {
        match msg {
            Message::User { content, .. } => Some(Self::User { content: content.as_text().to_string() }),
            Message::Assistant { content, .. } => {
                content.as_ref().map(|c| Self::Assistant { content: c.clone() })
            }
            _ => None,
        }
    }
}

/// Builder for assigning auto-increment IDs and timestamps to messages.
fn assign_message_id(msg: &mut Message, next_id: &mut usize) {
    let id = *next_id;
    let ts = chrono::Local::now().to_rfc3339();
    *next_id += 1;
    match msg {
        Message::System { meta, .. } => { meta.id = id; meta.timestamp = ts; }
        Message::User { meta, .. } => { meta.id = id; meta.timestamp = ts; }
        Message::Assistant { meta, .. } => { meta.id = id; meta.timestamp = ts; }
        Message::Tool { meta, .. } => { meta.id = id; meta.timestamp = ts; }
    }
}

pub struct SessionManager {
    sessions: HashMap<String, Session>,
    /// Per-session execution coordinators, keyed by session id.
    runners: HashMap<String, SessionRunner>,
    session_dir: PathBuf,
}

impl SessionManager {
    pub fn new(session_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&session_dir)?;
        Ok(Self {
            sessions: HashMap::new(),
            runners: HashMap::new(),
            session_dir,
        })
    }

    pub fn create_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// List all persisted session files on disk matching the given prefix.
    /// Returns (session_id, message_count, created_time_ms).
    pub fn list_persisted_sessions(&self, prefix: &str) -> Vec<(String, usize, i64)> {
        let mut results = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&self.session_dir) {
            for entry in entries.filter_map(|e| e.ok()) {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".jsonl") {
                    continue;
                }
                let session_id = name.trim_end_matches(".jsonl").to_string();
                if !session_id.starts_with(prefix) {
                    continue;
                }
                let msg_count = Self::count_messages_in_jsonl(&entry.path());
                let created = entry.metadata()
                    .ok()
                    .and_then(|m| m.created().ok())
                    .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64)
                    .unwrap_or(0);
                results.push((session_id, msg_count, created));
            }
        }
        results.sort_by(|a, b| a.2.cmp(&b.2).reverse());
        results
    }

    fn count_messages_in_jsonl(path: &std::path::Path) -> usize {
        std::fs::File::open(path)
            .map(|f| {
                let reader = std::io::BufReader::new(f);
                reader.lines().filter(|l| l.as_ref().map(|s| !s.trim().is_empty()).unwrap_or(false)).count()
            })
            .unwrap_or(0)
    }

    /// Load messages from a persisted session file for API response.
    /// Returns messages suitable for frontend display (user + assistant only).
    pub fn load_session_messages(&self, session_id: &str) -> Result<Vec<FrontendMessage>> {
        let msgs = Self::load_messages_from_jsonl(&self.session_dir, session_id, 0)?;
        Ok(msgs.iter().filter_map(|m| FrontendMessage::from_message(m)).collect())
    }

    fn meta_path(&self, session_id: &str) -> PathBuf {
        self.session_dir.join(format!("{}.meta.json", session_id))
    }

    fn load_meta_file(&self, session_id: &str) -> Result<Option<SessionMeta>> {
        let path = self.meta_path(session_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let meta: SessionMeta = serde_json::from_str(&content)?;
        Ok(Some(meta))
    }

    fn save_meta_file(&self, session_id: &str, meta: &SessionMeta) -> Result<()> {
        let content = serde_json::to_string(meta)?;
        write_file_atomic(&self.meta_path(session_id), &content)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    pub(crate) fn save_session_meta(&self, session_id: &str) {
        if let Some(meta) = self.sessions.get(session_id).map(|s| s.meta.clone()) {
            let _ = self.save_meta_file(session_id, &meta);
        }
    }

    pub async fn get_or_create(&mut self, session_id: &str) -> Result<&Session> {
        if self.sessions.contains_key(session_id) {
            return Ok(self.sessions.get(session_id).unwrap());
        }

        // Load meta from separate file
        let meta = self.load_meta_file(session_id)?;
        let (last_consolidated_id, mut next_id, char_per_token_ratio, last_summary) = match &meta {
            Some(m) => (m.last_consolidated_id, m.next_message_id, m.char_per_token_ratio, m.last_summary.clone()),
            None => (0, 1, 4.0, None),
        };

        // Load messages from append-only JSONL, skipping consolidated ones
        let messages = Self::load_messages_from_jsonl(&self.session_dir, session_id, last_consolidated_id)?;
        let max_loaded_id = messages.iter().map(|m| m.id()).max().unwrap_or(0);
        if max_loaded_id >= next_id {
            next_id = max_loaded_id + 1;
        }
        let loaded_count = messages.len();

        self.sessions.insert(
            session_id.to_string(),
            Session {
                id: session_id.to_string(),
                messages,
                meta: meta.unwrap_or(SessionMeta { last_consolidated_id, next_message_id: next_id, char_per_token_ratio, last_summary }),
                last_persisted_idx: loaded_count, // all loaded messages are already on disk
            },
        );
        self.runners.insert(session_id.to_string(), SessionRunner::new());
        Ok(self.sessions.get(session_id).unwrap())
    }

    /// Submit a SessionTask to a session. The session guarantees sequential execution.
    /// The task's closure is consumed and executed once.
    pub async fn submit_task(&mut self, task: SessionTask) {
        let runner = self.runners.get(&task.session_id).cloned().expect("no runner for session");
        let wrapped = task.wrap(runner.clone());
        runner.submit(wrapped);
    }

    pub async fn add_message(&mut self, session_id: &str, mut msg: Message) -> Result<()> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        assign_message_id(&mut msg, &mut session.meta.next_message_id);
        session.messages.push(msg);
        Ok(())
    }

    pub async fn get_messages(&self, session_id: &str) -> Vec<Message> {
        self.sessions
            .get(session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    }

    /// Return a snapshot of session data for consolidation analysis.
    pub fn get_session_data(&self, session_id: &str) -> Option<SessionData> {
        let session = self.sessions.get(session_id)?;
        Some(SessionData {
            messages: session.messages.clone(),
            char_per_token_ratio: session.char_per_token_ratio(),
            last_consolidated_id: session.last_consolidated_id(),
        })
    }

    /// Update the consolidation cursor for a session.
    /// Messages with id <= new_cursor_id are physically removed from memory
    /// and the persist offset is set to the remaining message count (already on disk).
    pub async fn update_consolidation_cursor(&mut self, session_id: &str, new_cursor_id: usize) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.update_consolidated_id(new_cursor_id);
            session.messages.retain(|m| m.id() > new_cursor_id);
            session.last_persisted_idx = session.messages.len();
        }
        // Save meta after releasing the mutable borrow
        self.save_session_meta(session_id);
    }

    /// Set the last consolidation summary for a session.
    pub async fn set_last_summary(&mut self, session_id: &str, summary: &str) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.set_last_summary(summary);
        }
    }

    /// Get the last consolidation summary for a session.
    pub async fn get_last_summary(&self, session_id: &str) -> Option<String> {
        self.sessions
            .get(session_id)
            .and_then(|s| s.last_summary().map(|s| s.to_string()))
    }

    pub fn update_token_ratio(&mut self, session_id: &str, prompt_tokens: u32) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.update_token_ratio(prompt_tokens);
        }
    }

    /// Clear all messages and reset consolidation state for a session.
    pub fn clear_session(&mut self, session_id: &str) -> bool {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.messages.clear();
            session.last_persisted_idx = 0;
            session.meta.last_consolidated_id = 0;
            session.meta.last_summary = None;
            true
        } else {
            false
        }
    }

    /// Truncate session messages to the given length.
    /// Used to roll back a turn's messages on cancellation or error.
    /// Does NOT persist — the caller decides whether to persist.
    pub async fn truncate_messages(&mut self, session_id: &str, new_len: usize) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.messages.truncate(new_len);
            if session.last_persisted_idx > new_len {
                session.last_persisted_idx = new_len;
            }
        }
    }

    /// Get the number of unconsolidated messages for a session.
    pub fn message_count(&self, session_id: &str) -> usize {
        self.sessions
            .get(session_id)
            .map(|s| s.messages.len())
            .unwrap_or(0)
    }

    /// Return all session IDs that match a prefix.
    pub fn list_session_ids(&self, prefix: &str) -> Vec<String> {
        self.sessions.keys()
            .filter(|k| k.starts_with(prefix))
            .map(|k| k.to_string())
            .collect()
    }

    /// Check if a session exists in memory.
    pub fn has_session(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Cancel all pending and running tasks for a session and reset to a fresh token.
    /// Returns the new (non-cancelled) token, which the sentinel task should use.
    /// Already-queued tasks hold clones of the old token and will observe cancellation.
    pub fn cancel_and_reset_session(&self, session_id: &str) -> Option<CancellationToken> {
        self.runners.get(session_id).map(|r| r.cancel_and_reset())
    }

    /// Get a clone of the session's cancellation token.
    pub fn session_cancel_token(&self, session_id: &str) -> Option<CancellationToken> {
        self.runners.get(session_id).map(|r| r.cancel_token())
    }

    /// Shutdown all sessions: cancel tokens, clear pending queues.
    pub fn shutdown_all(&self) {
        for (_, runner) in &self.runners {
            runner.shutdown();
        }
    }

    /// Wait for all running tasks to finish.
    pub async fn wait_all_idle(&self) {
        let runners: Vec<_> = self.runners.values().cloned().collect();
        for runner in runners {
            runner.wait_idle().await;
        }
    }

    /// Persist all session meta files and sync message + meta files to disk.
    pub fn sync_all_meta(&self) {
        for session_id in self.sessions.keys() {
            self.save_session_meta(session_id);
            // Sync the session's message JSONL file.
            let jsonl_path = self.session_dir.join(format!("{}.jsonl", session_id));
            if let Ok(file) = std::fs::OpenOptions::new().append(true).open(&jsonl_path) {
                let _ = file.sync_all();
            }
        }
        // Sync the session directory to ensure all files are durable.
        if let Ok(dir) = std::fs::OpenOptions::new().read(true).open(&self.session_dir) {
            let _ = dir.sync_all();
        }
    }

    pub async fn persist(&mut self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        let pending = &session.messages[session.last_persisted_idx..];
        let pending_count = pending.len();

        if pending_count > 0 {
            let path = self.session_dir.join(format!("{}.jsonl", session_id));
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            for msg in pending {
                let line = serde_json::to_string(msg)?;
                writeln!(file, "{}", line)?;
            }
            file.flush()?;
        }

        // Update offset and persist meta
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_persisted_idx = session.messages.len();
        }
        self.save_session_meta(session_id);

        Ok(())
    }

    fn load_messages_from_jsonl(
        session_dir: &PathBuf,
        session_id: &str,
        last_consolidated_id: usize,
    ) -> Result<Vec<Message>> {
        let file_path = session_dir.join(format!("{}.jsonl", session_id));
        if !file_path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&file_path)?;
        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(trimmed)
                .context(format!("Invalid JSONL format: {}", trimmed))?;
            // Skip consolidated messages (id <= cursor), except id=0 with cursor=0 for backward compat
            if msg.id() <= last_consolidated_id && !(msg.id() == 0 && last_consolidated_id == 0) {
                continue;
            }
            messages.push(msg);
        }
        Ok(messages)
    }
}

/// Convenience: ensure session exists
pub async fn ensure_session(sm: &SharedSessionManager, session_id: &str) -> Result<()> {
    let mut guard: tokio::sync::MutexGuard<'_, SessionManager> = sm.lock().await;
    guard.get_or_create(session_id).await?;
    Ok(())
}

/// Resolve origin channel/chat_id, falling back to parsing `session_id`.
pub fn parse_session_origin(session_id: &str, origin: (Option<&str>, Option<&str>)) -> (String, String) {
    match origin {
        (Some(ch), Some(cid)) => (ch.to_string(), cid.to_string()),
        _ => session_id
            .split_once(':')
            .map(|(c, i)| (c.to_string(), i.to_string()))
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_msg(content: &str) -> Message {
        Message::User { meta: MessageMeta { id: 0, timestamp: String::new() }, content: Content::Plain(content.to_string()) }
    }

    fn assistant_msg(content: &str) -> Message {
        Message::Assistant { meta: MessageMeta { id: 0, timestamp: String::new() }, content: Some(content.to_string()), tool_calls: None, reasoning_content: None, thinking_blocks: None }
    }

    #[tokio::test]
    async fn test_message_id_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("hello")).await.unwrap();
        sm.add_message("s1", assistant_msg("hi back")).await.unwrap();
        sm.add_message("s1", user_msg("another")).await.unwrap();

        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages[0].id(), 1);
        assert_eq!(session.messages[1].id(), 2);
        assert_eq!(session.messages[2].id(), 3);
        assert_eq!(session.next_message_id(), 4);
    }

    #[tokio::test]
    async fn test_append_only_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("msg1")).await.unwrap();
        sm.add_message("s1", assistant_msg("reply1")).await.unwrap();
        sm.persist("s1").await.unwrap();

        // Check JSONL has exactly 2 lines (no metadata line)
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 2);

        // Add more messages and persist again
        sm.add_message("s1", user_msg("msg2")).await.unwrap();
        sm.add_message("s1", assistant_msg("reply2")).await.unwrap();
        sm.persist("s1").await.unwrap();

        // JSONL should now have 4 lines (appended, not rewritten)
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        let lines: Vec<&str> = jsonl.lines().collect();
        assert_eq!(lines.len(), 4);

        // Meta file should exist
        assert!(session_dir.join("s1.meta.json").exists());
    }

    #[tokio::test]
    async fn test_consolidation_cursor_removes_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("a")).await.unwrap();       // id=1
        sm.add_message("s1", assistant_msg("b")).await.unwrap();   // id=2
        sm.add_message("s1", user_msg("c")).await.unwrap();        // id=3
        sm.add_message("s1", assistant_msg("d")).await.unwrap();   // id=4
        sm.persist("s1").await.unwrap();

        // Verify all 4 messages in memory
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.last_persisted_idx, 4);

        // Consolidate first 2 messages (by id) — they should be physically removed
        sm.update_consolidation_cursor("s1", 2).await;

        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].id(), 3);
        assert_eq!(session.messages[1].id(), 4);
        assert_eq!(session.last_persisted_idx, 2); // remaining messages already on disk

        // Persist: nothing new to write, JSONL still has 4 lines
        sm.persist("s1").await.unwrap();
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 4);

        // Reload with a fresh SessionManager
        let mut sm2 = SessionManager::new(session_dir).unwrap();
        let session = sm2.get_or_create("s1").await.unwrap();
        assert_eq!(session.last_consolidated_id(), 2);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].id(), 3);
        assert_eq!(session.messages[1].id(), 4);

        // get_messages returns all remaining messages (no filtering needed)
        let msgs = sm2.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_meta_separate_file() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("test")).await.unwrap();
        sm.persist("s1").await.unwrap();

        // Verify meta file exists separately
        let meta_path = session_dir.join("s1.meta.json");
        assert!(meta_path.exists());

        let meta_content = std::fs::read_to_string(&meta_path).unwrap();
        let meta: SessionMeta = serde_json::from_str(&meta_content).unwrap();
        assert_eq!(meta.last_consolidated_id, 0);
        assert_eq!(meta.next_message_id, 2);

        // Verify JSONL has no metadata line — just the message
        let jsonl_content = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        let lines: Vec<&str> = jsonl_content.lines().collect();
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].contains("_type"));
        assert!(lines[0].contains("user"));
    }

    #[tokio::test]
    async fn test_backward_compatibility_no_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Write old-format JSONL without id fields
        let jsonl_path = session_dir.join("s1.jsonl");
        std::fs::write(&jsonl_path, r#"{"role":"user","content":"old msg1"}
{"role":"assistant","content":"old reply"}
"#).unwrap();

        // Load should succeed, messages get id=0 (serde default)
        // When last_consolidated_id=0 and msg.id=0, we load them for backward compat
        // (they can't be filtered by consolidation cursor since there's no meta file)
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // get_messages returns all messages (no filtering when last_consolidated_id=0)
        let msgs = sm.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id(), 0);
        assert_eq!(msgs[1].id(), 0);
    }

    #[tokio::test]
    async fn test_persisted_idx_tracks_slice_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // Fresh session: nothing to persist yet
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.last_persisted_idx, 0);

        // Add messages
        sm.add_message("s1", user_msg("a")).await.unwrap();
        sm.add_message("s1", user_msg("b")).await.unwrap();
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.last_persisted_idx, 0); // not yet persisted

        // First persist: writes 2 messages
        sm.persist("s1").await.unwrap();
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.last_persisted_idx, 2); // now points past last message

        // Add one more message
        sm.add_message("s1", user_msg("c")).await.unwrap();
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 3);
        assert_eq!(session.last_persisted_idx, 2); // unchanged

        // Second persist: should only append message "c" (idx 2..3)
        sm.persist("s1").await.unwrap();
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.last_persisted_idx, 3);

        // JSONL should have exactly 3 lines
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 3);
    }

    #[tokio::test]
    async fn test_double_persist_no_duplication() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("once")).await.unwrap();
        sm.persist("s1").await.unwrap();

        // Calling persist again with no new messages should not write duplicates
        sm.persist("s1").await.unwrap();
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 1);
    }

    #[tokio::test]
    async fn test_reload_then_persist_no_duplication() {
        // Regression test: when a session is loaded from disk (via get_or_create),
        // last_persisted_idx must be set to the loaded message count so that
        // subsequent persist() calls do not re-write already-persisted messages.
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");

        // Phase 1: create session, add messages, persist
        {
            let mut sm = SessionManager::new(session_dir.clone()).unwrap();
            sm.get_or_create("s1").await.unwrap();
            sm.add_message("s1", user_msg("msg1")).await.unwrap();
            sm.add_message("s1", assistant_msg("reply1")).await.unwrap();
            sm.add_message("s1", user_msg("msg2")).await.unwrap();
            sm.persist("s1").await.unwrap();
        }

        // Verify 3 lines on disk
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 3);

        // Phase 2: reload session from disk (fresh SessionManager)
        let mut sm2 = SessionManager::new(session_dir.clone()).unwrap();
        sm2.get_or_create("s1").await.unwrap();

        // Persist again without adding new messages — should NOT create duplicates
        sm2.persist("s1").await.unwrap();
        let jsonl2 = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl2.lines().count(), 3);

        // Phase 3: add a new message after reload, persist — should only add the new one
        sm2.add_message("s1", user_msg("msg3")).await.unwrap();
        sm2.persist("s1").await.unwrap();
        let jsonl3 = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl3.lines().count(), 4);

        // Phase 4: reload again and verify all 4 messages with no duplicates
        let mut sm3 = SessionManager::new(session_dir).unwrap();
        sm3.get_or_create("s1").await.unwrap();
        let msgs = sm3.get_messages("s1").await;
        assert_eq!(msgs.len(), 4);

        // Verify all IDs are unique
        let mut ids: Vec<usize> = msgs.iter().map(|m| m.id()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 4, "all message IDs should be unique");
    }

    #[tokio::test]
    async fn test_consolidation_resets_persist_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // Add 4 messages and persist all
        sm.add_message("s1", user_msg("a")).await.unwrap();
        sm.add_message("s1", user_msg("b")).await.unwrap();
        sm.add_message("s1", user_msg("c")).await.unwrap();
        sm.add_message("s1", user_msg("d")).await.unwrap();
        sm.persist("s1").await.unwrap();

        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 4);
        assert_eq!(session.last_persisted_idx, 4);

        // Consolidate first 2 — messages removed, persist offset set to remaining count
        sm.update_consolidation_cursor("s1", 2).await;
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.last_persisted_idx, 2); // remaining messages already on disk

        // Persist should NOT write any new lines (nothing changed since consolidation)
        sm.persist("s1").await.unwrap();
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 4);

        // Verify all 4 messages are still on disk (old + new)
        let mut sm2 = SessionManager::new(session_dir).unwrap();
        sm2.get_or_create("s1").await.unwrap();
        let msgs = sm2.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].id(), 3);
        assert_eq!(msgs[1].id(), 4);
    }

    #[tokio::test]
    async fn test_consolidate_then_add_then_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("a")).await.unwrap();
        sm.add_message("s1", user_msg("b")).await.unwrap();
        sm.persist("s1").await.unwrap();

        // Consolidate a (id=1)
        sm.update_consolidation_cursor("s1", 1).await;
        assert_eq!(sm.sessions.get("s1").unwrap().messages.len(), 1); // only "b"
        // last_persisted_idx set to 1 (remaining messages already on disk)
        assert_eq!(sm.sessions.get("s1").unwrap().last_persisted_idx, 1);

        // Add new message
        sm.add_message("s1", user_msg("c")).await.unwrap();
        assert_eq!(sm.sessions.get("s1").unwrap().messages.len(), 2);

        // Persist: should only append c (index 1..2) — b is already on disk
        sm.persist("s1").await.unwrap();
        let jsonl = std::fs::read_to_string(session_dir.join("s1.jsonl")).unwrap();
        assert_eq!(jsonl.lines().count(), 3); // a, b, c

        // Reload and verify only unconsolidated messages
        let mut sm2 = SessionManager::new(session_dir).unwrap();
        sm2.get_or_create("s1").await.unwrap();
        let msgs = sm2.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().all(|m| m.id() > 1));
    }

    #[tokio::test]
    async fn test_load_jsonl_with_existing_consolidation_cursor() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        // Simulate old disk state: meta has consolidated cursor, JSONL has all messages
        let jsonl_path = session_dir.join("s1.jsonl");
        let meta_path = session_dir.join("s1.meta.json");

        let mut lines = Vec::new();
        let mut msg_id: usize = 0;
        for text in ["msg1", "msg2", "msg3", "msg4"] {
            msg_id += 1;
            lines.push(format!(
                r#"{{"role":"user","content":"{}","id":{}}}"#,
                text, msg_id
            ));
        }
        std::fs::write(&jsonl_path, lines.join("\n")).unwrap();
        std::fs::write(&meta_path, r#"{"last_consolidated_id":2,"next_message_id":5}"#).unwrap();

        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // Only messages with id > 2 should be loaded into memory
        let session = sm.sessions.get("s1").unwrap();
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[0].id(), 3);
        assert_eq!(session.messages[1].id(), 4);
        assert_eq!(session.last_consolidated_id(), 2);
        assert_eq!(session.last_persisted_idx, 2); // loaded messages already on disk, nothing new to append
    }

    #[tokio::test]
    async fn test_session_runner_cancel() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // Token should not be cancelled initially
        let old_token = sm.session_cancel_token("s1").unwrap();
        assert!(!old_token.is_cancelled());

        // Cancel and reset: old token is cancelled, new token is fresh
        let new_token = sm.cancel_and_reset_session("s1").unwrap();
        assert!(old_token.is_cancelled());
        assert!(!new_token.is_cancelled());

        // Subsequent tasks get the new token
        let current = sm.session_cancel_token("s1").unwrap();
        assert!(!current.is_cancelled());
    }

    #[tokio::test]
    async fn test_cancel_session_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let sm = SessionManager::new(session_dir).unwrap();

        // Should not panic for non-existent session
        assert!(sm.cancel_and_reset_session("nonexistent").is_none());
        assert!(sm.session_cancel_token("nonexistent").is_none());
    }

    #[test]
    fn test_session_runner_cancel_and_reset() {
        let runner = SessionRunner::new();
        let old_token = runner.cancel_token();

        // Cancel and reset returns a new, non-cancelled token
        let new_token = runner.cancel_and_reset();

        // Old clone is cancelled, new one is not
        assert!(old_token.is_cancelled());
        assert!(!new_token.is_cancelled());

        // Subsequent callers also see the new token
        let current = runner.cancel_token();
        assert!(!current.is_cancelled());
    }

    #[test]
    fn test_stop_semantics_before_and_after() {
        // Simulates the full /stop lifecycle:
        // 1. Tasks A and B capture the current token (like being queued)
        // 2. /stop triggers cancel_and_reset
        // 3. Sentinel task captures the new token
        // 4. Task C (queued after /stop) captures the new token
        let runner = SessionRunner::new();

        // Step 1: Tasks queued before /stop
        let task_a_token = runner.cancel_token();
        let task_b_token = runner.cancel_token();

        // Step 2: /stop arrives — cancel old, get new
        let sentinel_token = runner.cancel_and_reset();

        // Step 3: New task queued after /stop
        let task_c_token = runner.cancel_token();

        // Verify: pre-/stop tasks see cancellation
        assert!(task_a_token.is_cancelled());
        assert!(task_b_token.is_cancelled());

        // Verify: sentinel and post-/stop tasks see a fresh token
        assert!(!sentinel_token.is_cancelled());
        assert!(!task_c_token.is_cancelled());

        // Another cancel_and_reset creates yet another fresh token
        let token_v2 = runner.cancel_and_reset();
        assert!(!token_v2.is_cancelled());
        assert!(sentinel_token.is_cancelled()); // old one is now cancelled
        assert!(!runner.cancel_token().is_cancelled()); // current is fresh
    }

    #[test]
    fn test_session_runner_shutdown_clears_queue_and_cancels_token() {
        let runner = SessionRunner::new();

        // Token is not cancelled initially
        assert!(!runner.cancel_token().is_cancelled());

        // Shutdown should cancel token
        runner.shutdown();
        assert!(runner.cancel_token().is_cancelled());

        // Queue is empty (nothing was pushed)
        assert!(runner.task_queue.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_session_manager_shutdown_all() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();
        sm.get_or_create("s2").await.unwrap();

        // Get tokens before shutdown
        let t1 = sm.session_cancel_token("s1").unwrap();
        let t2 = sm.session_cancel_token("s2").unwrap();
        assert!(!t1.is_cancelled());
        assert!(!t2.is_cancelled());

        // Shutdown all
        sm.shutdown_all();

        // All tokens cancelled
        assert!(t1.is_cancelled());
        assert!(t2.is_cancelled());
    }

    #[tokio::test]
    async fn test_session_manager_wait_all_idle() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // No running tasks → should return immediately
        sm.wait_all_idle().await;
        assert!(sm.runners.get("s1").unwrap().is_idle());
    }

    #[tokio::test]
    async fn test_list_persisted_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        {
            let mut sm = SessionManager::new(session_dir.clone()).unwrap();
            // Create and persist two sessions with prefix "webui:"
            sm.get_or_create("webui:chat1").await.unwrap();
            sm.add_message("webui:chat1", user_msg("hello")).await.unwrap();
            sm.add_message("webui:chat1", assistant_msg("hi")).await.unwrap();
            sm.persist("webui:chat1").await.unwrap();

            sm.get_or_create("webui:chat2").await.unwrap();
            sm.add_message("webui:chat2", user_msg("second")).await.unwrap();
            sm.persist("webui:chat2").await.unwrap();

            // Also create a session with different prefix (should be filtered out)
            sm.get_or_create("cli:default").await.unwrap();
            sm.add_message("cli:default", user_msg("cli msg")).await.unwrap();
            sm.persist("cli:default").await.unwrap();
        }

        // Fresh session manager to test disk-only listing
        let sm = SessionManager::new(session_dir).unwrap();
        let sessions = sm.list_persisted_sessions("webui:");
        assert_eq!(sessions.len(), 2);
        // Should contain chat1 and chat2 with correct message counts
        let ids: Vec<&str> = sessions.iter().map(|(id, _, _)| id.as_str()).collect();
        assert!(ids.contains(&"webui:chat1"));
        assert!(ids.contains(&"webui:chat2"));
    }

    #[tokio::test]
    async fn test_load_session_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        {
            let mut sm = SessionManager::new(session_dir.clone()).unwrap();
            sm.get_or_create("webui:chat1").await.unwrap();
            sm.add_message("webui:chat1", user_msg("hello")).await.unwrap();
            sm.add_message("webui:chat1", assistant_msg("hi there")).await.unwrap();
            sm.add_message("webui:chat1", user_msg("how are you")).await.unwrap();
            sm.add_message("webui:chat1", assistant_msg("good thanks")).await.unwrap();
            sm.persist("webui:chat1").await.unwrap();
        }

        let sm = SessionManager::new(session_dir).unwrap();
        let messages = sm.load_session_messages("webui:chat1").unwrap();
        assert_eq!(messages.len(), 4);
        assert!(matches!(&messages[0], FrontendMessage::User { .. }));
        assert!(matches!(&messages[1], FrontendMessage::Assistant { .. }));
    }

    #[test]
    fn test_frontend_message_from_message() {
        let user = Message::User { meta: MessageMeta { id: 1, timestamp: String::new() }, content: Content::Plain("hello".to_string()) };
        let fm = FrontendMessage::from_message(&user);
        assert!(fm.is_some());
        assert!(matches!(fm.unwrap(), FrontendMessage::User { content } if content == "hello"));

        let asst = Message::Assistant { meta: MessageMeta { id: 2, timestamp: String::new() }, content: Some("hi".to_string()), tool_calls: None, reasoning_content: None, thinking_blocks: None };
        let fm = FrontendMessage::from_message(&asst);
        assert!(matches!(fm.unwrap(), FrontendMessage::Assistant { content } if content == "hi"));

        let system = Message::System { meta: MessageMeta { id: 0, timestamp: String::new() }, content: "sys".to_string() };
        assert!(FrontendMessage::from_message(&system).is_none());

        let tool = Message::Tool { meta: MessageMeta { id: 3, timestamp: String::new() }, content: "tool".to_string(), tool_call_id: "t1".to_string(), name: None };
        assert!(FrontendMessage::from_message(&tool).is_none());
    }

    #[test]
    fn test_frontend_message_serde() {
        let user = FrontendMessage::User { content: "hello".to_string() };
        let json = serde_json::to_string(&user).unwrap();
        assert!(json.contains("\"role\":\"user\""));
        assert!(json.contains("\"content\":\"hello\""));

        let asst = FrontendMessage::Assistant { content: "hi".to_string() };
        let json = serde_json::to_string(&asst).unwrap();
        assert!(json.contains("\"role\":\"assistant\""));
    }

    #[tokio::test]
    async fn test_timestamp_is_set_on_add_message() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        let before = chrono::Local::now().to_rfc3339();
        sm.add_message("s1", user_msg("hello")).await.unwrap();
        let after = chrono::Local::now().to_rfc3339();

        let msgs = sm.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
        let ts = msgs[0].timestamp();
        assert!(!ts.is_empty(), "timestamp should not be empty");
        assert!(ts >= before.as_str() && ts <= after.as_str(), "timestamp {} should be in [{}, {}]", ts, before, after);
    }

    #[tokio::test]
    async fn test_timestamp_persists_and_reloads() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        {
            let mut sm = SessionManager::new(session_dir.clone()).unwrap();
            sm.get_or_create("s1").await.unwrap();
            sm.add_message("s1", user_msg("hello")).await.unwrap();
            sm.add_message("s1", assistant_msg("hi")).await.unwrap();
            sm.persist("s1").await.unwrap();
        }

        let mut sm2 = SessionManager::new(session_dir).unwrap();
        sm2.get_or_create("s1").await.unwrap();
        let msgs = sm2.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
        assert!(!msgs[0].timestamp().is_empty(), "user message should have timestamp");
        assert!(!msgs[1].timestamp().is_empty(), "assistant message should have timestamp");
    }

    #[tokio::test]
    async fn test_backward_compat_old_jsonl_no_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let jsonl_path = session_dir.join("s1.jsonl");
        std::fs::write(&jsonl_path, r#"{"role":"user","content":"old msg","id":1}
{"role":"assistant","content":"old reply","id":2}
"#).unwrap();

        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        let msgs = sm.get_messages("s1").await;
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].timestamp(), ""); // defaults to empty string
        assert_eq!(msgs[1].timestamp(), "");
    }

    #[tokio::test]
    async fn test_timestamps_increase_across_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("first")).await.unwrap();
        std::thread::sleep(std::time::Duration::from_millis(2));
        sm.add_message("s1", assistant_msg("second")).await.unwrap();

        let msgs = sm.get_messages("s1").await;
        assert!(msgs[1].timestamp() >= msgs[0].timestamp());
    }

    #[tokio::test]
    async fn test_truncate_messages() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();

        sm.add_message("s1", user_msg("a")).await.unwrap();
        sm.add_message("s1", assistant_msg("b")).await.unwrap();
        sm.add_message("s1", user_msg("c")).await.unwrap();
        assert_eq!(sm.message_count("s1"), 3);

        // Truncate to 1 message
        sm.truncate_messages("s1", 1).await;
        assert_eq!(sm.message_count("s1"), 1);
        let msgs = sm.get_messages("s1").await;
        assert!(matches!(&msgs[0], Message::User { content, .. } if content.as_text() == "a"));

        // Truncate to 0
        sm.truncate_messages("s1", 0).await;
        assert_eq!(sm.message_count("s1"), 0);

        // Truncate non-existent session should not panic
        sm.truncate_messages("nonexistent", 0).await;
    }

    #[test]
    fn test_content_plain_serializes_as_string() {
        let content = Content::Plain("hello".to_string());
        let json = serde_json::to_string(&content).unwrap();
        assert_eq!(json, "\"hello\"");
    }

    #[test]
    fn test_content_multi_serializes_as_array() {
        let content = Content::Multi(vec![
            ContentBlock::Text { text: "look".to_string() },
            ContentBlock::Image { mime_type: "image/png".to_string(), base64_data: "abc".to_string() },
        ]);
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.starts_with('['));
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"type\":\"image\""));
    }

    #[test]
    fn test_content_to_openai_value_multi() {
        let content = Content::Multi(vec![
            ContentBlock::Image { mime_type: "image/png".to_string(), base64_data: "abc".to_string() },
        ]);
        let val = content.to_openai_value();
        assert!(val.is_array());
        assert!(serde_json::to_string(&val).unwrap().contains("image_url"));
        assert!(serde_json::to_string(&val).unwrap().contains("data:image/png;base64,abc"));
    }

    #[test]
    fn test_content_deserializes_from_string() {
        let content: Content = serde_json::from_str("\"hello\"").unwrap();
        assert!(matches!(content, Content::Plain(_)));
    }

    #[test]
    fn test_content_to_openai_value_plain() {
        let content = Content::Plain("hello".to_string());
        assert_eq!(content.to_openai_value(), serde_json::json!("hello"));
    }

    #[test]
    fn test_content_is_empty() {
        assert!(Content::Plain("".to_string()).is_empty());
        assert!(!Content::Plain("hi".to_string()).is_empty());
        assert!(Content::Multi(vec![]).is_empty());
    }

    #[test]
    fn test_content_as_text() {
        let c = Content::Plain("hello".to_string());
        assert_eq!(c.as_text(), "hello");

        let c = Content::Multi(vec![
            ContentBlock::Image { mime_type: "image/png".to_string(), base64_data: "abc".to_string() },
            ContentBlock::Text { text: "caption".to_string() },
        ]);
        assert_eq!(c.as_text(), "caption");
    }
}

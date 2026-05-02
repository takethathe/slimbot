use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::config::AgentConfig;
use crate::memory::MemoryStore;
use crate::message_bus::BusResult;
use crate::provider::Provider;
use crate::runner::AgentRunner;
use crate::tool::{ToolCall, ToolManager};
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
}

impl SessionRunner {
    fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            task_queue: Arc::new(std::sync::Mutex::new(VecDeque::new())),
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
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
    },
    Tool {
        content: String,
        tool_call_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
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
    memory_store: Option<Arc<MemoryStore>>,
    outbound_tx: Option<tokio::sync::mpsc::Sender<BusResult>>,
    channel_inject: Option<String>,
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

    pub fn memory_store(mut self, ms: Arc<MemoryStore>) -> Self {
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

#[allow(dead_code)]
pub struct Session {
    pub id: String,
    pub messages: Vec<Message>,
    /// Number of messages already consolidated into history files.
    /// Messages before this index are excluded from context building.
    pub last_consolidated: usize,
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

    pub async fn get_or_create(&mut self, session_id: &str) -> Result<&Session> {
        if self.sessions.contains_key(session_id) {
            return Ok(self.sessions.get(session_id).unwrap());
        }
        let (messages, last_consolidated) =
            Self::load_messages_from_jsonl(&self.session_dir, session_id)?;
        self.sessions.insert(
            session_id.to_string(),
            Session {
                id: session_id.to_string(),
                messages,
                last_consolidated,
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

    pub async fn add_message(&mut self, session_id: &str, msg: Message) -> Result<()> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        session.messages.push(msg);
        Ok(())
    }

    pub async fn get_messages(&self, session_id: &str) -> Vec<Message> {
        self.sessions
            .get(session_id)
            .map(|s| s.messages[s.last_consolidated..].to_vec())
            .unwrap_or_default()
    }

    /// Return all messages including consolidated ones (for token counting).
    pub async fn get_all_messages(&self, session_id: &str) -> Vec<Message> {
        self.sessions
            .get(session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    }

    /// Update the consolidation cursor for a session.
    pub async fn update_consolidation_cursor(&mut self, session_id: &str, new_cursor: usize) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.last_consolidated = new_cursor;
        }
    }

    pub async fn persist(&self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        let file_path = self.session_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::File::create(&file_path)?;
        // Write metadata line first
        let meta = serde_json::json!({
            "_type": "metadata",
            "last_consolidated": session.last_consolidated,
        });
        writeln!(file, "{}", serde_json::to_string(&meta)?)?;
        for msg in &session.messages {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    fn load_messages_from_jsonl(
        session_dir: &PathBuf,
        session_id: &str,
    ) -> Result<(Vec<Message>, usize)> {
        let file_path = session_dir.join(format!("{}.jsonl", session_id));
        if !file_path.exists() {
            return Ok((Vec::new(), 0));
        }
        let file = std::fs::File::open(&file_path)?;
        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();
        let mut last_consolidated: usize = 0;
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // Check for metadata line
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(trimmed) {
                if meta.get("_type").and_then(|v| v.as_str()) == Some("metadata") {
                    if let Some(lc) = meta.get("last_consolidated").and_then(|v| v.as_u64()) {
                        last_consolidated = lc as usize;
                    }
                    continue;
                }
            }
            // Regular message
            let msg: Message = serde_json::from_str(trimmed)
                .context(format!("Invalid JSONL format: {}", trimmed))?;
            messages.push(msg);
        }
        Ok((messages, last_consolidated))
    }
}

/// Convenience: ensure session exists
pub async fn ensure_session(sm: &SharedSessionManager, session_id: &str) -> Result<()> {
    let mut guard: tokio::sync::MutexGuard<'_, SessionManager> = sm.lock().await;
    guard.get_or_create(session_id).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_last_consolidated_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        let mut sm = SessionManager::new(session_dir.clone()).unwrap();
        sm.get_or_create("s1").await.unwrap();

        // Add some messages
        sm.add_message("s1", Message::User { content: "a".to_string() }).await.unwrap();
        sm.add_message("s1", Message::Assistant { content: Some("b".to_string()), tool_calls: None }).await.unwrap();
        sm.add_message("s1", Message::User { content: "c".to_string() }).await.unwrap();
        sm.add_message("s1", Message::Assistant { content: Some("d".to_string()), tool_calls: None }).await.unwrap();

        // Simulate consolidation: first 2 messages are consolidated
        sm.update_consolidation_cursor("s1", 2).await;
        sm.persist("s1").await.unwrap();

        // Reload with a fresh SessionManager — load directly from disk
        let (msgs, lc) = SessionManager::load_messages_from_jsonl(&session_dir, "s1").unwrap();
        assert_eq!(msgs.len(), 4);
        assert_eq!(lc, 2);

        // Create a session manager and verify get_messages skips consolidated
        let mut sm3 = SessionManager::new(session_dir).unwrap();
        let session = sm3.get_or_create("s1").await.unwrap();
        assert_eq!(session.last_consolidated, 2);
        let unconsolidated = sm3.get_messages("s1").await;
        assert_eq!(unconsolidated.len(), 2); // only messages after index 2
    }
}

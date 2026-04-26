use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::tool::ToolCall;

/// Shared SessionManager type alias
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;

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

pub struct SessionTask {
    pub id: String,
    pub content: String,
    pub hook: TaskHook,
    pub state: TaskState,
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

pub struct Session {
    pub id: String,
    pub task_queue: VecDeque<SessionTask>,
    pub tasks: HashMap<String, TaskState>,
    pub messages: Vec<Message>,
}

pub struct SessionManager {
    sessions: HashMap<String, Session>,
    session_dir: PathBuf,
}

impl SessionManager {
    pub fn new(session_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&session_dir)?;
        Ok(Self {
            sessions: HashMap::new(),
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
        let messages = Self::load_messages_from_jsonl(&self.session_dir, session_id)?;
        self.sessions.insert(
            session_id.to_string(),
            Session {
                id: session_id.to_string(),
                task_queue: VecDeque::new(),
                tasks: HashMap::new(),
                messages,
            },
        );
        Ok(self.sessions.get(session_id).unwrap())
    }

    pub async fn add_message(&mut self, session_id: &str, msg: Message) -> Result<()> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        session.messages.push(msg);
        Ok(())
    }

    pub async fn enqueue_task(&mut self, session_id: &str, task: SessionTask) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.tasks.insert(task.id.clone(), TaskState::Pending);
            session.task_queue.push_back(task);
        }
    }

    pub async fn dequeue_task(&mut self, session_id: &str) -> Option<SessionTask> {
        self.sessions
            .get_mut(session_id)
            .and_then(|s| s.task_queue.pop_front())
    }

    pub async fn update_task_state(&mut self, session_id: &str, task_id: &str, state: TaskState) {
        if let Some(session) = self.sessions.get_mut(session_id) {
            session.tasks.insert(task_id.to_string(), state);
        }
    }

    pub async fn get_messages(&self, session_id: &str) -> Vec<Message> {
        self.sessions
            .get(session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    }

    pub async fn persist(&self, session_id: &str) -> Result<()> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| anyhow::anyhow!("Session not found: {}", session_id))?;
        let file_path = self.session_dir.join(format!("{}.jsonl", session_id));
        let mut file = std::fs::File::create(&file_path)?;
        for msg in &session.messages {
            let line = serde_json::to_string(msg)?;
            writeln!(file, "{}", line)?;
        }
        Ok(())
    }

    fn load_messages_from_jsonl(session_dir: &PathBuf, session_id: &str) -> Result<Vec<Message>> {
        let file_path = session_dir.join(format!("{}.jsonl", session_id));
        if !file_path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&file_path)?;
        let reader = std::io::BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Message =
                serde_json::from_str(&line).context(format!("Invalid JSONL format: {}", line))?;
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

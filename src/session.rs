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
    /// Number of messages already consolidated into history files.
    /// Messages before this index are excluded from context building.
    pub last_consolidated: usize,
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
        let (messages, last_consolidated) =
            Self::load_messages_from_jsonl(&self.session_dir, session_id)?;
        self.sessions.insert(
            session_id.to_string(),
            Session {
                id: session_id.to_string(),
                task_queue: VecDeque::new(),
                tasks: HashMap::new(),
                messages,
                last_consolidated,
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

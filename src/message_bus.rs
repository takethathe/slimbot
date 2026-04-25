use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

use crate::agent_loop::AgentLoop;
use crate::session::{ensure_session, SessionManager, SessionTask, TaskHook, TaskState};

pub struct MessageBus {
    agent_loop: Arc<AgentLoop>,
}

pub struct BusRequest {
    pub session_id: String,
    pub content: String,
    pub channel_inject: Option<String>,
    pub hook: TaskHook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusResult {
    pub session_id: String,
    pub task_id: String,
    pub content: String,
}

impl MessageBus {
    pub fn new(agent_loop: Arc<AgentLoop>) -> Self {
        Self { agent_loop }
    }

    pub async fn send(&self, request: BusRequest) -> Result<BusResult> {
        // 1. Ensure session exists
        ensure_session(&self.agent_loop.session_manager(), &request.session_id).await?;

        // 2. Wrap as SessionTask
        let task_id = uuid::Uuid::new_v4().to_string();
        let task = SessionTask {
            id: task_id.clone(),
            content: request.content,
            hook: request.hook,
            state: TaskState::Pending,
        };

        // 3. Enqueue
        {
            let sm = self.agent_loop.session_manager();
            let mut guard: tokio::sync::MutexGuard<'_, SessionManager> = sm.lock().await;
            guard.enqueue_task(&request.session_id, task).await;
        }

        // 4. Dequeue
        let mut task = {
            let sm = self.agent_loop.session_manager();
            let mut guard: tokio::sync::MutexGuard<'_, SessionManager> = sm.lock().await;
            guard.dequeue_task(&request.session_id).await
        }.ok_or_else(|| anyhow!("Queue is empty"))?;

        // 5. Execute
        let result = self.agent_loop.run_task(
            &request.session_id,
            &mut task,
            request.channel_inject,
        ).await?;

        Ok(BusResult {
            session_id: request.session_id,
            task_id: task.id,
            content: result,
        })
    }
}

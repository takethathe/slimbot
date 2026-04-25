use std::sync::Arc;

use anyhow::Result;

use crate::config::AgentConfig;
use crate::context::ContextBuilder;
use crate::provider::Provider;
use crate::session::{Message, SessionManager, SessionTask, SharedSessionManager, TaskState};
use crate::tool::ToolManager;

pub struct AgentRunner {
    context_builder: ContextBuilder,
    tool_manager: Arc<ToolManager>,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    config: AgentConfig,
}

impl AgentRunner {
    pub fn new(
        context_builder: ContextBuilder,
        tool_manager: Arc<ToolManager>,
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        config: AgentConfig,
    ) -> Self {
        Self {
            context_builder,
            tool_manager,
            provider,
            session_manager,
            config,
        }
    }

    pub async fn run(
        &self,
        task: &mut SessionTask,
        session_id: &str,
        channel_inject: Option<String>,
    ) -> Result<String> {
        // 1. Write user message
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            sm.add_message(
                session_id,
                Message::User {
                    content: task.content.clone(),
                },
            )
            .await?;
        }

        // 2. Update task state to Running
        let running_state = TaskState::Running {
            current_iteration: 0,
        };
        {
            let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                self.session_manager.lock().await;
            sm.update_task_state(session_id, &task.id, running_state.clone())
                .await;
        }
        task.hook.notify_status_change(&running_state);

        let max_iterations = self.config.max_iterations;
        let mut iterations: u32 = 0;

        loop {
            // Exceeded max iterations
            if iterations >= max_iterations {
                let err_msg = format!("Reached max iterations {}", max_iterations);
                let failed_state = TaskState::Failed {
                    error: err_msg.clone(),
                };
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.update_task_state(session_id, &task.id, failed_state.clone())
                        .await;
                }
                task.hook.notify_status_change(&failed_state);
                {
                    let sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.persist(session_id).await?;
                }
                return Err(anyhow::anyhow!(err_msg));
            }

            // Build context
            let ctx = self
                .context_builder
                .build(session_id, channel_inject.clone())
                .await;

            // Request model
            let response = self
                .provider
                .chat(&ctx.messages, ctx.tools.as_deref())
                .await?;

            // No tool calls → done
            let has_tool_calls = response
                .tool_calls
                .as_ref()
                .is_some_and(|calls| !calls.is_empty());

            if !has_tool_calls {
                let text = response.content.clone().unwrap_or_default();
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.add_message(
                        session_id,
                        Message::Assistant {
                            content: Some(text.clone()),
                            tool_calls: None,
                        },
                    )
                    .await?;

                    let completed_state = TaskState::Completed {
                        result: text.clone(),
                    };
                    sm.update_task_state(session_id, &task.id, completed_state.clone())
                        .await;
                    sm.persist(session_id).await?;
                }
                task.hook.notify_status_change(&TaskState::Completed {
                    result: text.clone(),
                });
                return Ok(text);
            }

            // Has tool calls → execute and continue to next round
            if let Some(calls) = &response.tool_calls {
                for call in calls {
                    let raw_result = self
                        .tool_manager
                        .execute(&call.name, call.args.clone())
                        .await?;
                    let processed_result =
                        task.hook.process_tool_result(&call.name, &raw_result)?;

                    {
                        let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                            self.session_manager.lock().await;
                        sm.add_message(
                            session_id,
                            Message::Assistant {
                                content: None,
                                tool_calls: Some(vec![call.clone()]),
                            },
                        )
                        .await?;
                        sm.add_message(
                            session_id,
                            Message::Tool {
                                content: processed_result,
                                tool_call_id: call.id.clone(),
                            },
                        )
                        .await?;
                    }
                }
                iterations += 1;
                let running_state = TaskState::Running {
                    current_iteration: iterations,
                };
                {
                    let mut sm: tokio::sync::MutexGuard<'_, SessionManager> =
                        self.session_manager.lock().await;
                    sm.update_task_state(session_id, &task.id, running_state.clone())
                        .await;
                }
                task.hook.notify_status_change(&running_state);
            }
        }
    }
}

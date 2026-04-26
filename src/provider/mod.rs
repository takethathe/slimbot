mod openai;

pub use openai::OpenAIProvider;

use anyhow::Result;
use async_trait::async_trait;

use crate::session::Message;
use crate::tool::ToolDefinition;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Error,
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FinishReason::Stop => write!(f, "stop"),
            FinishReason::ToolCalls => write!(f, "tool_calls"),
            FinishReason::Length => write!(f, "length"),
            FinishReason::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<crate::tool::ToolCall>>,
    pub finish_reason: FinishReason,
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<ChatResponse>;
}

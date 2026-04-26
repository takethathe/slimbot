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

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub prompt_cache_hit_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Usage(prompt={}, cache_hit={}, completion={}, total={})",
            self.prompt_tokens, self.prompt_cache_hit_tokens, self.completion_tokens, self.total_tokens
        )
    }
}

#[derive(Debug, Clone)]
pub struct LLMResponse {
    pub content: Option<String>,
    pub tool_calls: Option<Vec<crate::tool::ToolCall>>,
    pub finish_reason: FinishReason,
    pub usage: Usage,
}

impl std::fmt::Display for LLMResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let content_preview = self
            .content
            .as_deref()
            .unwrap_or("(empty)")
            .chars()
            .take(100)
            .collect::<String>();
        let tool_count = self
            .tool_calls
            .as_ref()
            .map(|calls| calls.len())
            .unwrap_or(0);

        write!(
            f,
            "LLMResponse {{ finish: {}, content: {:?}, tool_calls: {}, {} }}",
            self.finish_reason, content_preview, tool_count, self.usage
        )
    }
}

#[async_trait]
pub trait Provider: Send + Sync {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LLMResponse>;
}

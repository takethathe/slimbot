mod openai;

pub use openai::OpenAIProvider;

use anyhow::Result;
use async_trait::async_trait;

use crate::session::Message;
use crate::tool::ToolDefinition;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
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
            "Usage(prompt={}, cached_tokens={}, completion={}, total={})",
            self.prompt_tokens,
            self.prompt_cache_hit_tokens,
            self.completion_tokens,
            self.total_tokens
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
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LLMResponse>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_finish_reason_display() {
        assert_eq!(FinishReason::Stop.to_string(), "stop");
        assert_eq!(FinishReason::ToolCalls.to_string(), "tool_calls");
        assert_eq!(FinishReason::Length.to_string(), "length");
        assert_eq!(FinishReason::Error.to_string(), "error");
    }

    #[test]
    fn test_finish_reason_eq() {
        assert_eq!(FinishReason::Stop, FinishReason::Stop);
        assert_ne!(FinishReason::Stop, FinishReason::Error);
    }

    #[test]
    fn test_usage_display() {
        let usage = Usage {
            prompt_tokens: 100,
            prompt_cache_hit_tokens: 20,
            completion_tokens: 50,
            total_tokens: 150,
        };
        let display = usage.to_string();
        assert!(display.contains("100"));
        assert!(display.contains("20"));
        assert!(display.contains("50"));
        assert!(display.contains("150"));
    }

    #[test]
    fn test_usage_default() {
        let usage = Usage::default();
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.prompt_cache_hit_tokens, 0);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn test_llm_response_display() {
        let response = LLMResponse {
            content: Some("Hello, world!".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 10,
                prompt_cache_hit_tokens: 2,
                completion_tokens: 5,
                total_tokens: 15,
            },
        };
        let display = response.to_string();
        assert!(display.contains("stop"));
        assert!(display.contains("Hello, world!"));
    }

    #[test]
    fn test_llm_response_display_empty_content() {
        let response = LLMResponse {
            content: None,
            tool_calls: Some(vec![]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        };
        let display = response.to_string();
        assert!(display.contains("(empty)"));
        assert!(display.contains("tool_calls"));
    }

    #[test]
    fn test_llm_response_display_long_content() {
        let long_content = "x".repeat(200);
        let response = LLMResponse {
            content: Some(long_content.clone()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        };
        let display = response.to_string();
        // Should truncate to 100 chars
        assert!(display.len() < long_content.len() + 100);
    }
}

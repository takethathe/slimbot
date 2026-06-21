use anyhow::Result;
use async_trait::async_trait;

use crate::session::Message;
use crate::tool::ToolDefinition;

use super::{FinishReason, LLMResponse, Provider, Usage};

/// Mock provider for testing - returns the same predefined response on every call.
///
/// For tests that need different responses on successive calls, use the sequential
/// MockProvider in `runner::tests::MockProvider` instead.
pub struct MockProvider {
    response: LLMResponse,
}

impl MockProvider {
    pub fn new() -> Self {
        Self {
            response: LLMResponse {
                content: Some("Mock response".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage {
                    prompt_tokens: 10,
                    prompt_cache_hit_tokens: 5,
                    completion_tokens: 5,
                    total_tokens: 15,
                },
            },
        }
    }

    pub fn with_response(response: LLMResponse) -> Self {
        Self { response }
    }
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn chat(
        &self,
        _messages: &[&Message],
        _tools: Option<&[ToolDefinition]>,
    ) -> Result<LLMResponse> {
        Ok(self.response.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_provider_returns_response() {
        let provider = MockProvider::new();
        let messages: Vec<&Message> = vec![];
        let result = provider.chat(&messages, None).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.content, Some("Mock response".to_string()));
        assert_eq!(response.finish_reason, FinishReason::Stop);
    }

    #[tokio::test]
    async fn test_mock_provider_with_custom_response() {
        let custom_response = LLMResponse {
            content: Some("Custom".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        };
        let provider = MockProvider::with_response(custom_response.clone());
        let messages: Vec<&Message> = vec![];
        let result = provider.chat(&messages, None).await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert_eq!(response.content, Some("Custom".to_string()));
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    }
}

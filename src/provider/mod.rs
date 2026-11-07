mod error;
#[cfg(test)]
mod mock;
mod openai;

pub use error::{ProviderError, ProviderErrorKind};
pub use openai::OpenAIProvider;

#[cfg(test)]
pub use mock::MockProvider;

use anyhow::Result;
use async_trait::async_trait;

use crate::session::Message;
use crate::tool::ToolDefinition;
use crate::trace::SpanRecorder;

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

// `double_must_use` is an `async_trait` desugaring artifact on methods
// returning `Result`, not a real attribute on this code.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait Provider: Send + Sync {
    /// Call the LLM.
    ///
    /// `recorder`, when provided, receives attributes recorded during the
    /// call (e.g. the serialized request body) so that they land on the
    /// `provider.chat` span even when the transport fails.
    async fn chat(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
    ) -> Result<LLMResponse>;

    /// Streaming variant of [`chat`](Self::chat): signals are handed to
    /// `on_signal` as they arrive; the returned `LLMResponse` carries the
    /// accumulated whole.
    ///
    /// The default implementation degrades to non-streaming: it calls `chat()`
    /// and emits the entire content as a single delta, so providers without a
    /// streaming endpoint (and existing test doubles) work unchanged. Callers
    /// must not read the silence of a degraded provider as a stall: providers
    /// that cannot stream emit no signals until the call completes.
    async fn chat_stream(
        &self,
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
        recorder: Option<&dyn SpanRecorder>,
        on_signal: &mut (dyn FnMut(StreamSignal) + Send),
    ) -> Result<LLMResponse> {
        let resp = self.chat(messages, tools, recorder).await?;
        if let Some(content) = &resp.content
            && !content.is_empty()
        {
            on_signal(StreamSignal::Content(content.clone()));
        }
        Ok(resp)
    }
}

/// One observation delivered to a [`Provider::chat_stream`] consumer as the
/// call progresses. Splitting content from mere liveness lets a reasoning
/// model's non-content chunks (or any keep-alive) hold off a consumer's
/// no-progress timeout without feeding the consumer's content pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StreamSignal {
    /// A content increment, to be rendered or compiled.
    Content(String),
    /// Transport-level progress with nothing to show yet: the call is alive.
    Activity,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_chat_stream_default_degrades_to_one_delta() {
        // Seam 2: a provider that only implements `chat` must still satisfy
        // `chat_stream`, emitting the whole content as a single delta.
        let provider = MockProvider::new();
        let mut signals: Vec<StreamSignal> = Vec::new();
        let resp = provider
            .chat_stream(&[], None, None, &mut |s| signals.push(s))
            .await
            .unwrap();
        assert_eq!(
            signals,
            vec![StreamSignal::Content("Mock response".to_string())]
        );
        assert_eq!(resp.content, Some("Mock response".to_string()));
    }

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

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::session::Message;
use crate::tool::ToolDefinition;
use crate::debug;

use super::{LLMResponse, Usage, FinishReason};

pub struct OpenAIProvider {
    client: reqwest::Client,
    config: ProviderConfig,
    /// Resolved API URL — derived from api_url, base_url, or default at construction.
    api_url: String,
}

impl OpenAIProvider {
    pub fn new(config: &ProviderConfig) -> Self {
        let api_url = Self::resolve_api_url(config);
        Self {
            client: reqwest::Client::new(),
            config: config.clone(),
            api_url,
        }
    }

    fn resolve_api_url(config: &ProviderConfig) -> String {
        if !config.api_url.is_empty() {
            return config.api_url.clone();
        }
        if !config.base_url.is_empty() {
            let base = config.base_url.trim_end_matches('/');
            if base.ends_with("/chat/completions") {
                return base.to_string();
            }
            if base.ends_with("/v1") {
                return format!("{}/chat/completions", base);
            }
            return format!("{}/v1/chat/completions", base);
        }
        "https://api.openai.com/v1/chat/completions".to_string()
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct ApiUsage {
    prompt_tokens: Option<u32>,
    prompt_cache_hit_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiMessage,
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiMessage {
    role: Option<String>,
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCall>>,
}

fn deserialize_null_default<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.unwrap_or_default())
}

#[derive(Deserialize)]
struct ApiToolCall {
    #[serde(default, deserialize_with = "deserialize_null_default")]
    id: String,
    r#type: String,
    function: ApiFunction,
}

#[derive(Deserialize)]
struct ApiFunction {
    name: String,
    arguments: String,
}

#[async_trait]
impl crate::provider::Provider for OpenAIProvider {
    async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LLMResponse> {
        debug!("[OpenAIProvider] POST {} (model={}, messages={}, tools={})",
            self.api_url, self.config.model, messages.len(), tools.map(|t| t.len()).unwrap_or(0));

        // Find the index of the last system message for cache_control injection
        let last_system_idx = messages.iter().enumerate().rev().find_map(|(i, m)| {
            matches!(m, Message::System { .. }).then_some(i)
        });

        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let is_cache_target = self.config.prompt_cache_enabled && Some(i) == last_system_idx;
                match m {
                    Message::System { content, .. } => {
                        let mut obj = serde_json::json!({"role": "system", "content": content});
                        if is_cache_target {
                            obj["content"] = serde_json::json!([
                                {"type": "text", "text": content, "cache_control": {"type": "ephemeral"}}
                            ]);
                        }
                        obj
                    }
                    Message::User { content, .. } => {
                        serde_json::json!({"role": "user", "content": content.to_openai_value()})
                    }
                    Message::Assistant {
                        content,
                        tool_calls,
                        reasoning_content,
                        thinking_blocks,
                        ..
                    } => {
                        let mut obj = serde_json::json!({"role": "assistant"});
                        if let Some(c) = content {
                            obj["content"] = serde_json::json!(c);
                        } else {
                            obj["content"] = serde_json::Value::Null;
                        }
                        if let Some(rc) = reasoning_content {
                            obj["reasoning_content"] = serde_json::json!(rc);
                        }
                        if let Some(tb) = thinking_blocks {
                            obj["thinking"] = serde_json::json!(tb);
                        }
                        if let Some(calls) = tool_calls {
                            let tc: Vec<_> = calls
                                .iter()
                                .map(|call| {
                                    serde_json::json!({
                                        "id": call.id,
                                        "type": "function",
                                        "function": {
                                            "name": call.name,
                                            "arguments": call.args.to_string(),
                                        }
                                    })
                                })
                                .collect();
                            obj["tool_calls"] = serde_json::json!(tc);
                        }
                        obj
                    }
                    Message::Tool {
                        content,
                        tool_call_id,
                        name,
                        ..
                    } => {
                        let mut obj = serde_json::json!({
                            "role": "tool",
                            "content": content,
                            "tool_call_id": tool_call_id,
                        });
                        if let Some(n) = name {
                            obj["name"] = serde_json::json!(n);
                        }
                        obj
                    }
                }
            })
            .collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": api_messages,
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
        });

        if let Some(tools) = tools {
            let tool_defs: Vec<serde_json::Value> = tools
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = serde_json::json!(tool_defs);
        }

        let resp = self
            .client
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await?;
            anyhow::bail!("API request failed: {} - {}", status, body);
        }

        let api_resp: ApiResponse = resp.json().await.context("Failed to parse API response")?;

        let choice = api_resp
            .choices
            .first()
            .context("API response has no result")?;

        let finish_reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };

        let tool_calls: Option<Vec<crate::tool::ToolCall>> = choice.message.tool_calls.as_ref().map(|calls| {
            calls
                .iter()
                .map(|call| {
                    let id = if call.id.is_empty() {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        call.id.clone()
                    };
                    crate::tool::ToolCall {
                        id,
                        name: call.function.name.clone(),
                        args: serde_json::from_str(&call.function.arguments)
                            .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                    }
                })
                .collect()
        });

        let usage = api_resp.usage.as_ref().map(|u| Usage {
            prompt_tokens: u.prompt_tokens.unwrap_or(0),
            prompt_cache_hit_tokens: u.prompt_cache_hit_tokens.unwrap_or(0),
            completion_tokens: u.completion_tokens.unwrap_or(0),
            total_tokens: u.total_tokens.unwrap_or(0),
        }).unwrap_or_default();

        let tool_call_count = match &tool_calls {
            Some(calls) => calls.len(),
            None => 0,
        };
        debug!(
            "[OpenAIProvider] Response: finish_reason={:?}, content_len={}, tool_calls={}, prompt_tokens={}, prompt_cache_hit={}, completion_tokens={}, total_tokens={}",
            finish_reason,
            choice.message.content.as_ref().map(|s| s.len()).unwrap_or(0),
            tool_call_count,
            usage.prompt_tokens,
            usage.prompt_cache_hit_tokens,
            usage.completion_tokens,
            usage.total_tokens,
        );

        Ok(LLMResponse {
            content: choice.message.content.clone(),
            tool_calls,
            finish_reason,
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Content;

    #[test]
    fn test_content_serialization_in_provider() {
        let content = Content::Plain("hello".to_string());
        let val = content.to_openai_value();
        assert_eq!(val, serde_json::json!("hello"));
    }

    fn make_config(api_url: &str, base_url: &str) -> ProviderConfig {
        ProviderConfig {
            r#type: "openai".to_string(),
            api_url: api_url.to_string(),
            base_url: base_url.to_string(),
            api_key: "test-key".to_string(),
            model: "gpt-4".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            prompt_cache_enabled: true,
            unknown: Default::default(),
        }
    }

    #[test]
    fn test_resolve_api_url_full_endpoint() {
        let provider = OpenAIProvider::new(&make_config("https://custom.url/full/path", "https://other.url"));
        assert_eq!(provider.api_url, "https://custom.url/full/path");
    }

    #[test]
    fn test_resolve_api_url_base_ends_with_v1() {
        let provider = OpenAIProvider::new(&make_config(
            "", "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ));
        assert_eq!(provider.api_url, "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions");
    }

    #[test]
    fn test_resolve_api_url_base_ends_with_v1_trailing_slash() {
        let provider = OpenAIProvider::new(&make_config(
            "", "https://dashscope.aliyuncs.com/compatible-mode/v1/",
        ));
        assert_eq!(provider.api_url, "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions");
    }

    #[test]
    fn test_resolve_api_url_base_already_has_chat_completions() {
        let provider = OpenAIProvider::new(&make_config(
            "", "https://example.com/v1/chat/completions",
        ));
        assert_eq!(provider.api_url, "https://example.com/v1/chat/completions");
    }

    #[test]
    fn test_resolve_api_url_generic_base() {
        let provider = OpenAIProvider::new(&make_config("", "https://example.com"));
        assert_eq!(provider.api_url, "https://example.com/v1/chat/completions");
    }

    #[test]
    fn test_resolve_api_url_no_base() {
        let provider = OpenAIProvider::new(&make_config("", ""));
        assert_eq!(provider.api_url, "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn test_tool_call_with_id() {
        let json = r#"{
            "id": "call-abc123",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\":\"test.md\"}"}
        }"#;
        let call: ApiToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(call.id, "call-abc123");
        assert_eq!(call.function.name, "read_file");
    }

    #[test]
    fn test_tool_call_without_id_generates_empty() {
        let json = r#"{
            "type": "function",
            "function": {"name": "write_file", "arguments": "{}"}
        }"#;
        let call: ApiToolCall = serde_json::from_str(json).unwrap();
        assert!(call.id.is_empty());
    }

    #[test]
    fn test_tool_call_with_null_id() {
        let json = r#"{
            "id": null,
            "type": "function",
            "function": {"name": "shell", "arguments": "{}"}
        }"#;
        let call: ApiToolCall = serde_json::from_str(json).unwrap();
        assert!(call.id.is_empty());
    }

    #[test]
    fn test_full_response_with_missing_tool_call_ids() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "Running tools",
                    "tool_calls": [
                        {
                            "type": "function",
                            "function": {"name": "tool_a", "arguments": "{}"}
                        },
                        {
                            "type": "function",
                            "function": {"name": "tool_b", "arguments": "{\"key\":\"val\"}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = resp.choices.first().unwrap();
        let calls = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].id.is_empty());
        assert!(calls[1].id.is_empty());

        // Verify the mapping generates UUIDs
        let mapped: Vec<crate::tool::ToolCall> = calls
            .iter()
            .map(|call| {
                let id = if call.id.is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    call.id.clone()
                };
                crate::tool::ToolCall {
                    id,
                    name: call.function.name.clone(),
                    args: serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                }
            })
            .collect();

        assert_eq!(mapped.len(), 2);
        // Generated IDs should be non-empty UUIDs
        assert!(!mapped[0].id.is_empty());
        assert!(!mapped[1].id.is_empty());
        // Should look like a UUID (36 chars with dashes)
        assert_eq!(mapped[0].id.len(), 36);
        assert_eq!(mapped[1].id.len(), 36);
        assert_eq!(mapped[0].name, "tool_a");
        assert_eq!(mapped[1].name, "tool_b");
    }

    #[test]
    fn test_full_response_with_existing_ids() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call-xyz",
                            "type": "function",
                            "function": {"name": "my_tool", "arguments": "{}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = resp.choices.first().unwrap();
        let calls = choice.message.tool_calls.as_ref().unwrap();

        let mapped: Vec<crate::tool::ToolCall> = calls
            .iter()
            .map(|call| {
                let id = if call.id.is_empty() {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    call.id.clone()
                };
                crate::tool::ToolCall {
                    id,
                    name: call.function.name.clone(),
                    args: serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                }
            })
            .collect();

        assert_eq!(mapped[0].id, "call-xyz");
    }
}

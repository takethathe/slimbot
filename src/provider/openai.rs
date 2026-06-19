use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::debug;
use crate::session::Message;
use crate::tool::ToolDefinition;

use super::{FinishReason, LLMResponse, Usage};

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
    completion_tokens: Option<u32>,
    total_tokens: Option<u32>,
    input_tokens_details: Option<ApiInputTokensDetails>,
}

#[derive(Deserialize)]
struct ApiInputTokensDetails {
    cached_tokens: Option<u32>,
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
        messages: &[&Message],
        tools: Option<&[ToolDefinition]>,
    ) -> Result<LLMResponse> {
        debug!(
            "[OpenAIProvider] POST {} (model={}, messages={}, tools={})",
            self.api_url,
            self.config.model,
            messages.len(),
            tools.map(|t| t.len()).unwrap_or(0)
        );

        // Find indices of the last system and user messages for cache_control injection.
        let last_system_idx = messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, m)| matches!(**m, Message::System { .. }).then_some(i));
        let last_user_idx = messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(i, m)| matches!(**m, Message::User { .. }).then_some(i));

        let api_messages: Vec<serde_json::Value> = messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let is_cache_target = self.config.prompt_cache_enabled
                    && (Some(i) == last_system_idx || Some(i) == last_user_idx);
                match **m {
                    Message::System { ref content, .. } => {
                        let content_blocks = serde_json::json!([
                            {"type": "text", "text": content}
                        ]);
                        let mut obj =
                            serde_json::json!({"role": "system", "content": content_blocks});
                        if is_cache_target {
                            obj["content"][0]["cache_control"] =
                                serde_json::json!({"type": "ephemeral"});
                        }
                        obj
                    }
                    Message::User {
                        ref content,
                        ref runtime_content,
                        ..
                    } => {
                        let mut content_blocks = content.to_openai_blocks();
                        if let Some(ctx) = runtime_content {
                            content_blocks
                                .insert(0, serde_json::json!({"type": "text", "text": ctx}));
                        }
                        if is_cache_target {
                            let last = content_blocks.len().saturating_sub(1);
                            content_blocks[last]["cache_control"] =
                                serde_json::json!({"type": "ephemeral"});
                        }
                        serde_json::json!({"role": "user", "content": content_blocks})
                    }
                    Message::Assistant {
                        ref content,
                        ref tool_calls,
                        ref reasoning_content,
                        ref thinking_blocks,
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
                        ref content,
                        ref tool_call_id,
                        ref name,
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

        let tool_calls: Option<Vec<crate::tool::ToolCall>> =
            choice.message.tool_calls.as_ref().map(|calls| {
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

        let usage = api_resp
            .usage
            .as_ref()
            .map(|u| Usage {
                prompt_tokens: u.prompt_tokens.unwrap_or(0),
                prompt_cache_hit_tokens: u
                    .input_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens)
                    .unwrap_or(0),
                completion_tokens: u.completion_tokens.unwrap_or(0),
                total_tokens: u.total_tokens.unwrap_or(0),
            })
            .unwrap_or_default();

        let tool_call_count = match &tool_calls {
            Some(calls) => calls.len(),
            None => 0,
        };
        debug!(
            "[OpenAIProvider] Response: finish_reason={:?}, content_len={}, tool_calls={}, prompt_tokens={}, cached_tokens={}, completion_tokens={}, total_tokens={}",
            finish_reason,
            choice
                .message
                .content
                .as_ref()
                .map(|s| s.len())
                .unwrap_or(0),
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
        let provider = OpenAIProvider::new(&make_config(
            "https://custom.url/full/path",
            "https://other.url",
        ));
        assert_eq!(provider.api_url, "https://custom.url/full/path");
    }

    #[test]
    fn test_resolve_api_url_base_ends_with_v1() {
        let provider = OpenAIProvider::new(&make_config(
            "",
            "https://dashscope.aliyuncs.com/compatible-mode/v1",
        ));
        assert_eq!(
            provider.api_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }

    #[test]
    fn test_resolve_api_url_base_ends_with_v1_trailing_slash() {
        let provider = OpenAIProvider::new(&make_config(
            "",
            "https://dashscope.aliyuncs.com/compatible-mode/v1/",
        ));
        assert_eq!(
            provider.api_url,
            "https://dashscope.aliyuncs.com/compatible-mode/v1/chat/completions"
        );
    }

    #[test]
    fn test_resolve_api_url_base_already_has_chat_completions() {
        let provider =
            OpenAIProvider::new(&make_config("", "https://example.com/v1/chat/completions"));
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
        assert_eq!(
            provider.api_url,
            "https://api.openai.com/v1/chat/completions"
        );
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

    #[test]
    fn test_finish_reason_stop() {
        let json = r#"{
            "choices": [{
                "message": { "content": "done", "tool_calls": null },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::Stop);
    }

    #[test]
    fn test_finish_reason_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": { "content": null, "tool_calls": [] },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::ToolCalls);
    }

    #[test]
    fn test_finish_reason_length() {
        let json = r#"{
            "choices": [{
                "message": { "content": "cut off", "tool_calls": null },
                "finish_reason": "length"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::Length);
    }

    #[test]
    fn test_finish_reason_error() {
        let json = r#"{
            "choices": [{
                "message": { "content": "error occurred", "tool_calls": null },
                "finish_reason": "error"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::Error);
    }

    #[test]
    fn test_finish_reason_null() {
        let json = r#"{
            "choices": [{
                "message": { "content": "no finish", "tool_calls": null },
                "finish_reason": null
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::Error);
    }

    #[test]
    fn test_finish_reason_unknown() {
        let json = r#"{
            "choices": [{
                "message": { "content": "weird", "tool_calls": null },
                "finish_reason": "content_filter"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let choice = &resp.choices[0];
        let reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };
        assert_eq!(reason, FinishReason::Error);
    }

    #[test]
    fn test_usage_parsing_with_null_fields() {
        let json = r#"{
            "choices": [{
                "message": { "content": "hi", "tool_calls": null },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": null,
                "total_tokens": null
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_some());
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert!(usage.completion_tokens.is_none());
        assert!(usage.total_tokens.is_none());
    }

    #[test]
    fn test_usage_parsing_with_cached_tokens() {
        let json = r#"{
            "choices": [{
                "message": { "content": "hi", "tool_calls": null },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "input_tokens_details": {
                    "cached_tokens": 50
                }
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(
            usage.input_tokens_details.as_ref().unwrap().cached_tokens,
            Some(50)
        );
    }

    #[test]
    fn test_usage_parsing_missing_usage() {
        let json = r#"{
            "choices": [{
                "message": { "content": "hi", "tool_calls": null },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none());
    }

    #[test]
    fn test_api_response_parsing_with_tool_calls_and_content() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "Let me help you with that",
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\": \"test.txt\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let choice = &resp.choices[0];
        assert_eq!(choice.finish_reason, Some("tool_calls".to_string()));
        assert_eq!(
            choice.message.content,
            Some("Let me help you with that".to_string())
        );
        let tool_calls = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].id, "call-1");
        assert_eq!(tool_calls[0].function.name, "read_file");
        assert!(resp.usage.is_some());
    }

    #[test]
    fn test_api_response_multiple_choices() {
        let json = r#"{
            "choices": [
                {
                    "message": { "content": "first", "tool_calls": null },
                    "finish_reason": "stop"
                },
                {
                    "message": { "content": "second", "tool_calls": null },
                    "finish_reason": "length"
                }
            ]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 2);
        assert_eq!(resp.choices[0].message.content, Some("first".to_string()));
        assert_eq!(resp.choices[1].message.content, Some("second".to_string()));
    }

    #[test]
    fn test_api_response_empty_content() {
        let json = r#"{
            "choices": [{
                "message": { "content": "", "tool_calls": null },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, Some("".to_string()));
    }

    #[test]
    fn test_api_response_null_content() {
        let json = r#"{
            "choices": [{
                "message": { "content": null, "tool_calls": null },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, None);
    }

    #[test]
    fn test_api_tool_call_with_invalid_json_arguments() {
        let json = r#"{
            "id": "call-1",
            "type": "function",
            "function": {
                "name": "test_tool",
                "arguments": "not valid json"
            }
        }"#;
        let call: ApiToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(call.function.name, "test_tool");
        assert_eq!(call.function.arguments, "not valid json");
    }

    #[test]
    fn test_api_tool_call_with_empty_arguments() {
        let json = r#"{
            "id": "call-1",
            "type": "function",
            "function": {
                "name": "test_tool",
                "arguments": ""
            }
        }"#;
        let call: ApiToolCall = serde_json::from_str(json).unwrap();
        assert_eq!(call.function.arguments, "");
    }

    #[test]
    fn test_api_message_serialization() {
        // Test that messages are correctly serialized for the API
        let user_msg = Message::user("Hello".to_string());
        let assistant_msg = Message::assistant(Some("Hi there".to_string()), None, None, None);
        let tool_msg = Message::tool(
            "Tool result".to_string(),
            "call-1".to_string(),
            Some("read_file".to_string()),
        );

        // Verify messages can be created
        assert!(matches!(user_msg, Message::User { .. }));
        assert!(matches!(assistant_msg, Message::Assistant { .. }));
        assert!(matches!(tool_msg, Message::Tool { .. }));
    }

    #[test]
    fn test_openai_provider_creation() {
        let config = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "https://api.openai.com/v1/chat/completions".to_string(),
            base_url: "".to_string(),
            api_key: "test-key".to_string(),
            model: "gpt-4o".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            prompt_cache_enabled: false,
            unknown: Default::default(),
        };

        let provider = OpenAIProvider::new(&config);
        assert_eq!(
            provider.api_url,
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_api_response_parsing_multiple_tool_calls() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call-1",
                            "type": "function",
                            "function": {"name": "tool1", "arguments": "{}"}
                        },
                        {
                            "id": "call-2",
                            "type": "function",
                            "function": {"name": "tool2", "arguments": "{\"arg\": \"value\"}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        let tool_calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].id, "call-1");
        assert_eq!(tool_calls[1].id, "call-2");
    }

    #[test]
    fn test_api_response_parsing_with_usage_details() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 20,
                "total_tokens": 120,
                "prompt_tokens_details": {
                    "cached_tokens": 50
                }
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.as_ref().unwrap();
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(120));
    }

    #[test]
    fn test_api_response_empty_choices() {
        let json = r#"{
            "choices": []
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_empty());
    }

    #[test]
    fn test_openai_provider_config_storage() {
        let config = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "".to_string(),
            base_url: "https://custom.api.com/v1".to_string(),
            api_key: "custom-key".to_string(),
            model: "custom-model".to_string(),
            temperature: 0.5,
            max_tokens: 2048,
            prompt_cache_enabled: true,
            unknown: Default::default(),
        };

        let provider = OpenAIProvider::new(&config);
        assert_eq!(provider.config.model, "custom-model");
        assert_eq!(provider.config.temperature, 0.5);
        assert_eq!(provider.config.max_tokens, 2048);
        assert!(provider.config.prompt_cache_enabled);
    }

    #[test]
    fn test_api_message_with_reasoning_content() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "answer",
                    "reasoning_content": "I need to think about this...",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, Some("answer".to_string()));
    }

    #[test]
    fn test_api_response_with_all_usage_fields() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "prompt_tokens_details": {
                    "cached_tokens": 30
                },
                "completion_tokens_details": {
                    "reasoning_tokens": 10
                }
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(50));
        assert_eq!(usage.total_tokens, Some(150));
    }

    #[test]
    fn test_api_tool_call_serialization_roundtrip() {
        let call = crate::tool::ToolCall {
            id: "call-123".to_string(),
            name: "read_file".to_string(),
            args: serde_json::json!({"path": "test.txt"}),
        };
        let json = serde_json::to_string(&call).unwrap();
        assert!(json.contains("call-123"));
        assert!(json.contains("read_file"));
        assert!(json.contains("test.txt"));

        let deserialized: crate::tool::ToolCall = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, "call-123");
        assert_eq!(deserialized.name, "read_file");
    }

    #[test]
    fn test_finish_reason_display() {
        assert_eq!(FinishReason::Stop.to_string(), "stop");
        assert_eq!(FinishReason::ToolCalls.to_string(), "tool_calls");
        assert_eq!(FinishReason::Length.to_string(), "length");
        assert_eq!(FinishReason::Error.to_string(), "error");
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
    fn test_llm_response_display() {
        let response = LLMResponse {
            content: Some("Hello".to_string()),
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
        assert!(display.contains("Hello"));
        assert!(display.contains("stop"));
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
    fn test_api_response_parsing_with_thinking_blocks() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "answer",
                    "thinking_blocks": [
                        {"type": "thinking", "thinking": "Let me think..."}
                    ],
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices[0].message.content, Some("answer".to_string()));
    }

    #[test]
    fn test_api_response_tool_call_with_complex_args() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call-complex",
                        "type": "function",
                        "function": {
                            "name": "write_file",
                            "arguments": "{\"path\": \"test.txt\", \"content\": \"hello\\nworld\", \"append\": true}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let tool_calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].function.name, "write_file");
        let args: serde_json::Value =
            serde_json::from_str(&tool_calls[0].function.arguments).unwrap();
        assert_eq!(args["path"], "test.txt");
        assert_eq!(args["content"], "hello\nworld");
        assert_eq!(args["append"], true);
    }

    #[test]
    fn test_api_response_with_system_fingerprint() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "system_fingerprint": "fp_44709d6fcb"
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        // system_fingerprint is ignored but should not cause parsing error
        assert_eq!(resp.choices.len(), 1);
    }

    #[test]
    fn test_openai_provider_with_different_models() {
        for model in &["gpt-4o", "gpt-4-turbo", "gpt-3.5-turbo", "custom-model"] {
            let config = crate::config::ProviderConfig {
                r#type: "openai".to_string(),
                api_url: "".to_string(),
                base_url: "https://api.openai.com".to_string(),
                api_key: "test-key".to_string(),
                model: model.to_string(),
                temperature: 0.7,
                max_tokens: 4096,
                prompt_cache_enabled: false,
                unknown: Default::default(),
            };
            let provider = OpenAIProvider::new(&config);
            assert_eq!(provider.config.model, *model);
        }
    }

    #[test]
    fn test_api_response_with_index_field() {
        let json = r#"{
            "choices": [{
                "index": 0,
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
    }

    #[test]
    fn test_api_response_with_logprobs_field() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop",
                "logprobs": null
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
    }

    #[test]
    fn test_usage_with_zero_tokens() {
        let usage = Usage {
            prompt_tokens: 0,
            prompt_cache_hit_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        };
        assert_eq!(usage.prompt_tokens, 0);
        assert_eq!(usage.total_tokens, 0);
        let display = usage.to_string();
        assert!(display.contains("0"));
    }

    #[test]
    fn test_finish_reason_equality() {
        assert_eq!(FinishReason::Stop, FinishReason::Stop);
        assert_eq!(FinishReason::ToolCalls, FinishReason::ToolCalls);
        assert_eq!(FinishReason::Length, FinishReason::Length);
        assert_eq!(FinishReason::Error, FinishReason::Error);
        assert_ne!(FinishReason::Stop, FinishReason::ToolCalls);
        assert_ne!(FinishReason::Stop, FinishReason::Length);
        assert_ne!(FinishReason::Stop, FinishReason::Error);
    }

    #[test]
    fn test_api_response_with_custom_fields() {
        let json = r#"{
            "id": "chatcmpl-123",
            "object": "chat.completion",
            "created": 1677858242,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!",
                    "tool_calls": null
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 9,
                "completion_tokens": 12,
                "total_tokens": 21
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(resp.choices[0].message.content, Some("Hello!".to_string()));
    }

    #[test]
    fn test_api_response_with_multiple_tool_calls_different_ids() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {"name": "read_file", "arguments": "{\"path\": \"test.txt\"}"}
                        },
                        {
                            "id": "call_def456",
                            "type": "function",
                            "function": {"name": "write_file", "arguments": "{\"path\": \"out.txt\"}"}
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let tool_calls = resp.choices[0].message.tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 2);
        assert_eq!(tool_calls[0].id, "call_abc123");
        assert_eq!(tool_calls[1].id, "call_def456");
    }

    #[test]
    fn test_api_response_tool_call_with_empty_id_generates_uuid() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "",
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let call = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert!(call.id.is_empty());
        // When converted to ToolCall, empty ID should become a UUID
        let tool_call: crate::tool::ToolCall = crate::tool::ToolCall {
            id: if call.id.is_empty() {
                uuid::Uuid::new_v4().to_string()
            } else {
                call.id.clone()
            },
            name: call.function.name.clone(),
            args: serde_json::from_str(&call.function.arguments)
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
        };
        assert!(!tool_call.id.is_empty());
        assert_eq!(tool_call.id.len(), 36); // UUID format
    }

    #[test]
    fn test_api_response_tool_call_with_null_id_generates_uuid() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": null,
                        "type": "function",
                        "function": {"name": "shell", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let call = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert!(call.id.is_empty());
    }

    #[test]
    fn test_openai_provider_with_custom_api_url() {
        let config = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "https://custom-api.example.com/v1/chat/completions".to_string(),
            base_url: "".to_string(),
            api_key: "sk-custom".to_string(),
            model: "custom-model".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            prompt_cache_enabled: false,
            unknown: Default::default(),
        };

        let provider = OpenAIProvider::new(&config);
        assert_eq!(
            provider.api_url,
            "https://custom-api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn test_usage_debug_format() {
        let usage = Usage {
            prompt_tokens: 100,
            prompt_cache_hit_tokens: 20,
            completion_tokens: 50,
            total_tokens: 150,
        };
        let debug_str = format!("{:?}", usage);
        assert!(debug_str.contains("100"));
        assert!(debug_str.contains("20"));
        assert!(debug_str.contains("50"));
        assert!(debug_str.contains("150"));
    }

    #[test]
    fn test_llm_response_debug_format() {
        let response = LLMResponse {
            content: Some("test content".to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage {
                prompt_tokens: 10,
                prompt_cache_hit_tokens: 2,
                completion_tokens: 5,
                total_tokens: 15,
            },
        };
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("test content"));
        assert!(debug_str.contains("Stop"));
    }

    #[test]
    fn test_api_response_with_partial_usage() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "total_tokens": 10
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert!(usage.completion_tokens.is_none());
        assert_eq!(usage.total_tokens, Some(10));
    }

    #[test]
    fn test_api_response_with_usage_details() {
        let json = r#"{
            "choices": [{
                "message": {"content": "hi", "tool_calls": null},
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "total_tokens": 150,
                "input_tokens_details": {
                    "cached_tokens": 40
                }
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(100));
        assert_eq!(usage.completion_tokens, Some(50));
        assert_eq!(usage.total_tokens, Some(150));
        assert_eq!(
            usage.input_tokens_details.as_ref().unwrap().cached_tokens,
            Some(40)
        );
    }
}

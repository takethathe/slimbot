use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::ProviderConfig;
use crate::session::Message;
use crate::tool::ToolDefinition;

use super::{ChatResponse, FinishReason};

pub struct OpenAIProvider {
    client: reqwest::Client,
    config: ProviderConfig,
}

impl OpenAIProvider {
    pub fn new(config: &ProviderConfig) -> Self {
        Self {
            client: reqwest::Client::new(),
            config: config.clone(),
        }
    }
}

#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
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

#[derive(Deserialize)]
struct ApiToolCall {
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
    ) -> Result<ChatResponse> {
        let api_messages: Vec<serde_json::Value> = messages.iter().map(|m| {
            match m {
                Message::System { content } => {
                    serde_json::json!({"role": "system", "content": content})
                }
                Message::User { content } => {
                    serde_json::json!({"role": "user", "content": content})
                }
                Message::Assistant { content, tool_calls } => {
                    let mut obj = serde_json::json!({"role": "assistant"});
                    if let Some(c) = content {
                        obj["content"] = serde_json::json!(c);
                    } else {
                        obj["content"] = serde_json::Value::Null;
                    }
                    if let Some(calls) = tool_calls {
                        let tc: Vec<_> = calls.iter().map(|call| {
                            serde_json::json!({
                                "id": call.id,
                                "type": "function",
                                "function": {
                                    "name": call.name,
                                    "arguments": call.args.to_string(),
                                }
                            })
                        }).collect();
                        obj["tool_calls"] = serde_json::json!(tc);
                    }
                    obj
                }
                Message::Tool { content, tool_call_id } => {
                    serde_json::json!({
                        "role": "tool",
                        "content": content,
                        "tool_call_id": tool_call_id,
                    })
                }
            }
        }).collect();

        let mut body = serde_json::json!({
            "model": self.config.model,
            "messages": api_messages,
            "temperature": self.config.temperature,
            "max_tokens": self.config.max_tokens,
        });

        if let Some(tools) = tools {
            let tool_defs: Vec<serde_json::Value> = tools.iter().map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            }).collect();
            body["tools"] = serde_json::json!(tool_defs);
        }

        let resp = self.client.post(&self.config.api_url)
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

        let api_resp: ApiResponse = resp.json().await
            .context("Failed to parse API response")?;

        let choice = api_resp.choices.first()
            .context("API response has no result")?;

        let finish_reason = match choice.finish_reason.as_deref() {
            Some("stop") => FinishReason::Stop,
            Some("tool_calls") => FinishReason::ToolCalls,
            Some("length") => FinishReason::Length,
            Some("error") | None => FinishReason::Error,
            _ => FinishReason::Error,
        };

        let tool_calls = choice.message.tool_calls.as_ref().map(|calls| {
            calls.iter().map(|call| {
                crate::tool::ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    args: serde_json::from_str(&call.function.arguments)
                        .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                }
            }).collect()
        });

        Ok(ChatResponse {
            content: choice.message.content.clone(),
            tool_calls,
            finish_reason,
        })
    }
}

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

use crate::tool::Tool;

/// Send callback result: `Ok(())` means the message was delivered, `Err(e)`
/// means delivery failed (the turn is only marked as sent on success).
type SendCallback = Arc<
    dyn Fn(
            String,
            String,
            String,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>
        + Send
        + Sync,
>;

pub struct MessageTool {
    send_callback: Option<SendCallback>,
}

impl MessageTool {
    pub fn new() -> Self {
        Self {
            send_callback: None,
        }
    }

    pub fn set_send_callback(&mut self, cb: SendCallback) {
        self.send_callback = Some(cb);
    }
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send a message to the user. This is the primary way to deliver results to a channel."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "The message content to send" },
                "channel": { "type": "string", "description": "Optional: target channel (defaults to originating channel)" },
                "chat_id": { "type": "string", "description": "Optional: target chat_id (defaults to originating chat)" }
            },
            "required": ["content"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        // Check the callback first so no turn-context work is done when
        // sending is not configured.
        let cb = match &self.send_callback {
            Some(cb) => cb,
            None => return Ok("Error: Message sending not configured".to_string()),
        };

        let (def_channel, def_chat_id) = crate::tool::current_turn_target();
        let channel = match args.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => def_channel,
        };
        let chat_id = match args.get("chat_id").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => def_chat_id,
        };

        if channel.is_empty() || chat_id.is_empty() {
            return Ok("Error: No target channel/chat specified".to_string());
        }

        match cb(channel.clone(), chat_id.clone(), content.clone()).await {
            Ok(()) => {
                // Only mark as sent when targeting the default (origin) context.
                // Cross-channel sends should not suppress the final response.
                if let Some(t) = crate::tool::current_turn() {
                    t.mark_sent_if_target(&channel, &chat_id);
                }
                Ok(format!("Message sent to {}:{}", channel, chat_id))
            }
            Err(e) => Ok(format!("Error: Message delivery failed: {}", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::test_util::{turn_ctx, with_turn};
    use crate::tool::with_turn_context;
    use std::sync::Arc;

    /// Channel pair used by the message-send callback tests.
    type SendChannel = (
        tokio::sync::mpsc::Sender<(String, String, String)>,
        tokio::sync::mpsc::Receiver<(String, String, String)>,
    );

    fn make_callback() -> SendChannel {
        tokio::sync::mpsc::channel::<(String, String, String)>(1)
    }

    fn send_cb(tx: tokio::sync::mpsc::Sender<(String, String, String)>) -> SendCallback {
        Arc::new(move |ch, cid, content| {
            let tx = tx.clone();
            Box::pin(async move { tx.send((ch, cid, content)).await.map_err(|e| e.to_string()) })
        })
    }

    #[tokio::test]
    async fn test_message_tool_defaults_context() {
        let mut tool = MessageTool::new();
        let (tx, mut rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        let c = turn_ctx("webui", "chat-1");
        let result = with_turn_context(c.clone(), async {
            tool.execute(serde_json::json!({ "content": "hello" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("webui:chat-1"));
        assert!(c.sent_in_turn(), "sent to origin must mark the turn");

        let (ch, cid, content) = rx.recv().await.unwrap();
        assert_eq!(ch, "webui");
        assert_eq!(cid, "chat-1");
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn test_message_tool_explicit_target() {
        let mut tool = MessageTool::new();
        let (tx, mut rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        let c = turn_ctx("webui", "chat-1");
        let result = with_turn_context(c.clone(), async {
            tool.execute(serde_json::json!({
                "content": "cross channel",
                "channel": "cli",
                "chat_id": "other"
            }))
            .await
            .unwrap()
        })
        .await;
        assert!(result.contains("cli:other"));
        assert!(
            !c.sent_in_turn(),
            "cross-channel send must not mark the turn"
        );

        let (ch, cid, _content) = rx.recv().await.unwrap();
        assert_eq!(ch, "cli");
        assert_eq!(cid, "other");
    }

    #[tokio::test]
    async fn test_message_tool_no_content() {
        let mut tool = MessageTool::new();
        let (tx, _rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        let result = with_turn("cli", "chat-1", async {
            tool.execute(serde_json::json!({})).await.unwrap()
        })
        .await;
        // Empty content defaults to "" but still sends to the default channel.
        assert!(result.contains("cli:chat-1"));
    }

    #[tokio::test]
    async fn test_message_tool_no_callback() {
        let tool = MessageTool::new();

        let result = with_turn("webui", "chat-1", async {
            tool.execute(serde_json::json!({ "content": "hello" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("not configured"));
    }

    #[tokio::test]
    async fn test_message_tool_no_context() {
        let mut tool = MessageTool::new();
        let (tx, _rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        // No with_turn_context scope -> current_turn() returns None.
        let result = tool
            .execute(serde_json::json!({ "content": "test" }))
            .await
            .unwrap();
        assert!(result.contains("No target channel"));
    }

    #[tokio::test]
    async fn test_message_tool_empty_turn_defaults_error() {
        let mut tool = MessageTool::new();
        let (tx, _rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        // Empty-origin turn (colon-less session id): default send must error
        // cleanly instead of leaking context from another run.
        let result = with_turn("", "", async {
            tool.execute(serde_json::json!({ "content": "hi" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("No target channel"));
    }

    #[tokio::test]
    async fn test_message_tool_empty_turn_explicit_target_not_marked() {
        let mut tool = MessageTool::new();
        let (tx, mut rx) = make_callback();
        tool.set_send_callback(send_cb(tx));

        // Empty-origin turn with an explicit target: the message is delivered,
        // but the turn must not be marked (origin is empty), so the caller
        // still emits the final response.
        let c = turn_ctx("", "");
        let result = with_turn_context(c.clone(), async {
            tool.execute(serde_json::json!({
                "content": "hi",
                "channel": "webui",
                "chat_id": "chat-1"
            }))
            .await
            .unwrap()
        })
        .await;
        assert!(result.contains("webui:chat-1"));
        assert!(!c.sent_in_turn(), "empty origin must not mark the turn");

        let (ch, cid, _content) = rx.recv().await.unwrap();
        assert_eq!(ch, "webui");
        assert_eq!(cid, "chat-1");
    }

    #[tokio::test]
    async fn test_message_tool_delivery_failure_not_marked() {
        let mut tool = MessageTool::new();
        tool.set_send_callback(Arc::new(move |_ch, _cid, _content| {
            Box::pin(async move { Err("outbound receiver dropped".to_string()) })
        }));

        let c = turn_ctx("webui", "chat-1");
        let result = with_turn_context(c.clone(), async {
            tool.execute(serde_json::json!({ "content": "hi" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("delivery failed"));
        assert!(!c.sent_in_turn(), "failed delivery must not mark the turn");
    }

    #[test]
    fn test_message_tool_description_and_parameters() {
        let tool = MessageTool::new();
        assert_eq!(tool.name(), "message");
        assert!(!tool.description().is_empty());
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["content"].is_object());
        assert!(
            params["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("content"))
        );
    }
}

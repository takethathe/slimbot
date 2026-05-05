use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;

use crate::tool::{Tool, ToolContext};

type SendCallback = Arc<dyn Fn(String, String, String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;

pub struct MessageTool {
    default_channel: Arc<std::sync::Mutex<String>>,
    default_chat_id: Arc<std::sync::Mutex<String>>,
    send_callback: Option<SendCallback>,
    sent_in_turn: AtomicBool,
}

impl MessageTool {
    pub fn new() -> Self {
        Self {
            default_channel: Arc::new(std::sync::Mutex::new(String::new())),
            default_chat_id: Arc::new(std::sync::Mutex::new(String::new())),
            send_callback: None,
            sent_in_turn: AtomicBool::new(false),
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

    fn start_turn(&self) {
        self.sent_in_turn.store(false, Ordering::Relaxed);
    }

    fn sent_in_turn(&self) -> bool {
        self.sent_in_turn.load(Ordering::Relaxed)
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
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string();

        let channel = match args.get("channel").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => self.default_channel.lock().unwrap().clone(),
        };
        let chat_id = match args.get("chat_id").and_then(|v| v.as_str()) {
            Some(c) => c.to_string(),
            None => self.default_chat_id.lock().unwrap().clone(),
        };

        if channel.is_empty() || chat_id.is_empty() {
            return Ok("Error: No target channel/chat specified".to_string());
        }

        if let Some(ref cb) = self.send_callback {
            cb(channel.clone(), chat_id.clone(), content.clone()).await;
            // Only mark as sent when targeting the default (origin) context.
            // Cross-channel sends should not suppress the final response.
            let default_ch = self.default_channel.lock().unwrap().clone();
            let default_chat = self.default_chat_id.lock().unwrap().clone();
            if channel == default_ch && chat_id == default_chat {
                self.sent_in_turn.store(true, Ordering::Relaxed);
            }
            Ok(format!("Message sent to {}:{}", channel, chat_id))
        } else {
            Ok("Error: Message sending not configured".to_string())
        }
    }

    fn set_context(&self, ctx: &ToolContext) {
        *self.default_channel.lock().unwrap() = ctx.channel.clone();
        *self.default_chat_id.lock().unwrap() = ctx.chat_id.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_message_tool_defaults_context() {
        let mut tool = MessageTool::new();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String, String)>(1);
        tool.set_send_callback(Arc::new(move |ch, cid, content| {
            let tx = tx.clone();
            Box::pin(async move { let _ = tx.send((ch, cid, content)).await; })
        }));

        let result = tool.execute(serde_json::json!({ "content": "hello" })).await.unwrap();
        assert!(result.contains("webui:chat-1"));
        assert!(tool.sent_in_turn());

        let (ch, cid, content) = rx.recv().await.unwrap();
        assert_eq!(ch, "webui");
        assert_eq!(cid, "chat-1");
        assert_eq!(content, "hello");
    }

    #[tokio::test]
    async fn test_message_tool_explicit_target() {
        let mut tool = MessageTool::new();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, String, String)>(1);
        tool.set_send_callback(Arc::new(move |ch, cid, content| {
            let tx = tx.clone();
            Box::pin(async move { let _ = tx.send((ch, cid, content)).await; })
        }));

        let result = tool.execute(serde_json::json!({
            "content": "cross channel",
            "channel": "cli",
            "chat_id": "other"
        })).await.unwrap();
        assert!(result.contains("cli:other"));
        assert!(!tool.sent_in_turn());

        let (ch, cid, _content) = rx.recv().await.unwrap();
        assert_eq!(ch, "cli");
        assert_eq!(cid, "other");
    }

    #[tokio::test]
    async fn test_message_tool_no_content() {
        let mut tool = MessageTool::new();
        tool.set_context(&ToolContext { channel: "cli".into(), chat_id: "chat-1".into() });

        let (tx, _rx) = tokio::sync::mpsc::channel::<(String, String, String)>(1);
        tool.set_send_callback(Arc::new(move |_ch, _cid, _content| {
            let _tx = tx.clone();
            Box::pin(async move {})
        }));

        // Empty content defaults to ""
        let result = tool.execute(serde_json::json!({})).await.unwrap();
        // Should still send (empty string is valid)
        assert!(result.contains("cli:chat-1"));
    }

    #[tokio::test]
    async fn test_message_tool_no_callback() {
        let tool = MessageTool::new();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let result = tool.execute(serde_json::json!({
            "content": "hello"
        })).await.unwrap();
        assert!(result.contains("not configured"));
    }

    #[tokio::test]
    async fn test_message_tool_no_context() {
        let mut tool = MessageTool::new();
        // No set_context called

        let (tx, _rx) = tokio::sync::mpsc::channel::<(String, String, String)>(1);
        tool.set_send_callback(Arc::new(move |_ch, _cid, _content| {
            let _tx = tx.clone();
            Box::pin(async move {})
        }));

        let result = tool.execute(serde_json::json!({
            "content": "test"
        })).await.unwrap();
        assert!(result.contains("No target channel"));
    }

    #[tokio::test]
    async fn test_message_tool_sent_in_turn_tracking() {
        let mut tool = MessageTool::new();
        tool.set_context(&ToolContext { channel: "webui".into(), chat_id: "chat-1".into() });

        let (tx, _rx) = tokio::sync::mpsc::channel::<(String, String, String)>(1);
        tool.set_send_callback(Arc::new(move |ch, cid, content| {
            let _tx = tx.clone();
            Box::pin(async move { let _ = (ch, cid, content); })
        }));

        tool.start_turn();
        assert!(!tool.sent_in_turn());

        // Send to default context
        tool.execute(serde_json::json!({ "content": "msg" })).await.unwrap();
        assert!(tool.sent_in_turn());

        // Start new turn
        tool.start_turn();
        assert!(!tool.sent_in_turn());

        // Send to different context
        tool.execute(serde_json::json!({
            "content": "msg",
            "channel": "other"
        })).await.unwrap();
        assert!(!tool.sent_in_turn());
    }

    #[test]
    fn test_message_tool_description_and_parameters() {
        let tool = MessageTool::new();
        assert_eq!(tool.name(), "message");
        assert!(!tool.description().is_empty());
        let params = tool.parameters();
        assert_eq!(params["type"], "object");
        assert!(params["properties"]["content"].is_object());
        assert!(params["required"].as_array().unwrap().contains(&serde_json::json!("content")));
    }
}

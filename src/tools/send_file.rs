//! `send_file` — deliver an agent-produced image or file back to a conversation
//! as media (ticket 28).
//!
//! The `message` tool only moves text. When the agent has produced a chart, a
//! screenshot, or a document it wants the owner to actually receive, it calls
//! this tool with a local path. The tool resolves the target conversation from
//! the turn context (or an explicit channel/chat_id) and hands a
//! [`MediaDelivery`] to the wiring callback, which publishes it onto the
//! dedicated media path so the owning channel can upload and send it.
//!
//! Path discipline (ADR 0017): the argument must be a local file path. A URL
//! (any `scheme://…`) is refused — media moves by local handle, never by a
//! remote address the tool would fetch, which would turn this into an SSRF
//! probe.

use anyhow::Result;
use async_trait::async_trait;
use serde_json::json;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::message_bus::MediaDelivery;
use crate::tool::Tool;

/// Delivery callback: hand a fully-built media delivery to the transport.
/// `Ok(())` means it was queued for the owning channel; `Err` is surfaced to
/// the model so it can fall back to a text answer.
type MediaCallback = Arc<
    dyn Fn(MediaDelivery) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>
        + Send
        + Sync,
>;

pub struct SendFileTool {
    send_callback: Option<MediaCallback>,
}

impl SendFileTool {
    pub fn new() -> Self {
        Self {
            send_callback: None,
        }
    }

    pub fn set_send_callback(&mut self, cb: MediaCallback) {
        self.send_callback = Some(cb);
    }

    /// True when the supplied path looks like a remote URL rather than a local
    /// file. We refuse these so the channel is never coaxed into fetching an
    /// arbitrary address (the outbound mirror of the file-key-only inbound rule).
    fn looks_like_url(path: &str) -> bool {
        let lower = path.trim().to_ascii_lowercase();
        // A scheme followed by `://` (http, https, ftp, gs, s3, file, …). We
        // also reject a bare `file://`, which would let a caller read outside
        // the workspace through a URL-shaped argument.
        match lower.find("://") {
            Some(idx) => {
                // The scheme must start at the beginning and be scheme-shaped
                // (letters/digits/+/-/.), so a Windows drive or a colon in a
                // caption does not false-positive.
                let scheme = &lower[..idx];
                !scheme.is_empty()
                    && scheme
                        .chars()
                        .next()
                        .is_some_and(|c| c.is_ascii_alphabetic())
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
            }
            None => false,
        }
    }
}

#[async_trait]
impl Tool for SendFileTool {
    fn name(&self) -> &str {
        "send_file"
    }

    /// Risk class (ADR 0011): sends a file to a channel the owner reads (and
    /// possibly to a third party).
    fn risk_level(&self) -> crate::approval::RiskLevel {
        crate::approval::RiskLevel::ExternalSend
    }

    fn description(&self) -> &str {
        "Send an image or file you produced back to the conversation. The path \
         must be a local file (never a URL)."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Local path to the file to send (not a URL)" },
                "as_image": { "type": "boolean", "description": "Send as a native image message when true, otherwise as a file attachment (default: false)" },
                "caption": { "type": "string", "description": "Optional short text to send alongside the file" },
                "channel": { "type": "string", "description": "Optional: target channel (defaults to the originating channel)" },
                "chat_id": { "type": "string", "description": "Optional: target chat_id (defaults to the originating chat)" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.trim().is_empty() {
            return Ok("Error: send_file requires a non-empty path".to_string());
        }
        if Self::looks_like_url(path) {
            return Ok("Error: send_file refuses URLs; pass a local file path only".to_string());
        }

        let cb = match &self.send_callback {
            Some(cb) => cb.clone(),
            None => return Ok("Error: file sending not configured".to_string()),
        };

        // Resolve the target conversation, mirroring the message tool: explicit
        // channel/chat_id win, else the turn's origin.
        let (def_channel, def_chat_id) = crate::tool::current_turn_target();
        let channel = args
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or(def_channel);
        let chat_id = args
            .get("chat_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or(def_chat_id);
        if channel.is_empty() || chat_id.is_empty() {
            return Ok("Error: No target channel/chat specified".to_string());
        }

        let as_image = args
            .get("as_image")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let caption = args
            .get("caption")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let media = if as_image {
            crate::message_bus::OutboundMedia::Image {
                path: path.to_string(),
            }
        } else {
            let name = std::path::Path::new(path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string());
            crate::message_bus::OutboundMedia::File {
                path: path.to_string(),
                name,
            }
        };
        let delivery = MediaDelivery {
            session_id: format!("{channel}:{chat_id}"),
            media,
            caption,
        };

        match cb(delivery).await {
            Ok(()) => Ok(format!("File sent to {channel}:{chat_id}")),
            Err(e) => Ok(format!("Error: file delivery failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::OutboundMedia;
    use crate::tool::test_util::turn_ctx;
    use crate::tool::with_turn_context;

    fn recording_cb(tx: tokio::sync::mpsc::Sender<MediaDelivery>) -> MediaCallback {
        Arc::new(move |delivery| {
            let tx = tx.clone();
            Box::pin(async move { tx.send(delivery).await.map_err(|e| e.to_string()) })
        })
    }

    #[tokio::test]
    async fn test_send_file_defaults_to_origin_as_file() {
        let mut tool = SendFileTool::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tool.set_send_callback(recording_cb(tx));

        let c = turn_ctx("feishu", "oc_conv1");
        let result = with_turn_context(c.clone(), async {
            tool.execute(json!({ "path": "out/report.pdf" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("feishu:oc_conv1"), "got: {result}");
        let d = rx.recv().await.unwrap();
        assert_eq!(d.session_id, "feishu:oc_conv1");
        assert!(
            matches!(d.media, OutboundMedia::File { ref path, .. } if path == "out/report.pdf"),
            "a non-image path lands as a file attachment"
        );
    }

    #[tokio::test]
    async fn test_send_file_as_image_flag_selects_image() {
        let mut tool = SendFileTool::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tool.set_send_callback(recording_cb(tx));

        let c = turn_ctx("feishu", "oc_conv1");
        with_turn_context(c.clone(), async {
            tool.execute(json!({ "path": "chart.png", "as_image": true }))
                .await
                .unwrap()
        })
        .await;
        let d = rx.recv().await.unwrap();
        assert!(
            matches!(d.media, OutboundMedia::Image { ref path } if path == "chart.png"),
            "as_image must select the image variant"
        );
    }

    #[tokio::test]
    async fn test_send_file_refuses_urls() {
        // A URL argument is refused before any delivery is queued — the tool
        // must never be turned into a remote fetch.
        let mut tool = SendFileTool::new();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tool.set_send_callback(recording_cb(tx));

        let c = turn_ctx("feishu", "oc_conv1");
        for url in [
            "http://evil.example/x.png",
            "https://evil.example/x.png",
            "file:///etc/passwd",
        ] {
            let result = with_turn_context(c.clone(), async {
                tool.execute(json!({ "path": url })).await.unwrap()
            })
            .await;
            assert!(result.contains("refuses URLs"), "got: {result} for {url}");
        }
        assert!(rx.try_recv().is_err(), "no delivery is queued for a URL");
    }

    #[test]
    fn test_looks_like_url_discriminators() {
        assert!(SendFileTool::looks_like_url("https://a/b"));
        assert!(SendFileTool::looks_like_url("HTTP://a/b"));
        assert!(SendFileTool::looks_like_url("file:///etc/passwd"));
        // Local paths, including a colon in a name, are not URLs.
        assert!(!SendFileTool::looks_like_url("out/report.pdf"));
        assert!(!SendFileTool::looks_like_url("./a/b"));
        assert!(!SendFileTool::looks_like_url("/abs/path"));
    }

    #[tokio::test]
    async fn test_send_file_no_callback_configured() {
        let tool = SendFileTool::new();
        let c = turn_ctx("feishu", "oc_conv1");
        let result = with_turn_context(c.clone(), async {
            tool.execute(json!({ "path": "out/report.pdf" }))
                .await
                .unwrap()
        })
        .await;
        assert!(result.contains("not configured"), "got: {result}");
    }
}

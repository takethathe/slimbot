use anyhow::{Result, anyhow};
use async_trait::async_trait;

use crate::runner::traits::PreTurnHook;
use crate::runner::types::{HookAction, RunSpec, RunState};
use crate::session::{Content, ContentBlock, Message};

/// Writes the user message that started this turn into the session.
pub struct WriteUserMessageHook;

#[async_trait]
impl PreTurnHook for WriteUserMessageHook {
    async fn call(&self, spec: &RunSpec, _state: &mut RunState) -> Result<HookAction> {
        // A plain text turn writes `Message::user` (Content::Plain). When the
        // inbound message carried image attachments, the same turn writes a
        // multi-modal message: a leading text block (the caption, which may be
        // empty) followed by the image blocks, so the provider serializes them
        // as `image_url` parts the model can actually see (ticket 28).
        let images: Vec<ContentBlock> = spec
            .task_hook
            .attachments()
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .cloned()
            .collect();
        let msg = if images.is_empty() {
            Message::user(spec.content.clone())
        } else {
            let mut blocks = Vec::with_capacity(images.len() + 1);
            if !spec.content.trim().is_empty() {
                blocks.push(ContentBlock::Text {
                    text: spec.content.clone(),
                });
            }
            blocks.extend(images);
            Message::User {
                meta: crate::session::MessageMeta::default(),
                content: Content::Multi(blocks),
                runtime_content: None,
            }
        };
        let mut sm = spec.session_manager.lock().await;
        sm.add_message(&spec.session_id, msg)
            .await
            .map_err(|e| anyhow!("Failed to write user message: {}", e))?;
        Ok(HookAction::Continue)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use super::*;
    use crate::memory::MemoryService;
    use crate::runner::types::RunSpec;
    use crate::session::{SessionManager, SharedSessionManager, TaskHook};
    use crate::tool::ToolManager;

    fn make_spec() -> RunSpec {
        let tmp = tempfile::TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();
        RunSpec {
            session_id: "s1".into(),
            content: "hello".into(),
            task_hook: TaskHook::new("s1"),
            max_iterations: 10,
            max_tokens: 100,
            cancel_token: None,
            workspace_dir,
            session_manager: Arc::new(Mutex::new(SessionManager::new(session_dir).unwrap())),
            tool_manager: Arc::new(ToolManager::new(PathBuf::from("/tmp"))),
            config: crate::config::AgentConfig::default(),
            pressure: Default::default(),
            memory_service: Arc::new(Mutex::new(MemoryService::new(std::path::Path::new("/tmp")))),
            consolidator: None,
            initial_message_count: 0,
            channel: "cli".into(),
            chat_id: "c1".into(),
            runtime_ctx: "ctx".into(),
            approvals: Default::default(),
            connectors: None,
            tracer: None,
        }
    }

    #[tokio::test]
    async fn test_write_user_message_writes_to_session() {
        let spec = make_spec();
        let sm: SharedSessionManager = spec.session_manager.clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        let action = WriteUserMessageHook.call(&spec, &mut state).await.unwrap();
        assert_eq!(action, HookAction::Continue);

        let msgs = sm.lock().await.get_messages("s1").await;
        assert_eq!(msgs.len(), 1);
        assert!(matches!(&msgs[0], Message::User { .. }));
    }

    #[tokio::test]
    async fn test_write_user_message_with_image_writes_multimodal() {
        // An inbound image rides on the hook and lands as a Content::Multi user
        // message: a text block for the caption plus the image block.
        let mut spec = make_spec();
        spec.content = "what is in this photo?".into();
        spec.task_hook =
            spec.task_hook
                .clone()
                .with_attachments(vec![crate::session::ContentBlock::Image {
                    mime_type: "image/png".into(),
                    base64_data: "AAAA".into(),
                }]);
        let sm: SharedSessionManager = spec.session_manager.clone();
        sm.lock().await.get_or_create("s1").await.unwrap();

        let mut state = crate::runner::types::RunState::new(Arc::new(
            crate::tool::TurnContext::new("cli".into(), "c1".into()),
        ));
        WriteUserMessageHook.call(&spec, &mut state).await.unwrap();

        let msgs = sm.lock().await.get_messages("s1").await;
        let Message::User { content, .. } = &msgs[0] else {
            panic!("expected a user message");
        };
        match content {
            crate::session::Content::Multi(blocks) => {
                assert_eq!(blocks.len(), 2, "caption + image");
                assert!(matches!(
                    &blocks[0],
                    crate::session::ContentBlock::Text { .. }
                ));
                assert!(matches!(
                    &blocks[1],
                    crate::session::ContentBlock::Image { .. }
                ));
            }
            other => panic!("expected multimodal content, got {other:?}"),
        }
    }
}

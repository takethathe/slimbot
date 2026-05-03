/// Consolidator: token-budget-triggered session summarization.
///
/// After each ReAct turn, if the LLM-reported prompt tokens exceed the safe
/// budget, this module selects a chunk of old messages (aligned to user-turn
/// boundaries), asks the LLM to summarize them, appends the summary to
/// `history.jsonl`, and updates the session's consolidation cursor.
use std::sync::Arc;

use anyhow::Result;

use crate::memory::SharedMemoryStore;
use crate::provider::{FinishReason, Provider};
use crate::session::{Message, SharedSessionManager, message_content_chars, message_content_str};
use crate::info;

/// Hard cap on messages per consolidation chunk.
const MAX_CHUNK_MESSAGES: usize = 60;
/// Extra headroom for tokenizer estimation drift.
const SAFETY_BUFFER: u32 = 512;

pub struct Consolidator {
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    memory_store: SharedMemoryStore,
    context_window_tokens: u32,
    max_completion_tokens: u32,
}

impl Consolidator {
    pub fn new(
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        memory_store: SharedMemoryStore,
        context_window_tokens: u32,
        max_completion_tokens: u32,
    ) -> Self {
        Self {
            provider,
            session_manager,
            memory_store,
            context_window_tokens,
            max_completion_tokens,
        }
    }

    /// Estimate token count for a single message using the session's observed
    /// chars-per-token ratio.
    pub fn estimate_message_tokens(msg: &Message, ratio: f64) -> u32 {
        let chars = message_content_chars(msg);
        if ratio > 0.0 {
            ((chars as f64 / ratio).ceil() as u32).max(4)
        } else {
            4
        }
    }

    fn format_messages(messages: &[Message]) -> String {
        let mut lines = Vec::new();
        for msg in messages {
            let content = message_content_str(msg);
            if content.is_empty() {
                continue;
            }
            let role = match msg {
                Message::System { .. } => "SYSTEM",
                Message::User { .. } => "USER",
                Message::Assistant { .. } => "ASSISTANT",
                Message::Tool { .. } => "TOOL",
            };
            lines.push(format!("[{role}] {content}"));
        }
        lines.join("\n")
    }

    /// Pick a user-turn boundary that removes at least `tokens_to_remove` tokens.
    /// Returns (end_index, removed_tokens).
    fn pick_consolidation_boundary(
        messages: &[Message],
        start_idx: usize,
        tokens_to_remove: u32,
        ratio: f64,
    ) -> Option<(usize, u32)> {
        if start_idx >= messages.len() || tokens_to_remove == 0 {
            return None;
        }

        let mut removed_tokens: u32 = 0;
        let mut last_boundary: Option<(usize, u32)> = None;

        for idx in start_idx..messages.len() {
            if idx > start_idx {
                if let Message::User { .. } = messages[idx] {
                    last_boundary = Some((idx, removed_tokens));
                    if removed_tokens >= tokens_to_remove {
                        return last_boundary;
                    }
                }
            }
            removed_tokens += Self::estimate_message_tokens(&messages[idx], ratio);
        }

        last_boundary
    }

    /// Clamp the chunk end index so we process at most MAX_CHUNK_MESSAGES messages,
    /// without breaking the user-turn boundary.
    fn cap_consolidation_boundary(
        messages: &[Message],
        start_idx: usize,
        end_idx: usize,
    ) -> Option<usize> {
        if end_idx - start_idx <= MAX_CHUNK_MESSAGES {
            return Some(end_idx);
        }
        let capped_end = start_idx + MAX_CHUNK_MESSAGES;
        for idx in (start_idx + 1..capped_end.min(messages.len())).rev() {
            if let Message::User { .. } = messages[idx] {
                return Some(idx);
            }
        }
        None
    }

    /// Summarize messages via LLM and append to history.jsonl.
    /// Returns the summary text on success.
    async fn archive(&self, messages: &[Message]) -> Result<Option<String>> {
        if messages.is_empty() {
            return Ok(None);
        }

        let formatted = Self::format_messages(messages);
        let system_prompt = self.system_prompt();

        let response = self
            .provider
            .chat(
                &[
                    Message::system(system_prompt),
                    Message::user(formatted),
                ],
                None,
            )
            .await?;

        if response.finish_reason == FinishReason::Error {
            anyhow::bail!("LLM returned error: {}", response.content.as_deref().unwrap_or("(empty)"));
        }

        let summary = response.content.unwrap_or_else(|| "(no summary)".to_string());
        if summary.is_empty() || summary == "(nothing)" {
            return Ok(None);
        }

        let mut ms = self.memory_store.lock().await;
        if let Err(e) = ms.append_history(&summary) {
            info!("[Consolidator] Failed to append summary to history: {e}");
        }

        info!("[Consolidator] Archived {} messages, summary {} chars", messages.len(), summary.len());
        Ok(Some(summary))
    }

    fn system_prompt(&self) -> String {
        "Extract key facts from this conversation. Only output items matching these categories, skip everything else:
- User facts: personal info, preferences, stated opinions, habits
- Decisions: choices made, conclusions reached
- Solutions: working approaches discovered through trial and error, especially non-obvious methods that succeeded after failed attempts
- Events: plans, deadlines, notable occurrences
- Preferences: communication style, tool preferences

Priority: user corrections and preferences > solutions > decisions > events > environment facts. The most valuable memory prevents the user from having to repeat themselves.

Skip: code patterns derivable from source, git history, or anything already captured in existing memory.

Output as concise bullet points, one fact per line. No preamble, no commentary.
If nothing noteworthy happened, output: (nothing)"
            .to_string()
    }

    /// Internal: perform one round of consolidation. Returns the end message ID
    /// that was consolidated (for cursor update).
    async fn consolidate_one_round(
        &self,
        session_id: &str,
        tokens_to_remove: u32,
    ) -> Result<Option<(usize, Option<String>)>> {
        let (messages, ratio, last_consolidated_id) = {
            let sm = self.session_manager.lock().await;
            let data = match sm.get_session_data(session_id) {
                Some(d) => d,
                None => return Ok(None),
            };
            if data.messages.is_empty() {
                return Ok(None);
            }
            (data.messages, data.char_per_token_ratio, data.last_consolidated_id)
        };

        let start_idx = messages
            .iter()
            .position(|m| m.id() > last_consolidated_id)
            .unwrap_or(0);

        if start_idx >= messages.len() {
            return Ok(None);
        }

        let Some((end_idx, _removed)) = Self::pick_consolidation_boundary(
            &messages,
            start_idx,
            tokens_to_remove,
            ratio,
        ) else {
            info!("[Consolidator] No safe boundary for {session_id}");
            return Ok(None);
        };

        let Some(end_idx) = Self::cap_consolidation_boundary(&messages, start_idx, end_idx) else {
            info!("[Consolidator] No capped boundary for {session_id}");
            return Ok(None);
        };

        let chunk: Vec<Message> = messages[start_idx..end_idx].to_vec();
        if chunk.is_empty() {
            return Ok(None);
        }

        let end_msg_id = messages[end_idx - 1].id();

        info!(
            "[Consolidator] Consolidating {} messages ({}..{}) for session {}",
            chunk.len(),
            start_idx,
            end_idx,
            session_id
        );

        let summary = self.archive(&chunk).await.ok().flatten();

        let mut sm = self.session_manager.lock().await;
        sm.update_consolidation_cursor(session_id, end_msg_id).await;
        if let Some(ref s) = summary {
            sm.set_last_summary(session_id, s).await;
        }
        sm.save_session_meta(session_id);

        Ok(Some((end_msg_id, summary)))
    }

    /// Main entry: check token budget, archive old messages if prompt exceeds budget.
    /// Called after each ReAct turn completes.
    pub async fn maybe_consolidate(&self, session_id: &str, prompt_tokens: u32) -> Result<()> {
        if self.context_window_tokens <= 0 {
            return Ok(());
        }

        let budget = self.context_window_tokens - self.max_completion_tokens - SAFETY_BUFFER;
        if prompt_tokens <= budget {
            return Ok(());
        }

        let target = budget / 2;
        let tokens_to_remove = prompt_tokens.saturating_sub(target);

        let result = self.consolidate_one_round(session_id, tokens_to_remove).await?;
        if let Some((_, summary)) = result {
            if let Some(s) = summary {
                info!("[Consolidator] Archived summary: {} chars", s.len());
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_message_tokens_with_ratio() {
        let msg = Message::user("hello world".to_string());
        // ratio = 4.0 → 11 / 4.0 = 2.75 → ceil = 3 → max(3, 4) = 4
        let tokens = Consolidator::estimate_message_tokens(&msg, 4.0);
        assert_eq!(tokens, 4);

        // ratio = 2.0 → 11 / 2.0 = 5.5 → ceil = 6
        let tokens = Consolidator::estimate_message_tokens(&msg, 2.0);
        assert_eq!(tokens, 6);
    }

    #[test]
    fn test_estimate_message_tokens_fallback() {
        let msg = Message::user("hello world 12345678".to_string()); // 20 chars
        // default: 20 / 4.0 = 5 → ceil = 5 → max(5, 4) = 5
        let tokens = Consolidator::estimate_message_tokens(&msg, 4.0);
        assert_eq!(tokens, 5);
    }

    #[test]
    fn test_format_messages() {
        let messages = vec![
            Message::user("hello".to_string()),
            Message::assistant(Some("hi there".to_string()), None),
            Message::tool("result".to_string(), "tc-1".to_string(), Some("echo".to_string())),
        ];
        let formatted = Consolidator::format_messages(&messages);
        assert!(formatted.contains("[USER] hello"));
        assert!(formatted.contains("[ASSISTANT] hi there"));
        assert!(formatted.contains("[TOOL] result"));
    }

    #[test]
    fn test_pick_consolidation_boundary() {
        let messages = vec![
            Message::user("a".repeat(100)),        // idx 0
            Message::assistant(Some("b".repeat(100)), None), // idx 1
            Message::user("c".repeat(100)),        // idx 2
            Message::assistant(Some("d".repeat(100)), None), // idx 3
            Message::user("e".repeat(100)),        // idx 4
        ];

        // Need to remove 60 tokens, ratio = 2.0 chars/token
        // Each message ~100/2.0 = 50 tokens
        let result = Consolidator::pick_consolidation_boundary(
            &messages, 0, 60, 2.0,
        );
        // After idx 0 (50 tokens), idx 1 (+50) = 100. First user boundary at idx 2 with 100 tokens.
        // 100 >= 60, so boundary at idx 2.
        assert!(result.is_some());
        let (end, removed) = result.unwrap();
        assert_eq!(end, 2);
        assert_eq!(removed, 100);
    }

    #[test]
    fn test_pick_no_boundary() {
        let messages = vec![
            Message::assistant(Some("only assistant".to_string()), None),
        ];
        let result = Consolidator::pick_consolidation_boundary(&messages, 0, 100, 2.0);
        // No user message found after start, should return None
        assert!(result.is_none());
    }

    #[test]
    fn test_cap_consolidation_boundary_under_limit() {
        let messages: Vec<Message> = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("u{i}"))
                } else {
                    Message::assistant(Some(format!("a{i}")), None)
                }
            })
            .collect();
        let result = Consolidator::cap_consolidation_boundary(&messages, 0, 4);
        assert_eq!(result, Some(4)); // under limit
    }

    #[test]
    fn test_content_chars() {
        use super::message_content_chars;
        let msg = Message::assistant(None, None);
        assert_eq!(message_content_chars(&msg), 0);

        let msg = Message::user("hello".to_string());
        assert_eq!(message_content_chars(&msg), 5);
    }
}

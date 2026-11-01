/// Consolidator: token-budget-triggered session summarization.
///
/// After each ReAct turn, if the LLM-reported prompt tokens exceed the safe
/// budget, this module selects a chunk of old messages (aligned to user-turn
/// boundaries), asks the LLM to summarize them, appends the summary to
/// `history.jsonl`, and updates the session's consolidation cursor.
use std::sync::Arc;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::info;
use crate::memory::SharedMemory;
use crate::pressure::{DEFAULT_CHAR_PER_TOKEN_RATIO, PressureAnchor};
use crate::provider::{FinishReason, Provider};
use crate::session::{Message, SharedSessionManager, message_content_chars, message_content_str};

/// Hard cap on messages per consolidation chunk.
///
/// A constant on purpose: raising it can silently drop conversation history the
/// user has no way to judge (ADR 0013, decision 13).
const MAX_CHUNK_MESSAGES: usize = 60;
/// Max retries for LLM-driven summary compression before force-truncating.
const MAX_COMPRESS_RETRIES: usize = 2;

pub struct Consolidator {
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    memory_service: SharedMemory,
    /// Window anchor + ratio table: the trigger, the target and the summary
    /// length are all percentages of it.
    anchor: PressureAnchor,
    cancel_token: CancellationToken,
}

impl Consolidator {
    pub fn new(
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        memory_service: SharedMemory,
        context_window_tokens: u32,
    ) -> Self {
        Self {
            provider,
            session_manager,
            memory_service,
            anchor: PressureAnchor::from_window(context_window_tokens),
            cancel_token: CancellationToken::new(),
        }
    }

    /// Replace the window anchor (window tokens + `pressure.*` ratio table).
    pub fn with_anchor(mut self, anchor: PressureAnchor) -> Self {
        self.anchor = anchor;
        self
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

        // Accumulate tokens from start_idx, checking for user-turn boundaries
        // after the start. The boundary check happens before adding that message's
        // tokens to `removed_tokens`, so `removed_tokens` reflects tokens strictly
        // before the boundary.
        #[allow(clippy::collapsible_if)]
        #[allow(clippy::needless_range_loop)]
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

    /// Clamp the chunk end index so no pinned (live approval) message is
    /// summarized. Returns `None` when the whole chunk is pinned.
    fn clamp_boundary_for_pins(
        messages: &[Message],
        start_idx: usize,
        end_idx: usize,
        protect: &std::collections::HashSet<String>,
    ) -> Option<usize> {
        if protect.is_empty() {
            return Some(end_idx);
        }
        // Nothing pinned inside this chunk: the boundary stands as chosen.
        let Some(pinned) = messages[start_idx..end_idx]
            .iter()
            .position(|m| m.approval_id().is_some_and(|id| protect.contains(id)))
        else {
            return Some(end_idx);
        };
        let mut cut = start_idx + pinned;
        // Walk back to a user-turn boundary so the chunk stays well-formed.
        while cut > start_idx && !matches!(messages[cut], Message::User { .. }) {
            cut -= 1;
        }
        (cut > start_idx).then_some(cut)
    }

    /// Summarize messages via LLM and append to history.jsonl.
    /// Returns the summary text on success.
    ///
    /// Enforces the window-derived summary budget in three layers:
    /// 1. The initial prompt states the character budget it must stay within.
    /// 2. If the result is still too large, re-prompt up to MAX_COMPRESS_RETRIES
    ///    times asking the LLM to compress its output.
    /// 3. As a last resort, force-truncate the result at a UTF-8 boundary.
    async fn archive(
        &self,
        messages: &[Message],
        summary_max_chars: usize,
    ) -> Result<Option<String>> {
        if messages.is_empty() {
            return Ok(None);
        }

        let formatted = Self::format_messages(messages);

        // Layer 1: initial summarization (prompt already specifies the size limit).
        let mut summary = match self
            .provider
            .chat(
                &[
                    &Message::system(self.system_prompt(summary_max_chars)),
                    &Message::user(formatted),
                ],
                None,
                None,
            )
            .await?
        {
            r if r.finish_reason == FinishReason::Error => {
                anyhow::bail!(
                    "LLM returned error: {}",
                    r.content.as_deref().unwrap_or("(empty)")
                );
            }
            r => r.content.unwrap_or_else(|| "(no summary)".to_string()),
        };

        if summary.is_empty() || summary == "(nothing)" {
            return Ok(None);
        }

        // Layer 2: LLM-driven compression retries.
        for attempt in 0..MAX_COMPRESS_RETRIES {
            if summary.chars().count() <= summary_max_chars {
                break;
            }
            info!(
                "[Consolidator] Summary too large ({} chars, budget {summary_max_chars}), compressing (attempt {}/{MAX_COMPRESS_RETRIES})",
                summary.chars().count(),
                attempt + 1,
            );

            let response = self
                .provider
                .chat(
                    &[
                        &Message::system(Self::compress_prompt(summary_max_chars)),
                        &Message::user(summary.clone()),
                    ],
                    None,
                    None,
                )
                .await?;

            if response.finish_reason == FinishReason::Error {
                info!(
                    "[Consolidator] Compress attempt {} failed: {}",
                    attempt + 1,
                    response.content.as_deref().unwrap_or("(empty)")
                );
                break;
            }

            match response.content {
                Some(s) if !s.is_empty() && s != "(nothing)" => summary = s,
                _ => break,
            }
        }

        // Layer 3: force-truncate if still over the limit.
        if summary.chars().count() > summary_max_chars {
            info!(
                "[Consolidator] Summary still too large after {MAX_COMPRESS_RETRIES} compress retries ({} chars, budget {summary_max_chars}), force-truncating",
                summary.chars().count()
            );
            let mut cut = summary
                .char_indices()
                .nth(summary_max_chars)
                .map(|(i, _)| i)
                .unwrap_or(summary.len());
            while cut > 0 && !summary.is_char_boundary(cut) {
                cut -= 1;
            }
            summary.truncate(cut);
        }

        let mut ms = self.memory_service.lock().await;
        if let Err(e) = ms.append_history(&summary) {
            info!("[Consolidator] Failed to append summary to history: {e}");
        }

        info!(
            "[Consolidator] Archived {} messages, summary {} chars",
            messages.len(),
            summary.len()
        );
        Ok(Some(summary))
    }

    fn system_prompt(&self, summary_max_chars: usize) -> String {
        format!("Extract key facts from this conversation. For each fact, annotate its memory attributes.

Only SNIP facts deserve a non-[skip] mark:
- Signal: would the user need to repeat this if forgotten?
- Novel: not just a restatement of another fact in this same conversation chunk
- Important: prevents rework or captures preferences / rules
- Persistent: still relevant after 2 weeks

Output one fact per line in this format:
- [mark] fact content

Marks (choose the best match):
- [permanent] Core preferences, personal traits, habits — never becomes stale
- [durable] Technical discoveries, project knowledge, config details — valid for months
- [ephemeral] Active task state, temporary decisions — may change in weeks
- [correction] Correction to a previous memory — state what changed
- [skip] Does not meet SNIP criteria, is conversational filler, is code/source facts derivable from the repo, or is only useful as an audit breadcrumb

Priority: user corrections and preferences > solutions > decisions > events > environment facts. The most valuable memory prevents the user from having to repeat themselves.

Do not mark something [skip] merely because it might already exist in long-term memory; Dream handles cross-file deduplication later.

Output concise bullet points only. No preamble, no commentary.
Your entire output MUST NOT exceed {summary_max_chars} characters. Be ruthlessly concise — every character counts.
If nothing noteworthy happened, output: (nothing)")
    }

    fn compress_prompt(summary_max_chars: usize) -> String {
        format!(
            "Compress the following summary to fit within {summary_max_chars} characters. Keep only the most important facts. Output the compressed summary directly, with no preamble or commentary."
        )
    }

    /// Internal: perform one round of consolidation. Returns the number of
    /// messages consolidated and an optional summary.
    async fn consolidate_one_round(
        &self,
        session_id: &str,
        tokens_to_remove: u32,
        protect: &std::collections::HashSet<String>,
    ) -> Result<Option<(usize, Option<String>)>> {
        let (messages, ratio) = {
            let sm = self.session_manager.lock().await;
            let data = match sm.get_session_data(session_id) {
                Some(d) => d,
                None => return Ok(None),
            };
            if data.messages.is_empty() {
                return Ok(None);
            }
            (data.messages, data.char_per_token_ratio)
        };

        // All loaded messages start after consolidated_lines, so start_idx = 0
        let start_idx = 0;

        if start_idx >= messages.len() {
            return Ok(None);
        }

        let Some((end_idx, _removed)) =
            Self::pick_consolidation_boundary(&messages, start_idx, tokens_to_remove, ratio)
        else {
            return Ok(None);
        };

        let Some(end_idx) = Self::cap_consolidation_boundary(&messages, start_idx, end_idx) else {
            return Ok(None);
        };

        // A live pending approval is pinned: never summarize the turn carrying
        // it, so the model keeps seeing what it is waiting for.
        let Some(end_idx) = Self::clamp_boundary_for_pins(&messages, start_idx, end_idx, protect)
        else {
            return Ok(None);
        };

        let chunk: Vec<Message> = messages[start_idx..end_idx].to_vec();
        if chunk.is_empty() {
            return Ok(None);
        }

        let chunk_count = end_idx - start_idx;

        info!(
            "[Consolidator] Consolidating {} messages ({}..{}) for session {}",
            chunk.len(),
            start_idx,
            end_idx,
            session_id
        );

        let summary_max_chars = self.anchor.limits(ratio).summary_max_chars;
        let summary = self.archive(&chunk, summary_max_chars).await.ok().flatten();

        // If cancelled during archive, skip all file I/O and meta updates.
        if self.cancel_token.is_cancelled() {
            info!(
                "[Consolidator] Cancelled during consolidate for {session_id}, skipping file writes"
            );
            return Ok(None);
        }

        let mut sm = self.session_manager.lock().await;
        sm.update_consolidated_lines(session_id, chunk_count).await;
        if let Some(ref s) = summary {
            sm.set_last_summary(session_id, s).await;
        }
        sm.save_session_meta(session_id);

        Ok(Some((chunk_count, summary)))
    }

    /// Main entry: check token budget, archive old messages if prompt exceeds budget.
    /// Called after each ReAct turn completes.
    /// Summarize the session when the prompt exceeds the token budget.
    /// Returns `true` when a consolidation round actually ran.
    pub async fn maybe_consolidate(&self, session_id: &str, prompt_tokens: u32) -> Result<bool> {
        self.maybe_consolidate_protecting(session_id, prompt_tokens, &Default::default())
            .await
    }

    /// [`Consolidator::maybe_consolidate`] that never summarizes messages
    /// carrying one of `protect` (live approval ids, ADR 0011): a pending
    /// confirmation must stay in the window until it is redeemed or cleared.
    pub async fn maybe_consolidate_protecting(
        &self,
        session_id: &str,
        prompt_tokens: u32,
        protect: &std::collections::HashSet<String>,
    ) -> Result<bool> {
        // The trigger is judged on the authoritative track (provider-reported
        // `prompt_tokens`), so only the derived token numbers matter here; the
        // chars-per-token ratio only scales the summary budget below.
        let limits = self.anchor.limits(DEFAULT_CHAR_PER_TOKEN_RATIO);
        if limits.consolidate_trigger_tokens == 0 {
            return Ok(false);
        }

        // Trigger = window × pressure.consolidate_trigger_ratio.
        if prompt_tokens <= limits.consolidate_trigger_tokens {
            return Ok(false);
        }

        let tokens_to_remove = prompt_tokens.saturating_sub(limits.consolidate_target_tokens);

        let consolidated = self
            .consolidate_one_round(session_id, tokens_to_remove, protect)
            .await?;

        Ok(consolidated.is_some())
    }

    /// Relieve context pressure after the provider refused a request for
    /// exceeding the window (`context_length_exceeded`, 票 19). Runs one
    /// consolidation round to bring the session from the full window down to
    /// the consolidation target, so the next turn fits — the routing that maps
    /// a `context_length` provider error onto context-pressure handling rather
    /// than treating it as a hard failure.
    pub async fn force_consolidate(
        &self,
        session_id: &str,
        protect: &std::collections::HashSet<String>,
    ) -> Result<bool> {
        let limits = self.anchor.limits(DEFAULT_CHAR_PER_TOKEN_RATIO);
        let tokens_to_remove = self
            .anchor
            .window_tokens
            .saturating_sub(limits.consolidate_target_tokens);
        if tokens_to_remove == 0 {
            return Ok(false);
        }
        let out = self
            .consolidate_one_round(session_id, tokens_to_remove, protect)
            .await?;
        Ok(out.is_some())
    }

    /// Signal the consolidator to cancel any ongoing consolidation.
    /// Does not wait for in-progress work — the cancellation is cooperative.
    pub fn shutdown(&self) {
        self.cancel_token.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Summary budget for the test anchor (32768-token window, cold-start ratio).
    fn summary_budget() -> usize {
        PressureAnchor::from_window(TEST_WINDOW_TOKENS)
            .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
            .summary_max_chars
    }

    const TEST_WINDOW_TOKENS: u32 = 32_768;

    #[test]
    fn test_consolidate_trigger_derives_from_window_ratio() {
        let limits =
            PressureAnchor::from_window(TEST_WINDOW_TOKENS).limits(DEFAULT_CHAR_PER_TOKEN_RATIO);
        assert_eq!(limits.consolidate_trigger_tokens, TEST_WINDOW_TOKENS / 2);
        assert_eq!(
            limits.consolidate_target_tokens,
            (limits.consolidate_trigger_tokens as f64 * 0.6) as u32
        );
    }

    #[test]
    fn test_consolidate_limits_double_with_window() {
        // Trigger, target and summary budget all scale with the single anchor.
        let small = PressureAnchor::from_window(32_768).limits(DEFAULT_CHAR_PER_TOKEN_RATIO);
        let large = PressureAnchor::from_window(65_536).limits(DEFAULT_CHAR_PER_TOKEN_RATIO);

        assert_eq!(
            large.consolidate_trigger_tokens,
            small.consolidate_trigger_tokens * 2
        );
        assert_eq!(
            large.consolidate_target_tokens,
            small.consolidate_target_tokens * 2
        );
        assert_eq!(large.summary_max_chars, small.summary_max_chars * 2);
    }

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
            Message::assistant(Some("hi there".to_string()), None, None, None),
            Message::tool(
                "result".to_string(),
                "tc-1".to_string(),
                Some("echo".to_string()),
            ),
        ];
        let formatted = Consolidator::format_messages(&messages);
        assert!(formatted.contains("[USER] hello"));
        assert!(formatted.contains("[ASSISTANT] hi there"));
        assert!(formatted.contains("[TOOL] result"));
    }

    #[test]
    fn test_pick_consolidation_boundary() {
        let messages = vec![
            Message::user("a".repeat(100)),                              // idx 0
            Message::assistant(Some("b".repeat(100)), None, None, None), // idx 1
            Message::user("c".repeat(100)),                              // idx 2
            Message::assistant(Some("d".repeat(100)), None, None, None), // idx 3
            Message::user("e".repeat(100)),                              // idx 4
        ];

        // Need to remove 60 tokens, ratio = 2.0 chars/token
        // Each message ~100/2.0 = 50 tokens
        let result = Consolidator::pick_consolidation_boundary(&messages, 0, 60, 2.0);
        // After idx 0 (50 tokens), idx 1 (+50) = 100. First user boundary at idx 2 with 100 tokens.
        // 100 >= 60, so boundary at idx 2.
        assert!(result.is_some());
        let (end, removed) = result.unwrap();
        assert_eq!(end, 2);
        assert_eq!(removed, 100);
    }

    #[test]
    fn test_consolidation_boundary_stops_before_a_pinned_approval_turn() {
        let messages = vec![
            Message::user("a".repeat(100)),                              // 0
            Message::assistant(Some("b".repeat(100)), None, None, None), // 1
            Message::user("c".repeat(100)),                              // 2 pinned turn opens
            Message::assistant(
                Some("call".into()),
                Some(vec![crate::tool::ToolCall {
                    id: "tc-1".into(),
                    name: "shell".into(),
                    args: serde_json::json!({}),
                }]),
                None,
                None,
            ), // 3
            Message::tool_marked(
                "NOT EXECUTED".into(),
                "tc-1".into(),
                Some("shell".into()),
                "appr-1".into(),
            ), // 4
            Message::user("d".repeat(100)),                              // 5
            Message::assistant(Some("e".repeat(100)), None, None, None), // 6
        ];

        // The boundary would land after the pinned turn, so it is pulled back to
        // the user line that opens it: that turn is never summarized.
        let protect = std::collections::HashSet::from(["appr-1".to_string()]);
        assert_eq!(
            Consolidator::clamp_boundary_for_pins(&messages, 0, 7, &protect),
            Some(2)
        );

        // An unrelated id protects nothing.
        let other = std::collections::HashSet::from(["appr-9".to_string()]);
        assert_eq!(
            Consolidator::clamp_boundary_for_pins(&messages, 0, 7, &other),
            Some(7)
        );

        // A chunk that is entirely pinned has no boundary at all: skip this round
        // rather than summarize the pending turn.
        assert_eq!(
            Consolidator::clamp_boundary_for_pins(&messages, 2, 7, &protect),
            None
        );
    }

    #[test]
    fn test_pick_no_boundary() {
        let messages = vec![Message::assistant(
            Some("only assistant".to_string()),
            None,
            None,
            None,
        )];
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
                    Message::assistant(Some(format!("a{i}")), None, None, None)
                }
            })
            .collect();
        let result = Consolidator::cap_consolidation_boundary(&messages, 0, 4);
        assert_eq!(result, Some(4)); // under limit
    }

    #[test]
    fn test_content_chars() {
        use super::message_content_chars;
        let msg = Message::assistant(None, None, None, None);
        assert_eq!(message_content_chars(&msg), 0);

        let msg = Message::user("hello".to_string());
        assert_eq!(message_content_chars(&msg), 5);
    }

    #[test]
    fn test_cap_consolidation_boundary_over_limit_returns_capped() {
        let messages: Vec<Message> = (0..80)
            .map(|i| {
                if i % 2 == 0 {
                    Message::user(format!("u{i}"))
                } else {
                    Message::assistant(Some(format!("a{i}")), None, None, None)
                }
            })
            .collect();
        let result = Consolidator::cap_consolidation_boundary(&messages, 0, 80);
        assert!(result.is_some());
        let end = result.unwrap();
        assert!(end <= 60); // capped to MAX_CHUNK_MESSAGES (60), looking back for user boundary
        assert!(end >= 58); // should be near the 60 boundary (58 is the last even index before 60)
        assert!(matches!(messages[end], Message::User { .. }));
    }

    #[test]
    fn test_cap_consolidation_boundary_over_limit_no_user_boundary() {
        let messages: Vec<Message> = (0..80)
            .map(|i| Message::assistant(Some(format!("a{i}")), None, None, None))
            .collect();
        let result = Consolidator::cap_consolidation_boundary(&messages, 0, 80);
        assert!(result.is_none()); // no user boundary to cap to
    }

    #[test]
    fn test_pick_consolidation_boundary_zero_tokens_to_remove() {
        let messages = vec![Message::user("hello".to_string())];
        let result = Consolidator::pick_consolidation_boundary(&messages, 0, 0, 2.0);
        assert!(result.is_none());
    }

    #[test]
    fn test_pick_consolidation_boundary_start_at_end() {
        let messages = vec![Message::user("hello".to_string())];
        let result = Consolidator::pick_consolidation_boundary(&messages, 1, 100, 2.0);
        assert!(result.is_none()); // start_idx >= messages.len()
    }

    #[test]
    fn test_format_messages_with_system_role() {
        let messages = vec![
            Message::system("system prompt".to_string()),
            Message::user("hello".to_string()),
        ];
        let formatted = Consolidator::format_messages(&messages);
        assert!(formatted.contains("[SYSTEM] system prompt"));
        assert!(formatted.contains("[USER] hello"));
    }

    #[test]
    fn test_format_messages_empty_message_skipped() {
        let messages = vec![
            Message::assistant(None, None, None, None),
            Message::user("hello".to_string()),
        ];
        let formatted = Consolidator::format_messages(&messages);
        assert!(!formatted.contains("[ASSISTANT]"));
        assert!(formatted.contains("[USER] hello"));
    }

    // ── Summary size limit tests ──

    use std::sync::Mutex;
    use tempfile::TempDir;

    use crate::memory::MemoryService;
    use crate::provider::{LLMResponse, Usage};
    use crate::session::SessionManager;

    /// Mock provider that returns different responses based on call count.
    struct SequenceMockProvider {
        responses: Mutex<Vec<(String, FinishReason)>>,
    }

    impl SequenceMockProvider {
        fn new(responses: Vec<(String, FinishReason)>) -> Self {
            Self {
                responses: Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for SequenceMockProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[crate::tool::ToolDefinition]>,
            _recorder: Option<&dyn crate::trace::SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            let mut responses = self.responses.lock().unwrap();
            let (content, finish_reason) = if responses.len() > 1 {
                responses.remove(0)
            } else {
                responses
                    .last()
                    .cloned()
                    .unwrap_or(("(nothing)".to_string(), FinishReason::Stop))
            };
            Ok(LLMResponse {
                content: Some(content),
                tool_calls: None,
                finish_reason,
                usage: Usage {
                    prompt_tokens: 100,
                    prompt_cache_hit_tokens: 0,
                    completion_tokens: 10,
                    total_tokens: 110,
                },
            })
        }
    }

    async fn setup_consolidator(provider: Arc<dyn Provider>) -> (Consolidator, TempDir, String) {
        let tmp = TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut sm = SessionManager::new(session_dir).unwrap();
        sm.get_or_create("s1").await.unwrap();
        for i in 0..5 {
            sm.add_message("s1", Message::user(format!("msg {i}")))
                .await
                .unwrap();
            sm.add_message(
                "s1",
                Message::assistant(Some(format!("reply {i}")), None, None, None),
            )
            .await
            .unwrap();
        }

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(sm));
        let ms: SharedMemory =
            Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        let consolidator = Consolidator::new(provider, sm, ms, 32768);
        (consolidator, tmp, "s1".to_string())
    }

    #[tokio::test]
    async fn test_maybe_consolidate_same_tokens_only_at_smaller_window() {
        // The same provider-reported token count must cross the trigger only
        // for the smaller window: the trigger is a percentage of the anchor.
        let provider = Arc::new(SequenceMockProvider::new(vec![(
            "- summary".to_string(),
            FinishReason::Stop,
        )]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        let wide = consolidator.with_anchor(PressureAnchor::from_window(65_536));
        assert!(
            !wide.maybe_consolidate(&session_id, 20_000).await.unwrap(),
            "20000 tokens is below half of a 64k window"
        );

        let narrow = wide.with_anchor(PressureAnchor::from_window(32_768));
        assert!(
            narrow.maybe_consolidate(&session_id, 20_000).await.unwrap(),
            "20000 tokens is above half of a 32k window"
        );
    }

    #[tokio::test]
    async fn test_force_consolidate_runs_one_round_without_reported_tokens() {
        // 票 19: a `context_length_exceeded` provider refusal arrives with no
        // usable prompt_tokens — relief is driven by the anchor alone, so the
        // round still runs and still respects the summary budget.
        let provider = Arc::new(SequenceMockProvider::new(vec![(
            "- summary of the whole session".to_string(),
            FinishReason::Stop,
        )]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        let did = consolidator
            .force_consolidate(&session_id, &std::collections::HashSet::new())
            .await
            .unwrap();
        assert!(did, "a full session gets one forced round");

        // A session with nothing to remove stays untouched without a call.
        let empty = Consolidator::new(
            Arc::new(SequenceMockProvider::new(vec![])),
            Arc::new(tokio::sync::Mutex::new(
                SessionManager::new(_tmp.path().join("empty_sessions")).unwrap(),
            )),
            consolidator.memory_service.clone(),
            32768,
        );
        let did = empty
            .force_consolidate("missing", &std::collections::HashSet::new())
            .await;
        assert!(
            !did.unwrap_or(true),
            "nothing to consolidate means no round"
        );
    }

    #[tokio::test]
    async fn test_summary_within_limit_no_compression() {
        let short_summary = "- User prefers dark mode".to_string();
        assert!(short_summary.len() <= summary_budget());

        let provider = Arc::new(SequenceMockProvider::new(vec![(
            short_summary.clone(),
            FinishReason::Stop,
        )]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("dark mode"));
    }

    #[tokio::test]
    async fn test_summary_over_limit_compresses_on_first_retry() {
        let large_summary = "x".repeat(summary_budget() + 100);
        let compressed_summary = "- User prefers dark mode".to_string();

        let provider = Arc::new(SequenceMockProvider::new(vec![
            (large_summary, FinishReason::Stop),
            (compressed_summary.clone(), FinishReason::Stop),
        ]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("dark mode"));
        assert!(entries[0].content.len() <= summary_budget());
    }

    #[tokio::test]
    async fn test_summary_over_limit_needs_multiple_retries() {
        let large_summary = "x".repeat(summary_budget() + 200);
        let medium_summary = "y".repeat(summary_budget() + 50);
        let compressed_summary = "- User chose SQLite".to_string();

        let provider = Arc::new(SequenceMockProvider::new(vec![
            (large_summary, FinishReason::Stop),
            (medium_summary, FinishReason::Stop),
            (compressed_summary.clone(), FinishReason::Stop),
        ]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].content.contains("SQLite"));
        assert!(entries[0].content.len() <= summary_budget());
    }

    #[tokio::test]
    async fn test_summary_over_limit_all_retries_exhausted_force_truncates() {
        let large_summary = "x".repeat(summary_budget() + 100);
        let still_large_1 = "y".repeat(summary_budget() + 50);
        let still_large_2 = "z".repeat(summary_budget() + 20);

        let provider = Arc::new(SequenceMockProvider::new(vec![
            (large_summary, FinishReason::Stop),
            (still_large_1, FinishReason::Stop),
            (still_large_2, FinishReason::Stop),
        ]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.len(), summary_budget());
        assert!(entries[0].content.chars().all(|c| c == 'z'));
    }

    #[tokio::test]
    async fn test_compress_returns_error_falls_to_truncation() {
        let large_summary = "x".repeat(summary_budget() + 100);

        let provider = Arc::new(SequenceMockProvider::new(vec![
            (large_summary.clone(), FinishReason::Stop),
            ("error occurred".to_string(), FinishReason::Error),
        ]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.len(), summary_budget());
    }

    #[tokio::test]
    async fn test_compress_returns_empty_falls_to_truncation() {
        let large_summary = "x".repeat(summary_budget() + 100);

        let provider = Arc::new(SequenceMockProvider::new(vec![
            (large_summary.clone(), FinishReason::Stop),
            ("".to_string(), FinishReason::Stop),
        ]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.len(), summary_budget());
    }

    #[tokio::test]
    async fn test_force_truncate_utf8_boundary() {
        // Multi-byte characters at the boundary must not be split.
        let mut large_summary = "🎉".repeat(summary_budget());
        large_summary.push_str("extra");

        let provider = Arc::new(SequenceMockProvider::new(vec![(
            large_summary,
            FinishReason::Stop,
        )]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        // Should truncate to the derived character budget, not break a character
        assert_eq!(entries[0].content, "🎉".repeat(summary_budget()));
        assert_eq!(entries[0].content.chars().count(), summary_budget());
    }

    #[tokio::test]
    async fn test_force_truncate_ascii_exact_boundary() {
        let large_summary = "a".repeat(summary_budget() + 10);

        let provider = Arc::new(SequenceMockProvider::new(vec![(
            large_summary,
            FinishReason::Stop,
        )]));
        let (consolidator, _tmp, session_id) = setup_consolidator(provider).await;

        consolidator
            .maybe_consolidate(&session_id, 30000)
            .await
            .unwrap();

        let entries = consolidator
            .memory_service
            .lock()
            .await
            .read_recent_history(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].content.len(), summary_budget());
    }

    #[test]
    fn test_compress_prompt_states_derived_budget() {
        let prompt = Consolidator::compress_prompt(summary_budget());
        assert!(prompt.contains(&format!("{} characters", summary_budget())));
        assert!(prompt.contains("Compress"));
    }

    #[test]
    fn test_system_prompt_states_derived_budget() {
        let tmp = TempDir::new().unwrap();
        let session_dir = tmp.path().join("sessions");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let sm = SessionManager::new(session_dir).unwrap();
        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(sm));
        let ms: SharedMemory =
            Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));

        let provider = Arc::new(SequenceMockProvider::new(vec![]));
        let consolidator = Consolidator::new(provider, sm, ms, 32768);

        let prompt = consolidator.system_prompt(summary_budget());
        assert!(
            prompt.contains(&format!("{} characters", summary_budget())),
            "the prompt states the window-derived budget"
        );
    }

    #[test]
    fn test_summary_budget_derives_from_window() {
        let small = PressureAnchor::from_window(32_768)
            .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
            .summary_max_chars;
        let large = PressureAnchor::from_window(65_536)
            .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
            .summary_max_chars;
        assert_eq!(large, small * 2);
        assert_ne!(small, 512, "no longer a hardcoded byte budget");
    }

    #[test]
    fn test_max_compress_retries_constant() {
        assert_eq!(MAX_COMPRESS_RETRIES, 2);
    }
}

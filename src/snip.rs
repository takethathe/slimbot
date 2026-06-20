//! Stateless context truncation before sending to the LLM.
//!
//! Snip ensures the prompt never exceeds the model's token limit by
//! truncating the oldest non-system messages. Unlike consolidation,
//! snip is fast (no LLM call) and stateless (no persistence).

use crate::session::{Message, message_content_chars};

/// Extra headroom for tokenizer estimation drift.
const SAFETY_BUFFER: u32 = 1024;
/// Fallback: keep at least this many non-system messages if nothing fits.
const MIN_KEEP_MESSAGES: usize = 4;

/// Estimate token count for a single message using the observed chars-per-token ratio.
fn estimate_message_tokens(msg: &Message, ratio: f64) -> u32 {
    let chars = message_content_chars(msg);
    if ratio > 0.0 {
        ((chars as f64 / ratio).ceil() as u32).max(4)
    } else {
        4
    }
}

/// Truncate messages to fit within the token budget.
///
/// Preserves all system messages. Iterates backwards through non-system
/// messages, keeping the most recent ones that fit within the budget.
/// Ensures the first non-system message is a User message (for GLM compatibility).
///
/// # Arguments
/// * `messages` - All messages (system + non-system)
/// * `context_window_tokens` - Model's context window size
/// * `max_output_tokens` - Reserved tokens for model output
/// * `char_per_token_ratio` - Observed chars/token ratio from LLM usage
///
/// # Returns
/// A new `Vec<Message>` with messages truncated to fit the budget.
pub fn snip_messages(
    messages: Vec<Message>,
    context_window_tokens: u32,
    max_output_tokens: u32,
    char_per_token_ratio: f64,
) -> Vec<Message> {
    if messages.is_empty() || context_window_tokens == 0 {
        return messages;
    }

    let budget = context_window_tokens
        .saturating_sub(max_output_tokens)
        .saturating_sub(SAFETY_BUFFER);
    if budget == 0 {
        return messages;
    }

    // Separate system and non-system messages
    let system_messages: Vec<Message> = messages
        .iter()
        .filter(|m| matches!(m, Message::System { .. }))
        .cloned()
        .collect();
    let non_system: Vec<Message> = messages
        .into_iter()
        .filter(|m| !matches!(m, Message::System { .. }))
        .collect();

    if non_system.is_empty() {
        return system_messages;
    }

    // Estimate system message tokens
    let system_tokens: u32 = system_messages
        .iter()
        .map(|m| estimate_message_tokens(m, char_per_token_ratio))
        .sum();
    let remaining_budget = budget.saturating_sub(system_tokens);

    // Iterate backwards, accumulating tokens until budget is exceeded
    let mut kept: Vec<Message> = Vec::new();
    let mut kept_tokens: u32 = 0;
    for message in non_system.iter().rev() {
        let msg_tokens = estimate_message_tokens(message, char_per_token_ratio);
        if !kept.is_empty() && kept_tokens + msg_tokens > remaining_budget {
            break;
        }
        kept.push(message.clone());
        kept_tokens += msg_tokens;
    }
    kept.reverse();

    // Ensure first non-system message is User (for GLM compatibility)
    if !kept.is_empty() {
        let first_user_idx = kept.iter().position(|m| matches!(m, Message::User { .. }));
        match first_user_idx {
            Some(idx) if idx > 0 => {
                kept = kept.split_off(idx);
            }
            None => {
                // No User found in kept window; search backwards in original list
                let mut found = false;
                for i in (0..non_system.len()).rev() {
                    if matches!(&non_system[i], Message::User { .. }) {
                        kept = non_system[i..].to_vec();
                        found = true;
                        break;
                    }
                }
                if !found {
                    // No User exists at all; fall back to last MIN_KEEP_MESSAGES
                    let keep_count = non_system.len().min(MIN_KEEP_MESSAGES);
                    kept = non_system[non_system.len() - keep_count..].to_vec();
                }
            }
            _ => {} // idx == 0, already starts with User
        }
    } else {
        // Nothing fit; fall back to last MIN_KEEP_MESSAGES
        let keep_count = non_system.len().min(MIN_KEEP_MESSAGES);
        kept = non_system[non_system.len() - keep_count..].to_vec();
    }

    // Reassemble: system messages + kept non-system
    let mut result = system_messages;
    result.extend(kept);
    result
}

/// Find the first index where all preceding tool results have matching assistant tool_calls.
/// Used to skip orphan tool results at the front of a message slice.
pub fn find_legal_message_start(messages: &[Message]) -> usize {
    let mut declared: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut start = 0;

    for (i, msg) in messages.iter().enumerate() {
        match msg {
            Message::Assistant {
                tool_calls: Some(calls),
                ..
            } => {
                for tc in calls {
                    declared.insert(tc.id.clone());
                }
            }
            Message::Tool { tool_call_id, .. } if !declared.contains(tool_call_id) => {
                start = i + 1;
                declared.clear();
            }
            _ => {}
        }
    }
    start
}

/// Maximum number of unconsolidated messages to keep in history.
pub const MAX_MESSAGES: usize = 120;

/// Limit message count to MAX_MESSAGES, keeping the most recent.
/// Ensures the slice starts at a legal position (no orphan tool results).
pub fn limit_message_count(messages: Vec<Message>, max: usize) -> Vec<Message> {
    if messages.len() <= max {
        return messages;
    }
    let start = messages.len() - max;
    let slice = &messages[start..];
    let legal_start = find_legal_message_start(slice);
    slice[legal_start..].to_vec()
}

/// Keep the most recent N compactable tool results.
const MICROCOMPACT_KEEP_RECENT: usize = 10;
/// Minimum content length (chars) for a tool result to be compacted.
const MICROCOMPACT_MIN_CHARS: usize = 500;
/// Tools whose results can be compacted (produce large output).
const COMPACTABLE_TOOLS: &[&str] = &["grep", "find_files", "file_reader", "shell", "list_dir"];

/// Replace old compactable tool results with one-line placeholders.
///
/// Keeps the most recent `MICROCOMPACT_KEEP_RECENT` compactable tool results.
/// Older ones with content >= `MICROCOMPACT_MIN_CHARS` are replaced with
/// `"[{tool_name} result omitted from context]"`.
pub fn microcompact(messages: Vec<Message>) -> Vec<Message> {
    // Collect indices of compactable tool results
    let compactable_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if let Message::Tool { name, .. } = m {
                name.as_ref()
                    .filter(|n| COMPACTABLE_TOOLS.contains(&n.as_str()))
                    .map(|_| i)
            } else {
                None
            }
        })
        .collect();

    if compactable_indices.len() <= MICROCOMPACT_KEEP_RECENT {
        return messages;
    }

    // Indices to compact: all except the last KEEP_RECENT
    let stale_count = compactable_indices.len() - MICROCOMPACT_KEEP_RECENT;
    let stale = &compactable_indices[..stale_count];

    let mut updated: Option<Vec<Message>> = None;
    for &idx in stale {
        if let Message::Tool { content, name, .. } = &messages[idx] {
            if content.len() < MICROCOMPACT_MIN_CHARS {
                continue;
            }
            if updated.is_none() {
                updated = Some(messages.clone());
            }
            let tool_name = name.as_deref().unwrap_or("tool");
            if let Some(ref mut msgs) = updated {
                let placeholder = format!("[{} result omitted from context]", tool_name);
                msgs[idx] = match &msgs[idx] {
                    Message::Tool {
                        meta,
                        tool_call_id,
                        name,
                        ..
                    } => Message::Tool {
                        meta: meta.clone(),
                        content: placeholder,
                        tool_call_id: tool_call_id.clone(),
                        name: name.clone(),
                    },
                    _ => unreachable!(),
                };
            }
        }
    }

    updated.unwrap_or(messages)
}

/// Maximum characters allowed in a single tool result.
const MAX_TOOL_RESULT_CHARS: usize = 16_000;
/// Suffix appended when a tool result is truncated.
const TRUNCATED_SUFFIX: &str = "\n... (truncated)";
/// Tools exempt from truncation (they control their own output size).
const EXEMPT_TOOLS: &[&str] = &["file_reader"];

/// Truncate tool results that exceed `MAX_TOOL_RESULT_CHARS`.
///
/// Exempt tools (e.g., `file_reader`) are not truncated.
pub fn apply_tool_result_budget(messages: Vec<Message>) -> Vec<Message> {
    let mut updated: Option<Vec<Message>> = None;
    for (i, msg) in messages.iter().enumerate() {
        if let Message::Tool { content, name, .. } = msg {
            // Skip exempt tools
            if name
                .as_ref()
                .is_some_and(|n| EXEMPT_TOOLS.contains(&n.as_str()))
            {
                continue;
            }
            if content.len() > MAX_TOOL_RESULT_CHARS {
                if updated.is_none() {
                    updated = Some(messages.clone());
                }
                if let Some(ref mut msgs) = updated {
                    let truncated =
                        format!("{}{}", &content[..MAX_TOOL_RESULT_CHARS], TRUNCATED_SUFFIX);
                    msgs[i] = match &msgs[i] {
                        Message::Tool {
                            meta,
                            tool_call_id,
                            name,
                            ..
                        } => Message::Tool {
                            meta: meta.clone(),
                            content: truncated,
                            tool_call_id: tool_call_id.clone(),
                            name: name.clone(),
                        },
                        _ => unreachable!(),
                    };
                }
            }
        }
    }
    updated.unwrap_or(messages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_user(s: &str) -> Message {
        Message::user(s.to_string())
    }

    fn msg_assistant(s: &str) -> Message {
        Message::assistant(Some(s.to_string()), None, None, None)
    }

    fn msg_system(s: &str) -> Message {
        Message::system(s.to_string())
    }

    fn msg_tool(content: &str, id: &str) -> Message {
        Message::tool(content.to_string(), id.to_string(), None)
    }

    fn msg_tool_with_name(content: &str, id: &str, name: &str) -> Message {
        Message::tool(content.to_string(), id.to_string(), Some(name.to_string()))
    }

    #[test]
    fn test_limit_message_count_no_change_when_under() {
        let msgs: Vec<Message> = (0..10).map(|i| msg_user(&format!("msg {}", i))).collect();
        let result = limit_message_count(msgs.clone(), 120);
        assert_eq!(result.len(), msgs.len());
    }

    #[test]
    fn test_limit_message_count_drops_oldest() {
        let msgs: Vec<Message> = (0..150).map(|i| msg_user(&format!("msg {}", i))).collect();
        let result = limit_message_count(msgs, 120);
        assert_eq!(result.len(), 120);
        // Should keep the most recent 120 messages (indices 30..150)
        if let Message::User { content, .. } = &result[0] {
            match content {
                crate::session::Content::Plain(s) => assert_eq!(s, "msg 30"),
                _ => panic!("expected Plain content"),
            }
        }
    }

    #[test]
    fn test_limit_message_count_skips_orphan_at_front() {
        // Build 121 messages: first is orphan tool, then 120 user messages
        let mut msgs = vec![msg_tool_with_name("orphan", "tc-orphan", "grep")];
        for i in 0..120 {
            msgs.push(msg_user(&format!("msg {}", i)));
        }
        let result = limit_message_count(msgs, 120);
        // After taking last 120, the orphan is excluded. After legal start, still no orphan.
        assert_eq!(result.len(), 120);
        assert!(matches!(&result[0], Message::User { .. }));
    }

    #[test]
    fn test_find_legal_message_start_no_orphans() {
        let msgs = vec![
            msg_user("hello"),
            Message::assistant(
                Some("reply".to_string()),
                Some(vec![crate::tool::ToolCall {
                    id: "tc-1".to_string(),
                    name: "grep".to_string(),
                    args: serde_json::json!({"pattern": "test"}),
                }]),
                None,
                None,
            ),
            msg_tool_with_name("result", "tc-1", "grep"),
        ];
        assert_eq!(find_legal_message_start(&msgs), 0);
    }

    #[test]
    fn test_find_legal_message_start_skips_orphan_tool() {
        let msgs = vec![
            msg_tool_with_name("orphan result", "tc-orphan", "grep"),
            msg_user("hello"),
        ];
        assert_eq!(find_legal_message_start(&msgs), 1);
    }

    #[test]
    fn test_find_legal_message_start_keeps_valid_tool_pair() {
        // Create assistant message with tool_calls using direct tool call
        let msgs = vec![
            Message::assistant(
                Some("thinking".to_string()),
                Some(vec![crate::tool::ToolCall {
                    id: "tc-1".to_string(),
                    name: "grep".to_string(),
                    args: serde_json::json!({"pattern": "test"}),
                }]),
                None,
                None,
            ),
            msg_tool_with_name("result", "tc-1", "grep"),
            msg_user("next"),
        ];
        assert_eq!(find_legal_message_start(&msgs), 0);
    }

    #[test]
    fn test_estimate_message_tokens_basic() {
        let msg = msg_user("hello world"); // 11 chars
        // ratio = 4.0 → 11 / 4.0 = 2.75 → ceil = 3 → max(3, 4) = 4
        assert_eq!(estimate_message_tokens(&msg, 4.0), 4);
        // ratio = 2.0 → 11 / 2.0 = 5.5 → ceil = 6
        assert_eq!(estimate_message_tokens(&msg, 2.0), 6);
    }

    #[test]
    fn test_estimate_message_tokens_zero_ratio() {
        let msg = msg_user("hello");
        assert_eq!(estimate_message_tokens(&msg, 0.0), 4);
    }

    #[test]
    fn test_snip_empty_messages() {
        let result = snip_messages(vec![], 32000, 4096, 4.0);
        assert!(result.is_empty());
    }

    #[test]
    fn test_snip_zero_context_window() {
        let msgs = vec![msg_user("hello")];
        let result = snip_messages(msgs.clone(), 0, 4096, 4.0);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_snip_all_fit_no_truncation() {
        // budget = 32000 - 4096 - 1024 = 26880
        // Each "hello" = 5 chars → 4 tokens (min). 10 messages = 40 tokens. Fits easily.
        let msgs: Vec<Message> = (0..10).map(|i| msg_user(&format!("msg {}", i))).collect();
        let result = snip_messages(msgs.clone(), 32000, 4096, 4.0);
        assert_eq!(result.len(), msgs.len());
    }

    #[test]
    fn test_snip_preserves_system_messages() {
        let msgs = vec![
            msg_system("system prompt"),
            msg_user("user 1"),
            msg_assistant("reply 1"),
        ];
        // Very small budget to force snipping
        let result = snip_messages(msgs, 100, 4096, 4.0);
        // System message should always be preserved
        assert!(result.iter().any(|m| matches!(m, Message::System { .. })));
    }

    #[test]
    fn test_snip_truncates_old_messages() {
        // budget = 2000 - 100 - 1024 = 876
        // Each message ~50 chars → ~13 tokens. 100 messages = ~1300 tokens > 876
        let mut msgs = vec![msg_system("sys")];
        for i in 0..50 {
            msgs.push(msg_user(&format!(
                "user message number {} with some padding text",
                i
            )));
            msgs.push(msg_assistant(&format!(
                "assistant reply number {} with some padding text",
                i
            )));
        }
        let original_len = msgs.len();
        let result = snip_messages(msgs, 2000, 100, 4.0);
        // Should have truncated (system + fewer non-system)
        assert!(result.len() < original_len);
        // System preserved
        assert!(matches!(&result[0], Message::System { .. }));
    }

    #[test]
    fn test_snip_user_first_guarantee() {
        // After snipping, first non-system message should be User
        let msgs = vec![
            msg_system("sys"),
            msg_assistant("old reply"), // Will be snipped
            msg_tool("result", "tc-1"), // Will be snipped
            msg_user("recent user"),
            msg_assistant("recent reply"),
        ];
        // Budget of 3000 tokens: enough to keep recent User-Assistant pair only
        // budget = 3000 - 100 - 1024 = 1876 tokens remaining (system takes ~4)
        // This should keep: User("recent user") + Assistant("recent reply")
        let result = snip_messages(msgs, 3000, 100, 4.0);
        let non_system: Vec<_> = result
            .iter()
            .filter(|m| !matches!(m, Message::System { .. }))
            .collect();
        if !non_system.is_empty() {
            assert!(
                matches!(non_system[0], Message::User { .. }),
                "First non-system message should be User, got {:?}",
                non_system[0]
            );
        }
    }

    #[test]
    fn test_snip_no_user_fallback() {
        // No User messages at all → fall back to last 4
        let msgs: Vec<Message> = (0..10)
            .map(|i| msg_assistant(&format!("reply {}", i)))
            .collect();
        // Budget that only allows 4 messages to fit (each ~50 chars = ~13 tokens)
        // budget = 1000 - 100 - 1024 = negative → 0, so use 3000
        // budget = 3000 - 100 - 1024 = 1876 tokens
        // Each assistant msg ~13 tokens, 4 = 52 tokens, should fit
        let result = snip_messages(msgs, 3000, 100, 4.0);
        // Should keep at most MIN_KEEP_MESSAGES (4) due to fallback
        assert!(result.len() <= MIN_KEEP_MESSAGES);
    }

    #[test]
    fn test_snip_only_system_messages() {
        let msgs = vec![msg_system("sys1"), msg_system("sys2")];
        let result = snip_messages(msgs, 32000, 4096, 4.0);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_snip_budget_zero_returns_as_is() {
        let msgs = vec![msg_user("hello")];
        // context_window = max_output + SAFETY_BUFFER → budget = 0
        let result = snip_messages(msgs.clone(), 5120, 4096, 4.0);
        assert_eq!(result.len(), msgs.len());
    }

    #[test]
    fn test_microcompact_keeps_recent_10() {
        // 15 grep tool results; only the oldest 5 should be compacted
        let mut msgs = vec![msg_user("start")];
        for i in 0..15 {
            let content = "x".repeat(600); // > MIN_CHARS (500)
            msgs.push(msg_tool_with_name(&content, &format!("tc-{}", i), "grep"));
        }
        let result = microcompact(msgs);
        // Count how many have been compacted (replaced with placeholder)
        let compacted = result
            .iter()
            .filter(|m| {
                if let Message::Tool { content, name, .. } = m {
                    name.as_deref() == Some("grep") && content.contains("omitted from context")
                } else {
                    false
                }
            })
            .count();
        assert_eq!(compacted, 5); // 15 - 10 = 5
    }

    #[test]
    fn test_microcompact_skips_short_content() {
        let mut msgs = vec![msg_user("start")];
        for i in 0..15 {
            let content = "x".repeat(100); // < MIN_CHARS (500)
            msgs.push(msg_tool_with_name(&content, &format!("tc-{}", i), "grep"));
        }
        let result = microcompact(msgs);
        // None should be compacted because all are < 500 chars
        let compacted = result
            .iter()
            .filter(|m| {
                if let Message::Tool { content, .. } = m {
                    content.contains("omitted from context")
                } else {
                    false
                }
            })
            .count();
        assert_eq!(compacted, 0);
    }

    #[test]
    fn test_microcompact_only_compactable_tools() {
        let mut msgs = vec![msg_user("start")];
        // 15 file_writer results (not compactable) + 15 grep results (compactable)
        for i in 0..15 {
            let content = "x".repeat(600);
            msgs.push(msg_tool_with_name(
                &content,
                &format!("tc-fw-{}", i),
                "file_writer",
            ));
        }
        for i in 0..15 {
            let content = "x".repeat(600);
            msgs.push(msg_tool_with_name(
                &content,
                &format!("tc-grep-{}", i),
                "grep",
            ));
        }
        let result = microcompact(msgs);
        // file_writer should NOT be compacted
        let fw_compacted = result
            .iter()
            .filter(|m| {
                if let Message::Tool { content, name, .. } = m {
                    name.as_deref() == Some("file_writer")
                        && content.contains("omitted from context")
                } else {
                    false
                }
            })
            .count();
        assert_eq!(fw_compacted, 0);
        // grep should have 5 compacted (15 - 10)
        let grep_compacted = result
            .iter()
            .filter(|m| {
                if let Message::Tool { content, name, .. } = m {
                    name.as_deref() == Some("grep") && content.contains("omitted from context")
                } else {
                    false
                }
            })
            .count();
        assert_eq!(grep_compacted, 5);
    }

    #[test]
    fn test_tool_result_budget_truncates_long_content() {
        let long_content = "x".repeat(20_000); // > MAX_TOOL_RESULT_CHARS (16_000)
        let msgs = vec![
            msg_user("start"),
            msg_tool_with_name(&long_content, "tc-1", "shell"),
        ];
        let result = apply_tool_result_budget(msgs);
        if let Message::Tool { content, .. } = &result[1] {
            assert!(content.len() < 20_000);
            assert!(content.contains("truncated"));
        } else {
            panic!("expected Tool message");
        }
    }

    #[test]
    fn test_tool_result_budget_exempt_tools() {
        let long_content = "x".repeat(20_000);
        let msgs = vec![
            msg_user("start"),
            msg_tool_with_name(&long_content, "tc-1", "file_reader"),
        ];
        let result = apply_tool_result_budget(msgs);
        if let Message::Tool { content, .. } = &result[1] {
            // file_reader is exempt, should NOT be truncated
            assert_eq!(content.len(), 20_000);
        } else {
            panic!("expected Tool message");
        }
    }

    #[test]
    fn test_tool_result_budget_short_content_unchanged() {
        let short_content = "x".repeat(100);
        let msgs = vec![
            msg_user("start"),
            msg_tool_with_name(&short_content, "tc-1", "shell"),
        ];
        let result = apply_tool_result_budget(msgs);
        if let Message::Tool { content, .. } = &result[1] {
            assert_eq!(content.len(), 100);
        } else {
            panic!("expected Tool message");
        }
    }
}

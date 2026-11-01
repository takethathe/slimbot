//! Stateless context truncation before sending to the LLM.
//!
//! Snip ensures the prompt never exceeds the model's token limit by
//! truncating the oldest non-system messages. Unlike consolidation,
//! snip is fast (no LLM call) and stateless (no persistence).

use std::path::Path;

use crate::pressure::PressureLimits;
use crate::session::{Message, message_content_chars};
use crate::utils::{existing_tool_result_path, persist_tool_result_file};

/// Estimate token count for a single message using the observed chars-per-token ratio.
fn estimate_message_tokens(msg: &Message, ratio: f64) -> u32 {
    let chars = message_content_chars(msg);
    if ratio > 0.0 {
        ((chars as f64 / ratio).ceil() as u32).max(4)
    } else {
        4
    }
}

/// Token budget for the prompt itself: window minus the reserved output and the
/// derived safety margin.
pub fn token_budget(limits: &PressureLimits, max_output_tokens: u32) -> u32 {
    limits
        .window_tokens
        .saturating_sub(max_output_tokens)
        .saturating_sub(limits.snip_safety_tokens)
}

/// Truncate messages to fit within the token budget.
///
/// Preserves all system messages. Iterates backwards through non-system
/// messages, keeping the most recent ones that fit within the budget.
/// Ensures the first non-system message is a User message (for GLM compatibility).
///
/// # Arguments
/// * `messages` - All messages (system + non-system)
/// * `limits` - Limits derived from the window anchor (window, safety margin,
///   chars-per-token ratio and the message-count floor)
/// * `max_output_tokens` - Reserved tokens for model output
///
/// # Returns
/// A new `Vec<Message>` with messages truncated to fit the budget.
pub fn snip_messages(
    messages: Vec<Message>,
    limits: &PressureLimits,
    max_output_tokens: u32,
) -> Vec<Message> {
    if messages.is_empty() || limits.window_tokens == 0 {
        return messages;
    }

    let char_per_token_ratio = limits.char_per_token_ratio;
    let budget = token_budget(limits, max_output_tokens);
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
                    // No User exists at all; fall back to the message-count floor
                    let keep_count = non_system.len().min(limits.min_keep_messages);
                    kept = non_system[non_system.len() - keep_count..].to_vec();
                }
            }
            _ => {} // idx == 0, already starts with User
        }
    } else {
        // Nothing fit; fall back to the message-count floor
        let keep_count = non_system.len().min(limits.min_keep_messages);
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

/// Minimum content length (chars) for a tool result to be compacted.
///
/// Deliberately a constant: lowering it silently drops history the user cannot
/// judge, so it is not exposed as configuration (ADR 0013, decision 13).
const MICROCOMPACT_MIN_CHARS: usize = 500;

/// Replace old non-exempt tool results with a placeholder that carries the
/// persisted path.
///
/// Keeps the most recent `limits.keep_recent_tool_results` results. Older ones
/// with content >= [`MICROCOMPACT_MIN_CHARS`] are replaced by
/// `"[{tool} result omitted · full output: tool-results/<call_id>.txt]"`.
///
/// Cleanup is paired with recoverability: if the result was never persisted
/// (it never exceeded the AfterTool limit), it is persisted *now* first, so the
/// model can always read back what was dropped instead of re-running the tool
/// (re-running `shell` or `web_fetch` is not idempotent).
///
/// `exempt` is the tool self-report (`Tool::context_exempt`): a single set, so
/// both trimming stages agree on what may be touched.
pub fn microcompact(
    messages: Vec<Message>,
    limits: &PressureLimits,
    workspace_dir: &Path,
    exempt: &dyn Fn(&str) -> bool,
) -> Vec<Message> {
    let keep_recent = limits.keep_recent_tool_results;

    // Collect indices of compactable (non-exempt) tool results.
    let compactable_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| match m {
            Message::Tool { name, .. } => {
                let is_exempt = name.as_deref().is_some_and(exempt);
                (!is_exempt).then_some(i)
            }
            _ => None,
        })
        .collect();

    if compactable_indices.len() <= keep_recent {
        return messages;
    }

    // Indices to compact: all except the last `keep_recent`.
    let stale_count = compactable_indices.len() - keep_recent;
    let stale = &compactable_indices[..stale_count];

    let mut updated: Option<Vec<Message>> = None;
    for &idx in stale {
        let Message::Tool {
            content,
            tool_call_id,
            name,
            ..
        } = &messages[idx]
        else {
            continue;
        };
        if content.chars().count() < MICROCOMPACT_MIN_CHARS {
            continue;
        }

        let tool_name = name.as_deref().unwrap_or("tool");
        let reference = existing_tool_result_path(workspace_dir, tool_call_id)
            .or_else(|| persist_tool_result_file(workspace_dir, tool_call_id, content));
        let placeholder = match reference {
            Some(path) => format!("[{tool_name} result omitted · full output: {path}]"),
            // Persisting failed (read-only workspace): still drop the body, but
            // do not pretend a path exists.
            None => format!("[{tool_name} result omitted from context]"),
        };

        if updated.is_none() {
            updated = Some(messages.clone());
        }
        if let Some(ref mut msgs) = updated {
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

    updated.unwrap_or(messages)
}

/// Truncate tool results that exceed the window-derived single-result limit.
///
/// A truncated result keeps head *and* tail (same strategy as the AfterTool
/// stage, so the two stages cannot disagree about what survives) and the
/// truncation text cites the persisted path when the full output is on disk.
/// Exempt tools are left untouched: one self-report serves both this stage and
/// [`microcompact`].
pub fn apply_tool_result_budget(
    messages: Vec<Message>,
    limits: &PressureLimits,
    workspace_dir: &Path,
    exempt: &dyn Fn(&str) -> bool,
) -> Vec<Message> {
    let max_chars = limits.single_tool_result_chars;
    if max_chars == 0 {
        // No anchor known: acting would be pure guesswork.
        return messages;
    }

    let mut updated: Option<Vec<Message>> = None;
    for (i, msg) in messages.iter().enumerate() {
        let Message::Tool {
            content,
            tool_call_id,
            name,
            ..
        } = msg
        else {
            continue;
        };
        if name.as_deref().is_some_and(exempt) {
            continue;
        }
        if content.chars().count() <= max_chars {
            continue;
        }
        // Idempotent: a result that already carries a truncation notice (from
        // this stage or the AfterTool stage) is not trimmed again, otherwise
        // every turn would shave a little more and stack another marker.
        if content.contains("(truncated") {
            continue;
        }

        let mut truncated = crate::utils::truncate_text_head_tail(content, max_chars);
        match existing_tool_result_path(workspace_dir, tool_call_id) {
            Some(path) => truncated.push_str(&format!("\n... (truncated · full output: {path})")),
            None => truncated.push_str("\n... (truncated)"),
        }

        if updated.is_none() {
            updated = Some(messages.clone());
        }
        if let Some(ref mut msgs) = updated {
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

    fn limits(window_tokens: u32, char_per_token_ratio: f64) -> PressureLimits {
        crate::pressure::PressureAnchor::from_window(window_tokens).limits(char_per_token_ratio)
    }

    /// Stand-in for `ToolManager::is_context_exempt` when nothing is exempt.
    #[allow(non_upper_case_globals)]
    const NONE_EXEMPT: fn(&str) -> bool = |_name: &str| false;

    /// Build an exemption predicate for the named tools.
    fn only(names: &'static [&'static str]) -> impl Fn(&str) -> bool {
        move |name: &str| names.contains(&name)
    }

    /// `count` tool results of the same tool and body, oldest first.
    fn tool_results(count: usize, tool: &str, body: String) -> Vec<Message> {
        let mut msgs = vec![msg_user("start")];
        for i in 0..count {
            msgs.push(msg_tool_with_name(&body, &format!("tc-{i}"), tool));
        }
        msgs
    }

    /// How many results of `tool` were replaced by a placeholder.
    fn placeholder_count(messages: &[Message], tool: &str) -> usize {
        messages
            .iter()
            .filter(|m| match m {
                Message::Tool { content, name, .. } => {
                    name.as_deref() == Some(tool) && content.contains("result omitted")
                }
                _ => false,
            })
            .count()
    }

    /// Content of the tool message carrying `tool_call_id`.
    fn tool_content(messages: &[Message], tool_call_id: &str) -> String {
        messages
            .iter()
            .find_map(|m| match m {
                Message::Tool {
                    content,
                    tool_call_id: id,
                    ..
                } if id == tool_call_id => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no tool message {tool_call_id}"))
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
        let result = snip_messages(vec![], &limits(32000, 4.0), 4096);
        assert!(result.is_empty());
    }

    #[test]
    fn test_snip_zero_context_window() {
        let msgs = vec![msg_user("hello")];
        let result = snip_messages(msgs.clone(), &limits(0, 4.0), 4096);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_snip_all_fit_no_truncation() {
        // budget = 32000 - 4096 - 960 (3% safety) = 26944
        // Each "hello" = 5 chars → 4 tokens (min). 10 messages = 40 tokens. Fits easily.
        let msgs: Vec<Message> = (0..10).map(|i| msg_user(&format!("msg {}", i))).collect();
        let result = snip_messages(msgs.clone(), &limits(32000, 4.0), 4096);
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
        let result = snip_messages(msgs, &limits(100, 4.0), 4096);
        // System message should always be preserved
        assert!(result.iter().any(|m| matches!(m, Message::System { .. })));
    }

    #[test]
    fn test_snip_truncates_old_messages() {
        // budget = 2000 - 1000 output - 60 (3% safety) = 940
        // Each message ~60 chars → ~15 tokens. 100 messages = ~1500 tokens > 940
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
        let result = snip_messages(msgs, &limits(2000, 4.0), 1000);
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
        // budget = 3000 - 100 - 90 (3% safety) = 2810 tokens remaining (system takes ~4)
        // This should keep: User("recent user") + Assistant("recent reply")
        let result = snip_messages(msgs, &limits(3000, 4.0), 100);
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
        // No User messages at all → fall back to the configured message floor.
        let msgs: Vec<Message> = (0..10)
            .map(|i| msg_assistant(&format!("reply {}", i)))
            .collect();
        let limits = limits(3000, 4.0);
        let result = snip_messages(msgs, &limits, 100);
        assert!(result.len() <= limits.min_keep_messages);
    }

    #[test]
    fn test_snip_only_system_messages() {
        let msgs = vec![msg_system("sys1"), msg_system("sys2")];
        let result = snip_messages(msgs, &limits(32000, 4.0), 4096);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_snip_messages_doubling_window_doubles_kept_window() {
        // The prompt budget is window-derived: doubling the anchor doubles how
        // much history survives, with no per-limit reconfiguration.
        let msgs: Vec<Message> = (0..400)
            .map(|i| msg_user(&format!("{}{}", "x".repeat(100), i)))
            .collect();
        let small = snip_messages(msgs.clone(), &limits(4_000, 3.0), 100);
        let large = snip_messages(msgs, &limits(8_000, 3.0), 100);

        let ratio = large.len() as f64 / small.len() as f64;
        assert!(
            (ratio - 2.0).abs() < 0.05,
            "doubling the window should double the kept message window, got {ratio} ({} vs {})",
            large.len(),
            small.len()
        );
    }

    #[test]
    fn test_snip_budget_zero_returns_as_is() {
        let msgs = vec![msg_user("hello")];
        // 4200 - 4096 output - 126 safety → budget saturates to 0.
        let result = snip_messages(msgs.clone(), &limits(4_200, 4.0), 4096);
        assert_eq!(result.len(), msgs.len());
    }

    #[test]
    fn test_microcompact_keeps_recent_results_from_limits() {
        // 15 grep results, keep_recent_tool_results = 10 → oldest 5 compacted.
        let tmp = tempfile::tempdir().unwrap();
        let result = microcompact(
            tool_results(15, "grep", "x".repeat(600)),
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        assert_eq!(placeholder_count(&result, "grep"), 5);
    }

    #[test]
    fn test_microcompact_skips_short_content() {
        let tmp = tempfile::tempdir().unwrap();
        let result = microcompact(
            tool_results(15, "grep", "x".repeat(100)),
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        assert_eq!(placeholder_count(&result, "grep"), 0);
    }

    #[test]
    fn test_microcompact_keeps_self_reported_exempt_results() {
        // `file_reader` self-reports exempt: the same self-report that disables
        // truncation also keeps the result from being replaced.
        let tmp = tempfile::tempdir().unwrap();
        let result = microcompact(
            tool_results(15, "file_reader", "x".repeat(600)),
            &limits(32_768, 3.0),
            tmp.path(),
            &only(&["file_reader"]),
        );
        assert_eq!(placeholder_count(&result, "file_reader"), 0);
        assert!(
            !tmp.path().join("tool-results").exists(),
            "nothing should be persisted for an exempt result"
        );
    }

    #[test]
    fn test_microcompact_placeholder_carries_persisted_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tool-results")).unwrap();
        std::fs::write(
            tmp.path().join("tool-results/tc-0.txt"),
            "already persisted",
        )
        .unwrap();

        let result = microcompact(
            tool_results(15, "grep", "x".repeat(600)),
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );

        let placeholder = tool_content(&result, "tc-0");
        assert!(
            placeholder.contains("tool-results/tc-0.txt"),
            "placeholder must carry the persisted path, got: {placeholder}"
        );
    }

    #[test]
    fn test_microcompact_persists_result_never_written_to_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let body = format!("{}UNIQUE-TAIL", "y".repeat(600));

        let result = microcompact(
            tool_results(15, "grep", body.clone()),
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );

        assert!(
            tool_content(&result, "tc-0").contains("tool-results/tc-0.txt"),
            "the never-persisted result is written at cleanup time"
        );
        let on_disk = std::fs::read_to_string(tmp.path().join("tool-results/tc-0.txt")).unwrap();
        assert_eq!(on_disk, body, "the full body is recoverable");
    }

    #[test]
    fn test_microcompact_reference_round_trips_to_dropped_content() {
        // Recoverability round-trip: read the path the placeholder cites and get
        // back exactly what was dropped from context.
        let tmp = tempfile::tempdir().unwrap();
        let body = format!("{}{}", "z".repeat(600), "ROUND-TRIP");
        let result = microcompact(
            tool_results(15, "grep", body.clone()),
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );

        let placeholder = tool_content(&result, "tc-0");
        let path = placeholder
            .split("full output: ")
            .nth(1)
            .and_then(|rest| rest.strip_suffix(']'))
            .expect("placeholder cites a path");

        let recovered = std::fs::read_to_string(tmp.path().join(path)).unwrap();
        assert_eq!(recovered, body);
    }

    #[test]
    fn test_microcompact_uses_configured_keep_recent_results() {
        let tmp = tempfile::tempdir().unwrap();
        let config = crate::config::PressureConfig {
            keep_recent_tool_results: 3,
            ..Default::default()
        };
        let limits = crate::pressure::PressureAnchor::new(32_768, config).limits(3.0);
        let result = microcompact(
            tool_results(15, "grep", "x".repeat(600)),
            &limits,
            tmp.path(),
            &NONE_EXEMPT,
        );
        assert_eq!(placeholder_count(&result, "grep"), 12);
    }

    #[test]
    fn test_apply_tool_result_budget_truncates_long_content() {
        // 4000-token window → 1000 tokens per result → 3000 chars.
        let tmp = tempfile::tempdir().unwrap();
        let result = apply_tool_result_budget(
            vec![
                msg_user("start"),
                msg_tool_with_name(&"x".repeat(10_000), "tc-1", "shell"),
            ],
            &limits(4_000, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        let content = tool_content(&result, "tc-1");
        assert!(content.len() < 10_000);
        assert!(content.contains("truncated"));
    }

    #[test]
    fn test_apply_tool_result_budget_keeps_head_and_tail() {
        // Head and tail survive (same strategy as the AfterTool stage), so the
        // two stages cannot disagree about which end of a result is useful.
        let tmp = tempfile::tempdir().unwrap();
        let mut body = String::from("HEAD-MARKER");
        body.push_str(&"m".repeat(10_000));
        body.push_str("TAIL-MARKER");
        let result = apply_tool_result_budget(
            vec![msg_tool_with_name(&body, "tc-1", "shell")],
            &limits(4_000, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        let content = tool_content(&result, "tc-1");
        assert!(content.starts_with("HEAD-MARKER"));
        assert!(content.contains("TAIL-MARKER"));
    }

    #[test]
    fn test_apply_tool_result_budget_leaves_exempt_tool_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "x".repeat(20_000);
        let result = apply_tool_result_budget(
            vec![msg_tool_with_name(&body, "tc-1", "file_reader")],
            &limits(4_000, 3.0),
            tmp.path(),
            &only(&["file_reader"]),
        );
        assert_eq!(tool_content(&result, "tc-1"), body);
    }

    #[test]
    fn test_apply_tool_result_budget_short_content_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let result = apply_tool_result_budget(
            vec![msg_tool_with_name(&"x".repeat(100), "tc-1", "shell")],
            &limits(32_768, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        assert_eq!(tool_content(&result, "tc-1").len(), 100);
    }

    #[test]
    fn test_apply_tool_result_budget_does_not_stack_markers() {
        // Running the stage twice must not keep shaving the same result: the
        // second pass sees its own truncation notice and leaves it alone.
        let tmp = tempfile::tempdir().unwrap();
        let once = apply_tool_result_budget(
            vec![msg_tool_with_name(&"x".repeat(10_000), "tc-1", "shell")],
            &limits(4_000, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        let twice =
            apply_tool_result_budget(once.clone(), &limits(4_000, 3.0), tmp.path(), &NONE_EXEMPT);

        assert_eq!(tool_content(&twice, "tc-1"), tool_content(&once, "tc-1"));
        assert_eq!(
            tool_content(&twice, "tc-1")
                .matches("... (truncated)")
                .count(),
            1,
            "the stage marker appears once, no matter how often the stage runs"
        );
    }

    #[test]
    fn test_apply_tool_result_budget_cites_persisted_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("tool-results")).unwrap();
        std::fs::write(tmp.path().join("tool-results/tc-9.txt"), "full").unwrap();

        let result = apply_tool_result_budget(
            vec![msg_tool_with_name(&"x".repeat(10_000), "tc-9", "shell")],
            &limits(4_000, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        let content = tool_content(&result, "tc-9");
        assert!(
            content.contains("full output: tool-results/tc-9.txt"),
            "truncation text must cite the persisted path, got tail: {}",
            &content[content.len().saturating_sub(80)..]
        );
    }

    #[test]
    fn test_apply_tool_result_budget_unknown_window_leaves_messages() {
        // Window 0 means "no anchor": acting would be guesswork.
        let tmp = tempfile::tempdir().unwrap();
        let body = "x".repeat(50_000);
        let result = apply_tool_result_budget(
            vec![msg_tool_with_name(&body, "tc-1", "shell")],
            &limits(0, 3.0),
            tmp.path(),
            &NONE_EXEMPT,
        );
        assert_eq!(tool_content(&result, "tc-1"), body);
    }
}

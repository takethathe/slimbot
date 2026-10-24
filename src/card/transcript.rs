//! Run transcript assembly with a byte budget.
//!
//! [`build_transcript`] turns the messages added during a single Run into the
//! text fed to a Card generation call. It applies the budget rules from the
//! spec:
//!
//! - a single tool result is truncated to `per_item` bytes (default 4 KB);
//! - serialized tool-call arguments are kept as just the tool name plus the
//!   args truncated to 512 characters (never the full call structure);
//! - when the joined transcript exceeds `total` bytes (default 64 KB), the
//!   earliest *middle* steps are dropped first, always keeping the first user
//!   message and the last assistant message — so the card is always grounded in
//!   "what was asked, what was answered".

use crate::session::Message;

/// Byte/character budgets for transcript assembly.
#[derive(Debug, Clone, Copy)]
pub struct TranscriptLimits {
    /// Max bytes for a single tool result.
    pub per_item: usize,
    /// Max total bytes for the joined transcript.
    pub total: usize,
}

impl Default for TranscriptLimits {
    fn default() -> Self {
        Self {
            per_item: 4 * 1024,
            total: 64 * 1024,
        }
    }
}

/// Max characters kept for a serialized tool-call argument blob.
const TOOL_ARGS_LIMIT: usize = 512;

/// Truncate to at most `max` bytes, snapping back to a UTF-8 char boundary and
/// appending a marker when truncation happens.
fn truncate_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // Find the largest end <= max that is a char boundary.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &s[..end])
}

/// Truncate to at most `max` characters.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}…[truncated]")
}

/// One rendered transcript segment plus whether it is pinned against budget
/// dropping (first user / last assistant are pinned).
struct Segment {
    text: String,
    pinned: bool,
}

/// Render the transcript's messages into a budget-constrained string.
pub fn build_transcript(messages: &[Message], limits: TranscriptLimits) -> String {
    // Render each message into one or more segments.
    let mut segments: Vec<Segment> = Vec::new();
    for msg in messages {
        match msg {
            Message::User { content, .. } => {
                segments.push(Segment {
                    text: format!("[user]\n{}", content.as_text()),
                    pinned: false,
                });
            }
            Message::Assistant {
                content,
                tool_calls,
                ..
            } => {
                if let Some(body) = content
                    && !body.trim().is_empty()
                {
                    segments.push(Segment {
                        text: format!("[assistant]\n{}", body),
                        pinned: false,
                    });
                }
                if let Some(calls) = tool_calls {
                    for call in calls {
                        let args = truncate_chars(&call.args.to_string(), TOOL_ARGS_LIMIT);
                        segments.push(Segment {
                            text: format!("[tool: {}]\n{}", call.name, args),
                            pinned: false,
                        });
                    }
                }
            }
            Message::Tool { name, content, .. } => {
                let label = name.clone().unwrap_or_else(|| "unknown".to_string());
                let body = truncate_bytes(content, limits.per_item);
                segments.push(Segment {
                    text: format!("[tool: {}]\n{}", label, body),
                    pinned: false,
                });
            }
            Message::System { .. } => {
                // System messages are not part of the Run transcript.
            }
        }
    }

    // Pin the first user segment and the last assistant segment.
    if let Some(first) = segments.iter().position(|s| s.text.starts_with("[user]")) {
        segments[first].pinned = true;
    }
    if let Some(last) = segments
        .iter()
        .rposition(|s| s.text.starts_with("[assistant]"))
    {
        segments[last].pinned = true;
    }

    // Enforce the total budget: drop the earliest non-pinned (middle) segments
    // until the joined transcript fits. Recomputing the joined length after
    // each drop keeps the arithmetic exact (segment counts per Run are small).
    let mut dropped: Vec<bool> = vec![false; segments.len()];
    let joined_len = |dropped: &Vec<bool>| -> usize {
        let texts: Vec<&str> = segments
            .iter()
            .zip(dropped.iter())
            .filter(|(_, d)| !**d)
            .map(|(s, _)| s.text.as_str())
            .collect();
        texts.join("\n\n").len()
    };
    let mut current_len = joined_len(&dropped);
    for cursor in 0..segments.len() {
        if current_len <= limits.total {
            break;
        }
        if !segments[cursor].pinned {
            dropped[cursor] = true;
            current_len = joined_len(&dropped);
        }
    }

    let kept: Vec<&str> = segments
        .iter()
        .zip(dropped.iter())
        .filter(|(_, d)| !**d)
        .map(|(s, _)| s.text.as_str())
        .collect();
    kept.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Message;
    use crate::tool::ToolCall;
    use serde_json::json;

    fn assistant_text(s: &str) -> Message {
        Message::assistant(Some(s.to_string()), None, None, None)
    }

    #[test]
    fn test_transcript_truncates_single_tool_result_to_limit() {
        let big = "x".repeat(10 * 1024);
        let msgs = vec![
            Message::user("question".to_string()),
            Message::tool(big.clone(), "call1".to_string(), Some("grep".to_string())),
            assistant_text("answer"),
        ];
        let limits = TranscriptLimits {
            per_item: 4 * 1024,
            total: 64 * 1024,
        };
        let out = build_transcript(&msgs, limits);
        // The tool segment should be truncated: the raw 10KB body must not appear.
        assert!(
            !out.contains(&big),
            "full 10KB tool result must be truncated"
        );
        assert!(out.contains("grep"), "tool name must be retained");
        assert!(out.contains("[truncated]"), "truncation marker expected");
        // Truncated tool body should be at most per_item + marker bytes.
        let tool_seg = out
            .split("\n\n")
            .find(|s| s.starts_with("[tool: grep]"))
            .unwrap();
        assert!(tool_seg.len() < 5 * 1024);
    }

    #[test]
    fn test_transcript_drops_earliest_middle_steps_when_over_budget() {
        // Build many middle segments to exceed a tiny total budget.
        let mut msgs = vec![Message::user("first question".to_string())];
        for i in 0..50 {
            msgs.push(Message::tool(
                "y".repeat(2000),
                format!("c{i}"),
                Some(format!("tool{i}")),
            ));
        }
        msgs.push(assistant_text("final answer"));
        let limits = TranscriptLimits {
            per_item: 4 * 1024,
            total: 10 * 1024,
        };
        let out = build_transcript(&msgs, limits);
        assert!(out.len() <= limits.total + 1024, "should be near budget");
        // Middle tools should have been dropped: not all present.
        assert!(
            !out.contains("tool0"),
            "earliest middle steps dropped first"
        );
        assert!(out.contains("first question"), "first user kept");
        assert!(out.contains("final answer"), "last assistant kept");
    }

    #[test]
    fn test_transcript_always_keeps_first_user_and_last_assistant() {
        // Even a single over-budget user + assistant pair is kept in full.
        let long = "z".repeat(3000);
        let msgs = vec![
            Message::user(format!("u-{long}")),
            assistant_text(&format!("a-{long}")),
        ];
        let limits = TranscriptLimits {
            per_item: 4 * 1024,
            total: 1024, // smaller than either message
        };
        let out = build_transcript(&msgs, limits);
        assert!(out.contains("u-zzz"), "first user must survive");
        assert!(out.contains("a-zzz"), "last assistant must survive");
    }

    #[test]
    fn test_transcript_includes_tool_names_without_full_args() {
        let huge_args = json!({ "cmd": "a".repeat(2000) });
        let call = ToolCall {
            id: "c1".to_string(),
            name: "shell".to_string(),
            args: huge_args,
        };
        let msgs = vec![
            Message::user("hi".to_string()),
            Message::assistant(None, Some(vec![call]), None, None),
            assistant_text("done"),
        ];
        let out = build_transcript(&msgs, TranscriptLimits::default());
        assert!(out.contains("[tool: shell]"), "tool name present");
        // The full 2000-char arg blob must be truncated to <= 512 chars.
        let seg = out
            .split("\n\n")
            .find(|s| s.starts_with("[tool: shell]"))
            .unwrap();
        // Body after the header line should not contain the whole repeated string.
        assert!(
            !seg.contains(&"a".repeat(600)),
            "serialized args must be truncated to 512 chars"
        );
    }
}

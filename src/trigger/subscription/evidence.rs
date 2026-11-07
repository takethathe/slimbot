//! The evidence budget and template rendering.
//!
//! Evidence is the **only** channel through which external content reaches a
//! model, so its limits are the security boundary: five items, two hundred
//! characters each, four kilobytes together. Over the limit is a truncation
//! with a warning, never an error — losing the sixth item is better than
//! losing the rule.

use serde_json::Value;

/// Maximum number of evidence items handed to an action.
pub const EVIDENCE_MAX_ITEMS: usize = 5;
/// Maximum characters of one evidence item's text.
pub const EVIDENCE_TEXT_MAX_CHARS: usize = 200;
/// `detail` plus all evidence, together.
pub const PAYLOAD_MAX_BYTES: usize = 4 * 1024;
/// A rendered template, after placeholders are filled in.
pub const RENDER_MAX_BYTES: usize = 8 * 1024;
/// Shown when a template asks for evidence and the verdict carried none.
pub const NO_EVIDENCE: &str = "(no evidence)";

/// A verdict after the budget has been applied.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Judgment {
    pub fire: bool,
    pub detail: String,
    /// Each item is `{source_id, at, text}`.
    pub evidence: Vec<Value>,
    /// Keys the script asks the host to remember as fired.
    pub keys: Vec<String>,
    /// What had to be cut, for the tick's warning list.
    pub truncated: Option<String>,
}

/// Validate and clamp one `judge()` return value.
pub fn clamp_verdict(raw: &Value) -> Result<Judgment, String> {
    let fire = raw
        .get("fire")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| "judge() must return {fire: true|false}".to_string())?;

    let raw_detail = raw.get("detail").and_then(|v| v.as_str()).unwrap_or("");
    let detail = truncate_chars(raw_detail, EVIDENCE_TEXT_MAX_CHARS * 4);

    let raw_items = match raw.get("evidence") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err("`evidence` must be an array when present".to_string()),
    };
    let keys = match raw.get("keys") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err("`keys` must be an array of strings when present".to_string()),
    };

    let mut warnings: Vec<String> = Vec::new();
    let mut evidence: Vec<Value> = Vec::new();
    let mut dropped_text = 0usize;
    for item in raw_items.iter().take(EVIDENCE_MAX_ITEMS) {
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let kept = truncate_chars(text, EVIDENCE_TEXT_MAX_CHARS);
        if kept != text {
            dropped_text += 1;
        }
        evidence.push(serde_json::json!({
            "source_id": item.get("source_id").and_then(|v| v.as_str()).unwrap_or(""),
            "at": item.get("at").and_then(|v| v.as_i64()).unwrap_or(0),
            "text": kept,
        }));
    }
    if raw_items.len() > EVIDENCE_MAX_ITEMS {
        warnings.push(format!(
            "evidence cut from {} to {EVIDENCE_MAX_ITEMS} item(s)",
            raw_items.len()
        ));
    }
    if dropped_text > 0 {
        warnings.push(format!(
            "{dropped_text} evidence text(s) shortened to {EVIDENCE_TEXT_MAX_CHARS} characters"
        ));
    }

    // Whole-payload budget: drop evidence from the tail until it fits. The
    // per-item limits are counted in characters, so a multi-byte verdict can
    // still cross this byte budget.
    let mut payload = render_payload(&detail, &evidence);
    let mut dropped_items = 0usize;
    while payload.len() > PAYLOAD_MAX_BYTES && !evidence.is_empty() {
        evidence.pop();
        dropped_items += 1;
        payload = render_payload(&detail, &evidence);
    }
    if dropped_items > 0 {
        warnings.push(format!(
            "{dropped_items} evidence item(s) dropped to fit the {}KB payload budget",
            PAYLOAD_MAX_BYTES / 1024
        ));
    }

    Ok(Judgment {
        fire,
        detail,
        evidence,
        keys,
        truncated: if warnings.is_empty() {
            None
        } else {
            Some(warnings.join("; "))
        },
    })
}

fn render_payload(detail: &str, evidence: &[Value]) -> String {
    let mut text = String::from(detail);
    for item in evidence {
        text.push_str(&item.to_string());
    }
    text
}

/// Evidence rendered for a template: one line per item, oldest-first as the
/// script produced them.
pub fn evidence_block(evidence: &[Value]) -> String {
    if evidence.is_empty() {
        return NO_EVIDENCE.to_string();
    }
    evidence
        .iter()
        .map(|item| {
            let source = item
                .get("source_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let at = item.get("at").and_then(|v| v.as_i64()).unwrap_or(0);
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            format!("- [{source} @ {}] {text}", iso_local(at))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Epoch milliseconds as a local RFC3339 timestamp.
pub fn iso_local(epoch_ms: i64) -> String {
    chrono::DateTime::<chrono::Local>::from(
        std::time::UNIX_EPOCH + std::time::Duration::from_millis(epoch_ms.max(0) as u64),
    )
    .to_rfc3339()
}

/// Fill the three allowed placeholders. The result is capped at 8KB: a template
/// that overflows is cut, and the caller is told, rather than handed to a model
/// as an unbounded prompt.
pub fn render_template(
    template: &str,
    evidence: &[Value],
    detail: &str,
    now_ms: i64,
) -> Result<String, String> {
    let block = evidence_block(evidence);
    let rendered = template
        .replace("{{evidence}}", &block)
        .replace("{{detail}}", detail)
        .replace("{{now}}", &iso_local(now_ms));
    if rendered.len() > RENDER_MAX_BYTES {
        let mut cut: String = rendered.chars().take(RENDER_MAX_BYTES).collect();
        cut.push_str("\n… (cut to the 8KB action budget)");
        return Ok(cut);
    }
    Ok(rendered)
}

/// Keep at most `max` characters, marking the cut with an ellipsis. Shared with
/// the act result contract, which applies the same text bound to `summary` and
/// to evidence text.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    let mut kept: String = text.chars().take(max.saturating_sub(1)).collect();
    kept.push('…');
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(source: &str, text: &str) -> Value {
        json!({"source_id": source, "at": 1_732_000_000_000i64, "text": text})
    }

    #[test]
    fn test_missing_fire_is_a_failure_not_a_silent_false() {
        let err = clamp_verdict(&json!({"detail": "hi"})).unwrap_err();
        assert!(err.contains("fire"), "{err}");
    }

    #[test]
    fn test_six_evidence_items_are_cut_to_five() {
        let evidence: Vec<Value> = (0..6).map(|i| item("src-1", &format!("n{i}"))).collect();
        let out = clamp_verdict(&json!({"fire": true, "evidence": evidence})).unwrap();
        assert_eq!(out.evidence.len(), EVIDENCE_MAX_ITEMS);
        let warning = out.truncated.expect("cutting must be reported");
        assert!(warning.contains("to 5"), "{warning}");
    }

    #[test]
    fn test_long_evidence_text_is_shortened_not_rejected() {
        let out = clamp_verdict(&json!({
            "fire": true,
            "evidence": [item("src-1", &"x".repeat(500))],
        }))
        .unwrap();
        let text = out.evidence[0]["text"].as_str().unwrap();
        assert_eq!(text.chars().count(), EVIDENCE_TEXT_MAX_CHARS);
        assert!(out.truncated.unwrap().contains("characters"));
    }

    #[test]
    fn test_payload_over_four_kilobytes_drops_from_the_tail() {
        // Per-item limits count characters, so five full items plus a full
        // detail in a multi-byte script still cross the four-kilobyte budget.
        let big = "字".repeat(EVIDENCE_TEXT_MAX_CHARS);
        let evidence: Vec<Value> = (0..EVIDENCE_MAX_ITEMS)
            .map(|_| item("src-1", &big))
            .collect();
        let out = clamp_verdict(&json!({
            "fire": true,
            "detail": "详".repeat(EVIDENCE_TEXT_MAX_CHARS * 4),
            "evidence": evidence,
        }))
        .unwrap();
        assert!(
            out.evidence.len() < EVIDENCE_MAX_ITEMS,
            "the payload budget must bite"
        );
        assert!(render_payload(&out.detail, &out.evidence).len() <= PAYLOAD_MAX_BYTES);
        let warning = out.truncated.expect("a drop must be reported");
        assert!(warning.contains("payload"), "{warning}");
    }

    #[test]
    fn test_evidence_block_format_and_empty_placeholder() {
        assert_eq!(evidence_block(&[]), NO_EVIDENCE);
        let block = evidence_block(&[item("src-7f3a", "rain likely")]);
        assert!(block.starts_with("- [src-7f3a @ "), "{block}");
        assert!(block.contains("rain likely"));
        assert!(block.contains("+"), "timestamp must be RFC3339: {block}");
    }

    #[test]
    fn test_template_renders_the_three_placeholders() {
        let out = render_template(
            "at {{now}}: {{detail}}\n{{evidence}}",
            &[item("src-1", "a")],
            "two new",
            1_732_000_000_000,
        )
        .unwrap();
        assert!(out.contains("two new"));
        assert!(out.contains("- [src-1 @ "));
        assert!(out.contains("at 2024-"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn test_template_over_budget_is_cut_with_a_note() {
        let out = render_template(&"z".repeat(RENDER_MAX_BYTES + 100), &[], "", 1).unwrap();
        assert!(out.contains("8KB action budget"));
    }

    #[test]
    fn test_keys_pass_through_for_the_fired_ledger() {
        let out = clamp_verdict(&json!({"fire": true, "keys": ["a", "b"]})).unwrap();
        assert_eq!(out.keys, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_evidence_that_is_not_an_array_is_an_error() {
        let err = clamp_verdict(&json!({"fire": true, "evidence": {"a": 1}})).unwrap_err();
        assert!(err.contains("array"), "{err}");
    }
}

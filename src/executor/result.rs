//! The `act()` result contract and the in-band failure classes.
//!
//! A script hands back `{ok, summary, evidence?}` and that object is
//! *intermediate product*: the calling subagent reads it, then writes the
//! message the owner eventually sees. Two consequences shape this module.
//!
//! **The shape is checked here, not in the sandbox.** The sandbox guarantees
//! parseable JSON within [`Limits::act`](crate::codemode::sandbox::Limits::act)'s
//! 16KB output ceiling and nothing else; the field rules live on the caller
//! side, right after `execute_async`, the way `clamp_verdict` owns a trigger
//! condition's `{fire, evidence?}`.
//!
//! **Evidence keeps its budget.** `act()` transmits whatever an external system
//! answered, so the item budget is the same one the collector and the trigger
//! condition use: at most [`EVIDENCE_MAX_ITEMS`] items, each text no longer than
//! [`EVIDENCE_TEXT_MAX_CHARS`], each item still under `ITEM_MAX_BYTES` once
//! serialized. Being an action rather than a collection does not exempt the
//! boundary. Cutting never fails a run: it shortens and says so, mirroring
//! `normalize` and `clamp_verdict`.
//!
//! **`summary` is bounded too.** It is the one field a script writes freely, and
//! it routinely quotes what an external system answered, so it is capped at
//! [`SUMMARY_MAX_CHARS`] — otherwise the 16KB output ceiling would be a budget a
//! hostile page could spend entirely on prose.
//!
//! Failures are in-band and classified, because "it failed" is not actionable:
//! [`FailureKind`] separates a refused action (approval) from a broken outside
//! world (an error answer) from a host gate that cut the run (timeout, spent
//! call budget, oversized result). Text is derived from the class, never
//! sniffed from a message, so a refusal can never be read back as a success.

use serde_json::{Map, Value};

use crate::codemode::error::{ScriptError, ScriptErrorKind};
use crate::codemode::sandbox::{AbortHandle, AbortKind};
use crate::trigger::source::normalize::{ITEM_MAX_BYTES, serialized_len, trim_to_budget};
use crate::trigger::subscription::evidence::{
    EVIDENCE_MAX_ITEMS, EVIDENCE_TEXT_MAX_CHARS, truncate_chars,
};

/// Most characters of `summary` kept.
///
/// `summary` is script-authored text that routinely quotes what an external
/// system answered, so without a cap the one unclipped field becomes a way
/// around every other bound: a script could spend the whole 16KB output ceiling
/// on prose from a hostile page. 500 characters holds a receipt and a one-line
/// judgement, which is all this field is for.
const SUMMARY_MAX_CHARS: usize = 500;

/// A validated `act()` result.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActResult {
    pub ok: bool,
    pub summary: String,
    /// Already trimmed to [`EVIDENCE_MAX_ITEMS`] items, [`EVIDENCE_TEXT_MAX_CHARS`]
    /// characters of text and [`ITEM_MAX_BYTES`] each.
    pub evidence: Vec<Value>,
    /// What had to be cut, rendered into the summary so the caller sees it.
    pub cut: Option<String>,
}

impl ActResult {
    /// The contract fields: exactly `{ok, summary, evidence?}`.
    pub fn into_json(self) -> Value {
        let mut map = Map::new();
        map.insert("ok".to_string(), Value::Bool(self.ok));
        let summary = match self.cut {
            Some(cut) => format!("{} [cut to fit: {cut}]", self.summary),
            None => self.summary,
        };
        map.insert("summary".to_string(), Value::String(summary));
        if !self.evidence.is_empty() {
            map.insert("evidence".to_string(), Value::Array(self.evidence));
        }
        Value::Object(map)
    }
}

/// Validate and shrink one `act()` return value.
///
/// `ok` and `summary` are structural: without them the caller has nothing to
/// report, so their absence is an error rather than a default. `evidence` is
/// optional and tolerant — a non-array is an error (the shape is declared), but
/// oversize items are trimmed and items that stay too large are dropped.
pub(crate) fn clamp_result(raw: &Value) -> Result<ActResult, String> {
    let Some(object) = raw.as_object() else {
        return Err("act() must return {ok, summary, evidence?}".to_string());
    };
    let Some(ok) = object.get("ok").and_then(|v| v.as_bool()) else {
        return Err(
            "act() must return {ok, summary, evidence?}: `ok` is missing or not a boolean"
                .to_string(),
        );
    };
    let Some(summary) = object.get("summary").and_then(|v| v.as_str()) else {
        return Err(
            "act() must return {ok, summary, evidence?}: `summary` is missing or not a string"
                .to_string(),
        );
    };

    let raw_items: Vec<Value> = match object.get("evidence") {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err("`evidence` must be an array when present".to_string()),
    };

    let mut cut: Vec<String> = Vec::new();
    let mut summary = summary.to_string();
    if summary.chars().count() > SUMMARY_MAX_CHARS {
        summary = truncate_chars(&summary, SUMMARY_MAX_CHARS);
        cut.push(format!(
            "summary shortened to {SUMMARY_MAX_CHARS} characters"
        ));
    }

    let mut evidence: Vec<Value> = Vec::with_capacity(raw_items.len());
    let mut trimmed = 0usize;
    let mut dropped = 0usize;
    let mut clipped = 0usize;
    let raw_count = raw_items.len();
    for mut item in raw_items.into_iter().take(EVIDENCE_MAX_ITEMS) {
        if serialized_len(&item) > ITEM_MAX_BYTES {
            if trim_to_budget(&mut item, ITEM_MAX_BYTES) {
                trimmed += 1;
            } else {
                dropped += 1;
                continue;
            }
        }
        if clip_text(&mut item, EVIDENCE_TEXT_MAX_CHARS) {
            clipped += 1;
        }
        evidence.push(item);
    }
    if raw_count > EVIDENCE_MAX_ITEMS {
        cut.push(format!(
            "evidence cut from {raw_count} to {EVIDENCE_MAX_ITEMS} item(s)"
        ));
    }
    if trimmed > 0 {
        cut.push(format!(
            "{trimmed} evidence item(s) shortened to the {}KB item limit",
            ITEM_MAX_BYTES / 1024
        ));
    }
    if clipped > 0 {
        cut.push(format!(
            "{clipped} evidence text(s) shortened to {EVIDENCE_TEXT_MAX_CHARS} characters"
        ));
    }
    if dropped > 0 {
        cut.push(format!(
            "{dropped} evidence item(s) dropped as still over the {}KB item limit",
            ITEM_MAX_BYTES / 1024
        ));
    }

    Ok(ActResult {
        ok,
        summary,
        evidence,
        cut: (!cut.is_empty()).then(|| cut.join("; ")),
    })
}

/// Shorten every string in `item` to `max` characters, reporting whether any was
/// cut. An evidence item is free-form JSON from an external system, so the text
/// bound has to apply wherever the script put the text.
fn clip_text(item: &mut Value, max: usize) -> bool {
    match item {
        Value::String(text) => {
            if text.chars().count() <= max {
                return false;
            }
            *text = truncate_chars(text, max);
            true
        }
        Value::Array(values) => values
            .iter_mut()
            .fold(false, |cut, value| clip_text(value, max) | cut),
        Value::Object(map) => map
            .values_mut()
            .fold(false, |cut, value| clip_text(value, max) | cut),
        _ => false,
    }
}

/// The classes a failed run is reported under.
///
/// The first three are spec G's: an approval refusal, an external failure, a
/// host gate. The last two are host-side cases that sit outside those classes
/// but must not be silent: a script that does not run cleanly, and a call that
/// never started because its arguments or the executor itself were not usable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    /// ① A policy gate refused a call; nothing was executed.
    ApprovalRefused,
    /// ② The outside world answered with an error (4xx/5xx) or did not answer.
    External,
    /// ③ A host gate cut the run: timeout, spent call budget, oversized result,
    /// or a write outside the executor's `allow_connectors` ceiling.
    HostStopped,
    /// `act.js` raised, or returned something that is not the contract shape.
    ScriptError,
    /// The run never started: bad args, unknown or broken executor.
    Rejected,
}

impl FailureKind {
    /// Machine-readable class, carried in the result as `failure`.
    pub fn label(self) -> &'static str {
        match self {
            FailureKind::ApprovalRefused => "approval_refused",
            FailureKind::External => "external_error",
            FailureKind::HostStopped => "host_stopped",
            FailureKind::ScriptError => "script_error",
            FailureKind::Rejected => "rejected",
        }
    }

    /// Names the root cause before the detail, so the classes are
    /// distinguishable in the first line of a report.
    fn headline(self) -> &'static str {
        match self {
            FailureKind::ApprovalRefused => "approval refused",
            FailureKind::External => "external call failed",
            FailureKind::HostStopped => "stopped by a host limit",
            FailureKind::ScriptError => "the action script failed",
            FailureKind::Rejected => "not started",
        }
    }

    /// What to do next. Never "retry the same call".
    fn next_step(self) -> &'static str {
        match self {
            FailureKind::ApprovalRefused => {
                "this run is unattended, so the action cannot be confirmed here; report it to the owner instead of retrying it"
            }
            FailureKind::External => {
                "the target refused or broke the call; check the arguments and the target, fix `act.js` with `executor update`, then run again"
            }
            FailureKind::HostStopped => {
                "narrow the action (fewer outward calls, a shorter result, or a write on a server \
                 the executor's `allow_connectors` lists) or raise `timeout_s`, then run again"
            }
            FailureKind::ScriptError => "fix `act.js` with `executor update`, then run again",
            FailureKind::Rejected => {
                "check `executor list` and the `args` you sent, then call `run` again"
            }
        }
    }
}

/// One classified failure, rendered in-band as `{ok:false, summary, failure}`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActFailure {
    kind: FailureKind,
    detail: String,
}

impl ActFailure {
    pub fn new(kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }

    /// ① approval refused the call.
    pub fn approval(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::ApprovalRefused, detail)
    }

    /// ② an outward call failed.
    pub fn external(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::External, detail)
    }

    /// ③ a host gate stopped the run.
    pub fn stopped(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::HostStopped, detail)
    }

    /// `act.js` itself failed.
    pub fn script(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::ScriptError, detail)
    }

    /// The run never started.
    pub fn rejected(detail: impl Into<String>) -> Self {
        Self::new(FailureKind::Rejected, detail)
    }

    /// `{ok: false, summary: "<headline>: <detail>. Next: <what to do>",
    /// "failure": "<class>"}`.
    pub fn into_json(self) -> Value {
        serde_json::json!({
            "ok": false,
            "summary": format!(
                "{}: {}. Next: {}",
                self.kind.headline(),
                self.detail.trim_end_matches('.'),
                self.kind.next_step()
            ),
            "failure": self.kind.label(),
        })
    }

    /// Map a sandbox failure onto a class.
    ///
    /// `outward` is what the host recorded about the outward call that did not
    /// answer as needed: a script exception caused by one under it is an
    /// external failure, not an authoring bug. The outward fact is the whole
    /// detail in that case — the exception is its consequence, and repeating it
    /// only buries the cause under engine boilerplate.
    pub fn from_script_error(error: &ScriptError, outward: Option<String>) -> Self {
        match error.kind {
            ScriptErrorKind::Exception => match outward {
                Some(outward) => Self::external(outward),
                None => Self::script(error.message.clone()),
            },
            // Timeout, memory and output ceilings are all host gates; `aborted`
            // usually travels through the abort handle instead, and lands here
            // only when the caller has no reason string.
            ScriptErrorKind::Timeout
            | ScriptErrorKind::Memory
            | ScriptErrorKind::Output
            | ScriptErrorKind::Aborted => Self::stopped(error.message.clone()),
            ScriptErrorKind::Syntax | ScriptErrorKind::MissingEntry | ScriptErrorKind::Internal => {
                Self::script(error.message.clone())
            }
        }
    }
}

/// The in-band intermediate product for one finished run.
///
/// Success goes through [`clamp_result`]; a failure is classified from the abort
/// handle first (it knows *who* stopped the run) and from the script error
/// second.
pub(crate) fn outcome_to_json(
    outcome: Result<&Value, &ScriptError>,
    abort: &AbortHandle,
    outward: Option<String>,
) -> Value {
    match outcome {
        Ok(raw) => match clamp_result(raw) {
            Ok(acted) => acted.into_json(),
            Err(problem) => ActFailure::script(problem).into_json(),
        },
        Err(error) => failure_of(error, abort, outward).into_json(),
    }
}

fn failure_of(error: &ScriptError, abort: &AbortHandle, outward: Option<String>) -> ActFailure {
    if let Some(reason) = abort.reason() {
        return match abort.kind() {
            Some(AbortKind::Refused) => ActFailure::approval(reason),
            _ => ActFailure::stopped(reason),
        };
    }
    ActFailure::from_script_error(error, outward)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn summary_of(value: &Value) -> &str {
        value["summary"].as_str().unwrap()
    }

    #[test]
    fn test_result_keeps_ok_summary_and_evidence() {
        let out = clamp_result(&json!({
            "ok": true,
            "summary": "opened issue #42 at https://example.test/issues/42",
            "evidence": [{"title": "bug report"}],
        }))
        .unwrap()
        .into_json();
        assert_eq!(out["ok"], true);
        assert!(summary_of(&out).starts_with("opened issue #42"));
        assert_eq!(out["evidence"][0]["title"], "bug report");
        assert_eq!(out.get("failure"), None);
    }

    #[test]
    fn test_result_without_evidence_omits_the_key() {
        let out = clamp_result(&json!({"ok": false, "summary": "nothing to do"}))
            .unwrap()
            .into_json();
        assert_eq!(out, json!({"ok": false, "summary": "nothing to do"}));
    }

    #[test]
    fn test_result_that_is_not_an_object_is_rejected() {
        let err = clamp_result(&json!("done")).unwrap_err();
        assert!(err.contains("{ok, summary, evidence?}"), "{err}");
    }

    #[test]
    fn test_every_result_contract_error_names_the_same_shape() {
        // A script author reads one of these three and rewrites `act()`, so all
        // three have to spell the contract the same way; a mangled shape in one
        // of them sends the author after a field that is not there.
        for raw in [
            json!("done"),
            json!({"summary": "did it"}),
            json!({"ok": true}),
        ] {
            let err = clamp_result(&raw).unwrap_err();
            assert!(err.contains("{ok, summary, evidence?}"), "{raw}: {err}");
        }
    }

    #[test]
    fn test_result_without_ok_is_rejected() {
        let err = clamp_result(&json!({"summary": "did it"})).unwrap_err();
        assert!(err.contains("`ok`"), "{err}");
    }

    #[test]
    fn test_result_without_summary_is_rejected() {
        let err = clamp_result(&json!({"ok": true})).unwrap_err();
        assert!(err.contains("`summary`"), "{err}");
    }

    #[test]
    fn test_evidence_that_is_not_an_array_is_rejected() {
        let err =
            clamp_result(&json!({"ok": true, "summary": "s", "evidence": {"a": 1}})).unwrap_err();
        assert!(err.contains("`evidence`"), "{err}");
    }

    #[test]
    fn test_evidence_over_the_item_limit_is_trimmed_and_reported() {
        let out = clamp_result(&json!({
            "ok": true,
            "summary": "read the page",
            "evidence": [{"url": "https://example.test/a", "text": "x".repeat(10_000)}],
        }))
        .unwrap()
        .into_json();

        let item = &out["evidence"][0];
        assert!(
            serialized_len(item) <= ITEM_MAX_BYTES,
            "item kept {} bytes",
            serialized_len(item)
        );
        // Shape survives: only the long text was cut.
        assert_eq!(item["url"], "https://example.test/a");
        let summary = summary_of(&out);
        assert!(summary.contains("4KB"), "{summary}");
        assert!(summary.contains("shortened"), "{summary}");
    }

    #[test]
    fn test_evidence_item_that_cannot_be_trimmed_is_dropped_and_reported() {
        // An array of many short numbers has no long string to shorten, so the
        // item is dropped instead of being passed through oversized.
        let numbers: Vec<Value> = (0..4_000).map(|i| json!(i)).collect();
        let out = clamp_result(&json!({
            "ok": true,
            "summary": "counted",
            "evidence": [{"values": numbers}, {"kept": true}],
        }))
        .unwrap()
        .into_json();
        assert_eq!(out["evidence"].as_array().unwrap().len(), 1);
        assert_eq!(out["evidence"][0]["kept"], true);
        assert!(summary_of(&out).contains("dropped"), "{out}");
    }

    #[test]
    fn test_the_three_failure_classes_are_distinguishable() {
        // Independent truth: the labels spec G names, and a text a reader can
        // route on without knowing the code.
        for (failure, label, headline) in [
            (
                ActFailure::approval("`mcp__github__create_issue` needs owner approval"),
                "approval_refused",
                "approval refused",
            ),
            (
                ActFailure::external("POST https://api.test/items answered HTTP 503"),
                "external_error",
                "external call failed",
            ),
            (
                ActFailure::stopped("script exceeded its 1000ms budget"),
                "host_stopped",
                "stopped by a host limit",
            ),
        ] {
            let out = failure.into_json();
            assert_eq!(out["ok"], false);
            assert_eq!(out["failure"], label);
            let summary = summary_of(&out);
            assert!(summary.starts_with(headline), "{label}: {summary}");
            assert!(summary.contains("Next: "), "{label}: {summary}");
            // The detail travels verbatim, so the root cause is not lost.
            assert!(summary.contains('.') && summary.len() > headline.len());
        }
    }

    #[test]
    fn test_a_refused_abort_is_reported_as_an_approval_refusal() {
        let abort = AbortHandle::new();
        abort.refuse("`mcp__github__create_issue` needs owner approval");
        let error = ScriptError::aborted("`mcp__github__create_issue` needs owner approval");
        let out = outcome_to_json(Err(&error), &abort, None);
        assert_eq!(out["failure"], "approval_refused");
        assert!(summary_of(&out).contains("needs owner approval"));
        assert!(summary_of(&out).contains("unattended"));
    }

    #[test]
    fn test_a_stopped_abort_is_reported_as_a_host_stop() {
        let abort = AbortHandle::new();
        abort.stop("act() asked for more than 20 external calls");
        let error = ScriptError::aborted("act() asked for more than 20 external calls");
        let out = outcome_to_json(Err(&error), &abort, None);
        assert_eq!(out["failure"], "host_stopped");
        assert!(summary_of(&out).contains("more than 20 external calls"));
    }

    #[test]
    fn test_a_timeout_is_a_host_stop() {
        let out = outcome_to_json(Err(&ScriptError::timeout(1_500)), &AbortHandle::new(), None);
        assert_eq!(out["failure"], "host_stopped");
        assert!(summary_of(&out).contains("1500ms"));
    }

    #[test]
    fn test_an_oversized_result_is_a_host_stop() {
        let error = ScriptError::output(20_000, 16_384);
        let out = outcome_to_json(Err(&error), &AbortHandle::new(), None);
        assert_eq!(out["failure"], "host_stopped");
        assert!(summary_of(&out).contains("16384"));
    }

    #[test]
    fn test_a_script_exception_with_an_outward_failure_is_external() {
        let error = ScriptError::new(ScriptErrorKind::Exception, "Error: boom");
        let out = outcome_to_json(
            Err(&error),
            &AbortHandle::new(),
            Some("GET https://api.test/x answered HTTP 500".to_string()),
        );
        assert_eq!(out["failure"], "external_error");
        // The outward fact is the root cause; the engine's echo of it is not
        // repeated into the report.
        let summary = summary_of(&out);
        assert_eq!(
            summary,
            "external call failed: GET https://api.test/x answered HTTP 500. Next: the target \
             refused or broke the call; check the arguments and the target, fix `act.js` with \
             `executor update`, then run again"
        );
    }

    #[test]
    fn test_a_script_exception_without_provenance_is_a_script_error() {
        let error = ScriptError::new(ScriptErrorKind::Exception, "Error: boom");
        let out = outcome_to_json(Err(&error), &AbortHandle::new(), None);
        assert_eq!(out["failure"], "script_error");
        assert!(summary_of(&out).contains("boom"));
    }

    #[test]
    fn test_every_script_error_kind_lands_in_a_failure_class() {
        // The class is what a caller routes on. An authoring bug is the script's
        // problem; a host-declared stop is the host's, even when the abort
        // handle carries no reason of its own.
        let abort = AbortHandle::new();
        for (error, label) in [
            (
                ScriptError::new(ScriptErrorKind::Syntax, "SyntaxError: unexpected token"),
                "script_error",
            ),
            (ScriptError::missing_entry("act"), "script_error"),
            (
                ScriptError::internal("script thread vanished without a result"),
                "script_error",
            ),
            (
                ScriptError::aborted("script ran past its budget"),
                "host_stopped",
            ),
        ] {
            let out = outcome_to_json(Err(&error), &abort, None);
            assert_eq!(out["ok"], false);
            assert_eq!(out["failure"], label, "{}: {out}", error.message);
            // The reason travels verbatim, so the reader is not left with the
            // class alone.
            assert!(summary_of(&out).contains(&error.message), "{out}");
            assert!(summary_of(&out).contains("Next: "), "{out}");
        }
    }

    #[test]
    fn test_a_result_that_breaks_the_shape_is_a_script_error() {
        let raw = json!({"summary": "no ok field"});
        let out = outcome_to_json(Ok(&raw), &AbortHandle::new(), None);
        assert_eq!(out["failure"], "script_error");
        assert!(summary_of(&out).contains("`ok`"));
    }

    #[test]
    fn test_a_run_that_never_started_is_rejected() {
        let out = ActFailure::rejected("`args.title` is required").into_json();
        assert_eq!(out["failure"], "rejected");
        assert_eq!(out["ok"], false);
        assert!(summary_of(&out).starts_with("not started: `args.title` is required"));
    }

    #[test]
    fn test_evidence_beyond_the_item_count_is_cut_and_reported() {
        // The same five-item budget the trigger layer puts on evidence: one run
        // must not hand the caller an unbounded pile of external text.
        let items: Vec<Value> = (0..8).map(|i| json!({"n": i})).collect();
        let out = clamp_result(&json!({"ok": true, "summary": "read", "evidence": items}))
            .unwrap()
            .into_json();
        assert_eq!(
            out["evidence"].as_array().unwrap().len(),
            EVIDENCE_MAX_ITEMS
        );
        let summary = summary_of(&out);
        assert!(summary.contains("evidence cut from 8 to 5"), "{summary}");
    }

    #[test]
    fn test_evidence_text_is_clipped_to_the_trigger_character_bound() {
        // 4KB per item is the byte ceiling; the trigger layer's per-text bound is
        // the one that keeps a single malicious page from owning the budget.
        let out = clamp_result(&json!({
            "ok": true,
            "summary": "read the page",
            "evidence": [{"url": "https://example.test/a", "text": "x".repeat(2_000)}],
        }))
        .unwrap()
        .into_json();

        let item = &out["evidence"][0];
        assert_eq!(
            item["text"].as_str().unwrap().chars().count(),
            EVIDENCE_TEXT_MAX_CHARS
        );
        // Shape survives: the short fields are untouched.
        assert_eq!(item["url"], "https://example.test/a");
        let summary = summary_of(&out);
        assert!(
            summary.contains(&format!("{EVIDENCE_TEXT_MAX_CHARS} characters")),
            "{summary}"
        );
    }

    #[test]
    fn test_a_summary_over_the_contract_is_shortened_and_reported() {
        // Otherwise a script could smuggle kilobytes of external prose through
        // the one field that has no per-item clipping.
        let out = clamp_result(&json!({"ok": true, "summary": "y".repeat(5_000)}))
            .unwrap()
            .into_json();

        let summary = summary_of(&out);
        let kept = summary.split(" [cut to fit: ").next().unwrap();
        assert_eq!(kept.chars().count(), SUMMARY_MAX_CHARS);
        assert!(
            summary.contains("summary shortened to 500 characters"),
            "{summary}"
        );
    }

    #[test]
    fn test_a_summary_at_the_cap_is_left_alone() {
        let exact = "z".repeat(SUMMARY_MAX_CHARS);
        let out = clamp_result(&json!({"ok": true, "summary": exact}))
            .unwrap()
            .into_json();
        assert_eq!(summary_of(&out), exact);
        assert!(!summary_of(&out).contains("cut to fit"));
    }
}

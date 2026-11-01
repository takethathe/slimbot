//! Fingerprint-level audit of memory writes: the data behind Dream's trace span.
//!
//! Dream's pass is not observable today: its `DreamResult` carries only
//! success/elapsed/message. Instead of writing a second copy of "what we think
//! happened" next to the documents (a second source of truth that would drift
//! from the owner's hand edits), the audit is **shaped as trace attributes**:
//! one record per write — document, old content fingerprint, new content
//! fingerprint, and the `EditOutcome`. Full text is never copied; the snapshot
//! directory already holds the old content when someone needs it.
//!
//! This module only records. Judging lives with the caller
//! ([`MemoryWriteAudit::success`]) and is **structural**: a run failed when the
//! run itself did not finish or a write errored. It is never derived from the
//! text the model emitted, and never from a count of changed entries (zero
//! changes is a legitimate, successful pass).

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use parking_lot::Mutex;

use super::backend::EditOutcome;

/// What happened to one document write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The document was written (full rewrite, or a unique in-place replace).
    Applied,
    /// `memory_edit` found no match — usually a re-run, never data loss.
    NotFound,
    /// `memory_edit` matched more than once; the value is the match count.
    Ambiguous(usize),
    /// The write path returned an error.
    Failed(String),
}

impl WriteOutcome {
    pub fn from_edit(outcome: EditOutcome) -> Self {
        match outcome {
            EditOutcome::Replaced => Self::Applied,
            EditOutcome::NotFound => Self::NotFound,
            EditOutcome::Ambiguous(n) => Self::Ambiguous(n),
        }
    }

    /// Short, stable string used in trace attributes.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::NotFound => "not_found",
            Self::Ambiguous(_) => "ambiguous",
            Self::Failed(_) => "failed",
        }
    }

    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One audited write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentWrite {
    /// Workspace-relative document path (`memory/MEMORY.md`).
    pub document: String,
    /// Fingerprint of the content before the write (`None` = no document yet).
    pub old_fingerprint: Option<String>,
    /// Fingerprint of the content after the write (`None` = nothing written).
    pub new_fingerprint: Option<String>,
    pub outcome: WriteOutcome,
}

impl DocumentWrite {
    /// Whether this write actually moved the document content.
    ///
    /// Only an applied write can be a change: `NotFound` / `Ambiguous` leave the
    /// document untouched, and `Failed` has no reliable "after" to compare.
    /// Counting a fingerprint mismatch alone would flag a no-match edit (whose
    /// new content is `None`) as a change and overstate the pass's blast radius.
    pub fn changed(&self) -> bool {
        matches!(self.outcome, WriteOutcome::Applied)
            && self.old_fingerprint != self.new_fingerprint
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "document": self.document,
            "old_fingerprint": self.old_fingerprint,
            "new_fingerprint": self.new_fingerprint,
            "outcome": self.outcome.as_str(),
            "changed": self.changed(),
        })
    }
}

/// Collects the writes of one Dream pass.
#[derive(Default)]
pub struct MemoryWriteAudit {
    writes: Mutex<Vec<DocumentWrite>>,
}

impl MemoryWriteAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget everything (start of a pass).
    pub fn clear(&self) {
        self.writes.lock().clear();
    }

    pub fn record(&self, write: DocumentWrite) {
        self.writes.lock().push(write);
    }

    /// Record a write from its two contents and its outcome.
    pub fn record_contents(
        &self,
        document: &str,
        before: Option<&str>,
        after: Option<&str>,
        outcome: WriteOutcome,
    ) {
        self.record(DocumentWrite {
            document: document.to_string(),
            old_fingerprint: before.map(fingerprint),
            new_fingerprint: after.map(fingerprint),
            outcome,
        });
    }

    pub fn writes(&self) -> Vec<DocumentWrite> {
        self.writes.lock().clone()
    }

    /// Whether any write returned an error.
    pub fn failed_write(&self) -> Option<DocumentWrite> {
        self.writes
            .lock()
            .iter()
            .find(|w| w.outcome.is_failure())
            .cloned()
    }

    /// Structure-based success judgement.
    ///
    /// `completed` states that the run finished normally (no transport failure,
    /// no iteration cap). A pass that wrote nothing is still successful: "the
    /// memory was already correct" is a legitimate outcome, and re-running on
    /// it would be the count-driven loop this replaces.
    pub fn success(&self, completed: bool) -> bool {
        completed && self.failed_write().is_none()
    }

    /// Flat span attributes: `memory.writes` (per-document fingerprints) plus
    /// `memory.changed` (the documents whose content actually moved).
    pub fn span_attributes(&self) -> BTreeMap<String, serde_json::Value> {
        let writes = self.writes();
        let mut attributes = BTreeMap::new();
        attributes.insert(
            "memory.writes".to_string(),
            serde_json::Value::Array(writes.iter().map(DocumentWrite::to_json).collect()),
        );
        attributes.insert(
            "memory.changed".to_string(),
            serde_json::Value::Array(
                writes
                    .iter()
                    .filter(|w| w.changed())
                    .map(|w| serde_json::Value::String(w.document.clone()))
                    .collect(),
            ),
        );
        if let Some(failed) = writes.iter().find(|w| w.outcome.is_failure())
            && let WriteOutcome::Failed(message) = &failed.outcome
        {
            attributes.insert(
                "memory.error".to_string(),
                serde_json::Value::String(message.clone()),
            );
        }
        attributes
    }
}

/// Content fingerprint for the audit.
///
/// `DefaultHasher` on purpose, matching the derived index: this identity is
/// only compared inside one process (before/after of one write, one span), so a
/// stable-across-releases hash would be a promise with no consumer — and a new
/// crypto dependency for it.
pub fn fingerprint(content: &str) -> String {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_is_stable_for_equal_content_and_differs_otherwise() {
        assert_eq!(fingerprint("alpha"), fingerprint("alpha"));
        assert_ne!(fingerprint("alpha"), fingerprint("alpha "));
        assert_eq!(fingerprint("alpha").len(), 16);
    }

    #[test]
    fn test_write_outcome_maps_edit_outcomes() {
        assert_eq!(
            WriteOutcome::from_edit(EditOutcome::Replaced),
            WriteOutcome::Applied
        );
        assert_eq!(
            WriteOutcome::from_edit(EditOutcome::NotFound),
            WriteOutcome::NotFound
        );
        assert_eq!(
            WriteOutcome::from_edit(EditOutcome::Ambiguous(3)),
            WriteOutcome::Ambiguous(3)
        );
        assert!(!WriteOutcome::NotFound.is_failure());
        assert!(WriteOutcome::Failed("disk".to_string()).is_failure());
    }

    #[test]
    fn test_audit_success_is_structural_and_zero_writes_is_success() {
        let audit = MemoryWriteAudit::new();
        assert!(
            audit.success(true),
            "a pass that changed nothing is a legitimate outcome"
        );
        assert!(
            !audit.success(false),
            "a run that did not finish is not a success"
        );

        audit.record_contents(
            "memory/MEMORY.md",
            Some("a"),
            Some("b"),
            WriteOutcome::Applied,
        );
        assert!(audit.success(true));

        audit.record_contents(
            "SOUL.md",
            Some("a"),
            None,
            WriteOutcome::Failed("disk full".to_string()),
        );
        assert!(
            !audit.success(true),
            "a write that errored means the pass failed"
        );
    }

    #[test]
    fn test_audit_records_fingerprints_not_content() {
        let audit = MemoryWriteAudit::new();
        audit.record_contents(
            "memory/MEMORY.md",
            Some("secret before"),
            Some("secret after"),
            WriteOutcome::Applied,
        );

        let writes = audit.writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].document, "memory/MEMORY.md");
        assert_eq!(
            writes[0].old_fingerprint,
            Some(fingerprint("secret before"))
        );
        assert_eq!(writes[0].new_fingerprint, Some(fingerprint("secret after")));
        assert!(writes[0].changed());

        let serialized = serde_json::to_string(&audit.span_attributes()).unwrap();
        assert!(!serialized.contains("secret before"));
        assert!(!serialized.contains("secret after"));
        assert!(serialized.contains(&fingerprint("secret after")));
    }

    #[test]
    fn test_audit_span_attributes_list_changed_documents_only() {
        let audit = MemoryWriteAudit::new();
        audit.record_contents(
            "memory/MEMORY.md",
            Some("same"),
            Some("same"),
            WriteOutcome::Applied,
        );
        audit.record_contents("USER.md", Some("a"), Some("b"), WriteOutcome::Applied);
        audit.record_contents("SOUL.md", Some("x"), None, WriteOutcome::NotFound);

        let attributes = audit.span_attributes();
        assert_eq!(
            attributes["memory.changed"],
            serde_json::json!(["USER.md"]),
            "an unchanged rewrite is not a change"
        );
        assert_eq!(attributes["memory.writes"].as_array().unwrap().len(), 3);
        assert_eq!(attributes["memory.writes"][2]["outcome"], "not_found");
        assert!(!attributes.contains_key("memory.error"));
    }

    #[test]
    fn test_audit_clear_starts_a_fresh_pass() {
        let audit = MemoryWriteAudit::new();
        audit.record_contents(
            "memory/MEMORY.md",
            Some("a"),
            Some("b"),
            WriteOutcome::Applied,
        );
        audit.clear();
        assert!(audit.writes().is_empty());
        assert!(audit.success(true));
    }
}

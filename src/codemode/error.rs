//! Structured script failures.
//!
//! Every way a script can fail is reported through [`ScriptError`] so callers
//! never have to inspect engine internals. The distinction between `timeout`
//! and `memory` cannot come from the engine: QuickJS reports an interrupt and
//! an exhausted allocation as the same generic `InternalError` exception. The
//! watchdog that fires on the deadline therefore sets its own flag, and
//! [`classify`] trusts that flag first.

use std::fmt;

use super::redact::Redactor;

/// Machine-readable failure class. `code()` is what ends up in logs, history
/// and state files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptErrorKind {
    /// The script text does not compile.
    Syntax,
    /// The script threw, or returned something that is not JSON.
    Exception,
    /// The deadline expired (script spinning or a host call in flight).
    Timeout,
    /// The runtime hit its memory ceiling.
    Memory,
    /// The JSON result was larger than the caller's budget.
    Output,
    /// The script did not define the entry function.
    MissingEntry,
    /// A host function stopped the run (spent call budget, refused call). The
    /// reason travels in the message; the caller has the [`AbortHandle`] and
    /// therefore knows which kind it was.
    ///
    /// [`AbortHandle`]: super::sandbox::AbortHandle
    Aborted,
    /// The sandbox itself failed (thread panic, engine start-up, size limits).
    Internal,
}

impl ScriptErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            ScriptErrorKind::Syntax => "syntax",
            ScriptErrorKind::Exception => "exception",
            ScriptErrorKind::Timeout => "timeout",
            ScriptErrorKind::Memory => "memory",
            ScriptErrorKind::Output => "output",
            ScriptErrorKind::MissingEntry => "missing_entry",
            ScriptErrorKind::Aborted => "aborted",
            ScriptErrorKind::Internal => "internal",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptError {
    pub kind: ScriptErrorKind,
    pub message: String,
}

impl ScriptError {
    pub fn new(kind: ScriptErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn timeout(budget_ms: u128) -> Self {
        Self::new(
            ScriptErrorKind::Timeout,
            format!("script exceeded its {budget_ms}ms budget"),
        )
    }

    pub fn output(size: usize, limit: usize) -> Self {
        Self::new(
            ScriptErrorKind::Output,
            format!("script returned {size} bytes, over the {limit} byte limit"),
        )
    }

    pub fn missing_entry(entry: &str) -> Self {
        Self::new(
            ScriptErrorKind::MissingEntry,
            format!("script does not define a `{entry}` function"),
        )
    }

    /// The run was stopped by a host function through its abort handle.
    pub fn aborted(reason: impl Into<String>) -> Self {
        Self::new(ScriptErrorKind::Aborted, reason)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ScriptErrorKind::Internal, message)
    }

    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code(), self.message)
    }
}

impl std::error::Error for ScriptError {}

/// Map an engine failure onto a [`ScriptError`].
///
/// `timed_out` is our own flag, set by the watchdog (or by the outer deadline
/// branch) *before* the engine is interrupted, so a spinning script is never
/// misreported as a user exception. Memory exhaustion has no equivalent hook:
/// QuickJS raises `InternalError: out of memory` from its allocator, and that
/// message is the only signal that survives.
pub fn classify(
    engine_message: &str,
    timed_out: bool,
    budget_ms: u128,
    redactor: &Redactor,
) -> ScriptError {
    if timed_out {
        return ScriptError::timeout(budget_ms);
    }
    let lower = engine_message.to_ascii_lowercase();
    if lower.contains("out of memory") {
        return ScriptError::new(ScriptErrorKind::Memory, redactor.redact(engine_message));
    }
    let kind = if lower.contains("syntaxerror") {
        ScriptErrorKind::Syntax
    } else {
        ScriptErrorKind::Exception
    };
    ScriptError::new(kind, redactor.redact(engine_message))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redactor() -> Redactor {
        Redactor::new(["s3cr3t"])
    }

    #[test]
    fn test_classify_timeout_flag_wins_over_engine_message() {
        let err = classify("InternalError: interrupted", true, 250, &redactor());
        assert_eq!(err.code(), "timeout");
        assert!(err.message.contains("250ms"));
    }

    #[test]
    fn test_classify_interrupted_without_flag_is_exception() {
        let err = classify("Error: InternalError: interrupted", false, 250, &redactor());
        assert_eq!(err.code(), "exception");
    }

    #[test]
    fn test_classify_out_of_memory_maps_to_memory() {
        let err = classify(
            "Error: InternalError: out of memory",
            false,
            250,
            &redactor(),
        );
        assert_eq!(err.code(), "memory");
    }

    #[test]
    fn test_classify_syntax_error() {
        let err = classify("SyntaxError: unexpected token", false, 250, &redactor());
        assert_eq!(err.code(), "syntax");
    }

    #[test]
    fn test_classify_redacts_secrets_in_message() {
        let err = classify("Error: bad token s3cr3t", false, 250, &redactor());
        assert_eq!(err.code(), "exception");
        assert!(!err.message.contains("s3cr3t"));
        assert!(err.message.contains("***"));
    }
}

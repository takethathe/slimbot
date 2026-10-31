//! Secret scrubbing.
//!
//! Scripts read credentials through `ctx.secret(name)`. Whatever they do with
//! the value afterwards, it must not reach a return value, a log line or an
//! error message, so every outbound string passes through a [`Redactor`].
//! Redaction is best-effort substring replacement: it stops accidental echoes,
//! not a determined script that base64s the value.

use std::sync::Arc;

/// What a secret is replaced with.
pub const MASK: &str = "***";

#[derive(Clone, Debug, Default)]
pub struct Redactor {
    secrets: Arc<Vec<String>>,
}

impl Redactor {
    /// Build a redactor from raw secret values. Empty values are dropped: an
    /// empty needle would turn every string into `***`.
    pub fn new<I, S>(values: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut secrets: Vec<String> = values
            .into_iter()
            .map(Into::into)
            .filter(|v| !v.trim().is_empty())
            .collect();
        // Longest first so a secret that contains another is masked whole.
        secrets.sort_by_key(|v| std::cmp::Reverse(v.len()));
        secrets.dedup();
        Self {
            secrets: Arc::new(secrets),
        }
    }

    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.secrets.is_empty()
    }

    /// Replace every known secret value with [`MASK`].
    pub fn redact(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for secret in self.secrets.iter() {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), MASK);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redact_replaces_every_occurrence() {
        let r = Redactor::new(["k1".to_string()]);
        assert_eq!(r.redact("a k1 b k1"), "a *** b ***");
    }

    #[test]
    fn test_redact_ignores_empty_secrets() {
        let r = Redactor::new(["", "  "]);
        assert!(r.is_empty());
        assert_eq!(r.redact("anything"), "anything");
    }

    #[test]
    fn test_redact_longer_secret_wins() {
        let r = Redactor::new(["abc", "abcdef"]);
        assert_eq!(r.redact("abcdef"), "***");
    }

    #[test]
    fn test_redact_leaves_clean_text_alone() {
        let r = Redactor::new(["s3cr3t"]);
        assert_eq!(r.redact("nothing here"), "nothing here");
    }
}

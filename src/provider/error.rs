//! Structured provider-error classification at the `Provider::chat` seam.
//!
//! The transport used to fail with a flat
//! `bail!("API request failed: {status} - {body}")`, which dumped the raw response
//! body into whatever string the caller logged or persisted and gave the runner no
//! way to tell a blip from a dead key. This module replaces that with a
//! classification driven only by the provider's structured `error.code` /
//! `error.type` and the HTTP status, and a [`ProviderError`] that carries the
//! *kind* plus the provider's own message — never the raw body (privacy: the
//! body must not land in session or logs).
//!
//! See `docs/adr/0018-reliability-minimal-posture.md` (ticket 11 / ticket 19).

use std::error::Error;
use std::fmt;

/// How a failed provider call should be treated by the retry loop and the
/// owner-facing passthrough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderErrorKind {
    /// A blip: rate limit (429), server error (5xx), or a connection/timeout.
    /// Retried a bounded number of times with backoff.
    Transient,
    /// Authentication / billing / quota (401/402/403, `insufficient_quota`).
    /// Never retried; the provider's own message is surfaced verbatim to the
    /// owner because only they can fix it.
    AuthQuota,
    /// The request exceeded the model's context window
    /// (`context_length_exceeded`). Routed into context-pressure handling
    /// rather than surfaced as a hard failure.
    ContextLength,
    /// The provider refused on content-policy grounds (`content_filter`).
    /// Surfaced to the caller, not retried.
    ContentFilter,
    /// Anything else: fail the turn with the provider's message, no retry.
    Other,
}

impl ProviderErrorKind {
    /// Whether the retry loop may try this failure again. Only transient
    /// failures qualify — auth/quota and policy refusals do not resolve by
    /// asking again.
    pub fn is_retryable(self) -> bool {
        matches!(self, ProviderErrorKind::Transient)
    }
}

impl fmt::Display for ProviderErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ProviderErrorKind::Transient => "transient",
            ProviderErrorKind::AuthQuota => "auth_quota",
            ProviderErrorKind::ContextLength => "context_length",
            ProviderErrorKind::ContentFilter => "content_filter",
            ProviderErrorKind::Other => "other",
        };
        f.write_str(s)
    }
}

/// A classified provider failure: the kind plus the provider's own message.
///
/// The `message` is safe to surface and log — it is the structured
/// `error.message`, never the raw response body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl Error for ProviderError {}

/// Derive the structured fields a provider error response carries. OpenAI-shape
/// bodies nest them under `error`; some gateways put `code` / `type` / `message`
/// at the top level. Missing fields yield `None`.
struct ErrorFields {
    code: Option<String>,
    etype: Option<String>,
    message: Option<String>,
}

impl ErrorFields {
    fn from(body: &str) -> Self {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            return Self {
                code: None,
                etype: None,
                message: None,
            };
        };
        let error = value.get("error").unwrap_or(&value);
        let str_field = |obj: &serde_json::Value, key: &str| {
            obj.get(key).and_then(|v| v.as_str()).map(|s| s.to_string())
        };
        let code = str_field(error, "code");
        let etype = str_field(error, "type");
        let message = str_field(error, "message").or_else(|| str_field(&value, "message"));
        Self {
            code,
            etype,
            message,
        }
    }
}

/// Classify a failed HTTP response by status plus the provider's structured
/// `error.code` / `error.type`, and extract the provider's own message.
///
/// The status is checked for the transient band first (a 429 that also names a
/// quota code is still retried — the code is not trusted over the status for
/// rate limits). Structured codes then refine: `context_length_exceeded` and
/// `content_filter` win over a generic status, and `insufficient_quota` /
/// authentication / billing types fold into [`ProviderErrorKind::AuthQuota`].
pub fn classify(status: u16, body: &str) -> ProviderError {
    let fields = ErrorFields::from(body);
    let kind = classify_kind(status, fields.code.as_deref(), fields.etype.as_deref());
    // The provider's own message, verbatim; fall back to a status-only line so a
    // non-JSON body still yields a safe message without echoing the raw bytes.
    let message = match fields.message {
        Some(m) if !m.trim().is_empty() => m,
        _ => format!("provider returned HTTP {status}"),
    };
    ProviderError::new(kind, message)
}

fn classify_kind(status: u16, code: Option<&str>, etype: Option<&str>) -> ProviderErrorKind {
    // Structured codes are the most specific signal and win over the band.
    if let Some(code) = code {
        match code {
            "context_length_exceeded" | "string_above_max_length" => {
                return ProviderErrorKind::ContextLength;
            }
            "content_filter" => return ProviderErrorKind::ContentFilter,
            "insufficient_quota" | "insufficient_current_quota" => {
                return ProviderErrorKind::AuthQuota;
            }
            _ => {}
        }
    }
    // Transient band: request timeouts, rate limits and server-side failures.
    // 429 is retried even when it also names a code, since it means "later".
    if matches!(status, 408 | 409 | 425 | 429) || (500..=599).contains(&status) {
        return ProviderErrorKind::Transient;
    }
    // Auth / billing status band.
    if (401..=403).contains(&status) {
        return ProviderErrorKind::AuthQuota;
    }
    // Error-type fallbacks some gateways use in place of a code.
    if let Some(etype) = etype {
        match etype {
            "context_length_exceeded" => return ProviderErrorKind::ContextLength,
            "content_filter" => return ProviderErrorKind::ContentFilter,
            "authentication_error" | "billing_error" | "insufficient_user_quota" => {
                return ProviderErrorKind::AuthQuota;
            }
            "rate_limit_exceeded" | "server_error" => return ProviderErrorKind::Transient,
            _ => {}
        }
    }
    ProviderErrorKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_rate_limit_429_is_transient() {
        let body = r#"{"error":{"message":"Rate limit reached","type":"requests","code":"rate_limit_exceeded"}}"#;
        let err = classify(429, body);
        assert_eq!(err.kind, ProviderErrorKind::Transient);
        assert!(err.kind.is_retryable());
        assert_eq!(err.message, "Rate limit reached");
    }

    #[test]
    fn test_classify_server_5xx_is_transient() {
        for status in [500u16, 502, 503, 504] {
            let err = classify(status, "upstream blew up, not json");
            assert_eq!(classify(status, "").kind, ProviderErrorKind::Transient);
            // A non-JSON body must not echo raw bytes into the message.
            assert!(!err.message.contains("blew up"));
        }
    }

    #[test]
    fn test_classify_connection_timeout_status_408_is_transient() {
        assert_eq!(
            classify(408, "").kind,
            ProviderErrorKind::Transient,
            "request timeout retries"
        );
    }

    #[test]
    fn test_classify_auth_statuses_never_retry() {
        for status in [401u16, 402, 403] {
            let err = classify(status, r#"{"error":{"message":"Invalid API key"}}"#);
            assert_eq!(err.kind, ProviderErrorKind::AuthQuota);
            assert!(!err.kind.is_retryable());
            assert_eq!(err.message, "Invalid API key");
        }
    }

    #[test]
    fn test_classify_insufficient_quota_code_is_auth_quota() {
        let body = r#"{"error":{"message":"You exceeded your current quota","code":"insufficient_quota","type":"rate_limit_exceeded"}}"#;
        let err = classify(429, body);
        // A named quota code outranks the 429 status: asking again cannot fix it.
        assert_eq!(err.kind, ProviderErrorKind::AuthQuota);
        assert!(!err.kind.is_retryable());
    }

    #[test]
    fn test_classify_context_length_exceeded_routes_to_pressure() {
        let body = r#"{"error":{"message":"This model's maximum context length is 8191 tokens.","code":"context_length_exceeded","type":"invalid_request_error"}}"#;
        let err = classify(400, body);
        assert_eq!(err.kind, ProviderErrorKind::ContextLength);
        assert!(!err.kind.is_retryable());
    }

    #[test]
    fn test_classify_content_filter_is_surfaced_not_retried() {
        let body = r#"{"error":{"message":"Filtered","code":"content_filter"}}"#;
        let err = classify(400, body);
        assert_eq!(err.kind, ProviderErrorKind::ContentFilter);
        assert!(!err.kind.is_retryable());
    }

    #[test]
    fn test_classify_top_level_fields_when_not_nested_under_error() {
        // Some gateways emit {code, type, message} flat rather than under "error".
        let body = r#"{"code":"insufficient_quota","message":"No quota","type":"exhausted"}"#;
        let err = classify(400, body);
        assert_eq!(err.kind, ProviderErrorKind::AuthQuota);
        assert_eq!(err.message, "No quota");
    }

    #[test]
    fn test_classify_unknown_400_falls_to_other() {
        let body = r#"{"error":{"message":"Bad request","type":"invalid_request_error"}}"#;
        let err = classify(400, body);
        assert_eq!(err.kind, ProviderErrorKind::Other);
        assert!(!err.kind.is_retryable());
        assert_eq!(err.message, "Bad request");
    }

    #[test]
    fn test_message_never_leaks_raw_body() {
        // A body that is valid JSON but has no message field, and contains a
        // sensitive-looking blob, must not surface that blob.
        let body = r#"{"error":{"secret":"sk-live-abcdef","request_id":"req_123"}}"#;
        let err = classify(400, body);
        assert!(!err.message.contains("sk-live"));
        assert!(!err.message.contains("req_123"));
    }
}

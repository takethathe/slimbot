//! Owner-facing alerts for hard provider failures (ticket 19 / ADR 0018).
//!
//! A hard auth/quota failure is something only the owner can fix — a dead key,
//! a billing wall — so the provider's own message must reach them through the
//! delivery seam, not just sit in a failed turn. Repeated hard failures reuse
//! the trigger alert-merging idea ([`crate::trigger`] dedupes one alert per
//! failure streak): the owner is told once per window, not flooded per retry.
//!
//! Privacy holds end to end: only the classified [`ProviderError`] travels
//! here — its message is the provider's structured `error.message`, never the
//! raw response body.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::delivery::{OwnerPrimaryStore, Resolution, resolve_target};
use crate::message_bus::BusResult;
use crate::provider::{ProviderError, ProviderErrorKind};

/// How long one alert covers a failure streak: a repeated hard failure inside
/// the window produces no second message (merged into the first alert).
pub const ALERT_DEDUP_WINDOW: Duration = Duration::from_secs(600);

/// Time-window dedupe for owner alerts, keyed by alert identity. Lives for the
/// process: the point is to quiet a flood of repeats, not to persist a ledger.
#[derive(Default)]
pub struct OwnerAlertDedup {
    last_alerted: Mutex<HashMap<String, Instant>>,
}

impl OwnerAlertDedup {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an alert for `key`, answering whether it should actually be sent:
    /// the first alert of a streak sends, any repeat within `window` merges
    /// into the silence behind it.
    pub fn try_alert(&self, key: &str, window: Duration) -> bool {
        let mut guard = self
            .last_alerted
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        if let Some(last) = guard.get(key)
            && now.duration_since(*last) < window
        {
            return false;
        }
        guard.insert(key.to_string(), now);
        true
    }
}

/// The dedupe key for a provider failure: the kind alone. Consecutive
/// auth/quota failures are the same problem, not one notice per turn.
pub fn provider_alert_key(err: &ProviderError) -> String {
    format!("provider_{}", err.kind)
}

/// Alert body: the provider's own message, verbatim — the one failure class
/// where the provider's wording is more useful than ours.
pub fn provider_alert_text(err: &ProviderError) -> String {
    format!(
        "[Provider alert] Provider {kind} failure: {message}\n\
         This needs you to fix (key / billing / quota); retrying the message \
         will not help until then.",
        kind = err.kind,
        message = err.message,
    )
}

/// Whether a classified failure is a hard owner-facing one. Only auth/quota
/// qualifies: transient failures are retried, context-length goes to pressure
/// handling, and everything else just fails the turn in place.
pub fn is_owner_alertable(err: &ProviderError) -> bool {
    err.kind == ProviderErrorKind::AuthQuota
}

/// Deliver the owner alert for a hard provider failure through the delivery
/// seam, merged into the current failure streak. Answers whether a message was
/// actually sent (a merged repeat or an unreachable owner sends nothing).
pub async fn deliver_provider_alert(
    outbound_tx: &mpsc::Sender<BusResult>,
    owner_primary: &OwnerPrimaryStore,
    dedup: &OwnerAlertDedup,
    err: &ProviderError,
) -> bool {
    if !is_owner_alertable(err) {
        return false;
    }
    // The alert rides the owner-primary leg of the seam: it stands for
    // "the operator must know", not "reply in this conversation".
    let Resolution::Deliver(target) = resolve_target(None, None, owner_primary.current()) else {
        crate::warn_log!("[provider] hard failure but no owner channel to alert: {err}");
        return false;
    };
    if !dedup.try_alert(&provider_alert_key(err), ALERT_DEDUP_WINDOW) {
        return false;
    }
    if outbound_tx.is_closed() {
        crate::warn_log!("[provider] no channel consumer; owner alert was dropped");
        return false;
    }
    let run_id = format!("owner-alert-{}", uuid::Uuid::new_v4());
    outbound_tx
        .send(BusResult {
            session_id: target.session_id(),
            run_id,
            content: provider_alert_text(err),
        })
        .await
        .is_ok()
}

/// The process-wide dedupe store, so every failing turn shares one streak
/// rather than one store per task.
pub fn global_dedup() -> &'static OwnerAlertDedup {
    static DEDUP: OnceLock<OwnerAlertDedup> = OnceLock::new();
    DEDUP.get_or_init(OwnerAlertDedup::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn auth_err(message: &str) -> ProviderError {
        ProviderError::new(ProviderErrorKind::AuthQuota, message)
    }

    fn tmp_owner(channel: &str, chat: &str) -> OwnerPrimaryStore {
        let tmp = tempfile::tempdir().unwrap();
        let store = OwnerPrimaryStore::load(tmp.path());
        store.record_first(
            &crate::delivery::DeliveryTarget::new(channel, chat).expect("test target"),
        );
        // Leak the tempdir so the store's file outlives the async test body.
        std::mem::forget(tmp);
        store
    }

    #[tokio::test]
    async fn test_deliver_provider_alert_reaches_owner_primary() {
        let (tx, mut rx) = mpsc::channel(4);
        let owner = tmp_owner("webui", "main");
        let dedup = OwnerAlertDedup::new();
        let sent = deliver_provider_alert(&tx, &owner, &dedup, &auth_err("Invalid API key")).await;
        assert!(sent);
        let msg = rx.recv().await.expect("one alert outbound");
        assert_eq!(msg.session_id, "webui:main");
        // The provider's message passes through verbatim.
        assert!(msg.content.contains("Invalid API key"), "{}", msg.content);
    }

    #[tokio::test]
    async fn test_repeated_hard_failure_merges_into_one_alert() {
        let (tx, mut rx) = mpsc::channel(4);
        let owner = tmp_owner("webui", "main");
        let dedup = OwnerAlertDedup::new();
        for _ in 0..3 {
            deliver_provider_alert(&tx, &owner, &dedup, &auth_err("No quota")).await;
        }
        let msg = rx.recv().await.expect("first alert");
        assert!(rx.try_recv().is_err(), "repeats must not flood");
        drop(msg);
    }

    #[tokio::test]
    async fn test_alert_resends_after_dedup_window_expires() {
        let (tx, rx) = mpsc::channel(4);
        let owner = tmp_owner("webui", "main");
        let dedup = OwnerAlertDedup::new();
        assert!(dedup.try_alert("provider_auth_quota", Duration::from_millis(10)));
        assert!(!dedup.try_alert("provider_auth_quota", Duration::from_millis(10)));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(dedup.try_alert("provider_auth_quota", Duration::from_millis(10)));
        // deliver_provider_alert uses the shared constant window; the store
        // above already used the key, so the direct deliver call merges.
        assert!(!deliver_provider_alert(&tx, &owner, &dedup, &auth_err("x")).await);
        drop(rx);
    }

    #[tokio::test]
    async fn test_transient_and_other_kinds_never_alert_owner() {
        let (tx, _rx) = mpsc::channel(4);
        let owner = tmp_owner("webui", "main");
        let dedup = OwnerAlertDedup::new();
        for kind in [
            ProviderErrorKind::Transient,
            ProviderErrorKind::ContextLength,
            ProviderErrorKind::ContentFilter,
            ProviderErrorKind::Other,
        ] {
            let err = ProviderError::new(kind, "not for the owner");
            assert!(
                !deliver_provider_alert(&tx, &owner, &dedup, &err).await,
                "{kind} is not an owner-facing hard failure"
            );
        }
    }

    #[tokio::test]
    async fn test_alert_without_owner_channel_is_dropped_not_panic() {
        let (tx, _rx) = mpsc::channel(4);
        let tmp = tempfile::tempdir().unwrap();
        let owner = OwnerPrimaryStore::load(tmp.path()); // never established
        let dedup = OwnerAlertDedup::new();
        assert!(!deliver_provider_alert(&tx, &owner, &dedup, &auth_err("dead key")).await);
    }

    #[tokio::test]
    async fn test_alert_with_closed_channel_is_false() {
        let (tx, rx) = mpsc::channel(4);
        drop(rx);
        let owner = tmp_owner("webui", "main");
        let dedup = OwnerAlertDedup::new();
        assert!(!deliver_provider_alert(&tx, &owner, &dedup, &auth_err("dead key")).await);
    }

    #[test]
    fn test_alert_text_carries_no_raw_body_shape() {
        // The alert text is built only from kind + message fields; a body-like
        // string never enters it because ProviderError cannot carry one.
        let err = auth_err("You exceeded your current quota");
        let text = provider_alert_text(&err);
        assert!(text.contains("You exceeded your current quota"));
        assert!(text.contains("auth_quota"));
    }
}

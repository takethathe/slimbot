//! Where system-originated output goes: the single delivery-target seam.
//!
//! Every proactive message — cron, heartbeat, trigger notices and alerts —
//! resolves its destination through [`resolve_target`], so "who gets told" has
//! exactly one rule:
//!
//! ```text
//! explicit deliver-to  >  the creating task's origin session  >  owner primary channel
//! ```
//!
//! Two conditions that look like "no message" are deliberately different:
//!
//! - **Silent delivery**: the task declared `deliver: false`. That decision is
//!   taken before the seam (each caller checks its own flag), because only the
//!   creator knows it meant to be quiet.
//! - **Nowhere**: the seam resolved no target at all (a source with no origin,
//!   or an explicit/origin session with an empty half). The caller degrades to
//!   log-only — a fallback, never a policy silence and never a wrong target.
//!
//! There is no persistent outbox and no global delivery ledger: best-effort
//! delivery, and a restart never backfills (ADR 0018).

use std::path::{Path, PathBuf};

use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// A two-segment destination: which channel, and which conversation on it.
///
/// Deliberately two segments — v1 has no thread/topic/priority dimension, and
/// an unreachable or empty half is "nowhere", not "somewhere close enough".
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryTarget {
    pub channel: String,
    pub chat_id: String,
}

impl DeliveryTarget {
    /// Build a target from a channel id and a chat id. An empty half means the
    /// destination does not exist (`webui:` would be handed to a channel that
    /// has no such conversation).
    pub fn new(channel: &str, chat_id: &str) -> Option<Self> {
        let channel = channel.trim();
        let chat_id = chat_id.trim();
        if channel.is_empty() || chat_id.is_empty() {
            return None;
        }
        Some(Self {
            channel: channel.to_string(),
            chat_id: chat_id.to_string(),
        })
    }

    /// Build a target from an optional `(channel, to)` pair, as declared by a
    /// cron payload or a trigger action.
    pub fn from_pair(channel: Option<&str>, chat_id: Option<&str>) -> Option<Self> {
        Self::new(channel?, chat_id?)
    }

    /// Parse a `{channel}:{chat_id}` session id. The chat id may itself contain
    /// colons; only the first one separates the two halves.
    pub fn from_session_id(session_id: &str) -> Option<Self> {
        let (channel, chat_id) = session_id.split_once(':')?;
        Self::new(channel, chat_id)
    }

    /// The session id the main loop and channels route by.
    pub fn session_id(&self) -> String {
        format!("{}:{}", self.channel, self.chat_id)
    }
}

/// What the seam decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Send here.
    Deliver(DeliveryTarget),
    /// Nothing to send to: the caller logs and stops (never mis-delivers).
    Nowhere,
}

impl Resolution {
    /// The resolved target, when there is one.
    pub fn target(&self) -> Option<&DeliveryTarget> {
        match self {
            Resolution::Deliver(target) => Some(target),
            Resolution::Nowhere => None,
        }
    }
}

/// Resolve where a system-originated message goes.
///
/// `owner_primary` is passed only by sources that fundamentally have no origin
/// (heartbeat, a cron job created without one). Trigger fires always carry an
/// origin, so they pass `None` and stay book-only when it is unreachable.
pub fn resolve_target(
    explicit: Option<DeliveryTarget>,
    origin: Option<DeliveryTarget>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    match explicit.or(origin).or(owner_primary) {
        Some(target) => Resolution::Deliver(target),
        None => Resolution::Nowhere,
    }
}

/// Resolve a heartbeat notice's destination: the configured override, else the
/// owner's primary channel. Heartbeat has no origin, so it is exactly the case
/// where the owner-primary fallback is correct.
pub fn heartbeat_target(
    override_target: Option<DeliveryTarget>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    resolve_target(override_target, None, owner_primary)
}

/// Resolve a cron result's destination: the conversation the job was created in
/// (its `channel`/`to`), else the owner's primary channel for a job created
/// without one.
pub fn cron_target(
    payload_channel: Option<&str>,
    payload_to: Option<&str>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    resolve_target(
        None,
        DeliveryTarget::from_pair(payload_channel, payload_to),
        owner_primary,
    )
}

/// The owner's primary channel: the conversation the owner first established,
/// remembered across restarts so an unattended notice still reaches them.
///
/// This is one small file holding one destination — not a delivery ledger: it
/// records *who the owner is reachable on*, not what was delivered.
pub struct OwnerPrimaryStore {
    path: PathBuf,
    current: RwLock<Option<DeliveryTarget>>,
}

impl OwnerPrimaryStore {
    /// File name inside the data dir.
    pub const FILE_NAME: &'static str = "owner-primary.json";

    /// Load the store from `{data_dir}/owner-primary.json`. A missing or
    /// unreadable file simply means "not established yet".
    pub fn load(data_dir: impl AsRef<Path>) -> Self {
        let path = data_dir.as_ref().join(Self::FILE_NAME);
        let current = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<DeliveryTarget>(&body).ok())
            .filter(|target| DeliveryTarget::new(&target.channel, &target.chat_id).is_some());
        Self {
            path,
            current: RwLock::new(current),
        }
    }

    /// The owner's primary channel, if one has been established.
    pub fn current(&self) -> Option<DeliveryTarget> {
        self.current.read().clone()
    }

    /// Record the conversation the owner is talking on. Only the first one is
    /// kept: the primary channel is stable, not "the most recent one".
    pub fn record_first(&self, target: &DeliveryTarget) {
        let mut guard = self.current.write();
        if guard.is_some() {
            return;
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(body) = serde_json::to_string_pretty(target) {
            let _ = crate::utils::write_file_atomic(&self.path, &body);
        }
        *guard = Some(target.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(channel: &str, chat: &str) -> DeliveryTarget {
        DeliveryTarget::new(channel, chat).expect("test target")
    }

    #[test]
    fn test_resolve_target_prefers_explicit_over_origin_and_owner() {
        let resolution = resolve_target(
            Some(target("feishu", "oc_explicit")),
            Some(target("webui", "main")),
            Some(target("cli", "default")),
        );
        assert_eq!(
            resolution,
            Resolution::Deliver(target("feishu", "oc_explicit"))
        );
    }

    #[test]
    fn test_resolve_target_falls_back_to_origin_before_owner_primary() {
        let resolution = resolve_target(
            None,
            Some(target("webui", "main")),
            Some(target("cli", "default")),
        );
        assert_eq!(resolution, Resolution::Deliver(target("webui", "main")));
    }

    #[test]
    fn test_resolve_target_uses_owner_primary_when_no_origin() {
        // Heartbeat: started by config, standing in no conversation.
        let resolution = resolve_target(None, None, Some(target("webui", "main")));
        assert_eq!(resolution, Resolution::Deliver(target("webui", "main")));
    }

    #[test]
    fn test_resolve_target_without_any_target_is_nowhere() {
        assert_eq!(resolve_target(None, None, None), Resolution::Nowhere);
    }

    #[test]
    fn test_resolve_target_never_invents_a_destination() {
        // A trigger whose origin is unreachable must stay book-only rather than
        // being misdelivered to the owner's primary channel.
        let unreachable_origin = DeliveryTarget::from_session_id("webui:");
        assert_eq!(unreachable_origin, None);
        assert_eq!(
            resolve_target(None, unreachable_origin, None),
            Resolution::Nowhere
        );
    }

    #[test]
    fn test_delivery_target_from_session_id_requires_both_halves() {
        assert_eq!(DeliveryTarget::from_session_id("webui:"), None);
        assert_eq!(DeliveryTarget::from_session_id(":main"), None);
        assert_eq!(DeliveryTarget::from_session_id("webui"), None);
        assert_eq!(
            DeliveryTarget::from_session_id("webui:main").map(|t| t.session_id()),
            Some("webui:main".to_string())
        );
    }

    #[test]
    fn test_delivery_target_from_session_id_keeps_colons_in_chat_id() {
        let target = DeliveryTarget::from_session_id("feishu:oc_123:thread").unwrap();
        assert_eq!(target.channel, "feishu");
        assert_eq!(target.chat_id, "oc_123:thread");
        assert_eq!(target.session_id(), "feishu:oc_123:thread");
    }

    #[test]
    fn test_delivery_target_from_pair_rejects_empty_halves() {
        assert_eq!(DeliveryTarget::from_pair(Some("cli"), Some("  ")), None);
        assert_eq!(DeliveryTarget::from_pair(None, Some("chat")), None);
        assert_eq!(
            DeliveryTarget::from_pair(Some(" cli "), Some(" chat ")),
            Some(target("cli", "chat"))
        );
    }

    #[test]
    fn test_heartbeat_target_prefers_config_override() {
        assert_eq!(
            heartbeat_target(Some(target("webui", "ops")), Some(target("webui", "main"))),
            Resolution::Deliver(target("webui", "ops"))
        );
    }

    #[test]
    fn test_heartbeat_target_lands_on_owner_primary_without_override() {
        // The pre-ticket hardcoded `webui:webui_main` is gone: an owner who only
        // ever talked on another channel is reached there instead.
        assert_eq!(
            heartbeat_target(None, Some(target("feishu", "oc_owner"))),
            Resolution::Deliver(target("feishu", "oc_owner"))
        );
    }

    #[test]
    fn test_heartbeat_target_nowhere_before_the_owner_talks() {
        assert_eq!(heartbeat_target(None, None), Resolution::Nowhere);
    }

    #[test]
    fn test_cron_target_uses_the_creating_conversation() {
        assert_eq!(
            cron_target(
                Some("webui"),
                Some("main"),
                Some(target("feishu", "oc_owner"))
            ),
            Resolution::Deliver(target("webui", "main"))
        );
    }

    #[test]
    fn test_cron_target_falls_back_to_owner_primary_when_originless() {
        assert_eq!(
            cron_target(None, None, Some(target("feishu", "oc_owner"))),
            Resolution::Deliver(target("feishu", "oc_owner"))
        );
        assert_eq!(cron_target(None, None, None), Resolution::Nowhere);
    }

    #[test]
    fn test_owner_primary_records_first_conversation_only() {
        let tmp = tempfile::tempdir().unwrap();
        let store = OwnerPrimaryStore::load(tmp.path());
        assert_eq!(store.current(), None);

        store.record_first(&target("webui", "main"));
        store.record_first(&target("feishu", "oc_2"));
        assert_eq!(store.current(), Some(target("webui", "main")));

        // Survives a restart: the file is the source of truth.
        let reloaded = OwnerPrimaryStore::load(tmp.path());
        assert_eq!(reloaded.current(), Some(target("webui", "main")));
    }

    #[test]
    fn test_owner_primary_ignores_unreadable_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(OwnerPrimaryStore::FILE_NAME),
            "not json at all",
        )
        .unwrap();
        assert_eq!(OwnerPrimaryStore::load(tmp.path()).current(), None);
    }
}

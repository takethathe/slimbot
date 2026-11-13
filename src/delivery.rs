//! Where system-originated output goes: the single delivery-target seam.
//!
//! Every proactive message — cron, heartbeat, trigger notices and alerts,
//! provider alerts — resolves its destination through [`resolve_target`], and
//! the caller says only what the message *is* ([`DeliverySource`]) plus the
//! facts it holds. Which leg wins, and which sources may fall back at all, is
//! decided here once:
//!
//! ```text
//!                            own target   origin   owner primary
//!   cron (payload channel/to)     yes         —          yes
//!   heartbeat (override)         yes         —          yes
//!   trigger notify / alert       yes        yes         never
//!   provider alert                —          —        only leg
//! ```
//!
//! A cron job's creating conversation *is* the payload's `channel`/`to` pair,
//! so it has no separate origin leg; only triggers carry one.
//!
//! Two conditions that look like "no message" are deliberately different:
//!
//! - **Silent delivery**: the task declared `deliver: false`. That decision is
//!   taken before the seam (each caller checks its own flag), because only the
//!   creator knows it meant to be quiet, and the five kinds of silence in this
//!   codebase mean different things.
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

/// Where a system-originated message came from.
///
/// The source, not the caller's arguments, decides which legs are available: a
/// trigger always carries an origin and never reaches the owner's primary
/// channel, a heartbeat has no origin at all, a cron job answers where it was
/// created. The halves stay raw because parsing them is part of the same
/// decision, so a caller can neither hand over a half-built target nor express
/// a policy by passing `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverySource<'a> {
    /// A cron job result: the conversation the job was created in, which the
    /// payload keeps as its `channel`/`to` pair.
    Cron {
        channel: Option<&'a str>,
        to: Option<&'a str>,
    },
    /// A heartbeat notice: the configured override, else the owner's primary
    /// channel. Heartbeat is started by config and stands in no conversation.
    Heartbeat {
        channel: Option<&'a str>,
        to: Option<&'a str>,
    },
    /// A trigger notice or alert: the action's own destination when it named
    /// one, else the conversation the rule was created in. `origin` is that
    /// conversation as `(channel, chat_id)`.
    Trigger {
        channel: Option<&'a str>,
        to: Option<&'a str>,
        origin: Option<(&'a str, &'a str)>,
    },
    /// A hard provider failure: only the owner can fix a dead key or a billing
    /// wall, so the owner's primary channel is the only leg.
    OwnerAlert,
}

impl DeliverySource<'_> {
    /// The priority table: the only place that knows which legs a source may
    /// use, so changing a fallback rule changes exactly one file.
    fn candidate(&self, owner_primary: Option<DeliveryTarget>) -> Option<DeliveryTarget> {
        match self {
            // Both of these can name their own target — cron the conversation it
            // was created in, heartbeat the configured override. When that is
            // missing the owner is the audience, because nobody else is waiting.
            DeliverySource::Cron { channel, to } | DeliverySource::Heartbeat { channel, to } => {
                DeliveryTarget::from_pair(*channel, *to).or(owner_primary)
            }
            // A trigger knows where it came from; an unreachable origin stays
            // book-only rather than being misdelivered to the owner.
            DeliverySource::Trigger {
                channel,
                to,
                origin,
            } => DeliveryTarget::from_pair(*channel, *to).or_else(|| {
                origin.and_then(|(channel, chat_id)| DeliveryTarget::new(channel, chat_id))
            }),
            DeliverySource::OwnerAlert => owner_primary,
        }
    }
}

/// Resolve where a system-originated message goes.
///
/// `owner_primary` is whatever the caller has (the [`OwnerPrimaryStore`] value,
/// or `None` on a path that holds no store); sources that never fall back
/// ignore it instead of being told what to do through the argument.
pub fn resolve_target(
    source: DeliverySource<'_>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    match source.candidate(owner_primary) {
        Some(target) => Resolution::Deliver(target),
        None => Resolution::Nowhere,
    }
}

/// Resolve a heartbeat notice's destination: the configured override, else the
/// owner's primary channel.
///
/// Kept as a named adapter so the gateway call site says what it is delivering
/// instead of assembling a source by hand; it holds no priority knowledge.
pub fn heartbeat_target(
    channel: Option<&str>,
    to: Option<&str>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    resolve_target(DeliverySource::Heartbeat { channel, to }, owner_primary)
}

/// Resolve a cron result's destination: the conversation the job was created
/// in (its payload `channel`/`to`), else the owner's primary channel for a job
/// created without one.
pub fn cron_target(
    payload_channel: Option<&str>,
    payload_to: Option<&str>,
    owner_primary: Option<DeliveryTarget>,
) -> Resolution {
    resolve_target(
        DeliverySource::Cron {
            channel: payload_channel,
            to: payload_to,
        },
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

    // ── Source categories: which legs a source may use ──

    #[test]
    fn test_delivery_source_trigger_never_falls_back_to_owner_primary() {
        let owner = Some(target("webui", "main"));
        // The rule was created in a conversation, so an unreachable origin is
        // book-only — even though the owner is reachable somewhere else.
        assert_eq!(
            resolve_target(
                DeliverySource::Trigger {
                    channel: None,
                    to: None,
                    origin: Some(("webui", "")),
                },
                owner.clone(),
            ),
            Resolution::Nowhere
        );
        // A trigger with no origin at all gets the same answer: the owner's
        // primary channel is not a leg for this source.
        assert_eq!(
            resolve_target(
                DeliverySource::Trigger {
                    channel: None,
                    to: None,
                    origin: None,
                },
                owner,
            ),
            Resolution::Nowhere
        );
    }

    #[test]
    fn test_delivery_origin_unreachable_resolves_nowhere_with_owner_primary_present() {
        // Any empty half means "that conversation does not exist"; a known
        // owner-primary channel does not make it deliverable.
        let owner = Some(target("webui", "main"));
        for origin in [("webui", ""), ("", "main"), ("webui", "   ")] {
            assert_eq!(
                resolve_target(
                    DeliverySource::Trigger {
                        channel: None,
                        to: None,
                        origin: Some(origin),
                    },
                    owner.clone(),
                ),
                Resolution::Nowhere,
                "origin {origin:?} must not fall back to the owner"
            );
        }
    }

    #[test]
    fn test_delivery_source_trigger_prefers_explicit_target_over_origin() {
        assert_eq!(
            resolve_target(
                DeliverySource::Trigger {
                    channel: Some("feishu"),
                    to: Some("oc_explicit"),
                    origin: Some(("webui", "main")),
                },
                Some(target("cli", "default")),
            ),
            Resolution::Deliver(target("feishu", "oc_explicit"))
        );
        // With no explicit target the creating conversation is the answer, and
        // the owner's primary channel still is not.
        assert_eq!(
            resolve_target(
                DeliverySource::Trigger {
                    channel: None,
                    to: None,
                    origin: Some(("webui", "main")),
                },
                Some(target("cli", "default")),
            ),
            Resolution::Deliver(target("webui", "main"))
        );
    }

    #[test]
    fn test_delivery_source_owner_alert_has_only_the_owner_leg() {
        assert_eq!(
            resolve_target(DeliverySource::OwnerAlert, Some(target("webui", "main"))),
            Resolution::Deliver(target("webui", "main"))
        );
        assert_eq!(
            resolve_target(DeliverySource::OwnerAlert, None),
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
    fn test_delivery_source_heartbeat_uses_owner_primary_without_origin() {
        // The configured override is the heartbeat's own target and wins.
        assert_eq!(
            heartbeat_target(Some("webui"), Some("ops"), Some(target("webui", "main"))),
            Resolution::Deliver(target("webui", "ops"))
        );
        // Without one, the owner is reached wherever the owner actually talks.
        assert_eq!(
            heartbeat_target(None, None, Some(target("feishu", "oc_owner"))),
            Resolution::Deliver(target("feishu", "oc_owner"))
        );
        // Before the owner ever talked there is nowhere to go.
        assert_eq!(heartbeat_target(None, None, None), Resolution::Nowhere);
    }

    #[test]
    fn test_delivery_source_cron_prefers_creating_conversation() {
        assert_eq!(
            cron_target(
                Some("webui"),
                Some("main"),
                Some(target("feishu", "oc_owner"))
            ),
            Resolution::Deliver(target("webui", "main"))
        );
        // A job created outside a conversation has nobody waiting in it, so the
        // owner is the audience.
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

    // ── Structural locks: the priority table lives in one place ──

    #[test]
    fn test_delivery_origin_parsing_shared_by_notify_and_alert() {
        // A trigger notice and a trigger alert hand the seam the same origin
        // halves, so a chat id keeps its own colons end to end.
        let origin = DeliverySource::Trigger {
            channel: None,
            to: None,
            origin: Some(("feishu", "oc_123:thread")),
        };
        assert_eq!(
            resolve_target(origin, None)
                .target()
                .map(DeliveryTarget::session_id)
                .as_deref(),
            Some("feishu:oc_123:thread")
        );
        // Parsing stays in the seam through one helper: the dispatcher hands
        // over the halves and never re-splits a session id to deliver.
        let dispatch = include_str!("trigger/dispatch.rs");
        assert_eq!(
            dispatch.matches("origin_pair(").count(),
            3,
            "one helper definition, two call sites (notify + alert)"
        );
        assert_eq!(
            dispatch.matches("DeliveryTarget::from_session_id").count(),
            1,
            "only the alertability check answers reachability; delivery parses in the seam"
        );
        assert_eq!(
            dispatch.matches("DeliverySource::Trigger").count(),
            2,
            "notify and alert must both name their source"
        );
    }

    #[test]
    fn test_delivery_call_sites_name_source_without_owner_fallback() {
        // A trigger never falls back to the owner, so it must not even reach for
        // the owner-primary store.
        let dispatch = include_str!("trigger/dispatch.rs");
        assert!(
            !dispatch.contains("OwnerPrimaryStore")
                && !dispatch.contains("owner_primary.current()"),
            "a trigger must not reference the owner-primary channel at all"
        );
        // The provider alert is the owner leg and nothing else.
        let owner_alert = include_str!("owner_alert.rs");
        assert_eq!(
            owner_alert.matches("DeliverySource::OwnerAlert").count(),
            1,
            "the provider alert must name its source"
        );
        // The gateway reaches the seam through the two source adapters, and the
        // override stays raw: parsing it is the adapter's job, not the caller's.
        let gateway = include_str!("gateway.rs");
        assert_eq!(gateway.matches("cron_target(").count(), 1);
        assert_eq!(gateway.matches("heartbeat_target(").count(), 1);
        assert!(
            !gateway.contains("DeliveryTarget::from_pair"),
            "the heartbeat override must be parsed inside the adapter"
        );
    }
}

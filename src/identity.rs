//! Identity layer for the single-owner (v1) trust boundary.
//!
//! A gateway serves exactly one `Owner` principal. Memory is shared per owner
//! while sessions stay scoped per `{channel}:{chat_id}` (`crate::session`), so
//! "who" and "where" stay independent.
//!
//! The `Author` of an inbound message decides what the message may *do*: only
//! owner-authored input may authorize a consequential action or write durable
//! memory. A bystander sharing a conversation context (a future group chat) can
//! be read as untrusted input, but can never borrow the owner's authority.

use serde::{Deserialize, Serialize};

use crate::config_defs::OwnerConfig;

/// The owner id used when config does not declare one. v1 is permanently
/// single-user, so the id only has to be stable, not configurable per user.
pub const DEFAULT_OWNER_ID: &str = "owner";

/// The single principal a gateway serves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    id: String,
    display_name: Option<String>,
}

impl Default for Owner {
    fn default() -> Self {
        Self {
            id: DEFAULT_OWNER_ID.to_string(),
            display_name: None,
        }
    }
}

impl Owner {
    /// Build the owner from the `gateway.owner` config section. An empty id
    /// falls back to [`DEFAULT_OWNER_ID`] so a half-filled section still yields
    /// one recognizable principal.
    pub fn from_config(config: &OwnerConfig) -> Self {
        let id = config.id.trim();
        Self {
            id: if id.is_empty() {
                DEFAULT_OWNER_ID.to_string()
            } else {
                id.to_string()
            },
            display_name: config
                .display_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// True when `author` may authorize a consequential action (high-risk tool,
    /// external send) and write durable memory on this owner's behalf.
    pub fn authorizes(&self, author: Author) -> bool {
        author.may_authorize()
    }
}

/// Who authored an inbound message, from the identity layer's point of view.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// The owner: local CLI / WebUI input (local machine is the owner), or the
    /// owner's own DM on an IM channel.
    #[default]
    Owner,
    /// Someone else sharing the conversation context (group chat member).
    Bystander,
}

impl Author {
    /// Only owner-authored input may authorize a consequential action.
    pub fn may_authorize(self) -> bool {
        matches!(self, Author::Owner)
    }

    /// Only owner-authored input may write durable memory.
    pub fn may_write_memory(self) -> bool {
        self.may_authorize()
    }
}

/// Reply sent back when a non-owner-authored message arrives. v1 has no group
/// conversations, so the input is refused outright instead of being read as
/// untrusted chatter: nothing a bystander says may drive this agent.
pub const UNAUTHORIZED_AUTHOR_REPLY: &str =
    "This conversation is not authorized to act for the owner, so the message was not processed.";

#[cfg(test)]
mod tests {
    use super::*;

    fn owner_config(id: &str, name: Option<&str>) -> OwnerConfig {
        OwnerConfig {
            id: id.to_string(),
            display_name: name.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn test_owner_from_config_reads_id_and_display_name() {
        let owner = Owner::from_config(&owner_config("alice", Some("Alice")));
        assert_eq!(owner.id(), "alice");
        assert_eq!(owner.display_name(), Some("Alice"));
    }

    #[test]
    fn test_owner_from_config_empty_id_falls_back_to_default() {
        let owner = Owner::from_config(&owner_config("   ", None));
        assert_eq!(owner.id(), DEFAULT_OWNER_ID);
        assert_eq!(owner.display_name(), None);
    }

    #[test]
    fn test_owner_is_single_principal_regardless_of_channel() {
        // There is exactly one principal: the same config yields the same owner,
        // and session scoping (channel:chat_id) is a separate concern.
        let config = owner_config("owner", None);
        assert_eq!(Owner::from_config(&config), Owner::from_config(&config));
    }

    #[test]
    fn test_author_owner_may_authorize_and_write_memory() {
        assert!(Author::Owner.may_authorize());
        assert!(Author::Owner.may_write_memory());
        assert!(Owner::default().authorizes(Author::Owner));
    }

    #[test]
    fn test_author_bystander_may_not_authorize_or_write_memory() {
        assert!(!Author::Bystander.may_authorize());
        assert!(!Author::Bystander.may_write_memory());
        assert!(!Owner::default().authorizes(Author::Bystander));
    }
}

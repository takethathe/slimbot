//! Catalog registration store.
//!
//! A WebUI client POSTs its Catalog prompt at page load — that upload doubles
//! as a capability handshake ("this client can render cards"). The server keeps
//! one registration per `chat_id`, overwritable, in-process, with a TTL so idle
//! chats do not retain a large prompt forever. An unregistered chat produces no
//! card (fail-closed), which is why `json_render.enabled` can default to true:
//! registration is the real switch.
//!
//! The uploaded prompt is untrusted input: it is length-checked and must never
//! be logged in full.

use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, SystemTime};

/// Max accepted prompt size, in bytes.
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// How long a registration stays valid without being refreshed.
pub const REGISTRATION_TTL: Duration = Duration::from_secs(24 * 3600);

/// Why a registration upload was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    /// Prompt was empty or whitespace-only.
    Empty,
    /// Prompt exceeded [`MAX_PROMPT_BYTES`].
    TooLarge,
}

/// One chat's Catalog registration.
#[derive(Debug, Clone)]
pub struct Registration {
    pub prompt: String,
    pub catalog_hash: String,
    /// When this registration was (re)stored, for TTL expiry.
    pub registered_at: SystemTime,
}

/// Whether a registration is still within [`REGISTRATION_TTL`].
fn is_fresh(reg: &Registration, now: SystemTime) -> bool {
    now.duration_since(reg.registered_at)
        .unwrap_or(Duration::ZERO)
        <= REGISTRATION_TTL
}

/// Thread-safe, per-`chat_id` registry of Catalog registrations.
pub struct CardRegistry {
    inner: RwLock<HashMap<String, Registration>>,
}

impl Default for CardRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CardRegistry {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Validate and store a registration for `chat_id`, overwriting any prior
    /// entry. Rejects empty prompts (after trimming) and oversized prompts.
    pub fn register(
        &self,
        chat_id: &str,
        prompt: String,
        catalog_hash: String,
    ) -> Result<(), RegisterError> {
        if prompt.trim().is_empty() {
            return Err(RegisterError::Empty);
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(RegisterError::TooLarge);
        }
        let reg = Registration {
            prompt,
            catalog_hash,
            registered_at: SystemTime::now(),
        };
        // Write under the lock; a poisoned lock falls back to the existing view
        // rather than panicking the request handler.
        if let Ok(mut map) = self.inner.write() {
            // Sweep expired registrations of *other* chats while we're here:
            // idle chats never call get again, so read-time eviction alone
            // would retain their prompts indefinitely.
            let now = SystemTime::now();
            map.retain(|_, reg| is_fresh(reg, now));
            map.insert(chat_id.to_string(), reg);
        }
        Ok(())
    }

    /// Return a clone of the live registration for `chat_id`, or `None` when
    /// absent or TTL-expired. An expired hit is evicted from the map.
    pub fn get(&self, chat_id: &str) -> Option<Registration> {
        let expired = {
            let map = self.inner.read().ok()?;
            match map.get(chat_id) {
                None => return None,
                Some(reg) if is_fresh(reg, SystemTime::now()) => return Some(reg.clone()),
                Some(_) => true,
            }
        };
        if expired {
            // Re-check under the write lock before dropping the entry.
            if let Ok(mut map) = self.inner.write()
                && let Some(reg) = map.get(chat_id)
                && !is_fresh(reg, SystemTime::now())
            {
                map.remove(chat_id);
            }
        }
        None
    }

    /// Test helper: store a registration with an explicit `registered_at` so
    /// TTL behaviour can be exercised without sleeping.
    #[cfg(test)]
    fn register_at(&self, chat_id: &str, prompt: &str, registered_at: SystemTime) {
        if let Ok(mut map) = self.inner.write() {
            map.insert(
                chat_id.to_string(),
                Registration {
                    prompt: prompt.to_string(),
                    catalog_hash: "h".to_string(),
                    registered_at,
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registry_rejects_empty_prompt() {
        let r = CardRegistry::new();
        assert_eq!(
            r.register("c1", "   ".to_string(), "h".to_string()),
            Err(RegisterError::Empty)
        );
        assert!(r.get("c1").is_none());
    }

    #[test]
    fn test_registry_rejects_prompt_over_limit() {
        let r = CardRegistry::new();
        let huge = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert_eq!(
            r.register("c1", huge, "h".to_string()),
            Err(RegisterError::TooLarge)
        );
        assert!(r.get("c1").is_none());
    }

    #[test]
    fn test_registry_accepts_at_limit() {
        let r = CardRegistry::new();
        let exact = "y".repeat(MAX_PROMPT_BYTES);
        assert!(r.register("c1", exact, "h".to_string()).is_ok());
        assert!(r.get("c1").is_some());
    }

    #[test]
    fn test_registry_overwrites_same_chat() {
        let r = CardRegistry::new();
        r.register("c1", "first".to_string(), "h1".to_string())
            .unwrap();
        r.register("c1", "second".to_string(), "h2".to_string())
            .unwrap();
        let reg = r.get("c1").unwrap();
        assert_eq!(reg.prompt, "second");
        assert_eq!(reg.catalog_hash, "h2");
    }

    #[test]
    fn test_registry_returns_none_after_ttl() {
        let r = CardRegistry::new();
        let stale = SystemTime::now() - REGISTRATION_TTL - Duration::from_secs(1);
        r.register_at("c1", "prompt", stale);
        assert!(
            r.get("c1").is_none(),
            "expired registration is not returned"
        );
    }

    #[test]
    fn test_registry_returns_fresh_registration() {
        let r = CardRegistry::new();
        r.register_at("c1", "prompt", SystemTime::now());
        assert!(r.get("c1").is_some());
    }

    #[test]
    fn test_registry_get_evicts_expired_entry() {
        // The TTL promise in the module docs is "do not retain a large prompt
        // forever" — judging an entry stale on read is not enough, the memory
        // must actually be released.
        let r = CardRegistry::new();
        let stale = SystemTime::now() - REGISTRATION_TTL - Duration::from_secs(1);
        r.register_at("c1", "prompt", stale);
        assert!(r.get("c1").is_none());
        assert!(
            !r.inner.read().unwrap().contains_key("c1"),
            "expired entry is evicted, not just hidden"
        );
    }

    #[test]
    fn test_registry_register_sweeps_expired_entries() {
        // Chats that never come back must not leak their prompts: any register
        // call sweeps the whole map of expired entries.
        let r = CardRegistry::new();
        let stale = SystemTime::now() - REGISTRATION_TTL - Duration::from_secs(1);
        r.register_at("old", "prompt", stale);
        r.register("c1", "fresh".to_string(), "h".to_string())
            .unwrap();
        let map = r.inner.read().unwrap();
        assert!(!map.contains_key("old"), "expired entry swept on register");
        assert!(map.contains_key("c1"), "fresh entry survives the sweep");
    }
}

//! Secret resolution chain: OS keychain → environment → config file.
//!
//! Every secret is addressed by a plain key string:
//!
//! - `slimbot.<provider-name>` — provider API key (one per entry of
//!   `providers`).
//! - `slimbot.trigger.<secret-name>` — trigger secret (one per entry of
//!   `triggers.secrets`).
//! - `slimbot.mcp.<server>.<name>` — credential for one MCP connector, reached
//!   from a `${secret:<name>}` placeholder inside `mcp.servers[]` env/header
//!   values (ticket 26).
//!
//! Each key maps onto exactly one place per source:
//!
//! - **Keychain** — the key is the keyring *service*, [`KEYRING_ACCOUNT`] is
//!   the *account*. On macOS that is a generic password in the login keychain
//!   (`security add-generic-password -s slimbot.openai -a slimbot -w …`).
//! - **Environment** — the key upper-cased with every non-alphanumeric
//!   character mapped to `_` (`SLIMBOT_OPENAI`,
//!   `SLIMBOT_TRIGGER_GITHUB_TOKEN`).
//! - **Config file** — the value already stored in the field, which is why it
//!   is the last source of the chain and needs no lookup.
//!
//! Resolution is strict: the first source with a non-empty value wins, so a
//! keychain entry always beats an environment variable, which always beats the
//! config file.
//!
//! Nothing on this path ever logs or dumps a value: the store reports only the
//! key and the source it came from ([`Resolved::report`]), and values travel as
//! [`Secret`], whose `Debug`/`Display` are masked.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;

use crate::config::{McpServerConfig, ProviderConfig, TriggersConfig};

/// Keyring *account* every slimbot credential is stored under; the secret key
/// is the keyring *service*.
pub const KEYRING_ACCOUNT: &str = "slimbot";

/// Config key of a provider API key: `slimbot.<provider-name>`.
pub fn provider_key(provider: &str) -> String {
    format!("{}.{}", KEYRING_ACCOUNT, provider)
}

/// Config key of a trigger secret: `slimbot.trigger.<secret-name>`.
pub fn trigger_key(secret: &str) -> String {
    format!("{}.trigger.{}", KEYRING_ACCOUNT, secret)
}

/// Config key of one MCP connector credential: `slimbot.mcp.<server>.<name>`.
pub fn mcp_key(server: &str, name: &str) -> String {
    format!("{}.mcp.{}.{}", KEYRING_ACCOUNT, server, name)
}

/// Environment variable carrying the secret for `key`: upper-cased, with every
/// non-alphanumeric character mapped to `_`.
pub fn env_var_name(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// Where a secret came from. The order of the variants is the order of the
/// chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Keychain,
    Env,
    Config,
}

impl SourceKind {
    /// Human-readable name, safe to log.
    pub fn label(self) -> &'static str {
        match self {
            SourceKind::Keychain => "keychain",
            SourceKind::Env => "env",
            SourceKind::Config => "config file",
        }
    }
}

/// One place a secret can be read from. Implementations must not fail loudly:
/// a source that is unavailable or empty simply returns `None`, so the chain
/// falls through to the next one.
pub trait SecretSource: Send + Sync {
    fn get(&self, key: &str) -> Option<String>;
}

/// OS keychain: macOS Keychain, Windows Credential Manager, Secret Service.
///
/// Only the `apple-native` keyring backend is compiled in for now, so this
/// source actually resolves on macOS and is a no-op (`None`) elsewhere — the
/// chain then falls through to env and the config file.
///
/// A missing entry and an unavailable store are both `None` — a headless
/// machine without a keychain must degrade to env/config, not break startup.
pub struct KeychainSource;

impl SecretSource for KeychainSource {
    fn get(&self, key: &str) -> Option<String> {
        let entry = match keyring::Entry::new(key, KEYRING_ACCOUNT) {
            Ok(entry) => entry,
            Err(_) => {
                crate::debug!(
                    "[secrets] keychain cannot address {} (service unavailable)",
                    key
                );
                return None;
            }
        };
        match entry.get_password() {
            Ok(value) if !value.is_empty() => Some(value),
            Ok(_) => None,
            Err(keyring::Error::NoEntry) => None,
            Err(_) => {
                // The error text is dropped on purpose: only the key is safe
                // to log.
                crate::debug!("[secrets] keychain lookup for {} failed", key);
                None
            }
        }
    }
}

/// Environment variables, named by [`env_var_name`].
pub struct EnvSource;

impl SecretSource for EnvSource {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(env_var_name(key))
            .ok()
            .filter(|value| !value.is_empty())
    }
}

/// Fixed key → value map. Used by tests (which must never touch the real
/// keychain) and by embedders that already hold the secret elsewhere.
pub struct MapSource {
    values: HashMap<String, String>,
}

impl MapSource {
    pub fn new<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        }
    }
}

impl SecretSource for MapSource {
    fn get(&self, key: &str) -> Option<String> {
        self.values.get(key).cloned()
    }
}

/// A secret value. `Debug` and `Display` are masked so an accidental
/// `{:?}`/`{}` — in a log line, an error or a trace payload — cannot leak it;
/// [`Secret::expose`] is the only way out.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The actual value. Callers must not log, format or persist the result.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// The outcome of one lookup: the value plus the key and source it came from.
/// `Debug` is masked (it goes through [`Secret`]); [`Resolved::report`] is the
/// loggable form.
#[derive(Debug, Clone)]
pub struct Resolved {
    key: String,
    source: SourceKind,
    secret: Secret,
}

impl Resolved {
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn source(&self) -> SourceKind {
        self.source
    }

    /// The actual value. Callers must not log, format or persist the result.
    pub fn expose(&self) -> &str {
        self.secret.expose()
    }

    /// Key + source only — the text that may be logged.
    pub fn report(&self) -> String {
        format!("resolved {} from {}", self.key, self.source.label())
    }
}

/// Ordered sources plus the config-file fallback. Keychain first, env second,
/// config last (the fallback lives in the caller's field, so it is
/// [`SecretStore::resolve_or`] that consults it).
pub struct SecretStore {
    sources: Vec<(SourceKind, Box<dyn SecretSource>)>,
}

impl SecretStore {
    pub fn new(sources: Vec<(SourceKind, Box<dyn SecretSource>)>) -> Self {
        Self { sources }
    }

    /// The production store: OS keychain, then environment.
    ///
    /// Under `cfg(test)` this is deliberately empty: no test may touch the real
    /// OS keychain. Tests that need the chain inject their own sources
    /// ([`SecretStore::new`] + [`MapSource`]).
    pub fn system() -> Self {
        #[cfg(test)]
        {
            Self::empty()
        }
        #[cfg(not(test))]
        {
            Self::new(vec![
                (SourceKind::Keychain, Box::new(KeychainSource)),
                (SourceKind::Env, Box::new(EnvSource)),
            ])
        }
    }

    /// No keychain, no env: whatever the config file says wins. Used when the
    /// chain must not be consulted at all.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// First non-empty value along the chain, ignoring the config file.
    pub fn resolve(&self, key: &str) -> Option<Resolved> {
        self.sources.iter().find_map(|(kind, source)| {
            source
                .get(key)
                .filter(|value| !value.is_empty())
                .map(|value| Resolved {
                    key: key.to_string(),
                    source: *kind,
                    secret: Secret::new(value),
                })
        })
    }

    /// The full chain: sources first, the value already in the config file
    /// last. Returns `None` when the secret is nowhere to be found.
    pub fn resolve_or(&self, key: &str, config_value: &str) -> Option<Resolved> {
        self.resolve(key).or_else(|| {
            (!config_value.is_empty()).then(|| Resolved {
                key: key.to_string(),
                source: SourceKind::Config,
                secret: Secret::new(config_value),
            })
        })
    }
}

/// Fill every provider's `api_key` from the chain.
///
/// Resolution is logged as key + source only (never the value). A key that the
/// chain does not provide keeps the config-file value, so an empty `api_key`
/// stays empty and `Config::validate` reports it.
pub fn resolve_provider_secrets(
    providers: &mut HashMap<String, ProviderConfig>,
    store: &SecretStore,
) {
    for (name, provider) in providers.iter_mut() {
        let key = provider_key(name);
        if let Some(resolved) = store.resolve_or(&key, &provider.api_key) {
            provider.api_key = resolved.expose().to_string();
            report(&resolved);
        }
    }
}

/// Fill every named entry of `triggers.secrets` from the chain (key
/// `slimbot.trigger.<name>`). Unnamed entries are skipped: they have no key to
/// look up.
pub fn resolve_trigger_secrets(triggers: &mut TriggersConfig, store: &SecretStore) {
    for entry in triggers.secrets.iter_mut() {
        if entry.name.trim().is_empty() {
            continue;
        }
        let key = trigger_key(&entry.name);
        if let Some(resolved) = store.resolve_or(&key, &entry.value) {
            entry.value = resolved.expose().to_string();
            report(&resolved);
        }
    }
}

/// Log a resolution without its value: the config-file source needs no line.
fn report(resolved: &Resolved) {
    if resolved.source() != SourceKind::Config {
        crate::debug!("[secrets] {}", resolved.report());
    }
}

/// The marker every connector credential placeholder starts with. A key that
/// does not already carry it is expanded under the owning server.
const PLACEHOLDER_PREFIX: &str = "${secret:";

/// Replace every `${secret:<key>}` in `value` with the chain's answer for that
/// key (keychain → env). Keys written without the `slimbot.` prefix are
/// resolved as [`mcp_key`] for the owning server, so an entry can say
/// `${secret:token}` instead of repeating its own address.
///
/// A key the chain does not have leaves the placeholder verbatim: the connector
/// then fails its handshake with that text visible to the owner, which is a
/// better diagnosis than silently starting with an empty credential. Only the
/// key is ever logged.
fn substitute(value: &str, server: &str, store: &SecretStore) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    loop {
        let Some(start) = rest.find(PLACEHOLDER_PREFIX) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..start]);
        let after = &rest[start + PLACEHOLDER_PREFIX.len()..];
        let Some(end) = after.find('}') else {
            // Unterminated: keep the text as written, a typo is not our job to
            // repair by guessing where it was meant to close.
            out.push_str(&rest[start..]);
            return out;
        };
        let written = &after[..end];
        let key = if written.starts_with("slimbot.") {
            written.to_string()
        } else {
            mcp_key(server, written)
        };
        match store.resolve(&key) {
            Some(resolved) => {
                out.push_str(resolved.expose());
                report(&resolved);
            }
            None => {
                crate::debug!("[secrets] no source for connector key {key} (server {server})");
                out.push_str(&rest[start..=start + PLACEHOLDER_PREFIX.len() + end]);
            }
        }
        rest = &after[end + 1..];
    }
}

/// Fill every `${secret:...}` placeholder in the connector env/header values
/// from the chain. Values without placeholders pass through untouched, so a
/// credential written straight into the `0600` config file keeps working.
pub fn resolve_mcp_secrets(servers: &mut [McpServerConfig], store: &SecretStore) {
    for server in servers.iter_mut() {
        for value in server.env.values_mut() {
            if value.contains(PLACEHOLDER_PREFIX) {
                *value = substitute(value, &server.name, store);
            }
        }
        for value in server.headers.values_mut() {
            if value.contains(PLACEHOLDER_PREFIX) {
                *value = substitute(value, &server.name, store);
            }
        }
    }
}

/// Restrict `path` to owner read/write (`0600`). Returns whether the mode had
/// to be changed. A no-op off Unix.
#[cfg(unix)]
pub fn restrict_owner_only(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;

    let current = std::fs::metadata(path)?.permissions().mode() & 0o777;
    if current & 0o077 == 0 {
        return Ok(false);
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(true)
}

/// No file modes to fix off Unix.
#[cfg(not(unix))]
pub fn restrict_owner_only(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_and_trigger_keys_use_documented_names() {
        assert_eq!(provider_key("openai"), "slimbot.openai");
        assert_eq!(trigger_key("github_token"), "slimbot.trigger.github_token");
        assert_eq!(mcp_key("github", "token"), "slimbot.mcp.github.token");
    }

    #[test]
    fn test_env_var_name_is_derived_from_key() {
        assert_eq!(env_var_name("slimbot.openai"), "SLIMBOT_OPENAI");
        assert_eq!(
            env_var_name("slimbot.trigger.github-token"),
            "SLIMBOT_TRIGGER_GITHUB_TOKEN"
        );
    }

    fn store(keychain: &[(&str, &str)], env: &[(&str, &str)]) -> SecretStore {
        SecretStore::new(vec![
            (
                SourceKind::Keychain,
                Box::new(MapSource::new(keychain.iter().copied())),
            ),
            (
                SourceKind::Env,
                Box::new(MapSource::new(env.iter().copied())),
            ),
        ])
    }

    #[test]
    fn test_secret_store_keychain_hit_wins_over_env() {
        let store = store(
            &[("slimbot.openai", "from-keychain")],
            &[("slimbot.openai", "from-env")],
        );
        let resolved = store.resolve_or("slimbot.openai", "from-config").unwrap();
        assert_eq!(resolved.expose(), "from-keychain");
        assert_eq!(resolved.source(), SourceKind::Keychain);
    }

    #[test]
    fn test_secret_store_env_hit_wins_over_config_fallback() {
        let store = store(&[], &[("slimbot.openai", "from-env")]);
        let resolved = store.resolve_or("slimbot.openai", "from-config").unwrap();
        assert_eq!(resolved.expose(), "from-env");
        assert_eq!(resolved.source(), SourceKind::Env);
    }

    #[test]
    fn test_secret_store_config_is_only_used_when_chain_is_empty() {
        let store = store(&[], &[]);
        let resolved = store.resolve_or("slimbot.openai", "from-config").unwrap();
        assert_eq!(resolved.expose(), "from-config");
        assert_eq!(resolved.source(), SourceKind::Config);
    }

    #[test]
    fn test_secret_store_missing_everywhere_yields_none() {
        let store = store(&[], &[]);
        assert!(store.resolve("slimbot.openai").is_none());
        assert!(store.resolve_or("slimbot.openai", "").is_none());
    }

    #[test]
    fn test_secret_store_skips_empty_values() {
        let store = store(&[("slimbot.openai", "")], &[("slimbot.openai", "from-env")]);
        let resolved = store.resolve("slimbot.openai").unwrap();
        assert_eq!(resolved.expose(), "from-env");
        assert_eq!(resolved.source(), SourceKind::Env);
    }

    #[test]
    fn test_secret_debug_and_display_are_masked() {
        let secret = Secret::new("s3cr3t-value");
        assert!(!format!("{:?}", secret).contains("s3cr3t-value"));
        assert_eq!(format!("{:?}", secret), "Secret(***)");
        assert_eq!(format!("{}", secret), "***");
        assert_eq!(secret.expose(), "s3cr3t-value");
    }

    #[test]
    fn test_resolution_report_contains_key_and_source_but_no_value() {
        let store = store(&[("slimbot.openai", "s3cr3t-value")], &[]);
        let resolved = store.resolve("slimbot.openai").unwrap();
        let report = resolved.report();
        assert!(report.contains("slimbot.openai"));
        assert!(report.contains("keychain"));
        assert!(!report.contains("s3cr3t-value"));
        // The structured dump is masked too.
        assert!(!format!("{:?}", resolved).contains("s3cr3t-value"));
    }

    #[test]
    fn test_env_source_reads_variable_named_after_the_key() {
        // SAFETY: the variable name is unique to this test, so no other test
        // observes it. (Rust 2024 marks `set_var` unsafe for cross-thread
        // environment races.)
        unsafe { std::env::set_var("SLIMBOT_TEST_ONLY_KEY", "from-env") };
        let store = SecretStore::new(vec![(SourceKind::Env, Box::new(EnvSource))]);
        let resolved = store.resolve("slimbot.test-only-key").unwrap();
        assert_eq!(resolved.expose(), "from-env");
        assert_eq!(resolved.source(), SourceKind::Env);
    }

    // ── Connector placeholders ──

    fn mcp_server(name: &str, env: &[(&str, &str)], headers: &[(&str, &str)]) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_resolve_mcp_secrets_expands_short_key_under_the_server() {
        let store = store(&[("slimbot.mcp.github.token", "ghp-from-keychain")], &[]);
        let mut servers = vec![mcp_server(
            "github",
            &[("GITHUB_PERSONAL_ACCESS_TOKEN", "${secret:token}")],
            &[],
        )];

        resolve_mcp_secrets(&mut servers, &store);

        assert_eq!(
            servers[0].env["GITHUB_PERSONAL_ACCESS_TOKEN"],
            "ghp-from-keychain"
        );
    }

    #[test]
    fn test_resolve_mcp_secrets_expands_absolute_key_inside_a_larger_value() {
        // A header needs the scheme around the credential, so substitution is
        // embedded rather than whole-value.
        let store = store(&[("slimbot.trigger.github_token", "ghp-abc")], &[]);
        let mut servers = vec![mcp_server(
            "api",
            &[],
            &[(
                "Authorization",
                "Bearer ${secret:slimbot.trigger.github_token}",
            )],
        )];

        resolve_mcp_secrets(&mut servers, &store);

        assert_eq!(
            servers[0].headers["Authorization"], "Bearer ghp-abc",
            "an absolute key is used as written, and env beats the short form"
        );
    }

    #[test]
    fn test_resolve_mcp_secrets_env_source_fills_a_placeholder() {
        // SAFETY: unique variable name, observed only by this test.
        unsafe { std::env::set_var("SLIMBOT_MCP_HTTP_TEST_TOKEN", "from-env") };
        let mut servers = vec![mcp_server(
            "http-test",
            &[("TOKEN", "${secret:token}")],
            &[],
        )];

        resolve_mcp_secrets(
            &mut servers,
            &SecretStore::new(vec![(SourceKind::Env, Box::new(EnvSource))]),
        );

        assert_eq!(servers[0].env["TOKEN"], "from-env");
    }

    #[test]
    fn test_resolve_mcp_secrets_keeps_literal_values_and_unresolved_placeholders() {
        let mut servers = vec![mcp_server(
            "github",
            &[
                ("PLAIN", "ghp-written-into-config"),
                ("MISSING", "${secret:nope}"),
                ("UNTERMINATED", "${secret:oops"),
            ],
            &[],
        )];

        resolve_mcp_secrets(&mut servers, &store(&[], &[]));

        assert_eq!(servers[0].env["PLAIN"], "ghp-written-into-config");
        // Left verbatim so the owner sees which key is missing instead of a
        // server that fails on an empty credential.
        assert_eq!(servers[0].env["MISSING"], "${secret:nope}");
        assert_eq!(servers[0].env["UNTERMINATED"], "${secret:oops");
    }

    #[cfg(unix)]
    #[test]
    fn test_restrict_owner_only_sets_owner_only_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let changed = restrict_owner_only(&path).unwrap();
        assert!(changed);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // Already owner-only: nothing to do.
        assert!(!restrict_owner_only(&path).unwrap());
    }
}

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::secrets::SecretStore;

// Re-export for backward compatibility (other modules import these from crate::config)
pub use crate::config_defs::{
    AgentConfig, CronConfig, DreamConfig, ExecutorsConfig, HeartbeatConfig, JsonRenderConfig,
    McpConfig, McpServerConfig, McpTriggerSubscription, MemoryConfig, OwnerConfig, PressureConfig,
    ProviderConfig, SecretEntry, SubagentConfig, TraceBackendEntry, TraceConfig, TransportKind,
    TriggersConfig, WebFetchConfig,
};

// ── Root Config ──

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    pub agent: AgentConfig,
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default, deserialize_with = "deserialize_tools")]
    pub tools: ToolsConfig,
    #[serde(default, deserialize_with = "deserialize_channels")]
    pub channels: HashMap<String, ChannelConfig>,
    #[serde(default)]
    pub gateway: GatewayConfig,
    #[serde(default)]
    pub web_fetch: Option<WebFetchConfig>,
    #[serde(default)]
    pub tracing: TraceConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
    /// WebUI card generation guardrails. Top-level section: ops can toggle
    /// it independently of the WebUI channel entry itself.
    #[serde(default)]
    pub json_render: JsonRenderConfig,
    /// Subagent runtime guardrails. Top level because the capability is shared
    /// by the main loop and the trigger dispatcher.
    #[serde(default)]
    pub subagent: SubagentConfig,
    /// Sprawl guardrails for the executor store. Top level because `executors/`
    /// sits at the workspace root, not under `triggers/` (spec A).
    #[serde(default)]
    pub executors: ExecutorsConfig,
    /// Trigger layer configuration (tick cadence, queue limits, secrets).
    #[serde(default)]
    pub triggers: TriggersConfig,
    /// Context-pressure ratio table: every length limit is a percentage of the
    /// provider window (see `docs/config.md` and ADR 0013).
    #[serde(default)]
    pub pressure: PressureConfig,
    /// MCP connectors. Gateway-level: the pool is owned by `McpService`, and
    /// its tools reach the model only through codemode (ADR 0016).
    #[serde(default)]
    pub mcp: McpConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GatewayConfig {
    /// The single principal this gateway serves (v1: permanently single-user).
    #[serde(default)]
    pub owner: OwnerConfig,
    #[serde(default)]
    pub cron: CronConfig,
    #[serde(default)]
    pub heartbeat: HeartbeatConfig,
    #[serde(default)]
    pub dream: DreamConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ChannelConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl Default for ChannelConfig {
    fn default() -> Self {
        ChannelConfig {
            enabled: true,
            extra: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// The `tools` section: the tool table plus the approval policy that gates it.
///
/// The approval policy is read once at startup and snapshotted into the
/// in-memory pending store; editing it while the gateway runs does not change
/// the policy in effect (restart to apply, ADR 0011).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolsConfig {
    #[serde(default)]
    pub entries: Vec<ToolEntry>,
    #[serde(default)]
    pub approval: crate::approval::ApprovalConfig,
}

/// Read `tools` from either the object shape (`{entries, approval}`) or the
/// legacy bare array of tool entries, so existing config files keep loading.
fn deserialize_tools<'de, D>(deserializer: D) -> std::result::Result<ToolsConfig, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ToolsRepr {
        Entries(Vec<ToolEntry>),
        Section(ToolsConfig),
    }

    Ok(match ToolsRepr::deserialize(deserializer)? {
        ToolsRepr::Entries(entries) => ToolsConfig {
            entries,
            approval: Default::default(),
        },
        ToolsRepr::Section(section) => section,
    })
}

fn default_true() -> bool {
    true
}

pub fn default_heartbeat_interval() -> u64 {
    1800
}

pub fn default_max_chars() -> usize {
    50000
}

pub fn default_timeout() -> u64 {
    30
}

pub fn default_user_agent() -> String {
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_7_2) AppleWebKit/537.36".to_string()
}

// ── Dream Config Helpers ──

pub fn default_dream_interval_ms() -> i64 {
    7200000
}

pub fn default_dream_max_entries() -> usize {
    20
}

// ── Backward compat: Vec<ChannelEntry> → HashMap ──

fn deserialize_channels<'de, D>(
    deserializer: D,
) -> std::result::Result<HashMap<String, ChannelConfig>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::{self, Visitor};

    struct ChannelsVisitor;

    impl<'de> Visitor<'de> for ChannelsVisitor {
        type Value = HashMap<String, ChannelConfig>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a map or a list of channel entries")
        }

        fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
        where
            M: de::MapAccess<'de>,
        {
            let mut m = HashMap::new();
            while let Some((key, value)) = map.next_entry::<String, ChannelConfig>()? {
                m.insert(key, value);
            }
            Ok(m)
        }

        fn visit_seq<A>(self, mut seq: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: de::SeqAccess<'de>,
        {
            let mut m = HashMap::new();
            let mut idx = 0;
            while let Some(entry) = seq.next_element::<ChannelEntryCompat>()? {
                let key = if entry.r#type.is_empty() {
                    format!("legacy_{idx}")
                } else {
                    entry.r#type.clone()
                };
                m.insert(key, entry.into_channel_config());
                idx += 1;
            }
            Ok(m)
        }
    }

    deserializer.deserialize_any(ChannelsVisitor)
}

#[derive(Debug, Deserialize)]
struct ChannelEntryCompat {
    #[serde(default)]
    r#type: String,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    config: serde_json::Value,
}

impl ChannelEntryCompat {
    fn into_channel_config(self) -> ChannelConfig {
        let mut extra = match self.config {
            serde_json::Value::Object(m) => m.into_iter().collect::<HashMap<_, _>>(),
            _ => HashMap::new(),
        };
        extra.insert("enabled".to_string(), serde_json::json!(self.enabled));
        ChannelConfig {
            enabled: self.enabled,
            extra,
        }
    }
}

/// Refuse a subscription that cannot name what to watch or where to put it.
///
/// `clamp()` has already forced the kind into the whitelist, so the match here
/// only has to split the two kinds apart; the empty `uri` / `tool` halves the
/// other kind leaves behind are not checked.
fn validate_trigger_subscriptions(server: &McpServerConfig) -> Result<()> {
    for (idx, subscription) in server.triggers.iter().enumerate() {
        let at = format!("mcp.servers.{}.triggers[{}]", server.name, idx);
        if subscription.source_id.trim().is_empty() {
            anyhow::bail!("{at}: needs a source_id");
        }
        match subscription.r#type.as_str() {
            "notification" if subscription.uri.trim().is_empty() => {
                anyhow::bail!("{at}: a \"notification\" subscription needs a uri");
            }
            "tool" if subscription.tool.trim().is_empty() => {
                anyhow::bail!("{at}: a \"tool\" subscription needs a tool name");
            }
            _ => {}
        }
    }
    Ok(())
}

/// Refuse two subscriptions that claim one source id.
///
/// A `source_id` is the key a condition script reads the tick frame under
/// (`ctx.sources[id]`), and the layer holds exactly one runtime per key — so two
/// declarations of one id would leave the script one source where the config
/// described two, with no way to tell which one answered. That is a
/// configuration question, not a runtime one: refuse it at load, naming both
/// declarations, rather than pick a winner.
///
/// This walks every server, not just the enabled ones: a disabled entry's
/// subscription comes back on the tick it is re-enabled, and two declarations
/// of one id would then collide at runtime.
fn validate_subscription_source_ids(servers: &[McpServerConfig]) -> Result<()> {
    /// Where one subscription of one server put a source id.
    struct Claim {
        server: String,
        at: String,
    }
    let mut claims: HashMap<String, Claim> = HashMap::new();
    for server in servers {
        for (idx, subscription) in server.triggers.iter().enumerate() {
            let id = subscription.source_id.trim();
            // An empty id is the other validator's refusal; it must not turn
            // into a collision report that hides the real cause.
            if id.is_empty() {
                continue;
            }
            let at = format!("mcp.servers.{}.triggers[{}]", server.name, idx);
            match claims.get(id) {
                Some(first) => anyhow::bail!(
                    "mcp.servers: source_id `{id}` is declared twice ({} on server `{}`, {} on server `{}`); a source id is the key a condition reads, so it must name exactly one subscription",
                    first.at,
                    first.server,
                    at,
                    server.name
                ),
                None => {
                    claims.insert(
                        id.to_string(),
                        Claim {
                            server: server.name.clone(),
                            at,
                        },
                    );
                }
            }
        }
    }
    Ok(())
}

impl Config {
    pub fn clamp(&mut self) {
        self.agent.clamp();
        for provider in self.providers.values_mut() {
            provider.clamp();
        }
        self.tracing.clamp();
        for backend in self.tracing.backends.iter_mut() {
            backend.clamp();
        }
        self.gateway.owner.clamp();
        self.gateway.cron.clamp();
        self.gateway.heartbeat.clamp();
        self.gateway.dream.clamp();
        self.json_render.clamp();
        self.memory.clamp();
        self.subagent.clamp();
        self.executors.clamp();
        self.triggers.clamp();
        for secret in self.triggers.secrets.iter_mut() {
            secret.clamp();
        }
        self.pressure.clamp();
        self.tools.approval.clamp();
        self.mcp.clamp();
        for server in self.mcp.servers.iter_mut() {
            server.clamp();
            // A subscription is stored in its declared shape, so an
            // over-long value is clamped once, on load, not on every use.
            for subscription in server.triggers.iter_mut() {
                subscription.clamp();
            }
        }
        if let Some(wf) = &mut self.web_fetch {
            wf.clamp();
        }
    }

    /// Normalize the parts of the config that have a canonical form.
    ///
    /// This is the only normalization step and it runs once, right after
    /// `clamp()` on the effective load path. A separate module with a trait
    /// would leave the same rules in two places without adding a seam.
    /// `api_key` is deliberately untouched: resolving the secret chain is a
    /// later step on [`Self::load_resolved`], never a normalization.
    pub fn normalize(&mut self) {
        if self.agent.provider.is_empty() {
            self.agent.provider = "default".to_string();
        }
        for provider in self.providers.values_mut() {
            if provider.api_url.is_empty() && !provider.base_url.is_empty() {
                let base = provider.base_url.trim_end_matches('/');
                provider.api_url = if base.ends_with("/chat/completions") {
                    base.to_string()
                } else if base.ends_with("/v1") {
                    format!("{}/chat/completions", base)
                } else {
                    format!("{}/v1/chat/completions", base)
                };
            }
            if provider.api_url.is_empty() {
                provider.api_url = "https://api.openai.com/v1/chat/completions".to_string();
            }
        }
        // Remove entries with empty name/type
        self.tools.entries.retain(|t| !t.name.is_empty());
        self.channels.retain(|name, _| !name.is_empty());
        // A connector without a name has no callable prefix and no loggable
        // identity, so it is dropped rather than validated into an error.
        self.mcp.servers.retain(|s| !s.name.trim().is_empty());
    }

    /// Connector-specific wiring checks. A malformed server entry is a
    /// configuration error, not a runtime one: better to refuse the gateway
    /// than to bring up a pool that can never answer a call.
    fn validate_mcp(&self) -> Result<()> {
        // Two subscriptions that claim one source id are refused whether or not
        // their servers are enabled: see `validate_subscription_source_ids`.
        validate_subscription_source_ids(&self.mcp.servers)?;
        let mut seen: HashMap<String, String> = HashMap::new();
        for server in &self.mcp.servers {
            // A disabled entry is never connected to, so its transport shape
            // cannot strand a call: don't refuse the whole gateway over a
            // toggle-off server that hasn't been filled in. The name-collision
            // check below still runs — the name matters if it's ever re-enabled.
            if server.enabled {
                match server.transport_kind() {
                    TransportKind::Stdio => {
                        if server.command.trim().is_empty() {
                            anyhow::bail!(
                                "mcp.servers.{}: transport \"stdio\" needs a command",
                                server.name
                            );
                        }
                    }
                    TransportKind::Http => {
                        if !server.url.starts_with("http://") && !server.url.starts_with("https://")
                        {
                            anyhow::bail!(
                                "mcp.servers.{}: transport \"http\" needs a url starting with http:// or https://",
                                server.name
                            );
                        }
                    }
                }
                // A subscription is a source declaration: without it the tick
                // has nothing to collect under and the entry is silently dead.
                validate_trigger_subscriptions(server)?;
            }
            // Two names that sanitize to the same callable prefix would be
            // indistinguishable to a script (`gh-mcp` vs `gh_mcp`).
            let prefix = crate::mcp::server_prefix(&server.name);
            if let Some(clash) = seen.insert(prefix.clone(), server.name.clone()) {
                anyhow::bail!(
                    "mcp.servers: names `{}` and `{}` both address tools as `{prefix}`",
                    clash,
                    server.name
                );
            }
        }
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        if self.agent.provider.is_empty() {
            anyhow::bail!("agent.provider must not be empty");
        }
        let provider = self.providers.get(&self.agent.provider).ok_or_else(|| {
            anyhow::anyhow!(
                "Provider '{}' referenced by agent not found in providers",
                self.agent.provider
            )
        })?;
        if provider.api_key.is_empty() {
            // Same error family as before, but it now names the whole
            // resolution chain: keychain → env → config file.
            anyhow::bail!(
                "provider '{}'.api_key must not be empty. Set it in your config file, export {}, or store it in the OS keychain as `{}`.",
                self.agent.provider,
                crate::secrets::env_var_name(&crate::secrets::provider_key(&self.agent.provider)),
                crate::secrets::provider_key(&self.agent.provider)
            );
        }
        if provider.model.is_empty() {
            anyhow::bail!("provider '{}'.model must not be empty", self.agent.provider);
        }
        self.validate_mcp()?;
        Ok(())
    }

    /// Read the file without validation or secret resolution.
    ///
    /// A preview of disk state, not an effective config: setup and the WebUI
    /// editor need to see a file whose `api_key` is empty, or whose provider
    /// reference is not yet filled in, without `validate()` refusing it.
    pub fn load_for_preview(path: &str) -> Result<Self> {
        Self::read(path)
    }

    /// The single effective entry: read → resolve secrets → validate.
    ///
    /// Both startup paths (CLI and gateway) go through here. The WebUI editor
    /// deliberately does not: it patches the file shape and validates a
    /// resolved *candidate* without writing the resolved values back.
    pub fn load_resolved(path: &str) -> Result<Self> {
        Self::load_resolved_with(path, &SecretStore::system())
    }

    /// [`Self::load_resolved`] with an injected store (tests, embedders).
    pub fn load_resolved_with(path: &str, store: &SecretStore) -> Result<Self> {
        let mut config = Self::read(path)?;
        config.resolve_secrets(store);
        config.validate()?;
        Ok(config)
    }

    /// Read + parse + clamp + normalize. Also tightens the file mode to `0600`
    /// when the file carries secret-typed values (Unix only).
    fn read(path: &str) -> Result<Self> {
        let content = std::fs::read_to_string(path).context("Failed to read config file")?;
        let mut config = Self::parse(&content)?;
        config.clamp();
        config.normalize();
        config.restrict_secret_file_mode(path);
        Ok(config)
    }

    /// Tighten `config.json` to `0600` when it carries secret-typed values. The
    /// mode is a property of the file, so a failure is logged (with the path
    /// only) rather than propagated: an unwritable mode must not make a
    /// readable config look broken.
    fn restrict_secret_file_mode(&self, path: &str) {
        if !self.contains_secrets() {
            return;
        }
        match crate::secrets::restrict_owner_only(Path::new(path)) {
            Ok(true) => crate::debug!("[secrets] {} restricted to 0600 (holds secrets)", path),
            Ok(false) => {}
            Err(e) => crate::warn_log!("[secrets] cannot restrict {} to 0600: {}", path, e),
        }
    }

    /// True when a secret-typed field (provider `api_key`, `triggers.secrets`
    /// value, connector env/header, channel credential field) carries a value,
    /// or a channel field holds a `${secret:}` placeholder. Drives the `0600`
    /// file mode.
    pub fn contains_secrets(&self) -> bool {
        self.providers.values().any(|p| !p.api_key.is_empty())
            || self.triggers.secrets.iter().any(|s| !s.value.is_empty())
            || self.mcp.servers.iter().any(|s| s.holds_secret())
            || self.channels.values().any(|c| {
                c.extra.iter().any(|(k, v)| {
                    let Some(text) = v.as_str() else { return false };
                    if text.is_empty() {
                        return false;
                    }
                    // A placeholder means the chain is involved; a literal in
                    // a known credential field is a raw secret sitting in the
                    // config file — both say the file must be owner-only.
                    text.contains("${secret:") || is_secret_field_name(k)
                })
            })
    }

    /// Replace secret-typed fields with the result of the resolution chain.
    ///
    /// Keychain wins over environment, which wins over the value already in the
    /// file. Nothing here logs a value: the resolution report carries only the
    /// key and the source it came from.
    pub fn resolve_secrets(&mut self, store: &SecretStore) {
        crate::secrets::resolve_provider_secrets(&mut self.providers, store);
        crate::secrets::resolve_trigger_secrets(&mut self.triggers, store);
        crate::secrets::resolve_mcp_secrets(&mut self.mcp.servers, store);
        crate::secrets::resolve_channel_secrets(&mut self.channels, store);
    }

    /// Parse a config document, applying the legacy-key migrations first.
    ///
    /// Old keys are read here (and their values honoured) before being removed
    /// from the document, so a migrated config is written back in the new shape
    /// by the next `setup`/save and the note only fires once.
    fn parse(content: &str) -> Result<Self> {
        let mut raw: serde_json::Value =
            serde_json::from_str(content).context("Invalid config JSON")?;
        for note in crate::config_migrate::migrate_legacy_keys(&mut raw) {
            crate::config_migrate::emit(&note);
        }
        serde_json::from_value(raw).context("Invalid config JSON")
    }

    pub fn save(&self, path: &str) -> Result<()> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        // A file that holds secrets is readable by its owner only; a file
        // without secrets keeps whatever mode the user gave it.
        self.restrict_secret_file_mode(path);
        Ok(())
    }
}

/// Channel `extra` field names that are known to hold a credential. The open
/// `extra` map has no schema to consult, so the list mirrors what the adapters
/// read: a literal in one of these fields is a raw secret in the config file
/// and must trigger the `0600` tightening, exactly like an inline provider key.
fn is_secret_field_name(key: &str) -> bool {
    matches!(
        key,
        "app_secret" | "verification_token" | "encrypt_key" | "client_secret" | "api_key" | "token"
    )
}

// ── Startup snapshot ──

/// The one config the running process was started with (ADR 0002).
///
/// Installed once at startup and never mutated afterwards: there is no hot
/// reload. Consumers built from a path alone (tool factories, the snapshot
/// store) read it instead of threading a config handle through every
/// constructor. `None` means no startup happened (unit tests), in which case
/// those consumers use their documented defaults.
static STARTUP_CONFIG: OnceLock<Config> = OnceLock::new();

impl Config {
    /// Install the process-wide startup snapshot, before any tool or service
    /// is built. A second install is a wiring bug, not a silent overwrite.
    pub fn install(config: &Config) -> Result<()> {
        STARTUP_CONFIG
            .set(config.clone())
            .map_err(|_| anyhow::anyhow!("startup config already installed"))
    }

    /// The installed startup snapshot, or `None` outside a started process.
    pub fn try_get() -> Option<&'static Config> {
        STARTUP_CONFIG.get()
    }
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_config() -> Config {
        let mut providers = HashMap::new();
        providers.insert(
            "default".to_string(),
            ProviderConfig {
                r#type: "openai".to_string(),
                api_url: String::new(),
                base_url: String::new(),
                api_key: "sk-test".to_string(),
                model: "gpt-4o".to_string(),
                temperature: 0.7,
                max_tokens: 4096,
                context_window_tokens: 32768,
                prompt_cache_enabled: true,
                disable_thinking: false,
                timeout_seconds: 180,
                unknown: Default::default(),
            },
        );
        Config {
            agent: AgentConfig {
                provider: "default".to_string(),
                max_iterations: 40,
                timeout_seconds: 120,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers,
            tools: Default::default(),
            channels: HashMap::new(),
            gateway: GatewayConfig::default(),
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
        }
    }

    #[test]
    fn test_config_validate_empty_provider() {
        let mut cfg = make_test_config();
        cfg.agent.provider = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_mcp_enabled_http_without_url_is_rejected() {
        let config: Config = serde_json::from_str(
            r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "k", "model": "m"}},
            "mcp": {"servers": [{"name": "gh", "transport": "http", "url": ""}]}
        }"#,
        )
        .unwrap();
        assert!(
            config.validate().is_err(),
            "an enabled http connector with no url must be refused"
        );
    }

    #[test]
    fn test_validate_mcp_duplicate_source_id_across_servers_is_refused() {
        // One source id is one key in the tick frame, so two servers declaring
        // it describe a source that cannot exist. The refusal has to name the
        // id and both servers: the owner's fix is to rename one of them, and
        // there is no other clue in the config.
        let config: Config = serde_json::from_str(
            r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "k", "model": "m"}},
            "mcp": {"servers": [
                {"name": "ci", "command": "a",
                 "triggers": [{"type": "notification", "uri": "notifications/builds", "source_id": "builds"}]},
                {"name": "deploys", "command": "b",
                 "triggers": [{"type": "notification", "uri": "notifications/builds", "source_id": "builds"}]}
            ]}
        }"#,
        )
        .unwrap();
        let err = config
            .validate()
            .expect_err("two servers may not declare one source id")
            .to_string();
        for expected in ["builds", "ci", "deploys"] {
            assert!(
                err.contains(expected),
                "the error names `{expected}`: {err}"
            );
        }
    }

    #[test]
    fn test_validate_mcp_duplicate_source_id_within_one_server_is_refused() {
        // The same collision inside one server: two subscriptions of one server
        // pointing at one id would lose one of them silently in the layer.
        let config: Config = serde_json::from_str(
            r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "k", "model": "m"}},
            "mcp": {"servers": [
                {"name": "ci", "command": "a",
                 "triggers": [
                    {"type": "notification", "uri": "notifications/builds", "source_id": "ci-events"},
                    {"type": "tool", "tool": "list_runs", "source_id": "ci-events"}
                 ]}
            ]}
        }"#,
        )
        .unwrap();
        let err = config
            .validate()
            .expect_err("one server may not declare one source id twice")
            .to_string();
        assert!(err.contains("ci-events"), "{err}");
    }

    #[test]
    fn test_validate_mcp_distinct_source_ids_across_servers_are_allowed() {
        // The control for the two refusals above: distinct ids, even on the
        // same uri, are the normal case and must load.
        let config: Config = serde_json::from_str(
            r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "k", "model": "m"}},
            "mcp": {"servers": [
                {"name": "ci", "command": "a",
                 "triggers": [{"type": "notification", "uri": "notifications/builds", "source_id": "builds"}]},
                {"name": "deploys", "command": "b",
                 "triggers": [{"type": "notification", "uri": "notifications/builds", "source_id": "deploys"}]}
            ]}
        }"#,
        )
        .unwrap();
        config.validate().expect("distinct ids are fine");
    }

    #[test]
    fn test_mcp_disabled_server_with_bad_shape_is_allowed() {
        // A toggle-off entry the pool never touches must not block startup.
        let config: Config = serde_json::from_str(
            r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"type": "openai", "api_key": "k", "model": "m"}},
            "mcp": {"servers": [{"name": "gh", "transport": "http", "enabled": false, "url": ""}]}
        }"#,
        )
        .unwrap();
        assert!(
            config.validate().is_ok(),
            "a disabled, half-filled connector must not refuse the gateway"
        );
    }

    #[test]
    fn test_web_fetch_config_defaults() {
        let config_str = r#"{
            "agent": {"provider": "test", "max_iterations": 10},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}}
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        assert!(config.web_fetch.is_none());
    }

    #[test]
    fn test_web_fetch_config_with_values() {
        let config_str = r#"{
            "agent": {"provider": "test", "max_iterations": 10},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "web_fetch": {
                "enabled": true,
                "max_chars": 10000,
                "timeout_s": 60,
                "user_agent": "CustomAgent/1.0"
            }
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        let wf = config.web_fetch.unwrap();
        assert_eq!(wf.max_chars, 10000);
        assert_eq!(wf.timeout_s, 60);
        assert_eq!(wf.user_agent, "CustomAgent/1.0");
    }

    #[test]
    fn test_web_fetch_config_partial_defaults() {
        let config_str = r#"{
            "agent": {"provider": "test", "max_iterations": 10},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "web_fetch": {
                "max_chars": 8000
            }
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        let wf = config.web_fetch.unwrap();
        // Provided value is used
        assert_eq!(wf.max_chars, 8000);
        // Missing fields fall back to defaults
        assert_eq!(wf.timeout_s, 30);
        assert!(!wf.user_agent.is_empty());
    }

    #[test]
    fn test_config_validate_missing_provider_ref() {
        let mut cfg = make_test_config();
        cfg.agent.provider = "nonexistent".to_string();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_validate_empty_api_key() {
        let mut cfg = make_test_config();
        cfg.providers.get_mut("default").unwrap().api_key = String::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn test_config_validate_pass() {
        let cfg = make_test_config();
        assert!(cfg.validate().is_ok());
    }

    // ── Secret resolution chain ──

    use crate::secrets::{MapSource, SecretStore, SourceKind};

    /// Injected sources only: no test ever touches the real OS keychain.
    fn secret_store(keychain: &[(&str, &str)], env: &[(&str, &str)]) -> SecretStore {
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

    fn write_config_file(tmp: &std::path::Path, api_key: &str) -> std::path::PathBuf {
        let path = tmp.join("config.json");
        std::fs::write(
            &path,
            format!(
                r#"{{"agent":{{"provider":"default"}},"providers":{{"default":{{"type":"openai","api_key":"{}","model":"gpt-4o"}}}}}}"#,
                api_key
            ),
        )
        .unwrap();
        path
    }

    #[test]
    fn test_resolve_secrets_keychain_hit_beats_env_and_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path(), "from-config");
        let store = secret_store(
            &[("slimbot.default", "from-keychain")],
            &[("slimbot.default", "from-env")],
        );

        let config = Config::load_resolved_with(path.to_str().unwrap(), &store).unwrap();
        assert_eq!(config.providers["default"].api_key, "from-keychain");
    }

    #[test]
    fn test_resolve_secrets_env_hit_beats_config() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path(), "from-config");
        let store = secret_store(&[], &[("slimbot.default", "from-env")]);

        let config = Config::load_resolved_with(path.to_str().unwrap(), &store).unwrap();
        assert_eq!(config.providers["default"].api_key, "from-env");
    }

    #[test]
    fn test_resolve_secrets_falls_back_to_config_value() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path(), "from-config");

        let config =
            Config::load_resolved_with(path.to_str().unwrap(), &SecretStore::empty()).unwrap();
        assert_eq!(config.providers["default"].api_key, "from-config");
    }

    #[test]
    fn test_resolve_secrets_fills_trigger_secret_from_keychain() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default"},"providers":{"default":{"type":"openai","api_key":"k","model":"gpt-4o"}},"triggers":{"secrets":[{"name":"github","value":""}]}}"#,
        )
        .unwrap();
        let store = secret_store(&[("slimbot.trigger.github", "ghp-from-keychain")], &[]);

        let config = Config::load_resolved_with(path.to_str().unwrap(), &store).unwrap();
        assert_eq!(config.triggers.secrets[0].value, "ghp-from-keychain");
    }

    #[test]
    fn test_load_resolved_missing_everywhere_keeps_api_key_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path(), "");

        let err = Config::load_resolved_with(path.to_str().unwrap(), &SecretStore::empty())
            .expect_err("empty api_key with no source must fail");
        let message = err.to_string();
        // Same error family as before; the message now names the other sources.
        assert!(message.contains("provider 'default'.api_key must not be empty"));
        assert!(message.contains("slimbot.default"));
        assert!(message.contains("SLIMBOT_DEFAULT"));
    }

    #[test]
    fn test_config_contains_secrets_tracks_secret_fields() {
        let mut cfg = make_test_config();
        assert!(cfg.contains_secrets());
        cfg.providers.get_mut("default").unwrap().api_key = String::new();
        assert!(!cfg.contains_secrets());
        cfg.triggers.secrets.push(crate::config::SecretEntry {
            name: "token".to_string(),
            value: "v".to_string(),
            ..Default::default()
        });
        assert!(cfg.contains_secrets());
        cfg.triggers.secrets.clear();
        // A channel placeholder pulls in the secret chain (0600)...
        cfg.channels.insert(
            "feishu".to_string(),
            ChannelConfig {
                enabled: true,
                extra: [(
                    "app_secret".to_string(),
                    serde_json::json!("${secret:app_secret}"),
                )]
                .into_iter()
                .collect(),
            },
        );
        assert!(cfg.contains_secrets());
        // ...and so does a literal credential in a known secret field, the
        // same stance as an inline provider api_key.
        cfg.channels.insert(
            "feishu".to_string(),
            ChannelConfig {
                enabled: true,
                extra: [("app_secret".to_string(), serde_json::json!("raw-secret"))]
                    .into_iter()
                    .collect(),
            },
        );
        assert!(cfg.contains_secrets());
        // A non-secret channel field is not.
        cfg.channels.insert(
            "feishu".to_string(),
            ChannelConfig {
                enabled: true,
                extra: [("owner_open_id".to_string(), serde_json::json!("ou_x"))]
                    .into_iter()
                    .collect(),
            },
        );
        assert!(!cfg.contains_secrets());
    }

    #[cfg(unix)]
    #[test]
    fn test_config_save_with_secrets_restricts_mode_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        // An existing, broadly readable file must be tightened on write.
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        make_test_config().save(path.to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn test_config_save_without_secrets_keeps_existing_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let mut cfg = make_test_config();
        cfg.providers.get_mut("default").unwrap().api_key = String::new();
        cfg.save(path.to_str().unwrap()).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[cfg(unix)]
    #[test]
    fn test_config_load_with_secrets_tightens_world_readable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path(), "sk-plaintext");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        Config::load_resolved_with(
            path.to_str().unwrap(),
            &crate::secrets::SecretStore::empty(),
        )
        .unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn test_config_clamp() {
        let mut cfg = make_test_config();
        cfg.agent.max_iterations = 9999;
        cfg.providers.get_mut("default").unwrap().temperature = 5.0;
        cfg.clamp();
        assert_eq!(cfg.agent.max_iterations, 200);
        assert!((cfg.providers["default"].temperature - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_memory_config_default_backend_is_markdown() {
        let cfg = make_test_config();
        assert_eq!(cfg.memory.backend, "markdown");
    }

    #[test]
    fn test_memory_config_invalid_backend_falls_back_to_default() {
        let mut cfg = make_test_config();
        cfg.memory.backend = "sqlite".to_string();
        cfg.clamp();
        assert_eq!(cfg.memory.backend, "markdown");
    }

    #[test]
    fn test_memory_config_valid_backend_kept() {
        let mut cfg = make_test_config();
        cfg.memory.backend = "markdown".to_string();
        cfg.clamp();
        assert_eq!(cfg.memory.backend, "markdown");
    }

    #[test]
    fn test_memory_config_load_missing_field_defaults() {
        let config_str = r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"api_key": "sk", "model": "m"}}
        }"#;
        let cfg: Config = serde_json::from_str(config_str).unwrap();
        assert_eq!(cfg.memory.backend, "markdown");
    }

    #[test]
    fn test_memory_config_load_custom_backend() {
        let config_str = r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"api_key": "sk", "model": "m"}},
            "memory": {"backend": "markdown"}
        }"#;
        let cfg: Config = serde_json::from_str(config_str).unwrap();
        assert_eq!(cfg.memory.backend, "markdown");
    }

    #[test]
    fn test_config_normalize() {
        let mut cfg = make_test_config();
        cfg.agent.provider = String::new();
        cfg.normalize();
        assert_eq!(cfg.agent.provider, "default");
    }

    #[test]
    fn test_config_clamp_zero_values() {
        let mut cfg = make_test_config();
        cfg.agent.max_iterations = 0;
        cfg.agent.timeout_seconds = 0;
        cfg.clamp();
        assert_eq!(cfg.agent.max_iterations, 1); // clamped to minimum
        assert_eq!(cfg.agent.timeout_seconds, 1); // clamped to minimum (range(1, 600))
    }

    #[test]
    fn test_executors_max_count_default_is_a_loose_twenty() {
        // Spec M: v1 ships the soft cap disabled in spirit -- a loose default
        // that a deployment opts into tightening. Pinned so nobody quietly turns
        // the knob into a hard gate on ordinary creation.
        assert_eq!(ExecutorsConfig::default().max_count, 20);
        assert_eq!(Config::default().executors.max_count, 20);
    }

    #[test]
    fn test_executors_max_count_zero_stays_disabled_through_clamp() {
        // `0` is the documented "no cap" value, so clamp must not raise it to a
        // minimum the way the other max_* knobs do.
        let mut cfg = make_test_config();
        cfg.executors.max_count = 0;
        cfg.clamp();
        assert_eq!(cfg.executors.max_count, 0);
    }

    #[test]
    fn test_executors_max_count_clamps_into_range() {
        let mut cfg = make_test_config();
        cfg.executors.max_count = 1_000_000;
        cfg.clamp();
        assert_eq!(cfg.executors.max_count, 10_000); // range(0, 10_000)
    }

    #[test]
    fn test_config_clamp_extreme_values() {
        let mut cfg = make_test_config();
        cfg.agent.max_iterations = u32::MAX;
        cfg.agent.timeout_seconds = u64::MAX;
        cfg.providers
            .get_mut("default")
            .unwrap()
            .context_window_tokens = u32::MAX;
        cfg.clamp();
        assert_eq!(cfg.agent.max_iterations, 200); // max clamp
        assert_eq!(cfg.agent.timeout_seconds, 600); // max clamp
        assert_eq!(
            cfg.providers.get("default").unwrap().context_window_tokens,
            200_000
        ); // max clamp from range(1024, 200_000)
    }

    #[test]
    fn test_config_load_with_negative_values_in_json() {
        // Negative numeric values in JSON are deserialized as the underlying integer type's
        // two's complement representation for unsigned types (serde_json behavior).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        // -1 as u32 in JSON would be rejected by serde_json as out of range
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default","max_iterations":-1},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}}}"#,
        )
        .unwrap();
        let result = Config::load_for_preview(path.to_str().unwrap());
        // Serde_json should reject negative for unsigned
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, "not valid json").unwrap();
        let result = Config::load_for_preview(path.to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn test_config_load_empty_providers() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(&path, r#"{"agent":{"provider":"default"},"providers":{}}"#).unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn test_config_unknown_fields_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default","unknown_field":42},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4","unknown":"ignored"}}}"#,
        )
        .unwrap();
        let result = Config::load_for_preview(path.to_str().unwrap());
        // Should succeed — unknown fields are ignored
        assert!(result.is_ok());
    }

    #[test]
    fn test_config_save_and_reload_with_zero_values() {
        // load_for_preview applies clamp, so zero values should be clamped to minimum.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default","max_iterations":0,"timeout_seconds":0},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}}}"#,
        )
        .unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.agent.max_iterations, 1); // clamp(0) -> 1
        assert_eq!(cfg.agent.timeout_seconds, 1); // clamp(0) -> 1
    }

    #[test]
    fn test_config_null_values() {
        // Null values for optional/required fields should be handled gracefully
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default"},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4","api_url":null}}}"#,
        )
        .unwrap();
        let result = Config::load_for_preview(path.to_str().unwrap());
        // Should either succeed with null ignored or fail gracefully
        assert!(result.is_err() || result.unwrap().providers.contains_key("default"));
    }

    #[test]
    fn test_config_tools_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default"},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}},"tools":[{"name":"shell","enabled":true},{"name":"file_reader","enabled":false}]}"#,
        )
        .unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.tools.entries.len(), 2);
        assert_eq!(cfg.tools.entries[0].name, "shell");
        assert!(cfg.tools.entries[0].enabled);
        assert_eq!(cfg.tools.entries[1].name, "file_reader");
        assert!(!cfg.tools.entries[1].enabled);
    }

    #[test]
    fn test_config_channels_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default"},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}},"channels":{"cli":{"enabled":true}}}"#,
        )
        .unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        assert!(cfg.channels.contains_key("cli"));
        assert!(cfg.channels["cli"].enabled);
    }

    #[test]
    fn test_config_gateway_serialization() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"agent":{"provider":"default"},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}},"gateway":{"cron":{"enabled":false},"heartbeat":{"enabled":true,"interval_s":30}}}"#,
        )
        .unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        assert!(!cfg.gateway.cron.enabled);
        assert!(cfg.gateway.heartbeat.enabled);
        assert_eq!(cfg.gateway.heartbeat.interval_s, 30);
    }

    #[test]
    fn test_config_load_for_preview_normalizes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        // Missing provider field - should be filled by normalization
        std::fs::write(
            &path,
            r#"{"agent":{},"providers":{"default":{"type":"openai","api_key":"test","model":"gpt-4"}}}"#,
        )
        .unwrap();
        let cfg = Config::load_for_preview(path.to_str().unwrap()).unwrap();
        // After normalization, provider should be set to "default"
        assert_eq!(cfg.agent.provider, "default");
    }

    #[test]
    fn test_config_tool_entry_serialization() {
        let entry = crate::config::ToolEntry {
            name: "shell".to_string(),
            enabled: true,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("shell"));
        assert!(json.contains("true"));

        let deserialized: crate::config::ToolEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.name, "shell");
        assert!(deserialized.enabled);
    }

    #[test]
    fn test_config_channel_config_serialization() {
        let entry = crate::config::ChannelConfig {
            enabled: true,
            extra: Default::default(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("true"));

        let deserialized: crate::config::ChannelConfig = serde_json::from_str(&json).unwrap();
        assert!(deserialized.enabled);
    }

    #[test]
    fn test_config_cron_config_defaults() {
        let cron_config = crate::config::CronConfig::default();
        assert!(cron_config.enabled);
    }

    #[test]
    fn test_config_heartbeat_config_defaults() {
        let heartbeat_config = crate::config::HeartbeatConfig::default();
        assert!(heartbeat_config.enabled);
        assert_eq!(heartbeat_config.interval_s, 1800);
    }

    #[test]
    fn test_config_gateway_config_defaults() {
        let gateway_config = crate::config::GatewayConfig::default();
        assert!(gateway_config.cron.enabled);
        assert!(gateway_config.heartbeat.enabled);
    }

    #[test]
    fn test_config_provider_config_serialization() {
        let provider = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "https://api.openai.com".to_string(),
            base_url: "".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            context_window_tokens: 32768,
            prompt_cache_enabled: true,
            disable_thinking: false,
            timeout_seconds: 180,
            unknown: Default::default(),
        };
        let json = serde_json::to_string(&provider).unwrap();
        assert!(json.contains("openai"));
        assert!(json.contains("gpt-4"));
    }

    #[test]
    fn test_config_agent_config_serialization() {
        let agent = crate::config::AgentConfig {
            provider: "default".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        };
        let json = serde_json::to_string(&agent).unwrap();
        assert!(json.contains("default"));
        assert!(json.contains("40"));
    }

    #[test]
    fn test_config_save_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested/dir/config.json");
        let cfg = make_test_config();
        let result = cfg.save(path.to_str().unwrap());
        assert!(result.is_ok());
        assert!(path.exists());
    }

    #[test]
    fn test_config_load_nonexistent_file() {
        let result = Config::load_for_preview("/nonexistent/config.json");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_provider_defaults() {
        let provider = crate::config::ProviderConfig::default();
        assert_eq!(provider.r#type, "openai");
        assert_eq!(provider.model, "gpt-4o");
        assert_eq!(provider.temperature, 0.7);
        assert_eq!(provider.max_tokens, 4096);
        assert!(provider.prompt_cache_enabled);
    }

    #[test]
    fn test_config_agent_defaults() {
        let agent = crate::config::AgentConfig::default();
        assert_eq!(agent.provider, "");
        assert_eq!(agent.max_iterations, 40);
        assert_eq!(agent.timeout_seconds, 120);
        assert!(agent.persist_tool_results);
    }

    #[test]
    fn test_config_pressure_defaults_match_adr() {
        let pressure = crate::config::PressureConfig::default();
        assert_eq!(pressure.single_tool_result_ratio, 0.25);
        assert_eq!(pressure.persisted_preview_ratio, 0.04);
        assert_eq!(pressure.snip_safety_ratio, 0.03);
        assert_eq!(pressure.consolidate_trigger_ratio, 0.50);
        assert_eq!(pressure.consolidate_target_ratio, 0.60);
        assert_eq!(pressure.summary_ratio, 0.02);
        assert_eq!(pressure.recent_history_ratio, 0.24);
        assert_eq!(pressure.tool_results_ttl_days, 7);
    }

    #[test]
    fn test_config_provider_owns_context_window() {
        // The window anchor moved out of `agent`: it describes the model, not
        // the loop.
        let provider = crate::config::ProviderConfig::default();
        assert_eq!(provider.context_window_tokens, 32768);
    }

    // ── Tracing config ──

    #[test]
    fn test_tracing_config_defaults_to_disabled() {
        let tracing = crate::config::TraceConfig::default();
        assert!(!tracing.enabled);
        assert!(tracing.backends.is_empty());
    }

    #[test]
    fn test_tracing_config_reset_on_start_defaults_true() {
        // The default reset-on-start behavior truncates trace files on Gateway
        // startup; it must default to true.
        let tracing = crate::config::TraceConfig::default();
        assert!(tracing.reset_on_start);
    }

    #[test]
    fn test_tracing_config_reset_on_start_missing_field_defaults_true() {
        // A tracing section without `reset_on_start` must default to true.
        let config_str = r#"{
            "agent": {"provider": "test"},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "tracing": {"enabled": true}
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        assert!(config.tracing.reset_on_start);
    }

    #[test]
    fn test_tracing_config_reset_on_start_explicit_false_preserved() {
        // An explicit false must be preserved, not reset to the default.
        let config_str = r#"{
            "agent": {"provider": "test"},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "tracing": {"enabled": true, "reset_on_start": false}
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        assert!(!config.tracing.reset_on_start);
    }

    #[test]
    fn test_tracing_config_reset_on_start_clamp_untouched() {
        // `clamp()` must not corrupt the reset_on_start field.
        let config_str = r#"{
            "agent": {"provider": "test"},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "tracing": {"enabled": true, "reset_on_start": false}
        }"#;
        let mut config: Config = serde_json::from_str(config_str).unwrap();
        config.clamp();
        assert!(!config.tracing.reset_on_start);
    }

    #[test]
    fn test_tracing_config_absent_in_config_defaults_off() {
        // A config without a `tracing` section must stay disabled.
        let config_str = r#"{
            "agent": {"provider": "test", "max_iterations": 10},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}}
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        assert!(!config.tracing.enabled);
        assert!(config.tracing.backends.is_empty());
    }

    #[test]
    fn test_tracing_config_parses_backends_and_multiple_instances() {
        let config_str = r#"{
            "agent": {"provider": "test"},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "tracing": {
                "enabled": true,
                "backends": [
                    { "type": "file", "enabled": true, "path": "traces.jsonl" },
                    { "type": "file", "enabled": true, "path": "/tmp/second.jsonl" },
                    { "type": "otel", "enabled": true, "endpoint": "http://collector:4318", "service_name": "mybot" },
                    { "type": "file", "enabled": false, "path": "/tmp/disabled.jsonl" }
                ]
            }
        }"#;
        let config: Config = serde_json::from_str(config_str).unwrap();
        assert!(config.tracing.enabled);
        let backends = &config.tracing.backends;
        assert_eq!(backends.len(), 4);
        // Same type multiple instances are preserved.
        let files: Vec<_> = backends.iter().filter(|b| b.r#type == "file").collect();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].path, "traces.jsonl");
        assert_eq!(files[1].path, "/tmp/second.jsonl");
        // Per-backend enable switch.
        assert!(!files[2].enabled);
        let otel = backends.iter().find(|b| b.r#type == "otel").unwrap();
        assert_eq!(otel.endpoint, "http://collector:4318");
        assert_eq!(otel.service_name, "mybot");
        // Defaults for unset fields.
        assert_eq!(otel.path, "");
        assert_eq!(files[0].endpoint, "http://localhost:4318");
        assert_eq!(files[0].service_name, "slimbot");
    }

    #[test]
    fn test_tracing_config_clamp_falls_back_unknown_backend_type() {
        // Backend entries carry their own constraints, so an unknown type is
        // replaced by the declared default instead of surviving until tracer
        // construction silently skips it.
        let config_str = r#"{
            "agent": {"provider": "test"},
            "providers": {"test": {"type": "openai", "api_key": "key", "model": "model"}},
            "tracing": {"enabled": true, "backends": [{"type": "nope", "enabled": true}]}
        }"#;
        let mut config: Config = serde_json::from_str(config_str).unwrap();
        config.clamp();
        assert!(config.tracing.enabled);
        assert_eq!(config.tracing.backends.len(), 1);
        assert_eq!(config.tracing.backends[0].r#type, "file");
    }

    #[test]
    fn test_config_provider_config_with_all_fields() {
        let provider = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "https://api.openai.com/v1/chat/completions".to_string(),
            base_url: "https://api.openai.com".to_string(),
            api_key: "sk-test-key-12345".to_string(),
            model: "gpt-4-turbo".to_string(),
            temperature: 0.3,
            max_tokens: 8192,
            context_window_tokens: 32768,
            prompt_cache_enabled: true,
            disable_thinking: false,
            timeout_seconds: 180,
            unknown: Default::default(),
        };
        assert_eq!(provider.r#type, "openai");
        assert_eq!(
            provider.api_url,
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(provider.base_url, "https://api.openai.com");
        assert_eq!(provider.api_key, "sk-test-key-12345");
        assert_eq!(provider.model, "gpt-4-turbo");
        assert_eq!(provider.temperature, 0.3);
        assert_eq!(provider.max_tokens, 8192);
        assert!(provider.prompt_cache_enabled);
    }

    #[test]
    fn test_config_agent_config_with_all_fields() {
        let agent = crate::config::AgentConfig {
            provider: "custom-provider".to_string(),
            max_iterations: 100,
            timeout_seconds: 300,
            persist_tool_results: true,
            unknown: Default::default(),
        };
        assert_eq!(agent.provider, "custom-provider");
        assert_eq!(agent.max_iterations, 100);
        assert_eq!(agent.timeout_seconds, 300);
        assert!(agent.persist_tool_results);
    }

    #[test]
    fn test_config_save_creates_config_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test-config.json");
        let cfg = make_test_config();
        let result = cfg.save(path.to_str().unwrap());
        assert!(result.is_ok());
        assert!(path.exists());

        // Verify the file contains valid JSON
        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed.is_object());
    }

    #[test]
    fn test_config_load_and_validate() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.json");
        let cfg = make_test_config();
        cfg.save(path.to_str().unwrap()).unwrap();

        let loaded = Config::load_resolved_with(
            path.to_str().unwrap(),
            &crate::secrets::SecretStore::empty(),
        );
        assert!(loaded.is_ok());
        let loaded = loaded.unwrap();
        let validation = loaded.validate();
        assert!(validation.is_ok());
    }

    #[test]
    fn test_config_provider_config_unknown_fields() {
        let provider = crate::config::ProviderConfig {
            r#type: "openai".to_string(),
            api_url: "".to_string(),
            base_url: "https://api.openai.com".to_string(),
            api_key: "sk-test".to_string(),
            model: "gpt-4".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            context_window_tokens: 32768,
            prompt_cache_enabled: true,
            disable_thinking: false,
            timeout_seconds: 180,
            unknown: Default::default(),
        };
        // Verify unknown fields are empty by default
        assert!(provider.unknown.is_empty());
    }

    #[test]
    fn test_config_agent_config_unknown_fields() {
        let agent = crate::config::AgentConfig {
            provider: "default".to_string(),
            max_iterations: 40,
            timeout_seconds: 120,
            persist_tool_results: false,
            unknown: Default::default(),
        };
        // Verify unknown fields are empty by default
        assert!(agent.unknown.is_empty());
    }

    #[test]
    fn test_config_tool_entry_disabled() {
        let entry = crate::config::ToolEntry {
            name: "shell".to_string(),
            enabled: false,
        };
        assert_eq!(entry.name, "shell");
        assert!(!entry.enabled);
    }

    #[test]
    fn test_config_channel_config_disabled() {
        let entry = crate::config::ChannelConfig {
            enabled: false,
            extra: Default::default(),
        };
        assert!(!entry.enabled);
    }

    #[test]
    fn test_config_cron_config_disabled() {
        let cron_config = crate::config::CronConfig {
            enabled: false,
            unknown: Default::default(),
        };
        assert!(!cron_config.enabled);
    }

    #[test]
    fn test_config_heartbeat_config_custom_interval() {
        let heartbeat_config = crate::config::HeartbeatConfig {
            channel: Default::default(),
            to: Default::default(),
            enabled: true,
            interval_s: 300,
            unknown: Default::default(),
        };
        assert!(heartbeat_config.enabled);
        assert_eq!(heartbeat_config.interval_s, 300);
    }

    #[test]
    fn test_config_gateway_config_custom() {
        let gateway_config = crate::config::GatewayConfig {
            owner: Default::default(),
            cron: crate::config::CronConfig {
                enabled: true,
                unknown: Default::default(),
            },
            heartbeat: crate::config::HeartbeatConfig {
                channel: Default::default(),
                to: Default::default(),
                enabled: true,
                interval_s: 120,
                unknown: Default::default(),
            },
            dream: crate::config::DreamConfig::default(),
        };
        assert!(gateway_config.cron.enabled);
        assert!(gateway_config.heartbeat.enabled);
        assert_eq!(gateway_config.heartbeat.interval_s, 120);
    }

    #[test]
    fn test_dream_config_defaults() {
        let dream_config = crate::config::DreamConfig::default();
        assert!(dream_config.enabled);
        assert_eq!(dream_config.interval_ms, 7200000);
        assert!(dream_config.model_override.is_none());
        assert_eq!(dream_config.max_entries, 20);
    }

    #[test]
    fn test_dream_config_custom_values() {
        let dream_config = crate::config::DreamConfig {
            enabled: false,
            interval_ms: 3600000,
            model_override: Some("gpt-4-turbo".to_string()),
            max_entries: 50,
            unknown: Default::default(),
        };
        assert!(!dream_config.enabled);
        assert_eq!(dream_config.interval_ms, 3600000);
        assert_eq!(dream_config.model_override, Some("gpt-4-turbo".to_string()));
        assert_eq!(dream_config.max_entries, 50);
    }

    #[test]
    fn test_dream_config_serialization() {
        let dream_config = crate::config::DreamConfig::default();
        let json = serde_json::to_string(&dream_config).unwrap();
        assert!(json.contains("enabled"));
        assert!(json.contains("interval_ms"));
        assert!(json.contains("max_entries"));
    }

    #[test]
    fn test_dream_config_deserialization() {
        let json =
            r#"{"enabled":true,"interval_ms":3600000,"model_override":null,"max_entries":10}"#;
        let dream_config: crate::config::DreamConfig = serde_json::from_str(json).unwrap();
        assert!(dream_config.enabled);
        assert_eq!(dream_config.interval_ms, 3600000);
        assert!(dream_config.model_override.is_none());
        assert_eq!(dream_config.max_entries, 10);
    }

    #[test]
    fn test_dream_config_deserialization_partial() {
        // Test that missing fields use defaults
        let json = r#"{"enabled":false}"#;
        let dream_config: crate::config::DreamConfig = serde_json::from_str(json).unwrap();
        assert!(!dream_config.enabled);
        assert_eq!(dream_config.interval_ms, 7200000);
        assert!(dream_config.model_override.is_none());
        assert_eq!(dream_config.max_entries, 20);
    }

    #[test]
    fn test_json_render_config_defaults() {
        let c = crate::config::JsonRenderConfig::default();
        assert!(
            c.enabled,
            "default on — the real switch is catalog registration"
        );
        assert!(c.model_override.is_none());
        assert_eq!(c.max_tokens, 2048);
        assert_eq!(c.timeout_seconds, 30);
    }

    #[test]
    fn test_json_render_config_deserialization_partial() {
        let json = r#"{"timeout_seconds": 10}"#;
        let c: crate::config::JsonRenderConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.timeout_seconds, 10);
        assert!(c.enabled);
        assert_eq!(c.max_tokens, 2048);
    }

    #[test]
    fn test_config_json_render_section_absent_uses_defaults() {
        // A config that predates the section must load with the defaults.
        let json = r#"{
            "agent": {"provider": "default"},
            "providers": {"default": {"api_key": "k", "model": "m"}}
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert!(cfg.json_render.enabled);
        assert_eq!(cfg.json_render.max_tokens, 2048);
        assert_eq!(cfg.json_render.timeout_seconds, 30);
    }
}

use anyhow::Result;

use crate::bootstrap::{bootstrap_files, skill_files};
use crate::config::Config;
use crate::config_defs::{AgentConfig, ProviderConfig, TraceBackendEntry, TraceConfig};
use crate::debug;
use crate::path::PathManager;

/// Build a default Config for initial setup. All values come from macro defaults.
fn default_config() -> Config {
    let mut providers = std::collections::HashMap::new();
    providers.insert("default".to_string(), ProviderConfig::default());
    Config {
        agent: AgentConfig::default(),
        providers,
        tools: Default::default(),
        channels: std::collections::HashMap::new(),
        gateway: Default::default(),
        web_fetch: None,
        // Trace recording is on by default for a fresh install so the local
        // `/traces` viewer is useful without reading docs first; the OTel
        // backend ships configured but disabled, one flag away from a
        // collector (ADR 0022).
        tracing: TraceConfig {
            enabled: true,
            reset_on_start: true,
            backends: vec![
                TraceBackendEntry {
                    r#type: "file".to_string(),
                    enabled: true,
                    path: String::new(),
                    ..Default::default()
                },
                TraceBackendEntry {
                    r#type: "otel".to_string(),
                    enabled: false,
                    ..Default::default()
                },
            ],
            ..Default::default()
        },
        memory: Default::default(),
        json_render: Default::default(),
        subagent: Default::default(),
        executors: Default::default(),
        triggers: Default::default(),
        pressure: Default::default(),
        mcp: Default::default(),
    }
}

/// Run the setup command: write default config or normalize existing one.
pub fn run_setup(
    config_path: Option<&str>,
    data_dir: &str,
    workspace_dir: Option<&str>,
) -> Result<()> {
    debug!("[setup] starting setup with data_dir={}", data_dir);
    let paths = PathManager::resolve(config_path, Some(data_dir), workspace_dir)?;
    let path = paths.config_path().to_str().unwrap();

    if std::path::Path::new(path).exists() {
        // Config exists — clamp, normalize, and write back (skip validation for preview)
        eprintln!("Config file found at: {}", path);
        let raw_content = std::fs::read_to_string(path)?;
        let config = Config::load_for_preview(path)?;
        let normalized = serde_json::to_string_pretty(&config)?;

        let needs_save = raw_content.trim() != normalized.trim();
        if needs_save {
            std::fs::write(path, &normalized)?;
            eprintln!("Missing default fields have been filled in.");
        } else {
            eprintln!("Config is already complete, no changes needed.");
        }

        // Create directories and bootstrap files
        if let Err(e) = create_directories_and_bootstrap(&paths) {
            eprintln!("Warning: failed to create workspace directories: {}", e);
        }

        // Use already-loaded config for summary
        print_config_summary(&config, &paths);
    } else {
        // Config doesn't exist — write full default config
        eprintln!("Writing default config to: {}", path);
        let config = default_config();
        config.save(path)?;
        eprintln!("Default config created successfully.");

        // Create directories and bootstrap files
        if let Err(e) = create_directories_and_bootstrap(&paths) {
            eprintln!("Warning: failed to create workspace directories: {}", e);
        }

        print_config_summary(&config, &paths);
    }

    Ok(())
}

fn print_config_summary(config: &Config, paths: &PathManager) {
    eprintln!("\nConfig preview:");
    eprintln!("  data_dir: {}", paths.data_dir().display());
    eprintln!("  workspace_dir: {}", paths.workspace_dir().display());
    eprintln!("  agent.provider: {}", config.agent.provider);
    eprintln!("  agent.max_iterations: {}", config.agent.max_iterations);
    eprintln!("  agent.timeout_seconds: {}", config.agent.timeout_seconds);
    eprintln!("  providers:");
    for (name, provider) in &config.providers {
        eprintln!(
            "    {}: type={} model={} api_key={} context_window_tokens={}",
            name,
            provider.r#type,
            provider.model,
            mask_api_key(&provider.api_key),
            provider.context_window_tokens,
        );
    }
    eprintln!("  tools: {} entries", config.tools.entries.len());
    eprintln!("  channels: {} entries", config.channels.len());
    eprintln!(
        "\nEdit {} to configure your API key and channels.",
        paths.config_path().display()
    );
}

fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return "<not set>".to_string();
    }
    if key.len() <= 8 {
        return "****".to_string();
    }
    format!("{}****{}", &key[..4], &key[key.len() - 4..])
}

/// Create workspace directories and write bootstrap files.
/// Skips files that already exist.
fn create_directories_and_bootstrap(paths: &PathManager) -> Result<()> {
    std::fs::create_dir_all(paths.data_dir())?;
    std::fs::create_dir_all(paths.workspace_dir())?;
    std::fs::create_dir_all(paths.skills_dir())?;
    std::fs::create_dir_all(paths.memory_dir())?;
    std::fs::create_dir_all(paths.agents_dir())?;
    std::fs::create_dir_all(paths.triggers_dir())?;
    std::fs::create_dir_all(paths.executors_dir())?;

    // Write bootstrap files (skip if already exists)
    for (dest, template) in bootstrap_files() {
        let path = paths.bootstrap_file(dest);
        if path.exists() {
            eprintln!("  {} already exists, skipping.", dest);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, template)?;
            eprintln!("  Created {}.", dest);
        }
    }

    // Write skill files (skip if already exists)
    for (_filename, content, dest) in skill_files() {
        let path = paths.workspace_dir().join(dest);
        if path.exists() {
            eprintln!("  {} already exists, skipping.", dest);
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            eprintln!("  Created {}.", dest);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_enables_file_tracing_and_offers_otel() {
        let config = default_config();
        assert!(config.tracing.enabled, "a fresh setup records traces");
        assert!(config.tracing.reset_on_start);

        let file = config
            .tracing
            .backends
            .iter()
            .find(|b| b.r#type == "file")
            .expect("a file backend ships by default");
        assert!(file.enabled);
        assert!(
            file.path.is_empty(),
            "empty path resolves to {{workspace}}/traces.jsonl"
        );

        let otel = config
            .tracing
            .backends
            .iter()
            .find(|b| b.r#type == "otel")
            .expect("the otel backend is pre-wired");
        assert!(!otel.enabled, "no collector by default");
        assert_eq!(otel.endpoint, "http://localhost:4318");
        assert_eq!(otel.service_name, "slimbot");
    }

    #[test]
    fn test_setup_leaves_tracing_disabled_on_a_config_without_the_section() {
        // An upgrade must never start writing prompts to disk behind the user's
        // back: only a fresh setup gets tracing on (ADR 0022).
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{
                "agent": { "provider": "default" },
                "providers": { "default": { "api_key": "k", "model": "m" } }
            }"#,
        )
        .unwrap();

        run_setup(
            Some(config_path.to_str().unwrap()),
            tmp.path().join("data").to_str().unwrap(),
            None,
        )
        .unwrap();

        let rewritten: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(rewritten["tracing"]["enabled"], serde_json::json!(false));
        assert_eq!(
            rewritten["tracing"]["backends"],
            serde_json::json!([]),
            "no backend is invented for an existing config"
        );
    }

    #[test]
    fn test_mask_api_key_empty() {
        assert_eq!(mask_api_key(""), "<not set>");
    }

    #[test]
    fn test_mask_api_key_short() {
        // 8 chars or less → full mask
        assert_eq!(mask_api_key("1234"), "****");
        assert_eq!(mask_api_key("12345678"), "****");
    }

    #[test]
    fn test_mask_api_key_long() {
        // More than 8 chars → show first 4 and last 4
        let result = mask_api_key("sk-1234567890abcdef");
        assert_eq!(result, "sk-1****cdef");
    }

    #[test]
    fn test_mask_api_key_exactly_9_chars() {
        let result = mask_api_key("123456789");
        assert_eq!(result, "1234****6789");
    }

    #[test]
    fn test_default_config_has_window_anchor_in_provider_section() {
        let config = default_config();
        assert_eq!(config.providers["default"].context_window_tokens, 32_768);
    }

    #[test]
    fn test_default_config_omits_deprecated_agent_keys() {
        let json = serde_json::to_value(default_config()).unwrap();
        assert!(json["agent"].get("context_window_tokens").is_none());
        assert!(json["agent"].get("max_tool_result_chars").is_none());
        assert_eq!(
            json["providers"]["default"]["context_window_tokens"],
            serde_json::json!(32_768)
        );
    }

    #[test]
    fn test_default_config_exposes_pressure_ratios() {
        let json = serde_json::to_value(default_config()).unwrap();
        let single = json["pressure"]["single_tool_result_ratio"]
            .as_f64()
            .unwrap() as f32;
        let preview = json["pressure"]["persisted_preview_ratio"]
            .as_f64()
            .unwrap() as f32;
        assert!((single - 0.25).abs() < 1e-6);
        assert!((preview - 0.04).abs() < 1e-6);
        assert_eq!(json["pressure"]["tool_results_ttl_days"].as_u64(), Some(7));
    }

    #[test]
    fn test_setup_migrates_legacy_window_key_and_rewrites_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            r#"{
                "agent": { "provider": "default", "context_window_tokens": 64000, "max_tool_result_chars": 8000 },
                "providers": { "default": { "api_key": "k", "model": "m" } }
            }"#,
        )
        .unwrap();

        run_setup(
            Some(config_path.to_str().unwrap()),
            tmp.path().join("data").to_str().unwrap(),
            None,
        )
        .unwrap();

        let rewritten: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(rewritten["agent"].get("context_window_tokens").is_none());
        assert!(rewritten["agent"].get("max_tool_result_chars").is_none());
        assert_eq!(
            rewritten["providers"]["default"]["context_window_tokens"],
            serde_json::json!(64_000)
        );
    }

    #[test]
    fn test_create_directories_and_bootstrap() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let paths = PathManager::resolve(None, Some(data_dir.to_str().unwrap()), None).unwrap();

        create_directories_and_bootstrap(&paths).unwrap();

        assert!(paths.data_dir().exists());
        assert!(paths.workspace_dir().exists());
        assert!(paths.skills_dir().exists());
        assert!(paths.memory_dir().exists());
        assert!(paths.agents_dir().exists());
        assert!(paths.triggers_dir().exists());
        assert!(paths.executors_dir().exists());

        // Bootstrap files should exist
        let workspace = paths.workspace_dir();
        assert!(workspace.join("AGENTS.md").exists());
        assert!(workspace.join("USER.md").exists());
        assert!(workspace.join("SOUL.md").exists());
        assert!(workspace.join("TOOLS.md").exists());
        assert!(workspace.join("memory").join("MEMORY.md").exists());
        assert!(workspace.join("agents").join("trigger.md").exists());
        assert!(workspace.join("agents").join("executor.md").exists());
        assert!(workspace.join("references").join("connector.md").exists());
        // identity/dream stay embedded-only — never written to the workspace.
        assert!(!workspace.join("identity.md").exists());
        assert!(!workspace.join("dream.md").exists());
    }

    /// Ticket 18: the shared connector reference lands under `references/` and
    /// never under `skills/`, so `# Available Skills` (built from the skills
    /// dir and injected into every prompt, main loop included) stays free of it.
    #[tokio::test]
    async fn test_setup_connector_reference_stays_out_of_available_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let paths = PathManager::resolve(None, Some(data_dir.to_str().unwrap()), None).unwrap();
        create_directories_and_bootstrap(&paths).unwrap();

        let workspace = paths.workspace_dir();
        let reference = workspace.join("references").join("connector.md");
        assert!(
            reference.exists(),
            "references/connector.md must be written"
        );

        // One non-always skill so `# Available Skills` is a populated section
        // rather than an absent one: the exclusion below is then a real filter.
        let optional = workspace.join("skills").join("optional");
        std::fs::create_dir_all(&optional).unwrap();
        std::fs::write(
            optional.join("SKILL.md"),
            "---\nname: optional\ndescription: An optional skill.\nalways: false\n---\n\nBody",
        )
        .unwrap();

        let ms = std::sync::Arc::new(tokio::sync::Mutex::new(crate::memory::MemoryService::new(
            workspace,
        )));
        ms.lock().await.init().unwrap();
        let cb = crate::context::ContextBuilder::new(
            std::sync::Arc::new(tokio::sync::Mutex::new(
                crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
            )),
            std::sync::Arc::new(crate::tool::ToolManager::new(workspace.to_path_buf())),
            workspace.to_path_buf(),
            ms.clone(),
        );

        let prompt = cb.build_system_prompt("cli", None).await;
        assert!(prompt.contains("# Available Skills"), "{prompt}");
        assert!(prompt.contains("skills/optional/SKILL.md"), "{prompt}");
        assert!(
            !prompt.contains("connector.md"),
            "the connector reference must never be advertised as a skill: {prompt}"
        );
    }

    #[test]
    fn test_create_directories_and_bootstrap_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path().join("data");
        let paths = PathManager::resolve(None, Some(data_dir.to_str().unwrap()), None).unwrap();

        // First call
        create_directories_and_bootstrap(&paths).unwrap();

        // Second call should also succeed (files already exist)
        create_directories_and_bootstrap(&paths).unwrap();
    }
}

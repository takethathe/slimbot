use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use chrono::Local;

use crate::config::{AgentConfig, DreamConfig};
use crate::memory::SharedMemory;
use crate::memory::audit::MemoryWriteAudit;
use crate::pressure::{MemoryDocBudget, PressureAnchor};
use crate::provider::Provider;
use crate::session::SharedSessionManager;
use crate::tool::ToolManager;
use crate::tools::memory_tools::{MemoryEditTool, MemoryReadTool, MemoryWriteTool};
use crate::trace::{SpanRecorder, SpanStatus, Tracer};

pub struct DreamService {
    workspace_dir: PathBuf,
    memory_service: SharedMemory,
    session_manager: SharedSessionManager,
    config: DreamConfig,
    /// Real agent configuration (max_iterations, timeout), never a default:
    /// Dream must obey the same window/iteration limits as the main agent.
    agent_config: AgentConfig,
    providers: Arc<HashMap<String, Arc<dyn Provider>>>,
    default_provider: Arc<dyn Provider>,
    /// Trace sink so a Dream pass is observable (its memory writes become tool
    /// spans plus a fingerprint audit span). `None` disables tracing entirely.
    tracer: Option<Arc<Tracer>>,
    /// Per-document budget Dream enforces, anchored on the **primary** provider
    /// window (ADR 0013: two separate anchors).
    doc_budget: MemoryDocBudget,
    /// Dream's *own* window anchor (its resolved `model_override` window): drives
    /// the runner's context/truncation limits and the tool-result TTL sweep.
    /// Separate from `doc_budget`, which stays on the primary window.
    pressure: PressureAnchor,
}

pub struct DreamResult {
    pub success: bool,
    pub elapsed_ms: i64,
    pub message: String,
}

impl DreamService {
    // The 9-argument signature wires a full service (memory, sessions, providers,
    // tracing, budgets); a spec struct would only move the coupling around.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace_dir: PathBuf,
        memory_service: SharedMemory,
        session_manager: SharedSessionManager,
        config: DreamConfig,
        agent_config: AgentConfig,
        providers: Arc<HashMap<String, Arc<dyn Provider>>>,
        default_provider: Arc<dyn Provider>,
        tracer: Option<Arc<Tracer>>,
        doc_budget: MemoryDocBudget,
    ) -> Self {
        Self {
            workspace_dir,
            memory_service,
            session_manager,
            config,
            agent_config,
            providers,
            default_provider,
            tracer,
            doc_budget,
            pressure: PressureAnchor::default(),
        }
    }

    /// Attach Dream's own window anchor: it drives the runner's truncation
    /// limits and the tool-result TTL sweep, both from the same `pressure`.
    pub fn with_pressure(mut self, pressure: PressureAnchor) -> Self {
        self.pressure = pressure;
        self
    }

    pub async fn build_prompt(&self) -> Option<(String, usize)> {
        let mut ms = self.memory_service.lock().await;
        let dream_cursor = ms.get_last_dream_cursor();
        let entries = ms.read_unprocessed_history(dream_cursor);

        if entries.is_empty() {
            return None;
        }

        let batch = if entries.len() > self.config.max_entries {
            &entries[0..self.config.max_entries]
        } else {
            &entries
        };

        let lines_processed = dream_cursor + batch.len();

        let history_text = batch
            .iter()
            .map(|e| {
                // Character-safe truncation: `content.len()` is bytes and the
                // slice must land on a char boundary, or a multibyte summary
                // panics here and wedges every later tick (cursor never moves).
                let content = truncate_chars(&e.content, 500);
                format!("[{}] {}", e.timestamp, content)
            })
            .collect::<Vec<_>>()
            .join("\n");

        // Entry-age side channel (ticket 06 #7): reconstruct each landing-document
        // entry's age from the snapshot sequence and hand it to Dream as prompt
        // context only. It is never written back into the document — the age is a
        // derived value with real uncertainty, and a restatable date is the very
        // hallucination the no-dates rule removes.
        let age_hint = self.entry_age_hint(&ms).await;
        drop(ms);

        let template = crate::embed::get_content("dream.md")?;
        let prompt = template.replace("{history_text}", &history_text);
        let prompt = if age_hint.is_empty() {
            prompt
        } else {
            format!("{prompt}\n\n{age_hint}")
        };

        Some((prompt, lines_processed))
    }

    /// Render the current landing document's entry ages as a Dream-only hint.
    /// Empty when there is no document, no snapshots, or the read fails — a
    /// missing age is never extrapolated into a claim.
    async fn entry_age_hint(&self, ms: &crate::memory::MemoryService) -> String {
        let Ok(Some(content)) = ms.get(&crate::MemoryCategory::Core, None).await else {
            return String::new();
        };
        let Ok(ages) =
            ms.snapshots()
                .entry_ages(crate::bootstrap::MEMORY_DOCUMENT, &content, Local::now())
        else {
            return String::new();
        };
        let mut lines = Vec::new();
        for age in ages {
            match age.age_days {
                Some(days) => lines.push(format!("- {} · 首次出现于 {days} 天前", age.text)),
                None => lines.push(format!("- {} · 本轮新增（时间未知）", age.text)),
            }
        }
        if lines.is_empty() {
            return String::new();
        }
        format!(
            "条目年龄（仅供你判断新旧，切勿把天数写回文档）:\n{}",
            lines.join("\n")
        )
    }

    /// Build the Dream sandbox tool set: only the memory tools, so Dream can
    /// touch long-term memory exclusively through the `MemoryService` facade.
    /// Existing skill names are injected into each tool description.
    pub async fn build_tools(&self) -> ToolManager {
        let mut tm = ToolManager::new(self.workspace_dir.clone());

        let skill_names = self
            .memory_service
            .lock()
            .await
            .list_skill_names()
            .await
            .unwrap_or_default();

        tm.register(Box::new(MemoryReadTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));
        tm.register(Box::new(MemoryWriteTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));
        tm.register(Box::new(MemoryEditTool::new(
            self.memory_service.clone(),
            &skill_names,
        )));

        tm
    }

    pub fn build_provider(&self) -> Arc<dyn Provider> {
        if let Some(ref model_name) = self.config.model_override {
            if let Some(provider) = self.providers.get(model_name) {
                return provider.clone();
            }
            crate::warn_log!(
                "[DreamService] model_override '{}' not found, using default provider",
                model_name
            );
        }
        self.default_provider.clone()
    }

    pub async fn run(&self) -> DreamResult {
        let start = std::time::Instant::now();

        let (prompt, lines_processed) = match self.build_prompt().await {
            Some(result) => result,
            None => {
                return DreamResult {
                    success: true,
                    elapsed_ms: start.elapsed().as_millis() as i64,
                    message: "Dream: nothing to process".to_string(),
                };
            }
        };

        let provider = self.build_provider();
        let tools = self.build_tools().await;
        let tool_manager = Arc::new(tools);

        // Attach the fingerprint audit sink and the primary-anchored document
        // budget to the facade for this pass only; both are detached in the
        // cleanup below so no other caller inherits them.
        let audit = Arc::new(MemoryWriteAudit::new());
        {
            let mut ms = self.memory_service.lock().await;
            ms.attach_audit(Some(audit.clone()));
            ms.set_doc_budget(Some(self.doc_budget));
        }

        // Dream's pass is observable (ticket 06 #4): each memory write becomes a
        // tool span through the runner's tracer, and this dedicated span carries
        // the aggregated fingerprint audit. A `None` tracer yields a no-op guard.
        let audit_span = self
            .tracer
            .as_ref()
            .map(|t| t.start_span("dream.memory.audit", None));

        let runner = crate::runner::AgentRunner::new(
            tool_manager,
            provider,
            self.session_manager.clone(),
            self.agent_config.clone(),
            self.workspace_dir.clone(),
            self.memory_service.clone(),
            None, // No consolidator for dream
            4096,
            self.tracer.clone(),
        )
        // Dream's own context/truncation limits ride on its resolved window
        // (`model_override` when set), separate from the primary-anchored
        // document budget (ticket 06 #5, ADR 0013 — two anchors kept apart).
        .with_pressure(self.pressure.clone());

        let hook = crate::session::TaskHook::new("system:dream");
        let result = runner
            .run(
                prompt,
                hook,
                "system:dream",
                None,
                Some(tokio_util::sync::CancellationToken::new()),
                None,
                None,
            )
            .await;

        let elapsed_ms = start.elapsed().as_millis() as i64;

        // Structural success (ticket 06 #9): the run finished normally **and** no
        // write errored. Never derived from the emitted text, and never from a
        // change count — zero changes is a legitimate, successful pass.
        let success = audit.success(result.success);

        // Detach the pass-scoped facade hooks, then shape the audit span from
        // the still-held local `audit` Arc.
        {
            let mut ms = self.memory_service.lock().await;
            ms.attach_audit(None);
            ms.set_doc_budget(None);
        }
        if let Some(span) = &audit_span {
            for (key, value) in audit.span_attributes() {
                span.set_attribute(&key, value);
            }
            span.set_status(if success {
                SpanStatus::Ok
            } else {
                SpanStatus::Error(result.content.clone())
            });
        }

        if success {
            // Cursor advance rides in the same branch as the writes it certifies,
            // so a crash mid-pass leaves the cursor on unprocessed work rather
            // than falsely ahead of it (ticket 06 #9).
            let mut ms = self.memory_service.lock().await;
            if let Err(e) = ms.set_last_dream_cursor(lines_processed) {
                crate::error!("[DreamService] Failed to set dream cursor: {}", e);
            }
            // Snapshot retention rotates on this same success branch, alongside
            // the tool-results TTL sweep, never a separate timer (ticket 06 #6).
            if let Err(e) = ms.rotate_snapshots(Local::now()) {
                crate::error!("[DreamService] Failed to rotate snapshots: {}", e);
            }
            drop(ms);

            // Compact history
            self.compact_history_after_dream().await;

            // Housekeeping rides on Dream's success branch instead of a new
            // timer: expired persisted tool results are swept only after a
            // successful pass (ADR 0013: cleanup must not orphan references).
            let swept = self.sweep_expired_tool_results();
            if swept > 0 {
                crate::info!("[DreamService] Swept {swept} expired tool result file(s)");
            }

            DreamResult {
                success: true,
                elapsed_ms,
                message: "Dream completed successfully".to_string(),
            }
        } else {
            DreamResult {
                success: false,
                elapsed_ms,
                message: format!("Dream did not complete: {}", result.content),
            }
        }
    }

    /// Delete persisted tool results older than `pressure.tool_results_ttl_days`.
    ///
    /// Returns the number of removed files. A missing `tool-results/` directory
    /// is not an error: most workspaces never persist a result.
    pub(crate) fn sweep_expired_tool_results(&self) -> usize {
        let days = self.pressure.config.tool_results_ttl_days.max(1);
        crate::utils::sweep_tool_results(
            &self.workspace_dir,
            std::time::Duration::from_secs(days * 86_400),
        )
    }

    async fn compact_history_after_dream(&self) {
        let mut ms = self.memory_service.lock().await;
        if let Err(e) = ms.compact_history_after_dream() {
            crate::error!("[DreamService] Failed to compact history: {}", e);
        }
    }
}

/// First `max_chars` characters of `s`, returned as a `&str` slice that always
/// lands on a UTF-8 char boundary (never a byte count, which panics on CJK).
fn truncate_chars(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::{TempDir, tempdir};

    use crate::memory::MemoryService;

    /// Map of provider name to provider instance.
    type ProviderMap = Arc<HashMap<String, Arc<dyn Provider>>>;

    async fn make_memory_service(workspace_dir: &std::path::Path) -> SharedMemory {
        let memory_service = Arc::new(tokio::sync::Mutex::new(MemoryService::new(workspace_dir)));
        memory_service
            .lock()
            .await
            .init()
            .expect("Failed to init memory store");
        memory_service
    }

    fn make_session_manager(workspace_dir: &std::path::Path) -> SharedSessionManager {
        Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(workspace_dir.join("sessions")).unwrap(),
        ))
    }

    fn make_providers() -> (ProviderMap, Arc<dyn Provider>) {
        use crate::config::ProviderConfig;
        use crate::provider::OpenAIProvider;

        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        let config = ProviderConfig::default();
        providers.insert(
            "default".to_string(),
            Arc::new(OpenAIProvider::new(&config)),
        );

        let default_provider: Arc<dyn Provider> = providers.get("default").unwrap().clone();
        (Arc::new(providers), default_provider)
    }

    async fn make_service() -> (DreamService, TempDir) {
        let tmp_dir = tempdir().unwrap();
        let workspace_dir = tmp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();

        let memory_service = make_memory_service(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);
        let (providers, default_provider) = make_providers();

        let config = DreamConfig::default();

        let service = DreamService::new(
            workspace_dir,
            memory_service,
            session_manager,
            config,
            crate::config::AgentConfig::default(),
            providers,
            default_provider,
            None,
            MemoryDocBudget::default(),
        );

        (service, tmp_dir)
    }

    #[tokio::test]
    async fn test_build_prompt_no_entries_returns_none() {
        let (service, _tmp_dir) = make_service().await;

        let result = service.build_prompt().await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_build_prompt_with_entries_formats_correctly() {
        let (service, _tmp_dir) = make_service().await;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        // Append entries directly using the memory store
        let mut ms = service.memory_service.lock().await;
        ms.append_history("First entry content").unwrap();
        ms.append_history("Second entry content").unwrap();
        drop(ms);

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, lines_processed) = result.unwrap();
        assert!(prompt.contains("["));
        assert!(prompt.contains("First entry content"));
        assert!(prompt.contains("Second entry content"));
        assert_eq!(lines_processed, 2);
    }

    #[tokio::test]
    async fn test_build_prompt_truncates_content_over_500_chars() {
        let (service, _tmp_dir) = make_service().await;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        let long_content = "a".repeat(600);
        service
            .memory_service
            .lock()
            .await
            .append_history(&long_content)
            .unwrap();

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, _) = result.unwrap();
        // Verify that the history text contains truncated content
        // Each entry is formatted as "[timestamp] content" where content is truncated to 500 chars
        // Check that the prompt contains exactly 500 'a' chars, not 501 or 600
        assert!(prompt.contains(&long_content[..500]));
        assert!(!prompt.contains(&long_content[..501]));
    }

    #[tokio::test]
    async fn test_build_prompt_respects_max_entries() {
        let (mut service, _tmp_dir) = make_service().await;
        // Override max_entries for this test
        service.config.max_entries = 2;

        // Set dream cursor to 0 so we can process new entries
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();

        for i in 1..=5 {
            service
                .memory_service
                .lock()
                .await
                .append_history(&format!("Entry {}", i))
                .unwrap();
        }

        let result = service.build_prompt().await;
        assert!(result.is_some());

        let (prompt, lines_processed) = result.unwrap();
        assert!(prompt.contains("Entry 1"));
        assert!(prompt.contains("Entry 2"));
        assert!(!prompt.contains("Entry 3"));
        assert_eq!(lines_processed, 2);
    }

    #[tokio::test]
    async fn test_build_tools_memory_only() {
        let (service, _tmp_dir) = make_service().await;

        let tools = service.build_tools().await;
        let tool_defs = tools.to_openai_functions();
        let tool_names: Vec<_> = tool_defs.iter().map(|t| t.name.as_str()).collect();

        assert!(tool_names.contains(&"memory_read"));
        assert!(tool_names.contains(&"memory_write"));
        assert!(tool_names.contains(&"memory_edit"));
        // No filesystem tools in the Dream sandbox.
        assert!(!tool_names.contains(&"file_reader"));
        assert!(!tool_names.contains(&"file_writer"));
        assert!(!tool_names.contains(&"file_editor"));
        assert!(!tool_names.contains(&"shell"));
        assert!(!tool_names.contains(&"grep"));
        assert_eq!(tool_names.len(), 3);
    }

    #[tokio::test]
    async fn test_build_tools_lists_existing_skills_in_description() {
        let (service, _tmp_dir) = make_service().await;
        service
            .memory_service
            .lock()
            .await
            .set(
                &crate::MemoryCategory::Skill,
                Some("alpha"),
                "---\nname: alpha\ndescription: a\nalways: false\n---\nbody",
            )
            .await
            .unwrap();

        let tools = service.build_tools().await;
        let tool_defs = tools.to_openai_functions();
        let read = tool_defs.iter().find(|t| t.name == "memory_read").unwrap();
        assert!(read.description.contains("Existing skills: alpha."));
    }

    #[tokio::test]
    async fn test_build_provider_without_override() {
        let (service, _tmp_dir) = make_service().await;
        let provider = service.build_provider();
        // Default provider from make_providers() is "default"
        assert!(Arc::ptr_eq(&provider, &service.default_provider));
    }

    #[tokio::test]
    async fn test_build_provider_with_override() {
        let tmp_dir = tempdir().unwrap();
        let workspace_dir = tmp_dir.path().join("workspace");
        fs::create_dir_all(&workspace_dir).unwrap();

        let memory_service = make_memory_service(&workspace_dir).await;
        let session_manager = make_session_manager(&workspace_dir);

        let mut providers: HashMap<String, Arc<dyn Provider>> = HashMap::new();
        let default_config = crate::config::ProviderConfig::default();
        let default_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&default_config));
        providers.insert("default".to_string(), default_provider.clone());

        // Create an override provider with different config
        let override_config = crate::config::ProviderConfig {
            model: "gpt-3.5-turbo".to_string(),
            ..Default::default()
        };
        let override_provider: Arc<dyn Provider> =
            Arc::new(crate::provider::OpenAIProvider::new(&override_config));
        providers.insert("cheap".to_string(), override_provider.clone());

        let config = DreamConfig {
            model_override: Some("cheap".to_string()),
            ..Default::default()
        };

        let service = DreamService::new(
            workspace_dir,
            memory_service,
            session_manager,
            config,
            crate::config::AgentConfig::default(),
            Arc::new(providers),
            default_provider,
            None,
            MemoryDocBudget::default(),
        );

        let provider = service.build_provider();
        // Should return the override provider
        assert!(Arc::ptr_eq(&provider, &override_provider));
    }

    #[tokio::test]
    async fn test_compact_history_keeps_unprocessed_and_recent() {
        let (service, _tmp_dir) = make_service().await;

        {
            let mut ms = service.memory_service.lock().await;
            ms.set_last_dream_cursor(0).unwrap();
            // Create 2100 entries (over HISTORY_MAX_ENTRIES limit of 2000)
            for i in 1..=2100 {
                ms.append_history(&format!("entry {}", i)).unwrap();
            }
            ms.set_last_dream_cursor(2000).unwrap();
        }

        service.compact_history_after_dream().await;

        let mut ms = service.memory_service.lock().await;
        let entries = ms.read_entries();
        // Only the contiguous processed prefix is dropped, and never past the
        // newest DREAM_KEEP_MINIMUM (50): with a 2000-cursor over 2100 entries
        // the drop count is min(2000, 2100-50) = 2000, so the 100 unprocessed
        // entries (2001..=2100) all survive. Unprocessed work is never lost.
        assert_eq!(entries.len(), 100);
        // First kept entry is the first one past the processed prefix.
        assert_eq!(entries[0].content, "entry 2001");
        // Last entry content
        assert_eq!(entries[99].content, "entry 2100");
        // Cursor rewrites to the index of the first unprocessed entry: 2000-2000.
        assert_eq!(ms.get_last_dream_cursor(), 0);
    }

    #[tokio::test]
    async fn test_build_prompt_appends_entry_age_side_channel() {
        // The age is reconstructed from the snapshot sequence and handed to
        // Dream as prompt context only (ticket 06 #7).
        let (service, _tmp_dir) = make_service().await;
        service
            .memory_service
            .lock()
            .await
            .set_last_dream_cursor(0)
            .unwrap();
        service
            .memory_service
            .lock()
            .await
            .append_history("a fact to consolidate")
            .unwrap();
        // Two writes leave the landing document with a body entry and a prior
        // snapshot, so an age can be reconstructed from the sequence.
        service
            .memory_service
            .lock()
            .await
            .set(&crate::MemoryCategory::Core, None, "- seed fact")
            .await
            .unwrap();
        service
            .memory_service
            .lock()
            .await
            .set(
                &crate::MemoryCategory::Core,
                None,
                "- seed fact\n- newer fact",
            )
            .await
            .unwrap();

        let (prompt, _) = service.build_prompt().await.expect("prompt built");
        assert!(
            prompt.contains("条目年龄"),
            "age side channel must be appended"
        );
        assert!(prompt.contains("seed fact"));
        assert!(prompt.contains("切勿把天数写回文档"));
    }

    /// Backdate a file's mtime so TTL tests do not sleep.
    fn backdate(path: &std::path::Path, days: u64) {
        use std::fs::FileTimes;
        let file = fs::File::options().write(true).open(path).unwrap();
        let when = std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400);
        file.set_times(FileTimes::new().set_modified(when)).unwrap();
    }

    fn write_tool_result(service: &DreamService, name: &str, days_old: u64) -> std::path::PathBuf {
        let dir = service.workspace_dir.join("tool-results");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, "full output").unwrap();
        backdate(&path, days_old);
        path
    }

    #[tokio::test]
    async fn test_dream_sweep_removes_expired_tool_results() {
        // Dream's success branch is the only place housekeeping runs.
        let (service, _tmp_dir) = make_service().await;
        let expired = write_tool_result(&service, "expired.txt", 8);

        assert_eq!(service.sweep_expired_tool_results(), 1);
        assert!(!expired.exists());
    }

    #[tokio::test]
    async fn test_dream_sweep_keeps_results_inside_ttl() {
        let (service, _tmp_dir) = make_service().await;
        let fresh = write_tool_result(&service, "fresh.txt", 1);

        assert_eq!(service.sweep_expired_tool_results(), 0);
        assert!(fresh.exists());
    }

    #[tokio::test]
    async fn test_dream_sweep_honours_configured_ttl() {
        let (service, _tmp_dir) = make_service().await;
        let service = service.with_pressure(PressureAnchor::new(
            32_768,
            crate::config::PressureConfig {
                tool_results_ttl_days: 30,
                ..Default::default()
            },
        ));
        let eight_days_old = write_tool_result(&service, "eight.txt", 8);

        assert_eq!(service.sweep_expired_tool_results(), 0);
        assert!(eight_days_old.exists(), "8 days is inside a 30-day TTL");
    }
}

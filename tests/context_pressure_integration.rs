//! Integration tests: the `# Recent History` section derives its length budget
//! from the context-pressure anchor (ADR 0013), not from a hard-coded token cap.
//!
//! The fixture writes `memory/history.jsonl` directly so the prompt-level
//! behaviour can be driven by swapping only the anchor window and the
//! chars-per-token ratio: no full runner harness is needed.

use std::path::PathBuf;
use std::sync::Arc;

use slimbot::pressure::{DEFAULT_CHAR_PER_TOKEN_RATIO, MAX_RECENT_HISTORY_CHARS, PressureAnchor};
use slimbot::{ContextBuilder, MemoryService, SessionManager, SharedSessionManager, ToolManager};
use tempfile::TempDir;
use tokio::sync::Mutex;

struct Fixture {
    _tmp: TempDir,
    session_manager: SharedSessionManager,
    tool_manager: Arc<ToolManager>,
    workspace_dir: PathBuf,
    memory_service: slimbot::SharedMemory,
}

/// Build an isolated workspace with `entries` history lines, each line carrying
/// a stable `entry-NNN` marker so fixed-count and tail-preserving behaviour can
/// be asserted on the rendered prompt.
fn fixture(entries: usize, content_chars: usize) -> Fixture {
    let tmp = TempDir::new().unwrap();
    let workspace_dir = tmp.path().join("workspace");
    let memory_dir = workspace_dir.join("memory");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::create_dir_all(tmp.path().join("sessions")).unwrap();

    let mut lines = String::new();
    for i in 0..entries {
        let record = serde_json::json!({
            "timestamp": format!("2026-06-20 12:{:02}", i % 60),
            "content": format!("entry-{i:03}-{}", "x".repeat(content_chars)),
        });
        lines.push_str(&record.to_string());
        lines.push('\n');
    }
    std::fs::write(memory_dir.join("history.jsonl"), lines).unwrap();

    let session_manager: SharedSessionManager = Arc::new(Mutex::new(
        SessionManager::new(tmp.path().join("sessions")).unwrap(),
    ));
    let memory_service: slimbot::SharedMemory =
        Arc::new(Mutex::new(MemoryService::new(&workspace_dir)));

    Fixture {
        _tmp: tmp,
        session_manager,
        tool_manager: Arc::new(ToolManager::new(workspace_dir.clone())),
        workspace_dir,
        memory_service,
    }
}

impl Fixture {
    fn builder(&self, window_tokens: u32) -> ContextBuilder {
        ContextBuilder::new(
            self.session_manager.clone(),
            self.tool_manager.clone(),
            self.workspace_dir.clone(),
            self.memory_service.clone(),
            PressureAnchor::from_window(window_tokens),
        )
    }

    async fn history_section(&self, window_tokens: u32, ratio: f64) -> String {
        let prompt = self
            .builder(window_tokens)
            .build_system_prompt("cli", None, ratio)
            .await;
        history_section(&prompt)
    }
}

/// The prompt joins its parts with "\n\n---\n\n", so the section ends there.
fn history_section(prompt: &str) -> String {
    let rest = prompt
        .split("# Recent History\n\n")
        .nth(1)
        .expect("the fixture always writes history entries");
    rest.split("\n\n---\n\n").next().unwrap_or(rest).to_string()
}

#[tokio::test]
async fn test_recent_history_budget_scales_with_context_window() {
    let small = PressureAnchor::from_window(32_768)
        .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
        .recent_history_chars;
    let large = PressureAnchor::from_window(200_000)
        .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
        .recent_history_chars;
    assert!(
        large > small,
        "a wider window must allow a longer history section: {small} -> {large}"
    );

    let fx = fixture(50, 900);
    let small_section = fx
        .history_section(32_768, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    let large_section = fx
        .history_section(200_000, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    assert!(
        large_section.len() > small_section.len(),
        "the same history text must be cut differently: {} -> {}",
        small_section.len(),
        large_section.len()
    );
    assert!(
        large_section.contains("entry-049"),
        "the newest line survives"
    );
}

#[tokio::test]
async fn test_recent_history_budget_honors_absolute_cap() {
    let capped = PressureAnchor::from_window(1_000_000)
        .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
        .recent_history_chars;
    assert_eq!(capped, MAX_RECENT_HISTORY_CHARS);

    let fx = fixture(50, 900);
    let section = fx
        .history_section(1_000_000, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    assert!(
        section.len() <= MAX_RECENT_HISTORY_CHARS,
        "an unbounded window must not grow the section past the cap: {}",
        section.len()
    );
    assert!(section.contains("(truncated)"));
}

#[tokio::test]
async fn test_recent_history_uses_session_measured_ratio() {
    let fx = fixture(50, 560);
    let cold = fx.history_section(32_768, 0.0).await;
    let three = fx
        .history_section(32_768, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    let measured = fx.history_section(32_768, 4.0).await;

    assert_eq!(
        cold.len(),
        three.len(),
        "an unknown ratio falls back to the cold-start default"
    );
    assert_ne!(
        three.len(),
        measured.len(),
        "the session-measured ratio must change where the section is cut"
    );
    assert!(three.contains("(truncated)"));
    assert!(
        !measured.contains("(truncated)"),
        "the deprecated 4.0 ruler would over-estimate the budget"
    );
    assert_eq!(DEFAULT_CHAR_PER_TOKEN_RATIO, 3.0);
}

#[test]
fn test_context_builder_requires_pressure_anchor() {
    // Structural lock: the anchor is a required `new` argument and the builder
    // no longer owns a hard-coded token budget of its own.
    let source = include_str!("../src/context.rs");
    assert!(
        source.contains("pressure: PressureAnchor"),
        "the builder must hold the anchor"
    );
    assert!(!source.contains("MAX_HISTORY_TOKENS"));
    assert!(!source.contains("truncate_recent_history(&history_text, MAX_HISTORY_TOKENS, 4.0)"));

    let hook = include_str!("../src/runner/hooks/build_context.rs");
    assert!(
        hook.contains("spec.pressure().clone()"),
        "the production construction site must pass the run's anchor"
    );
}

#[tokio::test]
async fn test_recent_history_zero_window_does_not_truncate() {
    assert_eq!(
        PressureAnchor::from_window(0)
            .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
            .recent_history_chars,
        0
    );
    let fx = fixture(20, 100);
    let section = fx.history_section(0, DEFAULT_CHAR_PER_TOKEN_RATIO).await;
    assert!(section.contains("entry-000"));
    assert!(section.contains("entry-019"));
    assert!(!section.contains("(truncated)"));
}

#[tokio::test]
async fn test_recent_history_keeps_fixed_message_count() {
    let fx = fixture(60, 10);
    let section = fx
        .history_section(32_768, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    assert!(section.contains("entry-059"), "the newest entry is present");
    assert!(
        !section.contains("entry-000"),
        "only the most recent 50 entries are read"
    );
    assert!(section.contains("entry-010"));
    assert!(!section.contains("(truncated)"));

    // The truncation suffix stays a property of the renderer, not the anchor.
    let long = fixture(50, 900);
    let capped = long
        .history_section(32_768, DEFAULT_CHAR_PER_TOKEN_RATIO)
        .await;
    assert!(capped.ends_with("... (truncated)"));
    assert!(capped.contains("entry-049"));
}

#[test]
fn test_consolidator_ratio_tracks_unchanged() {
    // ADR 0013:14-20 deliberately keeps two rulers. Unifying them would
    // reopen the decision, so lock both call sites and their different budgets.
    let source = include_str!("../src/consolidate.rs");
    assert!(source.contains("self.anchor.limits(ratio).summary_max_chars"));
    assert!(source.contains("self.anchor.limits(DEFAULT_CHAR_PER_TOKEN_RATIO)"));

    let anchor = PressureAnchor::from_window(32_768);
    let session_measured = anchor.limits(8.0).summary_max_chars;
    let cold_start = anchor
        .limits(DEFAULT_CHAR_PER_TOKEN_RATIO)
        .summary_max_chars;
    assert_ne!(session_measured, cold_start);
}

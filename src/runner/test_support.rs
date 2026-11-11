//! One fixture for every Run-level test.
//!
//! A Run's dependency set is assembled in exactly one place (`AgentRunner`'s
//! builder), so tests build it the same way instead of hand-copying twenty
//! fields: [`RunFixture`] owns a real [`AgentRunner`] plus the temp dirs behind
//! it, and hands out borrowed [`RunSpec`] views of it. Adding a Run-level
//! dependency therefore needs no test-fixture edit either.

use std::sync::Arc;

use tokio::sync::Mutex;

use super::{AgentRunner, AgentRunnerBuilder, RunSpec, RunTurn};
use crate::config::AgentConfig;
use crate::consolidate::Consolidator;
use crate::memory::{MemoryService, SharedMemory};
use crate::pressure::PressureAnchor;
use crate::provider::MockProvider;
use crate::session::{SessionManager, SharedSessionManager, TaskHook};
use crate::tool::ToolManager;

/// Session id used by the default spec.
pub(crate) const TEST_SESSION: &str = "s1";

/// The default per-turn inputs every Run test starts from: session `s1`, content
/// `hello`, origin `cli:c1`. Shared with harnesses that assemble their own
/// runner, so the literals live in one place.
pub(crate) fn default_turn(task_hook: TaskHook) -> RunTurn {
    RunTurn {
        session_id: task_hook.session_id().to_string(),
        content: "hello".to_string(),
        task_hook,
        channel: "cli".to_string(),
        chat_id: "c1".to_string(),
        runtime_ctx: "ctx".to_string(),
    }
}

/// An assembled Run plus the directory tree it points at.
pub(crate) struct RunFixture {
    /// Kept alive so the workspace and session files outlive the test.
    _tmp: tempfile::TempDir,
    runner: AgentRunner,
}

/// Options applied before the fixture assembles its runner.
pub(crate) struct RunFixtureBuilder {
    config: AgentConfig,
    max_tokens: u32,
    pressure: PressureAnchor,
    consolidator: Option<Arc<Consolidator>>,
}

impl RunFixture {
    /// A fixture with the stock config and no consolidator or tracer.
    pub(crate) fn new() -> Self {
        Self::builder().build()
    }

    pub(crate) fn builder() -> RunFixtureBuilder {
        RunFixtureBuilder {
            config: AgentConfig::default(),
            max_tokens: 100,
            pressure: PressureAnchor::default(),
            consolidator: None,
        }
    }

    /// A borrowed view for the default turn: [`TEST_SESSION`], content `hello`,
    /// origin `cli:c1`, no cancellation.
    pub(crate) fn spec(&self) -> RunSpec<'_> {
        self.spec_with(TaskHook::new(TEST_SESSION))
    }

    /// A borrowed view carrying the given task hook (for event / status tests).
    pub(crate) fn spec_with(&self, task_hook: TaskHook) -> RunSpec<'_> {
        RunSpec::new(&self.runner, default_turn(task_hook))
    }
}

impl RunFixtureBuilder {
    /// Adjust the agent config; the runner clones it at assembly time.
    pub(crate) fn config(mut self, edit: impl FnOnce(&mut AgentConfig)) -> Self {
        edit(&mut self.config);
        self
    }

    pub(crate) fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub(crate) fn pressure(mut self, pressure: PressureAnchor) -> Self {
        self.pressure = pressure;
        self
    }

    pub(crate) fn consolidator(mut self, consolidator: Arc<Consolidator>) -> Self {
        self.consolidator = Some(consolidator);
        self
    }

    pub(crate) fn build(self) -> RunFixture {
        let tmp = tempfile::TempDir::new().expect("temp dir");
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).expect("workspace dir");

        let session_manager: SharedSessionManager = Arc::new(Mutex::new(
            SessionManager::new(tmp.path().join("sessions")).expect("session manager"),
        ));
        let memory_service: SharedMemory = Arc::new(Mutex::new(MemoryService::new(&workspace_dir)));

        let runner = AgentRunnerBuilder::new()
            .session_manager(session_manager)
            .tool_manager(Arc::new(ToolManager::new(workspace_dir.clone())))
            .provider(Arc::new(MockProvider::new()))
            .config(self.config)
            .workspace_dir(workspace_dir)
            .memory_service(memory_service)
            .consolidator(self.consolidator)
            .max_tokens(self.max_tokens)
            .pressure(self.pressure)
            .with_defaults()
            .build();

        RunFixture { _tmp: tmp, runner }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_run_dependency_added_at_single_assembly_point() {
        // Every Run-level dependency the spec exposes is the runner's own
        // object, not a copy: wiring a new one is a builder edit and nothing
        // else, and a hook reading it goes through the same instance.
        let fixture = RunFixture::new();
        let spec = fixture.spec();
        let runner = spec.runner();

        assert!(std::ptr::eq(spec.config(), runner.config()));
        assert!(std::ptr::eq(spec.pressure(), runner.pressure()));
        assert!(std::ptr::eq(spec.workspace_dir(), runner.workspace_dir()));
        assert!(std::ptr::eq(
            spec.session_manager(),
            runner.session_manager()
        ));
        assert!(std::ptr::eq(spec.tool_manager(), runner.tool_manager()));
        assert!(std::ptr::eq(spec.memory_service(), runner.memory_service()));
        assert!(std::ptr::eq(spec.approvals(), runner.approvals()));
        assert_eq!(spec.max_tokens(), runner.max_tokens());
        assert_eq!(spec.max_iterations(), runner.config().max_iterations);
        assert!(spec.consolidator().is_none());
        assert!(spec.tracer().is_none());
        assert!(spec.connectors().is_none());
    }

    #[test]
    fn test_run_spec_is_borrowed_view_of_runner() {
        // Two views of one run share the dependency set and differ only in the
        // per-turn inputs: nothing about the dependencies is copied per turn.
        let fixture = RunFixture::new();
        let first = fixture.spec();
        let second = fixture.spec_with(TaskHook::new("s2"));

        assert!(std::ptr::eq(first.runner(), second.runner()));
        assert!(std::ptr::eq(first.config(), second.config()));
        assert!(std::ptr::eq(
            first.session_manager(),
            second.session_manager()
        ));
        assert!(std::ptr::eq(first.approvals(), second.approvals()));
        assert_ne!(first.session_id, second.session_id);
        assert_ne!(first.task_hook.run_id(), second.task_hook.run_id());
    }

    #[test]
    fn test_fixture_builder_shapes_the_runner_it_assembles() {
        let fixture = RunFixture::builder()
            .config(|c| c.persist_tool_results = false)
            .max_tokens(256)
            .pressure(PressureAnchor::from_window(1_024))
            .build();
        let spec = fixture.spec();

        assert!(!spec.config().persist_tool_results);
        assert_eq!(spec.max_tokens(), 256);
        assert_eq!(spec.pressure().window_tokens, 1_024);
    }

    #[test]
    fn test_fixture_spec_carries_the_turn_inputs() {
        let fixture = RunFixture::new();
        let hook = TaskHook::new(TEST_SESSION);
        let run_id = hook.run_id().to_string();
        let spec = fixture.spec_with(hook);

        assert_eq!(spec.session_id, TEST_SESSION);
        assert_eq!(spec.content, "hello");
        assert_eq!(spec.channel, "cli");
        assert_eq!(spec.chat_id, "c1");
        assert_eq!(spec.runtime_ctx, "ctx");
        assert_eq!(spec.task_hook.run_id(), run_id);
        assert!(spec.cancel_token.is_none());
    }
}

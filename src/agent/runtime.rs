//! Subagent runtime: one-shot runs in isolated, memory-only sessions.
//!
//! A run is started asynchronously and returns a `run_id` immediately. When it
//! finishes (success, failure or timeout) the result is injected back into the
//! originating conversation as an inbound message — the main loop then
//! summarizes it for the user — unless the agent (or the caller) opted out of
//! delivery, in which case only the trace records the run.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

use crate::config::AgentConfig;
use crate::memory::SharedMemory;
use crate::message_bus::{BusRequest, MessageBus};
use crate::provider::Provider;
use crate::runner::AgentRunner;
use crate::session::{SharedSessionManager, TaskHook};
use crate::trace::{SpanRecorder, SpanStatus, Tracer};
use crate::{debug, error, info};

use super::catalog::ToolCatalog;
use super::definition::{AgentDefinition, AgentRegistry, load_workspace_registry};

/// Where a run's result is delivered.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Origin {
    pub channel: String,
    pub chat_id: String,
}

impl Origin {
    pub fn new(channel: impl Into<String>, chat_id: impl Into<String>) -> Self {
        Self {
            channel: channel.into(),
            chat_id: chat_id.into(),
        }
    }

    /// `{channel}:{chat_id}` — the session id the main loop expects.
    pub fn session_id(&self) -> String {
        format!("{}:{}", self.channel, self.chat_id)
    }
}

/// What to start.
#[derive(Debug, Clone)]
pub struct StartRequest {
    pub agent: String,
    pub task: String,
    /// Originating conversation; `None` means "cannot deliver anywhere".
    pub origin: Option<Origin>,
    /// Caller override of the agent's `deliver` default (trigger actions).
    pub deliver: Option<bool>,
}

/// How a caller waits for a free slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcquireMode {
    /// Fail fast with `busy` — used by the `subagent` tool, whose caller (the
    /// model) can retry.
    RejectWhenFull,
    /// Wait for a slot — used by trigger fires, which have no caller to retry.
    WaitForSlot,
}

/// Why a run could not start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartReject {
    UnknownAgent { available: Vec<String> },
    MissingTools { missing: Vec<String> },
    Busy { current: usize, max: usize },
    Internal(String),
}

impl StartReject {
    pub fn code(&self) -> &'static str {
        match self {
            StartReject::UnknownAgent { .. } => "unknown_agent",
            StartReject::MissingTools { .. } => "missing_tools",
            StartReject::Busy { .. } => "busy",
            StartReject::Internal(_) => "internal",
        }
    }
}

/// Handle returned as soon as a run starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedRun {
    pub agent: String,
    pub run_id: String,
    pub delivers: bool,
    pub timeout_secs: u64,
}

pub struct SubagentRuntime {
    catalog: Arc<ToolCatalog>,
    workspace_dir: PathBuf,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    memory_service: SharedMemory,
    agent_config: AgentConfig,
    default_tools: Vec<String>,
    max_tokens: u32,
    tracer: Option<Arc<Tracer>>,
    bus: Arc<MessageBus>,
    slots: Arc<Semaphore>,
    max_concurrent: usize,
    running: Arc<AtomicUsize>,
}

/// Everything one run needs, cloned out of the runtime so the spawned task
/// does not borrow it.
struct RunPlan {
    definition: AgentDefinition,
    tool_names: Vec<String>,
    origin: Option<Origin>,
    deliver: bool,
    task: String,
    run_id: String,
}

impl SubagentRuntime {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        catalog: Arc<ToolCatalog>,
        workspace_dir: PathBuf,
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        memory_service: SharedMemory,
        agent_config: AgentConfig,
        default_tools: Vec<String>,
        max_tokens: u32,
        max_concurrent: usize,
        tracer: Option<Arc<Tracer>>,
        bus: Arc<MessageBus>,
    ) -> Self {
        let max_concurrent = max_concurrent.max(1);
        Self {
            catalog,
            workspace_dir,
            provider,
            session_manager,
            memory_service,
            agent_config,
            default_tools,
            max_tokens,
            tracer,
            bus,
            slots: Arc::new(Semaphore::new(max_concurrent)),
            max_concurrent,
            running: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Currently executing runs.
    pub fn running_count(&self) -> usize {
        self.running.load(Ordering::Relaxed)
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Load the current agent definitions.
    pub fn registry(&self) -> AgentRegistry {
        load_workspace_registry(&self.workspace_dir)
    }

    /// Start a run. Returns as soon as the task is spawned: the caller never
    /// waits for the agent to finish.
    pub async fn start(
        self: &Arc<Self>,
        req: StartRequest,
        mode: AcquireMode,
    ) -> Result<StartedRun, StartReject> {
        let def = {
            let registry = self.registry();
            match registry.get(&req.agent) {
                Some(def) => def.clone(),
                None => {
                    return Err(StartReject::UnknownAgent {
                        available: registry.names(),
                    });
                }
            }
        };

        // Second interception point: the definition may have gone stale since
        // load time (a tool removed from the catalog). Echo the full known set
        // so the caller can fix the definition.
        let known = super::catalog::known_tool_names();
        if super::definition::validate_tools(&def, &known).is_err() {
            return Err(StartReject::MissingTools {
                missing: def
                    .tools
                    .iter()
                    .filter(|t| !known.iter().any(|k| k == *t))
                    .cloned()
                    .collect(),
            });
        }

        let deliver = req.deliver.unwrap_or(def.deliver);
        let delivers = deliver && req.origin.is_some();
        if deliver && req.origin.is_none() {
            debug!(
                "[subagent] agent '{}' has no origin; result will only be traced",
                def.name
            );
        }

        let permit = match mode {
            AcquireMode::RejectWhenFull => match self.slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    return Err(StartReject::Busy {
                        current: self.running_count(),
                        max: self.max_concurrent,
                    });
                }
            },
            AcquireMode::WaitForSlot => match self.slots.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(e) => return Err(StartReject::Internal(e.to_string())),
            },
        };

        let run_id = new_run_id();
        let plan = RunPlan {
            tool_names: def.tool_names(&self.default_tools),
            definition: def,
            origin: req.origin,
            deliver: delivers,
            task: req.task,
            run_id,
        };

        let started = StartedRun {
            agent: plan.definition.name.clone(),
            run_id: plan.run_id.clone(),
            delivers: plan.deliver,
            timeout_secs: plan.definition.timeout_secs,
        };

        let this = self.clone();
        self.running.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            // The permit is released by drop when the run finishes (including
            // panics inside the runner task).
            let _permit = permit;
            this.execute(plan).await;
            this.running.fetch_sub(1, Ordering::Relaxed);
        });

        Ok(started)
    }

    async fn execute(&self, plan: RunPlan) {
        let session_id = format!("subagent:{}:{}", plan.definition.name, plan.run_id);
        let started = Instant::now();
        info!(
            "[subagent] start agent={} run_id={} session={}",
            plan.definition.name, plan.run_id, session_id
        );

        // One-shot session: memory only, with the agent body as system prompt.
        {
            let mut sm = self.session_manager.lock().await;
            if let Err(e) = sm.get_or_create(&session_id).await {
                error!("[subagent] failed to create session: {}", e);
                return;
            }
            sm.mark_ephemeral(&session_id);
            sm.set_system_override(&session_id, plan.definition.system_prompt.clone());
        }

        let tool_manager = Arc::new(
            self.catalog
                .build_tool_manager(&plan.tool_names, &self.workspace_dir),
        );
        let runner = AgentRunner::builder()
            .session_manager(self.session_manager.clone())
            .tool_manager(tool_manager)
            .provider(self.provider.clone())
            .config(self.agent_config.clone())
            .workspace_dir(self.workspace_dir.clone())
            .memory_service(self.memory_service.clone())
            .consolidator(None)
            .tracer(self.tracer.clone())
            .max_tokens(self.max_tokens)
            .with_defaults()
            .build();

        let span = self
            .tracer
            .as_ref()
            .map(|tracer| tracer.start_span("subagent.run", None));
        if let Some(span) = &span {
            span.set_attribute("agent", serde_json::json!(plan.definition.name));
            span.set_attribute("run_id", serde_json::json!(plan.run_id));
        }

        let (origin_channel, origin_chat_id) = match &plan.origin {
            Some(o) => (Some(o.channel.clone()), Some(o.chat_id.clone())),
            None => (None, None),
        };
        let timeout = Duration::from_secs(plan.definition.timeout_secs);
        let outcome = tokio::time::timeout(
            timeout,
            runner.run(
                plan.task.clone(),
                TaskHook::new(&session_id),
                &session_id,
                None,
                None,
                origin_channel,
                origin_chat_id,
            ),
        )
        .await;

        let (ok, body, elapsed) = match outcome {
            Ok(result) => (result.success, result.content, started.elapsed()),
            Err(_) => (
                false,
                format!(
                    "Subagent timed out after {}s (agent '{}').",
                    plan.definition.timeout_secs, plan.definition.name
                ),
                started.elapsed(),
            ),
        };

        // Always drop the session, on every exit path. Nothing was persisted
        // (the session was ephemeral), so this also guarantees no disk trace.
        {
            let mut sm = self.session_manager.lock().await;
            sm.drop_session(&session_id);
        }

        if let Some(span) = &span {
            span.set_attribute("ok", serde_json::json!(ok));
            span.set_attribute("elapsed_ms", serde_json::json!(elapsed.as_millis() as u64));
            span.set_status(if ok {
                SpanStatus::Ok
            } else {
                SpanStatus::Error(body.clone())
            });
        }

        info!(
            "[subagent] done agent={} run_id={} ok={} elapsed={}ms",
            plan.definition.name,
            plan.run_id,
            ok,
            elapsed.as_millis()
        );

        if !plan.deliver {
            return;
        }
        let origin = match &plan.origin {
            Some(o) => o,
            None => return,
        };
        let message = compose_result(
            &plan.definition.name,
            &plan.run_id,
            ok,
            elapsed.as_secs(),
            &body,
        );
        let session = origin.session_id();
        self.bus
            .publish_inbound(BusRequest::owner(
                session.clone(),
                message,
                TaskHook::new(&session),
            ))
            .await;
    }
}

/// The fixed result shape: structured header line, body, instruction line.
/// The header tells the model who finished and how long it took; the last line
/// asks for a summary instead of a paste, so the block is never mistaken for
/// something the user said.
pub fn compose_result(agent: &str, run_id: &str, ok: bool, elapsed_s: u64, body: &str) -> String {
    let instruction = if ok {
        "Summarize this for the user: what was done, what was found, and anything that needs their decision."
    } else {
        "Explain to the user what failed and give them a concrete next step."
    };
    format!(
        "[Subagent result] agent={} run_id={} ok={} elapsed={}s\n\n{}\n\n{}",
        agent,
        run_id,
        ok,
        elapsed_s,
        body.trim(),
        instruction
    )
}

fn new_run_id() -> String {
    use rand::Rng;
    let mut rng = rand::rng();
    let n: u32 = rng.random();
    format!("r-{:08x}", n)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use tokio::sync::mpsc;

    use crate::agent::definition::default_tool_set;
    use crate::message_bus::MessageBusReceivers;
    use crate::provider::{FinishReason, LLMResponse, MockProvider, Usage};
    use crate::session::{Message, SessionManager};
    use crate::tool::ToolDefinition;
    use crate::trace::SpanRecorder;

    struct Fixture {
        _tmp: tempfile::TempDir,
        runtime: Arc<SubagentRuntime>,
        receivers: MessageBusReceivers,
    }

    struct SlowProvider {
        delay: Duration,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Provider for SlowProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            tokio::time::sleep(self.delay).await;
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(LLMResponse {
                content: Some("slow answer".to_string()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn write_agent(workspace: &std::path::Path, file: &str, deliver: bool, timeout: u64) {
        let dir = workspace.join("agents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(file),
            format!(
                "---\nname: {}\ndescription: test agent\ndeliver: {}\ntimeout_secs: {}\n---\n\nYou are a test agent.\n",
                file.trim_end_matches(".md"),
                deliver,
                timeout
            ),
        )
        .unwrap();
    }

    fn fixture(provider: Arc<dyn Provider>, max_concurrent: usize) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut catalog = ToolCatalog::new();
        catalog.register_builtin("file_reader", &workspace);

        let bus = MessageBus::new();
        let (message_bus, receivers) = bus;
        let session_manager: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(sessions).unwrap(),
        ));
        let memory_service: SharedMemory = Arc::new(tokio::sync::Mutex::new(
            crate::memory::MemoryService::new(&workspace),
        ));

        let runtime = Arc::new(SubagentRuntime::new(
            Arc::new(catalog),
            workspace.clone(),
            provider,
            session_manager,
            memory_service,
            AgentConfig::default(),
            default_tool_set(&[]),
            512,
            max_concurrent,
            None,
            message_bus,
        ));

        Fixture {
            _tmp: tmp,
            runtime,
            receivers,
        }
    }

    async fn next_inbound(
        rx: &mut mpsc::Receiver<crate::message_bus::BusRequest>,
    ) -> Option<crate::message_bus::BusRequest> {
        tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn test_start_returns_immediately_and_delivers_result_to_origin() {
        let f = fixture(
            Arc::new(SlowProvider {
                delay: Duration::from_millis(200),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            3,
        );
        write_agent(
            f._tmp.path().join("workspace").as_path(),
            "echo.md",
            true,
            30,
        );

        let started = Instant::now();
        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "do the thing".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .expect("run must start");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "start must not wait for the run, took {:?}",
            started.elapsed()
        );
        assert!(run.run_id.starts_with("r-"));
        assert!(run.delivers);

        let mut rx = f.receivers.inbound;
        let req = next_inbound(&mut rx).await.expect("inbound result");
        assert_eq!(req.session_id, "cli:c1");
        assert!(req.content.starts_with(&format!(
            "[Subagent result] agent=echo run_id={} ok=true",
            run.run_id
        )));
        assert!(req.content.contains("slow answer"));
        assert!(req.content.contains("Summarize this for the user"));
    }

    #[tokio::test]
    async fn test_same_named_runs_are_distinct_and_both_succeed() {
        let f = fixture(Arc::new(MockProvider::new()), 3);
        write_agent(
            f._tmp.path().join("workspace").as_path(),
            "echo.md",
            true,
            30,
        );

        let a = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "one".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .unwrap();
        let b = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "two".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .unwrap();
        assert_ne!(a.run_id, b.run_id);

        let mut rx = f.receivers.inbound;
        let first = next_inbound(&mut rx).await.unwrap();
        let second = next_inbound(&mut rx).await.unwrap();
        let mut seen = [first.content, second.content];
        seen.sort();
        assert!(seen[0].contains(&a.run_id) || seen[0].contains(&b.run_id));
        assert!(seen.iter().all(|c| c.contains("ok=true")));
    }

    #[tokio::test]
    async fn test_unknown_agent_is_rejected() {
        let f = fixture(Arc::new(MockProvider::new()), 3);
        let err = f
            .runtime
            .start(
                StartRequest {
                    agent: "missing".into(),
                    task: "x".into(),
                    origin: None,
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, StartReject::UnknownAgent { .. }));
    }

    #[tokio::test]
    async fn test_concurrency_cap_rejects_and_waits() {
        let f = fixture(
            Arc::new(SlowProvider {
                delay: Duration::from_millis(400),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            1,
        );
        write_agent(
            f._tmp.path().join("workspace").as_path(),
            "echo.md",
            true,
            30,
        );

        let first = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "one".into(),
                    origin: None,
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await;
        assert!(first.is_ok());

        let rejected = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "two".into(),
                    origin: None,
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await;
        assert!(matches!(
            rejected,
            Err(StartReject::Busy { current: 1, max: 1 })
        ));

        // The waiter path blocks until the slot frees instead of erroring.
        let waited = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "three".into(),
                    origin: None,
                    deliver: None,
                },
                AcquireMode::WaitForSlot,
            )
            .await;
        assert!(waited.is_ok());
    }

    #[tokio::test]
    async fn test_timeout_produces_failure_result_and_no_session_files() {
        let f = fixture(
            Arc::new(SlowProvider {
                delay: Duration::from_secs(5),
                calls: Arc::new(AtomicUsize::new(0)),
            }),
            2,
        );
        let workspace = f._tmp.path().join("workspace");
        write_agent(&workspace, "echo.md", true, 1);

        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "echo".into(),
                    task: "sleepy".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .unwrap();

        let mut rx = f.receivers.inbound;
        let req = next_inbound(&mut rx)
            .await
            .expect("timeout must still deliver");
        assert!(
            req.content
                .contains(&format!("run_id={} ok=false", run.run_id))
        );
        assert!(req.content.contains("timed out"));
        assert!(req.content.contains("Explain to the user what failed"));

        // Session gone, nothing on disk.
        let session_id = format!("subagent:echo:{}", run.run_id);
        assert!(
            !f.runtime
                .session_manager
                .lock()
                .await
                .has_session(&session_id)
        );
        let leftover: Vec<_> = std::fs::read_dir(f._tmp.path().join("sessions"))
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            leftover.is_empty(),
            "subagent runs must leave no session files"
        );
    }

    #[tokio::test]
    async fn test_deliver_false_only_traces() {
        let f = fixture(Arc::new(MockProvider::new()), 2);
        let workspace = f._tmp.path().join("workspace");
        write_agent(&workspace, "quiet.md", false, 30);

        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "quiet".into(),
                    task: "silent".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .unwrap();
        assert!(!run.delivers);

        let mut rx = f.receivers.inbound;
        let got = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(got.is_err(), "deliver=false must not inject anything");
    }

    #[test]
    fn test_origin_session_id() {
        assert_eq!(Origin::new("webui", "main").session_id(), "webui:main");
    }

    #[test]
    fn test_run_id_is_unique_and_prefixed() {
        let a = new_run_id();
        let b = new_run_id();
        assert!(a.starts_with("r-"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_compose_result_success_shape() {
        let msg = compose_result("news", "r-1234", true, 42, "  found 3 items \n");
        assert!(msg.starts_with("[Subagent result] agent=news run_id=r-1234 ok=true elapsed=42s"));
        assert!(msg.contains("found 3 items"));
        assert!(msg.ends_with("Summarize this for the user: what was done, what was found, and anything that needs their decision."));
    }

    #[test]
    fn test_compose_result_failure_shape() {
        let msg = compose_result("news", "r-9", false, 5, "boom");
        assert!(msg.contains("ok=false"));
        assert!(
            msg.ends_with("Explain to the user what failed and give them a concrete next step.")
        );
    }
}

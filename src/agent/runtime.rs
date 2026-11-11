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
    /// Shared pending-approval store; subagent runs are unattended, so every
    /// `Ask` inside them is refused instead of proposed (ADR 0011).
    approvals: Arc<crate::approval::ApprovalStore>,
    /// The process-wide connector pool, when there is one. Shared rather than
    /// copied: a subagent's `codemode` scripts and its top-level gate must see
    /// the same servers, and an executor run reaches its connectors through
    /// them (ticket 15).
    connectors: Option<Arc<crate::mcp::McpService>>,
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
    /// The catalog every run assembles its tool set from. Test-only: production
    /// code reaches the catalog through `start`, and the wiring tests need to
    /// see what a declared tool can actually be built from.
    #[cfg(test)]
    pub(crate) fn catalog(&self) -> &ToolCatalog {
        self.catalog.as_ref()
    }

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
        approvals: Arc<crate::approval::ApprovalStore>,
        connectors: Option<Arc<crate::mcp::McpService>>,
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
            approvals,
            connectors,
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
        let runner = {
            let builder = AgentRunner::builder()
                .session_manager(self.session_manager.clone())
                .tool_manager(tool_manager)
                .provider(self.provider.clone())
                .config(self.agent_config.clone())
                .workspace_dir(self.workspace_dir.clone())
                .memory_service(self.memory_service.clone())
                .consolidator(None)
                .tracer(self.tracer.clone())
                .max_tokens(self.max_tokens)
                .approvals(self.approvals.clone());
            // Same pool the main loop reaches: a subagent script's
            // `tools.mcp__*` and an executor's connector writes both come from
            // here. Without a pool the run simply has no connector surface.
            let builder = match self.connectors.clone() {
                Some(pool) => builder.connectors(pool),
                None => builder,
            };
            builder.with_defaults().build()
        };

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
                // Subagents are unattended: no owner is in the loop to redeem
                // an approval, so the hook turns every `Ask` into a refusal.
                TaskHook::new(&session_id).with_turn_kind(crate::session::TurnKind::Subagent),
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
        ) -> crate::provider::ProviderResult<LLMResponse> {
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
        fixture_with(provider, max_concurrent, |_catalog, _workspace| {})
    }

    /// Same fixture, with a hook to register extra tool factories before the
    /// runtime is built (a run can only reach what the catalog can build).
    fn fixture_with(
        provider: Arc<dyn Provider>,
        max_concurrent: usize,
        extend: impl FnOnce(&mut ToolCatalog, &std::path::Path),
    ) -> Fixture {
        fixture_full(
            provider,
            max_concurrent,
            None,
            Arc::new(crate::approval::ApprovalStore::new(Default::default())),
            extend,
        )
    }

    /// The fixture with the process-wide dependencies a connector test needs:
    /// the pool a subagent reaches, and the approval policy its calls meet.
    fn fixture_full(
        provider: Arc<dyn Provider>,
        max_concurrent: usize,
        connectors: Option<Arc<crate::mcp::McpService>>,
        approvals: Arc<crate::approval::ApprovalStore>,
        extend: impl FnOnce(&mut ToolCatalog, &std::path::Path),
    ) -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("workspace");
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&workspace).unwrap();

        let mut catalog = ToolCatalog::new();
        catalog.register_builtin("file_reader", &workspace);
        extend(&mut catalog, &workspace);

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
            approvals,
            connectors,
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

    /// One scripted answer for [`ScriptedProvider`].
    enum Step {
        Reply(LLMResponse),
        /// The host assigns the executor id, so the `run` call is built from the
        /// conversation: the `create` result the model just read carries it.
        RunTheStoredExecutor,
        /// Report the most recent tool result verbatim — what the shipped
        /// template tells the agent to do with a failed run ("report what was
        /// asked and why it stopped here"), without scripting its wording.
        EchoLastToolResult,
    }

    /// Answers with a fixed script of responses, recording the tool names the
    /// run offered the model. (`crate::provider::MockProvider` is a single
    /// canned reply and cannot drive a tool-calling run.)
    struct ScriptedProvider {
        steps: std::sync::Mutex<Vec<Step>>,
        calls: AtomicUsize,
        offered_tools: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedProvider {
        fn new(steps: Vec<Step>) -> Self {
            Self {
                steps: std::sync::Mutex::new(steps),
                calls: AtomicUsize::new(0),
                offered_tools: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    fn tool_call(id: &str, name: &str, args: serde_json::Value) -> crate::tool::ToolCall {
        crate::tool::ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            args,
        }
    }

    /// The `exec-…` id from the most recent tool result that reports one.
    fn executor_id_from(messages: &[&Message]) -> String {
        messages
            .iter()
            .rev()
            .find_map(|message| match message {
                Message::Tool { content, .. } => serde_json::from_str::<serde_json::Value>(content)
                    .ok()?
                    .get("id")?
                    .as_str()
                    .filter(|id| id.starts_with("exec-"))
                    .map(str::to_string),
                _ => None,
            })
            .expect("the create result carries the executor id")
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat(
            &self,
            messages: &[&Message],
            tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> crate::provider::ProviderResult<LLMResponse> {
            if let Some(tools) = tools {
                self.offered_tools
                    .lock()
                    .unwrap()
                    .extend(tools.iter().map(|t| t.name.clone()));
            }
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            let steps = self.steps.lock().unwrap();
            match steps.get(index) {
                Some(Step::Reply(reply)) => Ok(reply.clone()),
                Some(Step::RunTheStoredExecutor) => {
                    let id = executor_id_from(messages);
                    Ok(LLMResponse {
                        content: Some("running it".to_string()),
                        tool_calls: Some(vec![tool_call(
                            "tc-run",
                            "executor",
                            // The brief the model read in step 1, handed to the
                            // action as `ctx.args`.
                            serde_json::json!({
                                "action": "run",
                                "id": id,
                                "args": { "text": "one two three four five" }
                            }),
                        )]),
                        finish_reason: FinishReason::ToolCalls,
                        usage: Usage::default(),
                    })
                }
                Some(Step::EchoLastToolResult) => {
                    let last = messages
                        .iter()
                        .rev()
                        .find_map(|message| match message {
                            Message::Tool { content, .. } => Some(content.clone()),
                            _ => None,
                        })
                        .expect("a tool result to report");
                    Ok(LLMResponse {
                        content: Some(last),
                        tool_calls: None,
                        finish_reason: FinishReason::Stop,
                        usage: Usage::default(),
                    })
                }
                None => panic!("ScriptedProvider: no more responses for call {index}"),
            }
        }
    }

    fn reply(content: &str) -> Step {
        Step::Reply(LLMResponse {
            content: Some(content.to_string()),
            tool_calls: None,
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
        })
    }

    fn call(name: &str, args: serde_json::Value) -> Step {
        Step::Reply(LLMResponse {
            content: None,
            tool_calls: Some(vec![tool_call("tc", name, args)]),
            finish_reason: FinishReason::ToolCalls,
            usage: Usage::default(),
        })
    }

    #[tokio::test]
    async fn test_the_executor_agent_creates_and_runs_an_action_script() {
        // The whole point of the entry: the definition, its `tools: executor`
        // increment and the tool itself line up, so one delegated task can read
        // a workspace file, record an executor and run it. The result travels
        // back as the ordinary `[Subagent result]` line.
        let script = "async function act(ctx) { \
             const words = String(ctx.args.text || '').trim().split(/\\s+/).filter(Boolean); \
             return { ok: true, summary: 'summarized ' + words.length + ' words' }; \
         }";
        let provider = Arc::new(ScriptedProvider::new(vec![
            call("file_reader", serde_json::json!({ "path": "brief.md" })),
            call(
                "executor",
                serde_json::json!({
                    "action": "create",
                    "name": "summarize-brief",
                    "description": "Summarizes the brief in one line",
                    "script": script,
                }),
            ),
            Step::RunTheStoredExecutor,
            reply("Recorded and run: summarized 5 words."),
        ]));

        let f = fixture_with(provider.clone(), 3, |catalog, workspace| {
            let root = workspace.join("executors");
            let secrets = Arc::new(std::collections::BTreeMap::new());
            catalog.register(
                "executor",
                Arc::new(move || {
                    Some(Box::new(crate::executor::tool::ExecutorTool::new(
                        root.clone(),
                        secrets.clone(),
                        0,
                    )) as Box<dyn crate::tool::Tool>)
                }),
            );
        });
        let workspace = f._tmp.path().join("workspace");
        // The shipped template, not a fixture: `tools: executor` in it is what
        // makes the tool reachable at all.
        std::fs::create_dir_all(workspace.join("agents")).unwrap();
        std::fs::write(
            workspace.join("agents/executor.md"),
            include_str!("../../resources/templates/agents/executor.md"),
        )
        .unwrap();
        std::fs::write(workspace.join("brief.md"), "one two three four five").unwrap();

        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "executor".into(),
                    task: "Summarize brief.md with an executor.".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .expect("the executor agent must start");

        let mut rx = f.receivers.inbound;
        let req = next_inbound(&mut rx).await.expect("inbound result");
        assert_eq!(req.session_id, "cli:c1");
        assert!(
            req.content.starts_with(&format!(
                "[Subagent result] agent=executor run_id={}",
                run.run_id
            )),
            "{}",
            req.content
        );
        assert!(req.content.contains("ok=true"));
        assert!(
            req.content.contains("summarized 5 words"),
            "the run's report must reach origin: {}",
            req.content
        );

        // The subagent model, unlike the main loop's, is offered the tool.
        let offered = provider.offered_tools.lock().unwrap().clone();
        assert!(offered.contains(&"executor".to_string()), "{offered:?}");

        // `create` went through the tool, not around it: one ready executor is
        // on disk with the name the model sent.
        let executors: Vec<_> = std::fs::read_dir(workspace.join("executors"))
            .expect("executors dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        assert_eq!(executors.len(), 1, "exactly one executor was created");
        let meta = std::fs::read_to_string(executors[0].path().join("meta.json")).unwrap();
        assert!(meta.contains("summarize-brief"), "{meta}");
        assert!(executors[0].path().join("act.js").exists());
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

    #[tokio::test]
    async fn test_subagent_reaches_the_connector_pool() {
        // The pool the process built is the subagent's too. Before ticket 15 the
        // subagent runner had none, so `tools.mcp__*` was structurally absent
        // there — the exact gap that made an executor's connector writes
        // unreachable.
        let (pool, peer) = crate::mcp::service::test_support::online_service(
            "github",
            vec![serde_json::json!({
                "name": "get_pull",
                "annotations": {"readOnlyHint": true, "destructiveHint": false},
            })],
        );
        crate::mcp::service::test_support::wait_for(|| pool.any_online(), "the fake server online")
            .await;

        let provider = Arc::new(ScriptedProvider::new(vec![
            call(
                "codemode",
                serde_json::json!({
                    "script": "async function main(ctx) { \
                         const out = await ctx.tools.mcp__github__get_pull({number: 1}); \
                         return { seen: out.includes('pong') }; }"
                }),
            ),
            reply("read the pull request"),
        ]));
        let mut approvals = crate::approval::ApprovalConfig::default();
        approvals.tools.insert(
            "mcp__github__get_pull".to_string(),
            crate::approval::ControlLevel::Allow,
        );
        let f = fixture_full(
            provider,
            3,
            Some(pool),
            Arc::new(crate::approval::ApprovalStore::new(approvals)),
            |catalog, _workspace| {
                catalog.register(
                    "codemode",
                    Arc::new(|| {
                        Some(Box::new(crate::tools::codemode::CodemodeTool)
                            as Box<dyn crate::tool::Tool>)
                    }),
                );
            },
        );
        // The agent has to declare the tool: `tools:` is the per-run increment.
        let workspace = f._tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("agents")).unwrap();
        std::fs::write(
            workspace.join("agents/reader.md"),
            "---\nname: reader\ndescription: reads a connector\ndeliver: true\ntimeout_secs: 30\ntools: codemode\n---\n\nYou are a test agent.\n",
        )
        .unwrap();

        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "reader".into(),
                    task: "read the pull request".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .expect("run must start");

        let mut rx = f.receivers.inbound;
        let req = next_inbound(&mut rx).await.expect("inbound result");
        assert!(
            req.content.starts_with(&format!(
                "[Subagent result] agent=reader run_id={}",
                run.run_id
            )),
            "{}",
            req.content
        );
        assert_eq!(
            peer.calls(),
            1,
            "the subagent's script must reach the connector: {}",
            req.content
        );
    }

    #[tokio::test]
    async fn test_the_executor_agent_reports_an_approval_refusal_to_origin() {
        // Demo (b) end to end: the action's write is `ExternalSend` and the
        // policy is the default, so an unattended run refuses it — and origin
        // reads why, instead of a cheerful summary of something never sent.
        let (pool, peer) = crate::mcp::service::test_support::online_service(
            "github",
            vec![serde_json::json!({"name": "create_issue"})],
        );
        crate::mcp::service::test_support::wait_for(|| pool.any_online(), "the fake server online")
            .await;

        let script = "async function act(ctx) { \
             const out = await ctx.tools.mcp__github__create_issue({ title: 'the bug' }); \
             return { ok: true, summary: 'filed: ' + out }; \
         }";
        let provider = Arc::new(ScriptedProvider::new(vec![
            call(
                "executor",
                serde_json::json!({
                    "action": "create",
                    "name": "file-the-bug",
                    "description": "Files one bug through the github connector",
                    "script": script,
                    "allow_connectors": ["github"],
                }),
            ),
            Step::RunTheStoredExecutor,
            Step::EchoLastToolResult,
        ]));
        // Default policy: every connector call is `ExternalSend`, and
        // `ExternalSend` defaults to `Ask`.
        let approvals = Arc::new(crate::approval::ApprovalStore::new(Default::default()));
        let f = fixture_full(
            provider,
            3,
            Some(pool),
            approvals.clone(),
            |catalog, workspace| {
                let root = workspace.join("executors");
                let secrets = Arc::new(std::collections::BTreeMap::new());
                catalog.register(
                    "executor",
                    Arc::new(move || {
                        Some(Box::new(crate::executor::tool::ExecutorTool::new(
                            root.clone(),
                            secrets.clone(),
                            0,
                        )) as Box<dyn crate::tool::Tool>)
                    }),
                );
            },
        );
        let workspace = f._tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("agents")).unwrap();
        std::fs::write(
            workspace.join("agents/executor.md"),
            include_str!("../../resources/templates/agents/executor.md"),
        )
        .unwrap();

        let run = f
            .runtime
            .start(
                StartRequest {
                    agent: "executor".into(),
                    task: "File the bug through github.".into(),
                    origin: Some(Origin::new("cli", "c1")),
                    deliver: None,
                },
                AcquireMode::RejectWhenFull,
            )
            .await
            .expect("the executor agent must start");

        let mut rx = f.receivers.inbound;
        let req = next_inbound(&mut rx).await.expect("inbound result");
        assert!(
            req.content.starts_with(&format!(
                "[Subagent result] agent=executor run_id={}",
                run.run_id
            )),
            "{}",
            req.content
        );
        assert!(
            req.content.contains("needs owner authorization"),
            "origin must read the refusal reason: {}",
            req.content
        );
        assert!(
            req.content.contains("nothing was executed"),
            "and that nothing was sent: {}",
            req.content
        );
        assert!(
            req.content.contains("approval_refused"),
            "the failure label travels with it: {}",
            req.content
        );
        assert_eq!(peer.calls(), 0, "nothing reached the connector");
        assert!(
            !approvals.has_pending("cli:c1"),
            "an unattended run must not leave a pending approval behind"
        );
    }
}

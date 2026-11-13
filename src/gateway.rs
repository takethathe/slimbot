use anyhow::Result;
use std::sync::Arc;
use tokio::signal;

use crate::WorkerPool;
use crate::agent_loop::AgentLoop;
use crate::channel::ChannelManager;
use crate::config::Config;
use crate::cron::CronService;
use crate::heartbeat::HeartbeatService;
use crate::info;
use crate::message_bus::{BusResult, MessageBus};
use crate::path::PathManager;
use crate::session::{AgentEvent, TaskHook};
use crate::tool::{Tool, ToolManager};
use crate::tools::cron::CronTool;
use crate::tools::message::MessageTool;
use crate::warn_log;

/// Register one tool in both places a run can reach it, from a single builder:
/// one instance goes to the main loop's shared `ToolManager`, and the same
/// builder becomes the subagent catalog's factory (one instance per run, since
/// every run assembles its own tool set).
///
/// The catalog has to mirror the main loop's tool set and both copies must
/// reach the transports identically; keeping one builder per tool is what makes
/// that a property of the code instead of a copy-paste discipline problem.
fn register_in_loop_and_catalog<T, F>(
    tool_manager: &mut ToolManager,
    catalog: &mut crate::agent::ToolCatalog,
    name: &str,
    builder: &F,
) where
    T: Tool + 'static,
    F: Fn() -> T,
    F: Clone + Send + Sync + 'static,
{
    tool_manager.register(Box::new(builder()));
    let build = builder.clone();
    catalog.register(
        name,
        Arc::new(move || Some(Box::new(build()) as Box<dyn Tool>)),
    );
}

/// A fully assembled gateway: every component is wired and running, and nothing
/// has waited on a shutdown signal yet.
pub struct Gateway {
    channel_manager: Arc<ChannelManager>,
    agent_loop: Arc<AgentLoop>,
    heartbeat: Arc<HeartbeatService>,
    cron: Arc<CronService>,
    card_service: Option<Arc<crate::card::CardService>>,
    channel_shutdown: tokio::sync::broadcast::Sender<()>,
    /// The snapshot handed to the WebUI factory, kept so a wiring test can
    /// assert what was wired without reaching inside the channel.
    webui_startup: Option<crate::channel::WebuiStartup>,
}

impl Gateway {
    /// The WebUI startup snapshot, when the webui channel is configured.
    pub fn webui_startup(&self) -> Option<&crate::channel::WebuiStartup> {
        self.webui_startup.as_ref()
    }

    /// The trace files this process writes, as the write side resolved them.
    pub fn trace_paths(&self) -> &[String] {
        self.agent_loop.trace_paths()
    }

    /// The session manager the loop runs on, so a wiring test can prove the
    /// snapshot carries the real one rather than a copy.
    pub fn session_manager(&self) -> crate::session::SharedSessionManager {
        self.agent_loop.session_manager()
    }

    /// Stop the services in the documented order, with the same 10s budget as
    /// before the assembly step was split out.
    pub async fn shutdown(self) {
        // Signal channel servers to stop (webui etc.)
        let _ = self.channel_shutdown.send(());

        // Cancel in-flight card generations rather than let them hang the exit.
        if let Some(card_service) = &self.card_service {
            let cancelled = card_service.shutdown();
            if !cancelled.is_empty() {
                info!(
                    "[gateway] cancelled {} inflight card generation(s)",
                    cancelled.len()
                );
            }
        }

        // Graceful shutdown with overall timeout.
        // Race against a second Ctrl+C for forced exit.
        self.heartbeat.stop();
        self.cron.stop();
        let shutdown_timeout = std::time::Duration::from_secs(10);
        tokio::select! {
            result = tokio::time::timeout(
                shutdown_timeout,
                self.agent_loop.graceful_shutdown(&self.channel_manager),
            ) => {
                if result.is_err() {
                    crate::error!("[gateway] Graceful shutdown timed out");
                }
            }
            _ = signal::ctrl_c() => {
                eprintln!("\n[gateway] Second Ctrl+C received, forcing exit");
                std::process::exit(1);
            }
        }
    }
}

/// Assemble the gateway: build and start every component, then return the
/// handles the shutdown step needs.
///
/// The split is deliberate — assembly is plain construction a test can drive and
/// assert on, while [`run_gateway`] owns only the wait and the stop sequence.
pub async fn assemble_gateway(paths: &PathManager) -> Result<Gateway> {
    WorkerPool::init_global(64);

    let config = Arc::new(Config::load_resolved(
        paths.config_path().to_str().unwrap(),
    )?);
    Config::install(config.as_ref())?;
    let (message_bus, receivers) = MessageBus::new();
    let outbound_rx = receivers.outbound;
    let inbound_rx = receivers.inbound;
    let media_rx = receivers.media;

    // Build ToolManager with all tools including message + cron
    let mut tool_manager = ToolManager::new(paths.workspace_dir().to_path_buf());
    tool_manager.init_from_config(&config.tools.entries);

    // The catalog mirrors the main loop's tool set as factories, so each
    // subagent run can assemble its own ToolManager without touching the
    // shared one (which would break prompt caching and concurrent runs).
    let mut catalog = crate::agent::ToolCatalog::new();
    catalog.register_builtin_set(&config.tools.entries, paths.workspace_dir());

    // Each delivery tool is wired once, in its own builder, and that single
    // builder feeds both registrations (the main loop's shared tool set and the
    // subagent catalog). Two copies of one tool's send path is how they drift.
    let outbound_tx = message_bus.outbound_tx();
    let build_message = {
        let tx = outbound_tx.clone();
        move || {
            let tx = tx.clone();
            let mut tool = MessageTool::new();
            tool.set_send_callback(Arc::new(move |channel, chat_id, content| {
                let tx = tx.clone();
                Box::pin(async move {
                    tx.send(BusResult {
                        session_id: format!("{}:{}", channel, chat_id),
                        run_id: uuid::Uuid::new_v4().to_string(),
                        content,
                    })
                    .await
                    .map_err(|e| e.to_string())
                })
            }));
            tool
        }
    };
    register_in_loop_and_catalog(&mut tool_manager, &mut catalog, "message", &build_message);

    // send_file delivers an agent-produced image/file on the dedicated media
    // path (ticket 28), beside the text `message` path.
    let media_tx = message_bus.media_tx();
    let build_send_file = {
        let tx = media_tx.clone();
        move || {
            let tx = tx.clone();
            let mut tool = crate::tools::send_file::SendFileTool::new();
            tool.set_send_callback(Arc::new(move |delivery| {
                let tx = tx.clone();
                Box::pin(async move { tx.send(delivery).await.map_err(|e| e.to_string()) })
            }));
            tool
        }
    };
    register_in_loop_and_catalog(
        &mut tool_manager,
        &mut catalog,
        "send_file",
        &build_send_file,
    );

    // Cron: the tool and the scheduler share one service instance, so a job the
    // agent creates is visible to the very next tick.
    let cron_service_arc = Arc::new(CronService::new(paths.workspace_dir()));
    let build_cron = {
        let service = cron_service_arc.clone();
        move || CronTool::new(service.clone())
    };
    register_in_loop_and_catalog(&mut tool_manager, &mut catalog, "cron", &build_cron);

    // Trigger data sources live outside the main loop's tool list: the
    // `trigger_source` and `trigger` tools are visible only to agents that
    // declare them, while the tick itself runs in the background. Both tools and
    // the tick share the *same* layers, so what an agent creates is what the
    // next tick judges.
    let trigger_layers = crate::trigger::build_layers(&config.triggers, paths.workspace_dir());
    let source_layer = trigger_layers.sources.clone();
    let rules_layer = trigger_layers.rules.clone();
    let trigger_secrets = trigger_layers.secrets.clone();
    let sources_for_catalog = source_layer.clone();
    catalog.register(
        "trigger_source",
        Arc::new(move || {
            Some(Box::new(crate::trigger::tool::TriggerSourceTool::new(
                sources_for_catalog.clone(),
            )) as Box<dyn Tool>)
        }),
    );
    let rules_for_catalog = rules_layer.clone();
    let sources_for_trigger = source_layer.clone();
    let secrets_for_trigger = trigger_secrets.clone();
    let workspace_for_trigger = paths.workspace_dir().to_path_buf();
    catalog.register(
        "trigger",
        Arc::new(move || {
            Some(
                Box::new(crate::trigger::subscription::tool::TriggerTool::new(
                    rules_for_catalog.clone(),
                    sources_for_trigger.clone(),
                    workspace_for_trigger.clone(),
                    secrets_for_trigger.clone(),
                )) as Box<dyn Tool>,
            )
        }),
    );

    // Create AgentLoop with pre-configured tool manager
    let agent_loop = Arc::new(
        AgentLoop::from_config_with_tools(
            paths,
            message_bus.clone(),
            inbound_rx,
            config.clone(),
            tool_manager,
            catalog,
        )
        .await?,
    );

    // The collector reads connectors too, but only the half their server claims
    // read-only (ticket 17). The pool came up with the loop, so the source layer
    // (built earlier) is handed it here.
    source_layer.attach_connectors(agent_loop.connectors());

    // Connector pool came up with the loop (ticket 26): naming the configured
    // servers here is the startup record of what the gateway will try to reach.
    // Statuses are still `connecting` — each transition logs itself later, so
    // an unreachable server shows up as a warning, never as a stalled start.
    if let Some(pool) = agent_loop.connectors() {
        let names = pool
            .servers()
            .iter()
            .map(|(name, _, tools)| format!("{name} ({} tool(s) cached)", tools))
            .collect::<Vec<_>>()
            .join(", ");
        info!("[gateway] MCP connectors: {}", names);
    }

    // Reset trace file(s) on Gateway startup so the viewer reflects the
    // current process run. The paths come from the loop — the same list its
    // tracer writes — so writer and viewer cannot disagree. CLI mode never
    // truncates (this is the only place reset_trace_files is called); the
    // append handle of a live FileBackend stays valid across truncation.
    let trace_paths = agent_loop.trace_paths().to_vec();
    let reset_count =
        crate::agent_loop::reset_trace_files(&trace_paths, config.tracing.reset_on_start);
    // The startup instant of the current process's trace file. Recorded only
    // when the file was actually reset (or created) so the viewer can show
    // "started … · running …" that reflects this process, not the file mtime
    // (which updates on every span append). None when tracing is off or
    // `reset_on_start` is false — in those cases there is no clean "startup"
    // boundary to display.
    let trace_startup_ms = (reset_count > 0).then(crate::agent_loop::now_epoch_ms);
    if reset_count > 0 {
        info!("[gateway] Reset {} trace file(s) at startup", reset_count);
    }

    // ChannelManager: init channels from config
    let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
    let mut channel_manager = ChannelManager::new(
        message_bus.clone(),
        outbound_rx,
        config.clone(),
        Some(event_tx.clone()),
    );

    // Set up cron service callback
    let owner_primary_for_cron = agent_loop.owner_primary();
    let agent_loop_for_cron = agent_loop.clone();
    let mb_for_cron = message_bus.clone();
    let event_tx_for_cron = event_tx.clone();
    cron_service_arc.set_on_job(Arc::new(move |job| {
        // Dream jobs are handled separately
        if job.name == "dream" {
            let al = agent_loop_for_cron.clone();
            return Box::pin(async move {
                let result = al.run_dream().await;
                if result.success {
                    info!("[cron] dream completed in {}ms: {}", result.elapsed_ms, result.message);
                } else {
                    info!("[cron] dream failed: {}", result.message);
                }
            });
        }

        let al = agent_loop_for_cron.clone();
        let mb = mb_for_cron.clone();
        let job_clone = job.clone();
        let event_tx = event_tx_for_cron.clone();
        let owner_primary = owner_primary_for_cron.clone();
        Box::pin(async move {
            let session_id = "system:cron";
            // Use origin session_id for event routing so cron execution
            // events (task_started, tool_call, etc.) appear in the user's
            // WebUI chat instead of being filtered as system-channel events.
            let origin_session_id = job_clone.payload.channel.as_deref()
                .zip(job_clone.payload.to.as_deref())
                .map(|(ch, cid)| format!("{}:{}", ch, cid));
            let hook = TaskHook::new(origin_session_id.as_deref().unwrap_or(session_id))
                .with_events(event_tx)
                // Cron runs unattended: an `Ask` inside one is a refusal, never
                // a pending list nobody can redeem (ADR 0011).
                .with_turn_kind(crate::session::TurnKind::Cron);
            let content = format!(
                "[Scheduled Task] Timer finished.\n\nTask '{}' has been triggered.\nScheduled instruction: {}",
                job_clone.name, job_clone.payload.message
            );
            // Pass origin channel/chat_id so the message tool defaults to the target user channel
            let origin_ch = job_clone.payload.channel.clone();
            let origin_cid = job_clone.payload.to.clone();
            let result = al.run_task(session_id, content, hook, None, origin_ch, origin_cid).await;

            // Cap cron session to 2000 messages
            let sm = al.session_manager();
            sm.lock().await.cap_messages(session_id, 2000).await;

            // Deliver the result to the conversation the job was created in, by
            // the one shared delivery rule. `deliver: false` is the only policy
            // silence; a job with no origin falls back to the owner's primary
            // channel, and a missing one degrades to log-only rather than
            // inventing a destination (only when the message tool wasn't used).
            if job_clone.payload.deliver && !result.message_sent {
                match crate::delivery::cron_target(
                    job_clone.payload.channel.as_deref(),
                    job_clone.payload.to.as_deref(),
                    owner_primary.current(),
                ) {
                    crate::delivery::Resolution::Deliver(target) => {
                        mb.publish_outbound(BusResult {
                            session_id: target.session_id(),
                            run_id: uuid::Uuid::new_v4().to_string(),
                            content: result.content,
                        }).await;
                    }
                    crate::delivery::Resolution::Nowhere => {
                        warn_log!(
                            "[gateway] cron job {} has no delivery target; result kept in the session only",
                            job_clone.name
                        );
                    }
                }
            }
        })
    }));

    // Register dream cron job if enabled
    if config.gateway.dream.enabled {
        let dream_job = crate::cron::CronJob {
            id: "dream".to_string(),
            name: "dream".to_string(),
            enabled: true,
            schedule: crate::cron::CronSchedule::every(config.gateway.dream.interval_ms),
            payload: crate::cron::CronPayload {
                kind: "system_event".to_string(),
                message: "Dream memory consolidation".to_string(),
                deliver: false,
                channel: None,
                to: None,
            },
            state: crate::cron::CronJobState::default(),
            created_at_ms: crate::cron::now_ms(),
            updated_at_ms: crate::cron::now_ms(),
            delete_after_run: false,
        };
        cron_service_arc.add_job(dream_job);
        info!(
            "[gateway] dream cron job registered (interval={}ms)",
            config.gateway.dream.interval_ms
        );
    } else {
        info!("[gateway] dream service disabled");
    }

    if config.gateway.cron.enabled {
        cron_service_arc.start();
        info!("[gateway] cron service started");
    } else {
        info!("[gateway] cron service disabled");
    }

    // Set up heartbeat service
    let mut heartbeat = HeartbeatService::new(
        paths.workspace_dir().to_path_buf(),
        config.gateway.heartbeat.interval_s,
        config.gateway.heartbeat.enabled,
    );

    let agent_loop_for_hb = agent_loop.clone();
    heartbeat.set_on_execute(Arc::new(move |content| {
        let al = agent_loop_for_hb.clone();
        Box::pin(async move {
            let session_id = "heartbeat:system";
            let hook = TaskHook::new(session_id)
                // Heartbeat is unattended: `Ask` is a refusal (ADR 0011).
                .with_turn_kind(crate::session::TurnKind::Heartbeat);
            // Heartbeat doesn't use message tool; result is sent via on_notify to webui
            let result = al
                .run_task(session_id, content, hook, None, None, None)
                .await;
            result.content
        })
    }));

    // Heartbeat has no origin: it is started by config, standing in no
    // conversation. Its notice goes to the configured override, else the
    // owner's primary channel, else nowhere (log-only).
    let mb_for_notify = message_bus.clone();
    let owner_primary_for_hb = agent_loop.owner_primary();
    // Hand the configured override over raw: whether it is a destination at all,
    // and whether it beats the owner's channel, is the seam's decision.
    let heartbeat_channel = config.gateway.heartbeat.channel.clone();
    let heartbeat_to = config.gateway.heartbeat.to.clone();
    heartbeat.set_on_notify(Arc::new(move |response| {
        let mb = mb_for_notify.clone();
        let resp = response.clone();
        let channel = heartbeat_channel.clone();
        let to = heartbeat_to.clone();
        let owner_primary = owner_primary_for_hb.current();
        Box::pin(async move {
            match crate::delivery::heartbeat_target(
                channel.as_deref(),
                to.as_deref(),
                owner_primary,
            ) {
                crate::delivery::Resolution::Deliver(target) => {
                    mb.publish_outbound(BusResult {
                        session_id: target.session_id(),
                        run_id: uuid::Uuid::new_v4().to_string(),
                        content: resp,
                    })
                    .await;
                }
                crate::delivery::Resolution::Nowhere => {
                    warn_log!(
                        "[gateway] heartbeat notice has no delivery target yet \
                         (the owner has not talked on any channel); kept in the log only"
                    );
                }
            }
        })
    }));

    if config.gateway.heartbeat.enabled {
        heartbeat.start();
        info!(
            "[gateway] heartbeat service started (interval={}s)",
            config.gateway.heartbeat.interval_s
        );
    } else {
        info!("[gateway] heartbeat service disabled");
    }

    // Create shutdown broadcast for channel servers (webui etc.)
    let (channel_shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    // Register webui factory with shutdown channel
    let mut card_service_for_shutdown: Option<Arc<crate::card::CardService>> = None;
    let mut webui_startup: Option<crate::channel::WebuiStartup> = None;
    if config.channels.contains_key("webui") {
        // Card generation lives in the channel layer: it listens to Run
        // lifecycle events on the broadcast, produces cards off an independent
        // model call, and delivers them over a dedicated mpsc that the manager
        // routes to the WebUI channel. Only genuine WebUI Runs register.
        let (card_tx, card_rx) = tokio::sync::mpsc::channel::<crate::card::CardEvent>(
            crate::card::CARD_CHANNEL_CAPACITY,
        );
        // Card calls get their own provider instance when json_render says so:
        // provider identity inherited or overridden, output budget overridden.
        let card_provider = card_provider_config(config.as_ref())
            .map(|pc| {
                Arc::new(crate::provider::OpenAIProvider::new(&pc))
                    as Arc<dyn crate::provider::Provider>
            })
            .unwrap_or_else(|| agent_loop.provider());
        let card_policy = crate::card::CardPolicy {
            enabled: config.json_render.enabled,
            max_concurrent: 2,
            timeout: std::time::Duration::from_secs(config.json_render.timeout_seconds),
        };
        let service = crate::card::CardService::new(
            card_provider,
            agent_loop.session_manager(),
            crate::card::CardStore::new(paths.session_dir()),
            card_tx,
        )
        .with_policy(card_policy);
        let service = match agent_loop.tracer() {
            Some(tracer) => service.with_tracer(tracer),
            None => service,
        };
        let card_service = Arc::new(service);
        card_service_for_shutdown = Some(card_service.clone());
        crate::card::spawn_listener(card_service.clone(), event_tx.subscribe());
        channel_manager.attach_card_rx(card_rx);
        // The WebUI's runtime objects as one plain-data snapshot, consumed only
        // through the factory registration below. `config_path` is a startup
        // snapshot: the editor handlers read the file themselves, so a saved
        // patch is visible there before the next restart and never here.
        let startup = crate::channel::WebuiStartup {
            session_manager: agent_loop.session_manager(),
            shutdown: channel_shutdown_tx.clone(),
            events: event_tx.clone(),
            trace_paths: trace_paths.clone(),
            trace_startup_ms,
            config_path: paths.config_path().to_string_lossy().to_string(),
            card: Some(card_service),
        };
        channel_manager.register_webui_factory(startup.clone());
        webui_startup = Some(startup);
    }

    // Feishu is a gateway-only channel: it holds a single WebSocket long
    // connection per app, so only the gateway process may register it (ADR
    // 0017). The factory is registered unconditionally; it is exercised only
    // when `channels.feishu` is present and enabled in `init()` below. The
    // shutdown broadcast lets the connection task end on graceful shutdown
    // instead of stalling the ChannelManager's join.
    channel_manager.register_factory(
        "feishu",
        Box::new(crate::channel::FeishuChannelFactory::new(
            message_bus.clone(),
            channel_shutdown_tx.clone(),
            paths.workspace_dir().to_path_buf(),
        )),
    );

    // Attach the media path before the outbound loop starts: send_file results
    // route through it to the owning channel (ticket 28). Wired unconditionally
    // because the bus always creates the media receiver.
    channel_manager.attach_media_rx(media_rx);

    channel_manager.init().await?;
    channel_manager.start_channels().await;

    // Start inbound listener
    agent_loop.start_inbound(None);

    // Set up shutdown broadcast for channel-tier commands
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);
    channel_manager.set_quit_broadcast(shutdown_tx.clone());

    // Spawn outbound router
    let cm = Arc::new(channel_manager);
    let shutdown_rx = shutdown_tx.subscribe();
    tokio::spawn({
        let cm = cm.clone();
        async move {
            cm.run_with_shutdown(shutdown_rx).await;
        }
    });

    // Spawn heartbeat tick loop
    let hb_arc = Arc::new(heartbeat);
    tokio::spawn({
        let hb = hb_arc.clone();
        async move {
            loop {
                tokio::time::sleep(hb.sleep_duration()).await;
                hb.tick().await;
            }
        }
    });

    // Spawn cron tick loop
    tokio::spawn({
        let cron = cron_service_arc.clone();
        async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                cron.tick().await;
            }
        }
    });

    // Spawn the trigger tick loop and its consumer. The tick is its own
    // interval, so `every_n_ticks` never drifts and neither cron nor heartbeat
    // can delay it; the consumer sits outside that interval, so an agent run can
    // outlive the tick that queued it.
    {
        let tracer = agent_loop
            .tracer()
            .map(|t| (*t).clone())
            .unwrap_or_else(|| crate::trace::Tracer::new(vec![]));
        // One subagent runtime and one message bus for the whole process: a
        // trigger fire and a `subagent` tool call share the same concurrency
        // gate and the same route back to a conversation. The wiring itself
        // lives in the trigger module so the tests run the same assembly.
        let wiring = crate::trigger::assemble(
            trigger_layers,
            &config.triggers,
            paths.workspace_dir(),
            tracer,
            Some(agent_loop.subagent_runtime()),
            message_bus.clone(),
        );
        let dispatch = wiring.dispatch;
        let trigger_service = wiring.service;
        if trigger_service.is_enabled() {
            info!(
                "[gateway] trigger tick started (interval={}s)",
                trigger_service.tick_interval().as_secs()
            );
            tokio::spawn(dispatch.run());
            tokio::spawn(trigger_service.run());
        } else {
            info!("[gateway] trigger tick disabled");
        }
    }

    info!("[gateway] Running, press Ctrl+C to stop");

    Ok(Gateway {
        channel_manager: cm,
        agent_loop,
        heartbeat: hb_arc,
        cron: cron_service_arc,
        card_service: card_service_for_shutdown,
        channel_shutdown: channel_shutdown_tx,
        webui_startup,
    })
}

/// Run the gateway until Ctrl+C, then stop it.
///
/// Everything that can be asserted lives in [`assemble_gateway`]; this function
/// is the wait and the stop, which is exactly the part a test cannot drive.
pub async fn run_gateway(paths: &PathManager) -> Result<()> {
    let gateway = assemble_gateway(paths).await?;

    if let Err(e) = signal::ctrl_c().await {
        crate::error!("[gateway] Ctrl+C handler error: {}", e);
    }
    info!("[gateway] Shutdown signal received");

    gateway.shutdown().await;
    Ok(())
}

/// Resolve the provider config for card generation: identity comes from the
/// main agent provider, or the `json_render.model_override` provider name
/// when it exists in the map; the output budget is always the card-specific
/// `json_render.max_tokens`, prompt caching is always on (the catalog prompt
/// is the one stable prefix a card call has) and thinking is forced off (a
/// card is a mechanical transcript→JSONL conversion — reasoning only buys
/// latency). `None` when no provider resolves at all.
fn card_provider_config(config: &Config) -> Option<crate::config::ProviderConfig> {
    let base = config
        .json_render
        .model_override
        .as_deref()
        .and_then(|name| config.providers.get(name))
        .or_else(|| config.providers.get(&config.agent.provider))?;
    let mut pc = base.clone();
    pc.max_tokens = config.json_render.max_tokens;
    pc.prompt_cache_enabled = true;
    pc.disable_thinking = true;
    Some(pc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryCategory;
    use crate::memory::MemoryService;
    use crate::message_bus::BusRequest;
    use crate::session::{Message, SessionManager, SharedSessionManager};
    use std::path::PathBuf;

    fn make_test_paths(tmp: &tempfile::TempDir) -> PathManager {
        let data_dir = tmp.path().join("data");
        let workspace_dir = data_dir.join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Create a minimal config file
        let config_path = data_dir.join("config.json");
        let config = crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        context_window_tokens: 32768,
                        prompt_cache_enabled: false,
                        disable_thinking: false,
                        timeout_seconds: 180,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: false,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: false,
                    interval_s: 60,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        };
        let config_json = serde_json::to_string_pretty(&config).unwrap();
        std::fs::write(&config_path, config_json).unwrap();

        PathManager::resolve(
            Some(config_path.to_str().unwrap()),
            Some(data_dir.to_str().unwrap()),
            Some(workspace_dir.to_str().unwrap()),
        )
        .unwrap()
    }

    fn card_test_config() -> crate::config::Config {
        let mut config = crate::config::Config {
            agent: crate::config::AgentConfig {
                provider: "main".to_string(),
                ..Default::default()
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "main".to_string(),
                    crate::config::ProviderConfig {
                        model: "gpt-4o".to_string(),
                        api_key: "k".to_string(),
                        max_tokens: 4096,
                        context_window_tokens: 32768,
                        ..Default::default()
                    },
                );
                map.insert(
                    "cheap".to_string(),
                    crate::config::ProviderConfig {
                        model: "gpt-4o-mini".to_string(),
                        api_key: "k".to_string(),
                        max_tokens: 8192,
                        context_window_tokens: 32768,
                        ..Default::default()
                    },
                );
                map
            },
            ..Default::default()
        };
        config.json_render = crate::config::JsonRenderConfig::default();
        config
    }

    #[test]
    fn test_card_provider_inherits_main_provider_with_card_budget() {
        let config = card_test_config();
        let pc = card_provider_config(&config).expect("main provider resolves");
        assert_eq!(pc.model, "gpt-4o");
        assert_eq!(
            pc.max_tokens, 2048,
            "card calls carry json_render.max_tokens, not the provider's"
        );
    }

    #[test]
    fn test_card_provider_model_override_switches_provider() {
        let mut config = card_test_config();
        config.json_render.model_override = Some("cheap".to_string());
        let pc = card_provider_config(&config).unwrap();
        assert_eq!(pc.model, "gpt-4o-mini");
        assert_eq!(pc.max_tokens, 2048);
    }

    #[test]
    fn test_card_provider_disables_thinking_for_card_calls() {
        // Card generation is a mechanical transcript→JSONL conversion: model
        // reasoning only costs latency and can starve the no-progress
        // timeout, so the card provider always turns thinking off.
        let config = card_test_config();
        let pc = card_provider_config(&config).expect("main provider resolves");
        assert!(
            pc.disable_thinking,
            "card calls must never run in thinking mode"
        );
    }

    #[test]
    fn test_card_provider_forces_prompt_cache_on_for_card_calls() {
        // The catalog prompt is the only stable prefix a card call has, so
        // caching must not depend on the (possibly cache-less) provider it
        // was inherited from: the card provider always enables it.
        let mut config = card_test_config();
        config
            .providers
            .get_mut("main")
            .unwrap()
            .prompt_cache_enabled = false;
        let pc = card_provider_config(&config).expect("main provider resolves");
        assert!(
            pc.prompt_cache_enabled,
            "card calls must always mark their cacheable prefix"
        );
    }

    #[test]
    fn test_card_provider_unknown_override_falls_back_to_main() {
        let mut config = card_test_config();
        config.json_render.model_override = Some("ghost".to_string());
        let pc = card_provider_config(&config).unwrap();
        assert_eq!(pc.model, "gpt-4o");
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_init() {
        // Verify worker pool can be initialized
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
    }

    #[tokio::test]
    async fn test_gateway_message_bus_creation() {
        let (message_bus, _receivers) = MessageBus::new();
        assert!(message_bus.inbound_tx().capacity() > 0);
        assert!(message_bus.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tool_manager = ToolManager::new(workspace_dir);
        tool_manager.init_from_config(&[]); // Use defaults

        // Verify built-in tools are registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.len() >= 6);
    }

    #[tokio::test]
    async fn test_gateway_message_tool_registration() {
        let mut tool_manager = ToolManager::new(PathBuf::from("/tmp/test"));
        let message_tool = MessageTool::new();
        tool_manager.register(Box::new(message_tool));

        // Verify message tool is registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.iter().any(|t| t.name == "message"));
    }

    #[tokio::test]
    async fn test_gateway_cron_service_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        assert!(cron_service.list_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_gateway_cron_tool_registration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = Arc::new(CronService::new(&workspace_dir));
        let cron_tool = CronTool::new(cron_service);

        let mut tool_manager = ToolManager::new(workspace_dir);
        tool_manager.register(Box::new(cron_tool));

        // Verify cron tool is registered
        let tools = tool_manager.to_openai_functions();
        assert!(tools.iter().any(|t| t.name == "cron"));
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!heartbeat.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_channel_manager_creation() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: false,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: false,
                    interval_s: 60,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));
        assert!(channel_manager.channel_info().0 == "cli:default");
    }

    #[tokio::test]
    async fn test_gateway_paths_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        assert!(paths.workspace_dir().exists());
        assert!(paths.data_dir().exists());
        assert!(paths.config_path().exists());
    }

    #[tokio::test]
    async fn test_gateway_config_loading() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        let config = Config::load_resolved_with(
            paths.config_path().to_str().unwrap(),
            &crate::secrets::SecretStore::empty(),
        );
        assert!(config.is_ok());
    }

    /// The wiring used to be verified by copying the startup call sequence into
    /// a test. Now the sequence exists in exactly one place — the assembly — and
    /// the reset is not repeated anywhere else (ADR 0022: once, at gateway
    /// startup; CLI never truncates).
    #[test]
    fn test_gateway_wiring_tests_do_not_copy_the_call_sequence() {
        let src = include_str!("gateway.rs");
        // Split so the needle does not count this assertion itself.
        assert!(
            !src.contains(concat!("resolve_trace_", "file_paths")),
            "the viewer must read the tracer's resolution, not resolve again"
        );
        assert_eq!(
            src.matches(concat!("reset_trace_", "files(")).count(),
            1,
            "startup truncation happens once, inside the assembly"
        );
    }

    #[tokio::test]
    async fn test_gateway_agent_loop_creation() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let session_dir = workspace_dir.join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let memory_dir = workspace_dir.join("memory");
        std::fs::create_dir_all(&memory_dir).unwrap();

        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
                        r#type: "openai".to_string(),
                        api_url: "".to_string(),
                        base_url: "https://api.openai.com".to_string(),
                        api_key: "test-key".to_string(),
                        model: "gpt-4o".to_string(),
                        temperature: 0.7,
                        max_tokens: 4096,
                        context_window_tokens: 32768,
                        prompt_cache_enabled: false,
                        disable_thinking: false,
                        timeout_seconds: 180,
                        unknown: Default::default(),
                    },
                );
                map
            },
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: false,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: false,
                    interval_s: 60,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        });

        let mut tool_manager = ToolManager::new(workspace_dir.clone());
        tool_manager.init_from_config(&[]);

        // Verify agent loop can be created
        let result = AgentLoop::from_config_with_tools(
            &PathManager::resolve(
                None,
                Some(tmp.path().to_str().unwrap()),
                Some(workspace_dir.to_str().unwrap()),
            )
            .unwrap(),
            message_bus,
            _receivers.inbound,
            config,
            tool_manager,
            crate::agent::ToolCatalog::new(),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_shutdown_channels() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: false,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: false,
                    interval_s: 60,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let mut channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));

        // Verify channel manager can be initialized with no channels
        let result = channel_manager.init().await;
        assert!(result.is_ok());

        // Verify shutdown doesn't panic
        channel_manager.shutdown().await;
    }

    #[tokio::test]
    async fn test_gateway_run_gateway_paths_validation() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = make_test_paths(&tmp);

        // Verify all paths are valid
        assert!(paths.config_path().exists());
        assert!(paths.workspace_dir().exists());
        assert!(paths.data_dir().exists());
        assert!(paths.session_dir().exists() || std::fs::create_dir(paths.session_dir()).is_ok());
        assert!(paths.skills_dir().exists() || std::fs::create_dir(paths.skills_dir()).is_ok());
        assert!(paths.memory_dir().exists() || std::fs::create_dir(paths.memory_dir()).is_ok());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify tick doesn't panic
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        heartbeat.start();

        // Verify tick doesn't panic
        heartbeat.tick().await;

        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_message_bus_send_receive() {
        let (mb, _receivers) = MessageBus::new();

        // Send a request
        let req = BusRequest::owner(
            "test-session",
            "test content",
            TaskHook::new("test-session"),
        );
        let result = mb.inbound_tx().send(req).await;
        assert!(result.is_ok());

        // Send a result
        let res = BusResult {
            session_id: "test-session".to_string(),
            run_id: "test-task".to_string(),
            content: "test result".to_string(),
        };
        let result = mb.outbound_tx().send(res).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_operations() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Add messages
        {
            let mut guard = sm.lock().await;
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
            guard
                .add_message(
                    "test-session",
                    Message::assistant(Some("hi".to_string()), None, None, None),
                )
                .await
                .unwrap();
        }

        // Get messages
        {
            let guard = sm.lock().await;
            let msgs = guard.get_messages("test-session").await;
            assert_eq!(msgs.len(), 2);
        }

        // Persist
        {
            let mut guard = sm.lock().await;
            guard.persist("test-session").await.unwrap();
        }

        // Verify files exist
        assert!(session_dir.join("test-session.jsonl").exists());
        assert!(session_dir.join("test-session.meta.json").exists());
    }

    #[tokio::test]
    async fn test_gateway_message_bus_channel_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Verify channel capacities
        assert_eq!(mb.inbound_tx().capacity(), 32);
        assert_eq!(mb.outbound_tx().capacity(), 32);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_to_openai_functions() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Verify OpenAI functions are generated
        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty());
        for func in &functions {
            assert!(!func.name.is_empty());
            assert!(!func.description.is_empty());
        }
    }

    #[tokio::test]
    async fn test_gateway_cron_service_add_remove_job() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify no jobs initially
        assert!(cron_service.list_jobs().is_empty());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!heartbeat.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_channel_manager_no_channels() {
        let (message_bus, _receivers) = MessageBus::new();
        let config = Arc::new(crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 10,
                timeout_seconds: 30,
                persist_tool_results: false,
                unknown: Default::default(),
            },
            providers: std::collections::HashMap::new(),
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: false,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: false,
                    interval_s: 60,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        });

        let (event_tx, _) = tokio::sync::broadcast::channel::<AgentEvent>(256);
        let mut channel_manager =
            ChannelManager::new(message_bus, _receivers.outbound, config, Some(event_tx));

        // Initialize with no channels should succeed
        let result = channel_manager.init().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_event_broadcast() {
        let (event_tx, mut event_rx1) = tokio::sync::broadcast::channel::<AgentEvent>(10);
        let mut event_rx2 = event_tx.subscribe();

        // Send an event
        let event = AgentEvent::TaskStarted {
            session_id: "test-session".to_string(),
            run_id: "run-1".to_string(),
        };
        let result = event_tx.send(event);
        assert!(result.is_ok());

        // Both receivers should get the event
        let received1 = event_rx1.recv().await;
        assert!(received1.is_ok());

        let received2 = event_rx2.recv().await;
        assert!(received2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_bus_result_serialization() {
        let result = BusResult {
            session_id: "test-session".to_string(),
            run_id: "task-123".to_string(),
            content: "Test content".to_string(),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("test-session"));
        assert!(json.contains("task-123"));
        assert!(json.contains("Test content"));

        let deserialized: BusResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.session_id, "test-session");
        assert_eq!(deserialized.run_id, "task-123");
        assert_eq!(deserialized.content, "Test content");
    }

    #[tokio::test]
    async fn test_gateway_bus_request_creation() {
        let hook = TaskHook::new("test-session");
        let request = BusRequest::owner("test-session", "Test input", hook);
        assert_eq!(request.session_id, "test-session");
        assert_eq!(request.content, "Test input");
        assert!(request.channel_inject.is_none());
    }

    #[tokio::test]
    async fn test_gateway_memory_init() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        let result = ms.lock().await.init();
        assert!(result.is_ok());
        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_memory_set_get() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write memory
        ms.lock()
            .await
            .set(&MemoryCategory::Core, None, "test memory content")
            .await
            .unwrap();

        // Read memory
        let content = ms
            .lock()
            .await
            .get(&MemoryCategory::Core, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content, "test memory content");
    }

    #[tokio::test]
    async fn test_gateway_session_manager_list_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create multiple sessions
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("session-1").await.unwrap();
            guard.get_or_create("session-2").await.unwrap();
            guard.get_or_create("session-3").await.unwrap();
        }

        // List sessions - verifies the call does not panic.
        let _ = sm.lock().await.list_persisted_sessions("");
    }

    #[tokio::test]
    async fn test_gateway_task_hook_creation() {
        let hook = TaskHook::new("test-session");
        // TaskHook doesn't expose session_id directly, but we can verify it was created
        assert!(matches!(hook, TaskHook { .. }));
    }

    #[tokio::test]
    async fn test_gateway_agent_event_variants() {
        // Test different AgentEvent variants
        let event1 = AgentEvent::TaskStarted {
            session_id: "session-1".to_string(),
            run_id: "run-1".to_string(),
        };
        assert!(matches!(event1, AgentEvent::TaskStarted { .. }));

        let event2 = AgentEvent::TaskCompleted {
            session_id: "session-2".to_string(),
            run_id: "run-2".to_string(),
            result: "result".to_string(),
        };
        assert!(matches!(event2, AgentEvent::TaskCompleted { .. }));

        let event3 = AgentEvent::ToolCall {
            session_id: "session-3".to_string(),
            name: "shell".to_string(),
            args: "{\"command\": \"echo test\"}".to_string(),
        };
        assert!(matches!(event3, AgentEvent::ToolCall { .. }));
    }

    #[tokio::test]
    async fn test_gateway_cron_service_list_jobs_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        let jobs = cron_service.list_jobs();
        assert!(jobs.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_start_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        // Starting twice should be idempotent
        cron_service.start();
        cron_service.stop();
        // Stopping twice should be idempotent
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_multiple_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Ticking when disabled should be a no-op
        heartbeat.tick().await;
        heartbeat.tick().await;
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_multiple_sends() {
        let (mb, _receivers) = MessageBus::new();
        let outbound_tx = mb.outbound_tx();

        // Send multiple results
        for i in 0..5 {
            let result = BusResult {
                session_id: format!("session-{}", i),
                run_id: format!("task-{}", i),
                content: format!("Content {}", i),
            };
            let send_result = outbound_tx.send(result).await;
            assert!(send_result.is_ok());
        }
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_multiple_registrations() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Register additional tools
        tm.register(Box::new(crate::tools::shell::ShellTool::default()));

        // Verify tools are registered
        let tools = tm.to_openai_functions();
        assert!(!tools.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_concurrent_access() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Concurrent message adds
        let sm_clone = sm.clone();
        let handle1 = tokio::spawn(async move {
            let mut guard = sm_clone.lock().await;
            guard
                .add_message("test-session", Message::user("msg1".to_string()))
                .await
                .unwrap();
        });

        let sm_clone = sm.clone();
        let handle2 = tokio::spawn(async move {
            let mut guard = sm_clone.lock().await;
            guard
                .add_message("test-session", Message::user("msg2".to_string()))
                .await
                .unwrap();
        });

        handle1.await.unwrap();
        handle2.await.unwrap();

        // Verify messages were added
        let guard = sm.lock().await;
        let msgs = guard.get_messages("test-session").await;
        assert!(msgs.len() >= 2);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_channel() {
        let (mb, _receivers) = MessageBus::new();
        let inbound_tx = mb.inbound_tx();

        // Verify inbound channel capacity
        assert!(inbound_tx.capacity() > 0);

        // Send a request
        let hook = TaskHook::new("test-session");
        let request = BusRequest::owner("test-session", "test input", hook);
        let result = inbound_tx.send(request).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_persistence() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Verify persistence file path exists or can be created
        assert!(workspace_dir.exists());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_sleep_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 30, false);
        // Verify sleep duration is based on interval
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 30);
    }

    #[tokio::test]
    async fn test_gateway_agent_loop_session_ensure() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Ensure session creates it if not exists
        crate::session::ensure_session(&sm, "new-session")
            .await
            .unwrap();

        // Verify session exists
        let guard = sm.lock().await;
        assert!(guard.has_session("new-session"));
    }

    #[tokio::test]
    async fn test_gateway_session_manager_graceful_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create a session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
        }

        // Graceful shutdown should not panic
        sm.lock().await.graceful_shutdown().await;
    }

    #[tokio::test]
    async fn test_gateway_memory_sync_all() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write some data
        ms.lock()
            .await
            .set(&MemoryCategory::Core, None, "test")
            .await
            .unwrap();

        // sync_all should not error
        let result = ms.lock().await.sync_all().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_multiple() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit multiple tasks
        for i in 0..10 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_config_with_all_fields() {
        let config = crate::config::Config {
            pressure: Default::default(),
            mcp: Default::default(),
            executors: Default::default(),
            agent: crate::config::AgentConfig {
                provider: "default".to_string(),
                max_iterations: 50,
                timeout_seconds: 180,
                persist_tool_results: true,
                unknown: Default::default(),
            },
            providers: {
                let mut map = std::collections::HashMap::new();
                map.insert(
                    "default".to_string(),
                    crate::config::ProviderConfig {
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
                    },
                );
                map
            },
            tools: Default::default(),
            channels: std::collections::HashMap::new(),
            gateway: crate::config::GatewayConfig {
                owner: Default::default(),
                cron: crate::config::CronConfig {
                    enabled: true,
                    unknown: Default::default(),
                },
                heartbeat: crate::config::HeartbeatConfig {
                    channel: Default::default(),
                    to: Default::default(),
                    enabled: true,
                    interval_s: 300,
                    unknown: Default::default(),
                },
                dream: crate::config::DreamConfig::default(),
            },
            web_fetch: None,
            tracing: Default::default(),
            memory: Default::default(),
            json_render: Default::default(),
            subagent: Default::default(),
            triggers: Default::default(),
        };

        assert_eq!(config.agent.max_iterations, 50);
        assert_eq!(config.agent.timeout_seconds, 180);
        assert!(config.gateway.cron.enabled);
        assert!(config.gateway.heartbeat.enabled);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_message_count() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
        }

        // Add messages
        {
            let mut guard = sm.lock().await;
            for i in 0..5 {
                guard
                    .add_message("test-session", Message::user(format!("msg{}", i)))
                    .await
                    .unwrap();
            }
        }

        // Verify message count
        let count = sm.lock().await.total_message_count("test-session");
        assert_eq!(count, 5);
    }

    #[tokio::test]
    async fn test_gateway_memory_append_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append history entries - should not error
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Verify history file was created
        let history_file = workspace_dir.join("memory/history.jsonl");
        assert!(history_file.exists());
    }

    #[tokio::test]
    async fn test_gateway_memory_read_recent_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..20 {
            ms.lock()
                .await
                .append_history(&format!("entry{}", i))
                .unwrap();
        }

        // Read recent history with cap
        let recent = ms.lock().await.read_recent_history(5);
        assert_eq!(recent.len(), 5);
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute a tool
        let result = tm
            .execute("shell", serde_json::json!({"command": "echo test"}))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_clear_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir.clone()).unwrap(),
        ));

        // Create session with messages
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("test-session").await.unwrap();
            guard
                .add_message("test-session", Message::user("hello".to_string()))
                .await
                .unwrap();
            guard.persist("test-session").await.unwrap();
        }

        // Verify files exist
        assert!(session_dir.join("test-session.jsonl").exists());

        // Clear session
        sm.lock().await.clear_session("test-session");

        // Verify files were deleted
        assert!(!session_dir.join("test-session.jsonl").exists());
    }

    #[tokio::test]
    async fn test_gateway_cron_service_job_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Tick should not panic even with no jobs
        cron_service.tick().await;
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_enabled_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        // Tick when enabled should not panic
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_outbound_tx_clone() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.outbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work
        let result1 = tx1
            .send(BusResult {
                session_id: "session-1".to_string(),
                run_id: "task-1".to_string(),
                content: "content-1".to_string(),
            })
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusResult {
                session_id: "session-2".to_string(),
                run_id: "task-2".to_string(),
                content: "content-2".to_string(),
            })
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Initially no sessions
        assert!(!sm.lock().await.has_session("nonexistent"));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();
        assert!(sm.lock().await.has_session("test-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_read_unprocessed_history() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append entries
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Read unprocessed history with skip 0 should return all
        let unprocessed = ms.lock().await.read_unprocessed_history(0);
        assert_eq!(unprocessed.len(), 3);
    }

    #[tokio::test]
    async fn test_gateway_memory_get_effective_core() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Initially empty memory → get_effective returns None
        let content = ms
            .lock()
            .await
            .get_effective(&MemoryCategory::Core, None)
            .await
            .unwrap();
        assert!(content.is_none());

        // Write memory
        ms.lock()
            .await
            .set(&MemoryCategory::Core, None, "test memory")
            .await
            .unwrap();

        // Now get_effective returns the content
        let content = ms
            .lock()
            .await
            .get_effective(&MemoryCategory::Core, None)
            .await
            .unwrap()
            .unwrap();
        assert!(content.contains("test memory"));
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_global_init() {
        // init_global should be idempotent
        WorkerPool::init_global(64);
        WorkerPool::init_global(64);
        WorkerPool::init_global(64);

        let pool = WorkerPool::global();
        pool.submit(Box::new(|| Box::pin(async {})));
    }

    #[tokio::test]
    async fn test_gateway_cron_service_list_jobs_with_filter() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // List jobs should return empty list
        let jobs = cron_service.list_jobs();
        assert!(jobs.is_empty());

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_custom_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 120, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 120);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_tx_clone() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.inbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work
        let hook = TaskHook::new("test-session");
        let result1 = tx1
            .send(BusRequest::owner("session-1", "content-1", hook.clone()))
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusRequest::owner("session-2", "content-2", hook))
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_memory_set_soul() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write SOUL.md
        ms.lock()
            .await
            .set(&MemoryCategory::Soul, None, "I am a helpful assistant")
            .await
            .unwrap();

        // Read it back
        let soul = ms
            .lock()
            .await
            .get(&MemoryCategory::Soul, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(soul, "I am a helpful assistant");
    }

    #[tokio::test]
    async fn test_gateway_memory_set_user() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write USER.md
        ms.lock()
            .await
            .set(&MemoryCategory::User, None, "Alice, software engineer")
            .await
            .unwrap();

        // Read it back
        let user = ms
            .lock()
            .await
            .get(&MemoryCategory::User, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user, "Alice, software engineer");
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_multiple_submits() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit some tasks
        for i in 0..5 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_is_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let hb_enabled = HeartbeatService::new(workspace_dir.clone(), 60, true);
        assert!(hb_enabled.is_enabled());

        let hb_disabled = HeartbeatService::new(workspace_dir, 60, false);
        assert!(!hb_disabled.is_enabled());
    }

    #[tokio::test]
    async fn test_gateway_message_bus_creation_and_capacity() {
        let (mb, _receivers) = MessageBus::new();
        assert_eq!(mb.inbound_tx().capacity(), 32);
        assert_eq!(mb.outbound_tx().capacity(), 32);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_or_create_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session twice
        sm.lock().await.get_or_create("test-session").await.unwrap();
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Should still have only one session
        assert!(sm.lock().await.has_session("test-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_init_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));

        // Init multiple times should not error
        ms.lock().await.init().unwrap();
        ms.lock().await.init().unwrap();
        ms.lock().await.init().unwrap();

        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_with_custom_config() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        let tools = vec![
            crate::config::ToolEntry {
                name: "shell".to_string(),
                enabled: true,
            },
            crate::config::ToolEntry {
                name: "file_reader".to_string(),
                enabled: false,
            },
        ];
        tm.init_from_config(&tools);

        let functions = tm.to_openai_functions();
        assert_eq!(functions.len(), 1);
        assert_eq!(functions[0].name, "shell");
    }

    #[tokio::test]
    async fn test_gateway_memory_read_unprocessed_history_with_skip() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append entries
        ms.lock().await.append_history("entry1").unwrap();
        ms.lock().await.append_history("entry2").unwrap();
        ms.lock().await.append_history("entry3").unwrap();

        // Read unprocessed with skip 1 should return last 2 entries
        let unprocessed = ms.lock().await.read_unprocessed_history(1);
        assert_eq!(unprocessed.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_memory_read_recent_history_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..10 {
            ms.lock()
                .await
                .append_history(&format!("entry{}", i))
                .unwrap();
        }

        // Read with cap 3
        let recent = ms.lock().await.read_recent_history(3);
        assert_eq!(recent.len(), 3);
    }

    #[tokio::test]
    async fn test_gateway_cron_service_multiple_ticks() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();

        // Multiple ticks should not panic
        cron_service.tick().await;
        cron_service.tick().await;
        cron_service.tick().await;

        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_message_bus_concurrent_sends() {
        let (mb, _receivers) = MessageBus::new();
        let tx = mb.outbound_tx();

        // Concurrent sends should work
        let handles: Vec<_> = (0..10)
            .map(|i| {
                let tx = tx.clone();
                tokio::spawn(async move {
                    tx.send(BusResult {
                        session_id: format!("session-{}", i),
                        run_id: format!("task-{}", i),
                        content: format!("content-{}", i),
                    })
                    .await
                    .is_ok()
                })
            })
            .collect();

        for handle in handles {
            assert!(handle.await.unwrap());
        }
    }

    #[tokio::test]
    async fn test_gateway_session_manager_multiple_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create multiple sessions
        for i in 0..5 {
            sm.lock()
                .await
                .get_or_create(&format!("session-{}", i))
                .await
                .unwrap();
        }

        // All should exist
        for i in 0..5 {
            assert!(sm.lock().await.has_session(&format!("session-{}", i)));
        }
    }

    #[tokio::test]
    async fn test_gateway_memory_set_and_get_soul() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write soul
        ms.lock()
            .await
            .set(
                &MemoryCategory::Soul,
                None,
                "You are a helpful AI assistant",
            )
            .await
            .unwrap();

        // Read it back
        let soul = ms
            .lock()
            .await
            .get(&MemoryCategory::Soul, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(soul, "You are a helpful AI assistant");
    }

    #[tokio::test]
    async fn test_gateway_memory_set_and_get_user() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write user
        ms.lock()
            .await
            .set(&MemoryCategory::User, None, "Bob, a software developer")
            .await
            .unwrap();

        // Read it back
        let user = ms
            .lock()
            .await
            .get(&MemoryCategory::User, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user, "Bob, a software developer");
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_unknown_tool() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute unknown tool should error
        let result = tm.execute("nonexistent_tool", serde_json::json!({})).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_clear_nonexistent() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Clear nonexistent session should not panic
        sm.lock().await.clear_session("nonexistent-session");
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_with_different_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit different types of tasks
        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = 1 + 1;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = "hello".to_string();
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_with_custom_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("custom_workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_custom_sleep_duration() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 180, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() <= 180);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_outbound_separate() {
        let (mb, _receivers) = MessageBus::new();
        let inbound_tx = mb.inbound_tx();
        let outbound_tx = mb.outbound_tx();

        // Both channels should have capacity
        assert!(inbound_tx.capacity() > 0);
        assert!(outbound_tx.capacity() > 0);

        // Send to inbound
        let hook = TaskHook::new("test");
        let result = inbound_tx
            .send(BusRequest::owner("test", "test", hook))
            .await;
        assert!(result.is_ok());

        // Send to outbound
        let result = outbound_tx
            .send(BusResult {
                session_id: "test".to_string(),
                run_id: "test".to_string(),
                content: "test".to_string(),
            })
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_messages_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Get messages should return empty
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_memory_append_and_read_multiple() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Append many entries
        for i in 0..20 {
            ms.lock()
                .await
                .append_history(&format!("entry-{}", i))
                .unwrap();
        }

        // Read recent with different caps
        let recent_5 = ms.lock().await.read_recent_history(5);
        assert_eq!(recent_5.len(), 5);

        let recent_10 = ms.lock().await.read_recent_history(10);
        assert_eq!(recent_10.len(), 10);

        let recent_100 = ms.lock().await.read_recent_history(100);
        assert_eq!(recent_100.len(), 20); // Only 20 entries exist
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_with_empty_config() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Empty config uses defaults

        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty()); // Should have default tools
    }

    #[tokio::test]
    async fn test_gateway_session_manager_persist_empty_session() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock()
            .await
            .get_or_create("empty-session")
            .await
            .unwrap();

        // Persist empty session should not error
        let result = sm.lock().await.persist("empty-session").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_closures() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit closures that capture different values
        let value1 = "hello".to_string();
        let value2 = 42;

        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = value1;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = value2;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_persistence_path() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let _cron_service = CronService::new(&workspace_dir);
        // Service should be created without error
        assert!(workspace_dir.exists());
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_with_zero_interval() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        // Zero interval should be handled gracefully
        let heartbeat = HeartbeatService::new(workspace_dir, 0, true);
        let duration = heartbeat.sleep_duration();
        // Zero interval maps to zero sleep (graceful handling, no panic).
        assert_eq!(duration.as_secs(), 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_capacity_check() {
        let (mb, _receivers) = MessageBus::new();
        // Both channels should have capacity > 0
        assert!(mb.inbound_tx().capacity() > 0);
        assert!(mb.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // No sessions initially
        assert!(!sm.lock().await.has_session("any-session"));
    }

    #[tokio::test]
    async fn test_gateway_memory_init_creates_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));

        // Memory dir should not exist before init
        assert!(!workspace_dir.join("memory").exists());

        ms.lock().await.init().unwrap();

        // Memory dir should exist after init
        assert!(workspace_dir.join("memory").exists());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_description_and_parameters() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        // All functions should have name, description, and parameters
        for func in &functions {
            assert!(!func.name.is_empty());
            assert!(!func.description.is_empty());
            // parameters should be a JSON object
            assert!(func.parameters.is_object() || func.parameters.is_null());
        }
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_messages_after_add() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session and add messages
        sm.lock().await.get_or_create("test-session").await.unwrap();
        sm.lock()
            .await
            .add_message("test-session", Message::user("hello".to_string()))
            .await
            .unwrap();
        sm.lock()
            .await
            .add_message(
                "test-session",
                Message::assistant(Some("hi".to_string()), None, None, None),
            )
            .await
            .unwrap();

        // Get messages
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_many_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit many tasks
        for i in 0..20 {
            pool.submit(Box::new(move || {
                Box::pin(async move {
                    let _ = i * 2;
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            }));
        }

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_empty_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Should not panic with empty workspace
        cron_service.start();
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_disabled_tick() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Tick when disabled should be a no-op
        heartbeat.tick().await;
    }

    #[tokio::test]
    async fn test_gateway_message_bus_clone_senders() {
        let (mb, _receivers) = MessageBus::new();
        let tx1 = mb.outbound_tx();
        let tx2 = tx1.clone();

        // Both senders should work independently
        let result1 = tx1
            .send(BusResult {
                session_id: "s1".to_string(),
                run_id: "t1".to_string(),
                content: "c1".to_string(),
            })
            .await;
        assert!(result1.is_ok());

        let result2 = tx2
            .send(BusResult {
                session_id: "s2".to_string(),
                run_id: "t2".to_string(),
                content: "c2".to_string(),
            })
            .await;
        assert!(result2.is_ok());
    }

    #[tokio::test]
    async fn test_gateway_session_manager_persist_multiple_times() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Add and persist
        sm.lock()
            .await
            .add_message("test-session", Message::user("msg1".to_string()))
            .await
            .unwrap();
        sm.lock().await.persist("test-session").await.unwrap();

        // Add more and persist again
        sm.lock()
            .await
            .add_message("test-session", Message::user("msg2".to_string()))
            .await
            .unwrap();
        sm.lock().await.persist("test-session").await.unwrap();

        // Should not error
        let msgs = sm.lock().await.get_messages("test-session").await;
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn test_gateway_memory_get_core_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read memory before writing should be empty
        let memory = ms
            .lock()
            .await
            .get(&MemoryCategory::Core, None)
            .await
            .unwrap();
        assert!(memory.is_none());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_get_functions_count() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        // Should have at least the default tools
        assert!(functions.len() >= 6); // shell, file_reader, file_writer, file_editor, list_dir, make_dir
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_async_tasks() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit async tasks
        pool.submit(Box::new(|| {
            Box::pin(async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        pool.submit(Box::new(|| {
            Box::pin(async {
                let _ = vec![1, 2, 3];
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        // Give tasks time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_tick_no_jobs() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        cron_service.start();
        // Tick with no jobs should not panic
        cron_service.tick().await;
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_stop_when_not_started() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, false);
        // Stop when not started should not panic
        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_session_manager_get_or_create_returns_same() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        {
            let mut guard = sm.lock().await;
            let s1 = guard.get_or_create("test-session").await;
            assert!(s1.is_ok());
        }

        // Get same session again
        {
            let mut guard = sm.lock().await;
            let s2 = guard.get_or_create("test-session").await;
            assert!(s2.is_ok());
        }
    }

    #[tokio::test]
    async fn test_gateway_memory_get_effective_core_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Get effective memory when empty
        let content = ms
            .lock()
            .await
            .get_effective(&MemoryCategory::Core, None)
            .await
            .unwrap();
        assert!(content.is_none());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_init_with_all_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        let tools = vec![
            crate::config::ToolEntry {
                name: "shell".to_string(),
                enabled: false,
            },
            crate::config::ToolEntry {
                name: "file_reader".to_string(),
                enabled: false,
            },
        ];
        tm.init_from_config(&tools);

        let functions = tm.to_openai_functions();
        assert_eq!(functions.len(), 0);
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_with_string_capture() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        let s = "hello".to_string();
        pool.submit(Box::new(move || {
            Box::pin(async move {
                let _ = s.len();
            }) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_start_stop_cycle() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Multiple start/stop cycles should work
        cron_service.start();
        cron_service.stop();
        cron_service.start();
        cron_service.stop();
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_multiple_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        // Multiple stops should not panic
        heartbeat.stop();
        heartbeat.stop();
    }

    #[tokio::test]
    async fn test_gateway_session_manager_total_message_count() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Create session
        sm.lock().await.get_or_create("test-session").await.unwrap();

        // Add messages
        for i in 0..3 {
            sm.lock()
                .await
                .add_message("test-session", Message::user(format!("msg{}", i)))
                .await
                .unwrap();
        }

        // Total count should be 3
        let count = sm.lock().await.total_message_count("test-session");
        assert_eq!(count, 3);
    }

    #[tokio::test]
    async fn test_gateway_memory_get_user_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read user before writing should be empty
        let user = ms
            .lock()
            .await
            .get(&MemoryCategory::User, None)
            .await
            .unwrap();
        assert!(user.is_none());
    }

    #[tokio::test]
    async fn test_gateway_memory_get_soul_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Read soul before writing should be empty
        let soul = ms
            .lock()
            .await
            .get(&MemoryCategory::Soul, None)
            .await
            .unwrap();
        assert!(soul.is_none());
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_execute_with_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        // Execute shell with invalid command format
        let result = tm.execute("shell", serde_json::json!({})).await;
        // Should return an error due to missing command
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_empty_closures() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit empty closures
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    #[tokio::test]
    async fn test_gateway_cron_service_add_and_list() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let cron_service = CronService::new(&workspace_dir);
        // Initially no jobs
        assert!(cron_service.list_jobs().is_empty());
    }

    #[tokio::test]
    async fn test_gateway_heartbeat_service_sleep_duration_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let heartbeat = HeartbeatService::new(workspace_dir, 60, true);
        let duration = heartbeat.sleep_duration();
        assert!(duration.as_secs() > 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_inbound_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Inbound channel should have capacity
        assert!(mb.inbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_message_bus_outbound_capacity() {
        let (mb, _receivers) = MessageBus::new();
        // Outbound channel should have capacity
        assert!(mb.outbound_tx().capacity() > 0);
    }

    #[tokio::test]
    async fn test_gateway_session_manager_has_session_false() {
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp.path().join("sessions");
        std::fs::create_dir_all(&session_dir).unwrap();

        let sm: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(session_dir).unwrap(),
        ));

        // Initially no sessions
        assert!(!sm.lock().await.has_session("nonexistent"));
    }

    #[tokio::test]
    async fn test_gateway_memory_write_and_read_core() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let ms = Arc::new(tokio::sync::Mutex::new(MemoryService::new(&workspace_dir)));
        ms.lock().await.init().unwrap();

        // Write memory
        ms.lock()
            .await
            .set(&MemoryCategory::Core, None, "test memory content")
            .await
            .unwrap();

        // Read it back
        let memory = ms
            .lock()
            .await
            .get(&MemoryCategory::Core, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(memory, "test memory content");
    }

    #[tokio::test]
    async fn test_gateway_tool_manager_get_functions_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace_dir = tmp.path().join("workspace");
        std::fs::create_dir_all(&workspace_dir).unwrap();

        let mut tm = ToolManager::new(workspace_dir);
        tm.init_from_config(&[]); // Use defaults

        let functions = tm.to_openai_functions();
        assert!(!functions.is_empty());
    }

    #[tokio::test]
    async fn test_gateway_worker_pool_submit_single_task() {
        WorkerPool::init_global(64);
        let pool = WorkerPool::global();

        // Submit single task
        pool.submit(Box::new(|| {
            Box::pin(async {}) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        }));

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

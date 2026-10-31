//! The trigger layer: the tick that drives data sources and rules.
//!
//! Triggers are a layer *above* cron, not a superset of it: the tick is an
//! independent fixed interval, nothing is written into `jobs.json`, and no
//! existing cron job is migrated. That separation is what keeps "every n ticks"
//! meaningful — a cron loop that sleeps between rounds accumulates its own
//! runtime and drifts, while a tick counter does not.
//!
//! One tick is: reload definitions → freeze the snapshots → collect what is due
//! → judge what is due against the frozen frame → hand the fires to the dispatch
//! queue. Acting on a fire (running an agent, telling a user) is deliberately
//! *not* in the tick.

// Parts of this layer are reached only from the tests written inside it: the
// synchronous `drain`, the ledger reader, health probes and the layer accessors
// a caller is expected to use when it needs them. A build without tests cannot
// see those call sites, so dead_code would fire on code that is very much live.
#![cfg_attr(not(test), allow(dead_code))]

pub mod dispatch;
pub mod source;
pub mod subscription;
pub mod tool;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use crate::config::TriggersConfig;
use crate::debug;
use crate::message_bus::MessageBus;
use crate::trace::{SpanRecorder, SpanStatus, Tracer};

use dispatch::Dispatcher;
use source::{SourceLayer, TickReport};
use subscription::{Alert, Fire, SubscriptionLayer};

/// What one tick produced, end to end.
#[derive(Debug, Default)]
pub struct TickOutcome {
    pub sources: TickReport,
    pub judged: usize,
    pub fired: Vec<Fire>,
    pub alerts: Vec<Alert>,
    /// Work items that made it into the dispatch queue.
    pub queued: usize,
}

impl TickOutcome {
    /// Nothing to do and nothing happened: no span, no history line.
    pub fn is_idle(&self) -> bool {
        self.sources.is_idle() && self.judged == 0 && self.fired.is_empty()
    }
}

/// Owns the tick loop and everything one tick needs.
pub struct TriggerService {
    sources: Arc<SourceLayer>,
    rules: Arc<SubscriptionLayer>,
    secrets: Arc<BTreeMap<String, String>>,
    dispatch: Arc<Dispatcher>,
    enabled: bool,
    tick_interval: Duration,
    tick_timeout: Duration,
    max_concurrent: usize,
    tracer: Tracer,
}

impl TriggerService {
    /// Build the service from config: one source layer, one rule layer, one
    /// secret map, shared with the tools that read them.
    ///
    /// The dispatcher has no subagent runtime and its bus has no consumers, so
    /// fires get booked and nothing reaches a conversation. That is the shape a
    /// tick-only test wants; a real process builds the dispatcher itself and
    /// uses [`Self::from_parts`].
    pub fn new(config: &TriggersConfig, workspace_dir: &Path, tracer: Tracer) -> Arc<Self> {
        let (sources, rules, secrets) = build_layers(config, workspace_dir);
        let (bus, _idle_consumers) = MessageBus::new();
        let dispatch = Dispatcher::new(
            None,
            bus,
            history_path(workspace_dir),
            config.history_lines,
            config.max_queue,
            config.max_merge,
            tracer.clone(),
        );
        Self::assemble(sources, rules, secrets, dispatch, config, tracer)
    }

    /// Build the service around existing layers and an existing dispatcher.
    ///
    /// The wiring layer needs the layers *before* the tick starts running — they
    /// are what the `trigger_source` and `trigger` tools are built from — and it
    /// needs the dispatcher to share the subagent runtime and the message bus
    /// with the rest of the process. One layer, one snapshot, one queue.
    pub fn from_parts(
        sources: Arc<SourceLayer>,
        rules: Arc<SubscriptionLayer>,
        secrets: Arc<BTreeMap<String, String>>,
        dispatch: Arc<Dispatcher>,
        config: &TriggersConfig,
        tracer: Tracer,
    ) -> Arc<Self> {
        Self::assemble(sources, rules, secrets, dispatch, config, tracer)
    }

    fn assemble(
        sources: Arc<SourceLayer>,
        rules: Arc<SubscriptionLayer>,
        secrets: Arc<BTreeMap<String, String>>,
        dispatch: Arc<Dispatcher>,
        config: &TriggersConfig,
        tracer: Tracer,
    ) -> Arc<Self> {
        Arc::new(Self {
            sources,
            rules,
            secrets,
            dispatch,
            enabled: config.enabled,
            tick_interval: Duration::from_secs(config.tick_interval_s.max(1)),
            tick_timeout: Duration::from_secs(config.tick_timeout_s.max(1)),
            max_concurrent: config.max_concurrent_collect.max(1),
            tracer,
        })
    }

    /// The dispatch queue: the consumer side belongs outside the tick.
    pub fn dispatch(&self) -> Arc<Dispatcher> {
        self.dispatch.clone()
    }

    pub fn secrets(&self) -> Arc<BTreeMap<String, String>> {
        self.secrets.clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    /// One tick: reload, collect, judge, book the fires.
    ///
    /// A tick with nothing due produces no span and no work — a 60-second
    /// cadence must not fill the trace file with empty rounds.
    pub async fn tick(&self) -> TickOutcome {
        if !self.enabled {
            return TickOutcome::default();
        }
        self.sources.reload();
        self.rules.reload();
        let index = self.sources.begin_tick();
        let due_sources = self.sources.due(index);
        let due_rules = self.rules.due(index);
        if due_sources.is_empty() && due_rules.is_empty() {
            debug!("[trigger] tick {index} has nothing due");
            let sources = self.sources.collect(index, self.max_concurrent).await;
            return TickOutcome {
                sources,
                ..Default::default()
            };
        }

        let span = self.tracer.start_span("trigger.tick", None);
        let started = std::time::Instant::now();
        let outcome = match tokio::time::timeout(self.tick_timeout, self.round(index)).await {
            Ok(outcome) => outcome,
            Err(_) => {
                span.set_status(SpanStatus::Error("tick exceeded its budget".to_string()));
                crate::warn_log!(
                    "[trigger] tick {index} exceeded its {}s budget",
                    self.tick_timeout.as_secs()
                );
                TickOutcome::default()
            }
        };

        span.set_attribute("tick_index", json!(index));
        span.set_attribute("sources_due", json!(due_sources.len()));
        span.set_attribute("sources_ok", json!(outcome.sources.succeeded.len()));
        span.set_attribute("sources_failed", json!(outcome.sources.failed.len()));
        span.set_attribute("triggers_due", json!(due_rules.len()));
        span.set_attribute("triggers_judged", json!(outcome.judged));
        span.set_attribute("fired", json!(outcome.fired.len()));
        span.set_attribute("elapsed_ms", json!(started.elapsed().as_millis() as u64));

        if !outcome.fired.is_empty() || !outcome.sources.failed.is_empty() {
            crate::info!(
                "[trigger] tick {index}: {} source(s) collected, {} rule(s) judged, {} fired",
                outcome.sources.succeeded.len(),
                outcome.judged,
                outcome.fired.len()
            );
        }
        outcome
    }

    /// The body of a tick, so the whole round can sit behind one deadline.
    ///
    /// Ending in the queue rather than in a delivery is the point: the deadline
    /// may cut collection and judgment, but never an agent run that outlives it.
    async fn round(&self, index: u64) -> TickOutcome {
        let sources = self.sources.collect(index, self.max_concurrent).await;
        // Judgment reads the frame frozen *before* this tick collected, so no
        // rule can see a half-committed world.
        let report = self.rules.judge(index, &sources.frame, &self.secrets).await;
        for warning in &report.warnings {
            crate::warn_log!("[trigger] tick {index}: {warning}");
        }
        let fired = report.fired;
        // An alert that could never reach a reader is not queued at all: the
        // failure latch is already set, and a signal the queue would drop is
        // better dropped here, where the reason is the rule's own silence.
        let alerts: Vec<Alert> = report
            .alerts
            .into_iter()
            .filter(|alert| self.dispatch.is_alertable(alert))
            .collect();
        let queued = self.dispatch.enqueue_fires(fired.clone())
            + self.dispatch.enqueue_alerts(alerts.clone());
        TickOutcome {
            sources,
            judged: report.judged,
            fired,
            alerts,
            queued,
        }
    }

    /// Drive the tick on its own fixed interval. `interval` (not a sleep chain)
    /// keeps the cadence from drifting and never blocks cron or heartbeat.
    pub async fn run(self: Arc<Self>) {
        if !self.enabled {
            debug!("[trigger] trigger tick disabled");
            return;
        }
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval` yields immediately once; consume it so the first real
        // tick lands one interval after startup.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            self.tick().await;
        }
    }
}

/// Where the fire ledger lives: next to the rule directories, one file.
pub fn history_path(workspace_dir: &Path) -> PathBuf {
    workspace_dir.join("triggers").join(dispatch::HISTORY_FILE)
}

/// Build both layers plus the secret map they share. The wiring layer hands the
/// layers to the tools and the service, so a tick and an agent always read the
/// same snapshot.
pub fn build_layers(
    config: &TriggersConfig,
    workspace_dir: &Path,
) -> (
    Arc<SourceLayer>,
    Arc<SubscriptionLayer>,
    Arc<BTreeMap<String, String>>,
) {
    let secrets: BTreeMap<String, String> = secret_map(config);
    let sources = Arc::new(SourceLayer::new(
        workspace_dir.join("triggers").join("sources"),
        workspace_dir.to_path_buf(),
        Arc::new(secrets.clone()),
        Duration::from_secs(config.tick_interval_s.max(1)),
        source::DEFAULT_TTL,
    ));
    let rules = SubscriptionLayer::new(workspace_dir.join("triggers").join("triggers"));
    (sources, rules, Arc::new(secrets))
}

fn secret_map(config: &TriggersConfig) -> BTreeMap<String, String> {
    config
        .secrets
        .iter()
        .filter(|entry| !entry.name.trim().is_empty())
        .map(|entry| (entry.name.clone(), entry.value.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::TriggersConfig;
    use crate::message_bus::MessageBusReceivers;
    use crate::trigger::source::meta as source_file;
    use crate::trigger::subscription::meta::{ActionKind, TriggerAction, TriggerMeta};

    fn config(enabled: bool) -> TriggersConfig {
        TriggersConfig {
            enabled,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_disabled_service_does_no_work() {
        let tmp = tempfile::tempdir().unwrap();
        let service = TriggerService::new(&config(false), tmp.path(), Tracer::new(vec![]));
        let outcome = service.tick().await;
        assert!(outcome.is_idle());
        assert_eq!(outcome.sources.tick_index, 0);
    }

    #[tokio::test]
    async fn test_empty_tick_advances_the_counter_without_spans() {
        let tmp = tempfile::tempdir().unwrap();
        let service = TriggerService::new(&config(true), tmp.path(), Tracer::new(vec![]));
        let first = service.tick().await;
        let second = service.tick().await;
        assert_eq!(first.sources.tick_index, 1);
        assert_eq!(second.sources.tick_index, 2);
        assert!(first.is_idle());
    }

    #[tokio::test]
    async fn test_tick_interval_comes_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(true);
        cfg.tick_interval_s = 5;
        let service = TriggerService::new(&cfg, tmp.path(), Tracer::new(vec![]));
        assert_eq!(service.tick_interval(), Duration::from_secs(5));
    }

    #[tokio::test]
    async fn test_secrets_are_read_from_config() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = config(true);
        cfg.secrets = vec![crate::config::SecretEntry {
            name: "token".to_string(),
            value: "abc123".to_string(),
            ..Default::default()
        }];
        let service = TriggerService::new(&cfg, tmp.path(), Tracer::new(vec![]));
        // The secret reaches the layer the tool hands to scripts.
        assert_eq!(
            service.secrets().get("token").map(String::as_str),
            Some("abc123")
        );
    }

    #[tokio::test]
    async fn test_history_is_only_written_when_something_fires() {
        let h = Harness::new();
        h.service.tick().await;
        h.service.tick().await;
        assert!(!history_path(h.workspace()).exists());
        assert_eq!(h.service.dispatch().drain().await, 0);
    }

    #[tokio::test]
    async fn test_a_tick_queues_its_fires_and_the_consumer_delivers_one_notice() {
        // The whole chain, driven the way the gateway drives it: the tick stops
        // at the queue, and the consumer is what talks to a conversation.
        let mut h = Harness::new();
        h.source("src-0001");
        let mut meta = TriggerMeta::new("trg-a", "daily todo");
        meta.sources = vec!["src-0001".to_string()];
        meta.origin = Some(crate::agent::Origin::new("webui", "chat-1"));
        meta.action = TriggerAction {
            kind: ActionKind::Notify,
            text_template: Some("todo: {{detail}} {{evidence}}".to_string()),
            ..Default::default()
        };
        h.rule(
            &meta,
            "function judge() { return {fire: true, detail: 'fire', \
                        evidence: [{source_id: 'src-0001', text: 'two open'}]}; }",
        );

        // First tick samples the source; judgment reads the frame frozen before
        // that, so the rule is not ready and must stay quiet.
        let cold = h.service.tick().await;
        assert!(cold.fired.is_empty(), "{cold:?}");
        let warm = h.service.tick().await;
        assert_eq!(warm.fired.len(), 1);
        assert_eq!(warm.queued, 1, "the fire must reach the queue");
        assert!(
            warm.alerts.is_empty(),
            "a rule that judged cleanly has nothing to complain about"
        );

        assert_eq!(h.service.dispatch().drain().await, 1);
        let inbound = h.consumers.inbound.recv().await.expect("one notice");
        assert_eq!(inbound.session_id, "webui:chat-1");
        assert!(
            inbound.content.contains("todo: fire"),
            "the template must be filled: {}",
            inbound.content
        );
        assert!(
            inbound.content.contains("[src-0001 @"),
            "evidence lines carry their source: {}",
            inbound.content
        );
        assert!(
            inbound.content.contains("not a message from the user"),
            "the main loop must not read a trigger as the user talking: {}",
            inbound.content
        );

        let rows = dispatch::read_history(&history_path(h.workspace())).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ok"], json!(true));
        assert_eq!(rows[0]["merged"], json!(1));
    }

    #[tokio::test]
    async fn test_a_record_fire_is_booked_without_reaching_anybody() {
        let mut h = Harness::new();
        let meta = TriggerMeta::new("trg-r", "just note it");
        h.rule(
            &meta,
            "function judge() { return {fire: true, detail: 'noted'}; }",
        );

        // A rule with no sources is ready at once, and a condition that stays
        // true is announced exactly once — which for `record` means one row.
        let first = h.service.tick().await;
        assert_eq!(first.fired.len(), 1, "a schedule-less rule still judges");
        let second = h.service.tick().await;
        assert!(
            second.fired.is_empty(),
            "the edge must not rise a second time"
        );
        assert_eq!(h.service.dispatch().drain().await, 1);

        let rows = dispatch::read_history(&history_path(h.workspace())).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["action_kind"], json!("record"));
        assert_eq!(rows[0]["ok"], json!(true), "booking is all it had to do");
        assert!(
            h.consumers.inbound.try_recv().is_err(),
            "a record rule must not put anything in a conversation"
        );
    }

    #[tokio::test]
    async fn test_a_rule_that_keeps_failing_says_so_once() {
        // End to end on the alert path: the tick counts failures, the consumer
        // speaks. A rule whose script never works is degraded to a slow cadence,
        // so many ticks pass before the fifth failure — and still only one alert
        // is raised, because the latch holds until the rule recovers.
        let mut h = Harness::new();
        let mut meta = TriggerMeta::new("trg-broken", "always throws");
        meta.origin = Some(crate::agent::Origin::new("webui", "chat-1"));
        meta.action = TriggerAction {
            kind: ActionKind::Notify,
            text_template: Some("{{detail}}".to_string()),
            ..Default::default()
        };
        h.rule(&meta, "function judge() { throw new Error('boom'); }");

        let mut raised = 0;
        for _ in 0..40 {
            raised += h.service.tick().await.alerts.len();
        }
        assert_eq!(
            raised, 1,
            "five failures cost the user one alert, not forty"
        );

        assert_eq!(h.service.dispatch().drain().await, 1);
        let alert = h
            .consumers
            .outbound
            .try_recv()
            .expect("the alert reaches the channel itself");
        assert_eq!(alert.session_id, "webui:chat-1");
        assert!(
            alert.content.starts_with("[Trigger alert]"),
            "{}",
            alert.content
        );
        assert!(alert.content.contains("trg-broken"), "{}", alert.content);
        assert!(
            alert.content.contains("judged"),
            "the alert also says what happens next: {}",
            alert.content
        );
    }

    const COLLECT: &str = "function collect() { return {items: [{title: 'a'}]}; }";

    struct Harness {
        _tmp: tempfile::TempDir,
        sources: Arc<SourceLayer>,
        rules: Arc<SubscriptionLayer>,
        consumers: MessageBusReceivers,
        service: Arc<TriggerService>,
    }

    impl Harness {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let workspace = tmp.path().to_path_buf();
            let cfg = config(true);
            let (sources, rules, secrets) = build_layers(&cfg, &workspace);
            let (bus, consumers) = MessageBus::new();
            let dispatch = Dispatcher::new(
                None,
                bus,
                history_path(&workspace),
                cfg.history_lines,
                cfg.max_queue,
                cfg.max_merge,
                Tracer::new(vec![]),
            );
            let service = TriggerService::from_parts(
                sources.clone(),
                rules.clone(),
                secrets,
                dispatch,
                &cfg,
                Tracer::new(vec![]),
            );
            Self {
                _tmp: tmp,
                sources,
                rules,
                consumers,
                service,
            }
        }

        fn workspace(&self) -> &Path {
            self._tmp.path()
        }

        fn source(&self, id: &str) {
            let root = self.sources.root().to_path_buf();
            let mut meta = source_file::SourceMeta::new(id, id);
            meta.timeout_s = 1;
            source_file::save_meta(&root, &meta).unwrap();
            source_file::save_script(&root, id, COLLECT).unwrap();
        }

        /// Write a definition the way the tool would, then let the next tick
        /// reload it from disk.
        fn rule(&self, meta: &TriggerMeta, script: &str) {
            let root = self.rules.root().to_path_buf();
            subscription::meta::save_meta(&root, meta).unwrap();
            subscription::meta::save_script(&root, &meta.id, script).unwrap();
        }
    }
}

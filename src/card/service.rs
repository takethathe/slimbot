//! Card generation orchestration (channel-layer).
//!
//! [`CardService`] turns a finished Run into a Card spec through an
//! *independent* model call, then hands the result to a dedicated mpsc that the
//! `ChannelManager` routes to the owning channel. It is deliberately outside the
//! runner: `BusResult` is published only after `run()` returns, so any in-runner
//! hook would either delay the body text or make the agent core aware of a
//! presentation-layer feature (see `docs/adr/0005`).
//!
//! Two fail-closed gates keep cards honest:
//!
//! 1. **Eligible Run** — only Runs whose `run_id` was minted by the WebUI's
//!    inbound path are tracked. cron / heartbeat runs reuse a user's session for
//!    event routing but never register here, so they produce no card even though
//!    their lifecycle events carry a WebUI `session_id`.
//! 2. **Catalog registered** — the chat must have POSTed a Catalog prompt, which
//!    doubles as the "this client can render cards" handshake.
//!
//! Cards never touch the session JSONL, the agent context, or session token
//! accounting; a failure on any step is silent to the user (only logs remain).
//!
//! Runtime guardrails live in [`CardPolicy`] (concurrency cap, per-call
//! no-progress timeout, ops off-switch), the generated span
//! `json_render.generate` carries patch count and token usage for debugging,
//! and [`CardService::shutdown`] cancels in-flight generations at process exit.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{broadcast, mpsc};

use super::event::CardEvent;
use super::registry::{CardRegistry, RegisterError};
use super::spec::SpecCompiler;
use super::store::{CardRecord, CardStore};
use super::transcript::{TranscriptLimits, build_transcript};
use crate::provider::{LLMResponse, Provider, StreamSignal};
use crate::session::{AgentEvent, Message, SharedSessionManager};
use crate::trace::{SpanGuard, SpanRecorder, SpanStatus, Tracer};
use crate::{debug, error, info, warn_log};
use std::sync::Arc;

/// mpsc capacity for the card-event lane. Small: at most a couple of events
/// per Run, and cards are best-effort (a full lane must never block the Run).
pub const CARD_CHANNEL_CAPACITY: usize = 64;

/// Tracked state for one eligible Run between start and completion.
struct RunTrack {
    session_id: String,
    chat_id: String,
    /// Index into the session's message list where this Run's turn begins.
    /// `None` until the `TaskStarted` event resolves it against the message
    /// list (the user message is already written at that point).
    cursor: Option<usize>,
}

/// Runtime guardrails for card generation — cost and load limits, not
/// feature flags. Defaults follow the spec: at most two concurrent
/// generations, thirty seconds per call. Whether cards exist at all is
/// decided by Catalog registration, not here.
#[derive(Debug, Clone)]
pub struct CardPolicy {
    /// Master off-switch (ops escape hatch; default on).
    pub enabled: bool,
    /// Hard cap on simultaneously in-flight generation tasks.
    pub max_concurrent: usize,
    /// No-progress window for one streaming generation call: how long the
    /// stream may stay silent (no content *and* no liveness signal) before
    /// the call is cut off as stalled. A living stream is never capped by it.
    pub timeout: Duration,
}

impl Default for CardPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_concurrent: 2,
            timeout: Duration::from_secs(30),
        }
    }
}

/// What the provider task hands back once its stream call settles: the raw
/// call result plus the compiler's final state (the compiler lives on the
/// worker because the signal closure owns it).
struct GenerationOutcome {
    resp: anyhow::Result<LLMResponse>,
    spec: Option<serde_json::Value>,
    patch_count: usize,
}

/// Card generation service, shared behind an `Arc`.
pub struct CardService {
    registry: CardRegistry,
    store: CardStore,
    provider: Arc<dyn Provider>,
    session_manager: SharedSessionManager,
    card_tx: mpsc::Sender<CardEvent>,
    policy: CardPolicy,
    /// In-flight generation count, guarded by `policy.max_concurrent`.
    active: AtomicUsize,
    /// Generation tasks currently spawned, so shutdown can cancel them
    /// instead of leaving them hanging.
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Set once `shutdown` ran: no new generations are spawned.
    closed: AtomicBool,
    /// Optional tracer for the `json_render.generate` span (wiring-time).
    tracer: Option<Arc<Tracer>>,
    /// Eligible Runs, keyed by `run_id`. Populated by the WebUI inbound path
    /// and drained on completion/failure.
    runs: RwLock<HashMap<String, RunTrack>>,
}

impl CardService {
    pub fn new(
        provider: Arc<dyn Provider>,
        session_manager: SharedSessionManager,
        store: CardStore,
        card_tx: mpsc::Sender<CardEvent>,
    ) -> Self {
        Self {
            registry: CardRegistry::new(),
            store,
            provider,
            session_manager,
            card_tx,
            policy: CardPolicy::default(),
            active: AtomicUsize::new(0),
            tasks: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
            tracer: None,
            runs: RwLock::new(HashMap::new()),
        }
    }

    /// Override the runtime guardrails (wiring-time only, before `Arc`).
    pub fn with_policy(mut self, policy: CardPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Emit a `json_render.generate` span per generation (wiring-time only).
    pub fn with_tracer(mut self, tracer: Arc<Tracer>) -> Self {
        self.tracer = Some(tracer);
        self
    }

    /// Take a generation slot if the concurrency cap leaves room. Fail-soft:
    /// over-cap Runs are skipped, never queued.
    fn try_acquire_slot(&self) -> bool {
        let mut current = self.active.load(Ordering::Relaxed);
        loop {
            if current >= self.policy.max_concurrent {
                return false;
            }
            match self.active.compare_exchange_weak(
                current,
                current + 1,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(n) => current = n,
            }
        }
    }

    fn release_slot(&self) {
        self.active.fetch_sub(1, Ordering::Release);
    }

    /// Cancel every in-flight generation and stop the service (process
    /// shutdown path). Returns the aborted handles so the caller can await
    /// their completion; later Runs are inert once this ran.
    pub fn shutdown(&self) -> Vec<tokio::task::JoinHandle<()>> {
        self.closed.store(true, Ordering::Relaxed);
        if let Ok(mut runs) = self.runs.write() {
            runs.clear();
        }
        let handles: Vec<_> = match self.tasks.lock() {
            Ok(mut t) => t.drain(..).collect(),
            Err(poisoned) => poisoned.into_inner().drain(..).collect(),
        };
        for h in &handles {
            h.abort();
        }
        handles
    }

    pub fn registry(&self) -> &CardRegistry {
        &self.registry
    }

    pub fn store(&self) -> &CardStore {
        &self.store
    }

    /// Validate + store a chat's Catalog registration. Rejects empty and
    /// oversized prompts; the prompt text is never logged in full.
    pub fn register_catalog(
        &self,
        chat_id: &str,
        prompt: String,
        catalog_hash: String,
    ) -> Result<(), RegisterError> {
        let len = prompt.len();
        let result = self.registry.register(chat_id, prompt, catalog_hash);
        match &result {
            Ok(()) => info!("[card] catalog registered chat={} len={}", chat_id, len),
            Err(RegisterError::Empty) => {
                warn_log!("[card] catalog rejected (empty) chat={}", chat_id)
            }
            Err(RegisterError::TooLarge) => {
                warn_log!(
                    "[card] catalog rejected (too large) chat={} len={}",
                    chat_id,
                    len
                )
            }
        }
        result
    }

    /// Mark a Run as WebUI-eligible. Called from the WebUI inbound path, where
    /// the `run_id` is freshly minted. A non-WebUI Run never appears here.
    pub fn register_run(&self, run_id: &str, session_id: &str, chat_id: &str) {
        info!(
            "[card] run registered run={} session={} chat={}",
            run_id, session_id, chat_id
        );
        if let Ok(mut map) = self.runs.write() {
            map.insert(
                run_id.to_string(),
                RunTrack {
                    session_id: session_id.to_string(),
                    chat_id: chat_id.to_string(),
                    cursor: None,
                },
            );
        }
    }

    fn drop_run(&self, run_id: &str) -> Option<RunTrack> {
        self.runs.write().ok().and_then(|mut m| m.remove(run_id))
    }

    /// Resolve the turn's starting cursor for an eligible Run. The `TaskStarted`
    /// event fires *after* the user message is written, so back off to the last
    /// user message; a transcript that omitted it would lose "what was asked".
    async fn on_started(&self, run_id: &str) {
        // Only track Runs that registered through the WebUI inbound path.
        if self
            .runs
            .read()
            .map(|m| !m.contains_key(run_id))
            .unwrap_or(true)
        {
            debug!(
                "[card] TaskStarted for unregistered run {}, ignoring",
                run_id
            );
            return;
        }
        // Snapshot enough to find the cursor without holding the write lock.
        let session_id = {
            match self.runs.read() {
                Ok(m) => m.get(run_id).map(|t| t.session_id.clone()),
                Err(_) => None,
            }
        };
        let Some(session_id) = session_id else { return };
        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(&session_id).await
        };
        let cursor = messages
            .iter()
            .rposition(|m| matches!(m, Message::User { .. }))
            .unwrap_or(messages.len());
        if let Ok(mut m) = self.runs.write()
            && let Some(track) = m.get_mut(run_id)
        {
            track.cursor = Some(cursor);
        }
    }

    /// Complete an eligible Run: read the turn transcript, generate a card, and
    /// emit + persist it. Non-eligible Runs (cron/heartbeat/CLI/unregistered)
    /// are ignored here because they never registered.
    async fn on_completed(self: &Arc<Self>, run_id: &str) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        let track = match self.drop_run(run_id) {
            Some(t) => t,
            None => {
                debug!(
                    "[card] TaskCompleted for unregistered run {}, ignoring",
                    run_id
                );
                return;
            }
        };
        // Ops escape hatch: policy switched off means no new generations —
        // the run is still drained so bookkeeping stays clean.
        if !self.policy.enabled {
            debug!("[card] policy disabled, skipping run {}", run_id);
            return;
        }
        let Some(cursor) = track.cursor else {
            // No TaskStarted seen (e.g. cancelled before start) — nothing to do.
            debug!("[card] no TaskStarted cursor for run {}, skipping", run_id);
            return;
        };
        // Fail-closed gate: no Catalog registration, no card. Info-level on
        // purpose: "never saw a card" is almost always this gate or the run
        // never registering — both diagnosable from the log line alone.
        let Some(registration) = self.registry.get(&track.chat_id) else {
            info!(
                "[card] run {} skipped: chat {} has no catalog registration",
                run_id, track.chat_id
            );
            return;
        };

        let messages = {
            let sm = self.session_manager.lock().await;
            sm.get_messages(&track.session_id).await
        };
        let start = cursor.min(messages.len());
        let turn = &messages[start..];
        // Anchor = timestamp of this Run's last assistant message, used for
        // sidecar replay matching (survives consolidation's full rewrite).
        let anchor = turn.iter().rev().find_map(|m| match m {
            Message::Assistant { .. } => {
                let ts = m.timestamp();
                if ts.is_empty() {
                    None
                } else {
                    Some(ts.to_string())
                }
            }
            _ => None,
        });
        let transcript = build_transcript(turn, TranscriptLimits::default());
        if transcript.trim().is_empty() {
            debug!("[card] empty transcript for run {}, skipping", run_id);
            return;
        }
        // Concurrency gate: over-cap Runs are skipped outright — no queueing,
        // no blocking, only a log line.
        if !self.try_acquire_slot() {
            warn_log!(
                "[card] run {} skipped: {} generations already in flight (cap {})",
                run_id,
                self.active.load(Ordering::Relaxed),
                self.policy.max_concurrent
            );
            return;
        }

        let service = Arc::clone(self);
        let session_id = track.session_id.clone();
        let chat_id = track.chat_id.clone();
        let run_id = run_id.to_string();
        // Generate off the listener task so a slow call never stalls other Runs.
        let handle = tokio::spawn(async move {
            let svc = Arc::clone(&service);
            svc.generate(
                session_id,
                chat_id,
                run_id,
                registration.prompt,
                transcript,
                anchor,
            )
            .await;
            service.release_slot();
        });
        // Track the task for shutdown cancellation, pruning finished ones.
        if let Ok(mut t) = self.tasks.lock() {
            t.retain(|h| !h.is_finished());
            t.push(handle);
        }
    }

    async fn generate(
        self: Arc<Self>,
        session_id: String,
        chat_id: String,
        run_id: String,
        catalog_prompt: String,
        transcript: String,
        anchor: Option<String>,
    ) {
        // One independent span per generation: patch count and token usage
        // ride the attributes; every failure path leaves a trace via status.
        let span: Option<SpanGuard> = self
            .tracer
            .as_ref()
            .map(|t| t.start_span("json_render.generate", None));
        let set_status = |span: &Option<SpanGuard>, status: SpanStatus| {
            if let Some(s) = span {
                s.set_status(status);
            }
        };

        let system = Message::system(catalog_prompt);
        let user = Message::user(format!(
            "Generate a card that visualizes the result of this exchange. \
             Use only the components described in the catalog and base every \
             value on the transcript below.\n\n[run transcript]\n{transcript}"
        ));

        // All gates passed and generation actually begins. `try_send`: a full
        // lane must never stall the pipeline (Started is purely cosmetic).
        info!(
            "[card] generation started run={} chat={} transcript_len={} no_progress_window={:?}",
            run_id,
            chat_id,
            transcript.len(),
            self.policy.timeout
        );
        let _ = self.card_tx.try_send(CardEvent::Started {
            session_id: session_id.clone(),
            run_id: run_id.clone(),
        });

        // The provider call runs on its own task; every stream signal it
        // emits (content or liveness) ticks a watchdog. The policy timeout is
        // a *no-progress* budget, not a wall-clock cap: reasoning models spend
        // minutes in non-content chunks and must not be killed for it, while a
        // genuinely stalled stream is cut off and cancelled after the window.
        let (tick_tx, mut tick_rx) = mpsc::channel::<()>(64);
        let (done_tx, mut done_rx) = mpsc::channel::<GenerationOutcome>(1);
        let provider = Arc::clone(&self.provider);
        let tx = self.card_tx.clone();
        let worker_session_id = session_id.clone();
        let worker_run_id = run_id.clone();
        let msgs = [system, user];
        let worker = tokio::spawn(async move {
            // Feed streamed text straight into the compiler; each batch of
            // newly applied ops rides out as a Delta. Losing Deltas only costs
            // smoothness — the authoritative spec follows in `Completed`.
            let mut compiler = SpecCompiler::new();
            let mut patch_count = 0usize;
            let mut saw_content = false;
            let mut on_signal = |sig: StreamSignal| {
                let _ = tick_tx.try_send(());
                if let StreamSignal::Content(chunk) = sig {
                    if !saw_content {
                        saw_content = true;
                        debug!(
                            "[card] first content chunk run={} len={}",
                            worker_run_id,
                            chunk.len()
                        );
                    }
                    let ops = compiler.push(&chunk);
                    if !ops.is_empty() {
                        patch_count += ops.len();
                        let _ = tx.try_send(CardEvent::Delta {
                            session_id: worker_session_id.clone(),
                            run_id: worker_run_id.clone(),
                            ops,
                        });
                    }
                }
            };
            let m: [&Message; 2] = [&msgs[0], &msgs[1]];
            let resp = provider.chat_stream(&m, None, None, &mut on_signal).await;
            let spec = compiler.finish();
            let _ = done_tx
                .send(GenerationOutcome {
                    resp,
                    spec,
                    patch_count,
                })
                .await;
        });
        // If this future itself is cancelled (shutdown), the worker must die
        // with it instead of streaming into channels nobody reads anymore.
        struct AbortOnDrop(Option<tokio::task::JoinHandle<()>>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                if let Some(h) = &self.0
                    && !h.is_finished()
                {
                    h.abort();
                }
            }
        }
        let mut guard = AbortOnDrop(Some(worker));

        let window = self.policy.timeout;
        let mut deadline = Box::pin(tokio::time::sleep(window));
        let outcome = loop {
            tokio::select! {
                biased;
                o = done_rx.recv() => match o {
                    Some(o) => break o,
                    None => {
                        // Worker panicked or was cancelled before reporting.
                        error!("[card] generation worker vanished run={}", run_id);
                        set_status(&span, SpanStatus::Error("generation worker lost".into()));
                        let _ = self
                            .card_tx
                            .send(CardEvent::Failed {
                                session_id: session_id.clone(),
                                run_id: run_id.clone(),
                            })
                            .await;
                        return;
                    }
                },
                Some(()) = tick_rx.recv() => {
                    deadline.as_mut().reset(tokio::time::Instant::now() + window);
                }
                _ = &mut deadline => {
                    warn_log!(
                        "[card] generation stalled: no stream progress for {:?} run={}",
                        window,
                        run_id
                    );
                    set_status(&span, SpanStatus::Error("generation stalled".into()));
                    let _ = self
                        .card_tx
                        .send(CardEvent::Failed {
                            session_id: session_id.clone(),
                            run_id: run_id.clone(),
                        })
                        .await;
                    return;
                }
            }
        };
        guard.0.take(); // worker reported its outcome and finished on its own
        let resp = match outcome.resp {
            Ok(resp) => resp,
            Err(e) => {
                error!("[card] generation call failed run={}: {}", run_id, e);
                set_status(&span, SpanStatus::Error(e.to_string()));
                let _ = self
                    .card_tx
                    .send(CardEvent::Failed {
                        session_id: session_id.clone(),
                        run_id: run_id.clone(),
                    })
                    .await;
                return;
            }
        };

        if let Some(s) = &span {
            s.set_attribute(
                "json_render.patch_count",
                serde_json::json!(outcome.patch_count),
            );
            s.set_attribute(
                "gen_ai.usage.input_tokens",
                serde_json::json!(resp.usage.prompt_tokens),
            );
            s.set_attribute(
                "gen_ai.usage.output_tokens",
                serde_json::json!(resp.usage.completion_tokens),
            );
        }

        let spec = match outcome.spec {
            Some(s) => s,
            None => {
                // Empty spec → no card: retract with Failed, write nothing.
                info!("[card] empty spec, no card for run {}", run_id);
                set_status(&span, SpanStatus::Error("empty spec".into()));
                let _ = self
                    .card_tx
                    .send(CardEvent::Failed {
                        session_id: session_id.clone(),
                        run_id: run_id.clone(),
                    })
                    .await;
                return;
            }
        };

        // Authoritative Completed event (best-effort: a full lane must not
        // stall the pipeline; the sidecar still makes the card replayable).
        info!(
            "[card] generation completed run={} patches={} in_tokens={} out_tokens={}",
            run_id, outcome.patch_count, resp.usage.prompt_tokens, resp.usage.completion_tokens
        );
        let _ = self
            .card_tx
            .send(CardEvent::Completed {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
                spec: spec.clone(),
            })
            .await;

        // Persist to the sidecar only when we have a stable anchor; the record
        // stays out of the session JSONL entirely.
        if let Some(ts) = anchor {
            let created_at_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let record = CardRecord {
                ts,
                created_at_ms,
                spec,
            };
            if let Err(e) = self.store.append(&chat_id, &record) {
                warn_log!("[card] sidecar append failed chat={}: {}", chat_id, e);
            } else {
                debug!("[card] sidecar appended chat={} run={}", chat_id, run_id);
            }
        } else {
            debug!(
                "[card] no assistant anchor for run {}; skipping sidecar write",
                run_id
            );
        }
    }
}

/// Spawn the listener task that drives card generation off the `AgentEvent`
/// broadcast. Returns the join handle so the caller can track it.
pub fn spawn_listener(
    service: Arc<CardService>,
    mut rx: broadcast::Receiver<AgentEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => match event {
                    AgentEvent::TaskStarted { run_id, .. } => {
                        service.on_started(&run_id).await;
                    }
                    AgentEvent::TaskCompleted { run_id, .. } => {
                        service.on_completed(&run_id).await;
                    }
                    AgentEvent::TaskFailed { run_id, .. } => {
                        service.drop_run(&run_id);
                    }
                    _ => {}
                },
                Err(broadcast::error::RecvError::Closed) => break,
                // Lagging is acceptable: cards are best-effort. We simply may
                // miss a start/complete pair and generate nothing for it.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn_log!("[card] listener lagged by {} events", n);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{FinishReason, LLMResponse, Usage};
    use crate::session::SessionManager;
    use crate::tool::ToolDefinition;
    use crate::trace::SpanRecorder;
    use async_trait::async_trait;
    use serde_json::json;
    use std::time::Duration;

    /// Provider returning a fixed spec (or an error) that also records the
    /// transcript text it was handed, so "the transcript must contain this
    /// turn's question" can be asserted through the public call surface.
    struct ScriptedProvider {
        content: Option<String>,
        fail: bool,
        capture: Arc<std::sync::Mutex<Option<String>>>,
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn chat(
            &self,
            messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            if self.fail {
                return Err(anyhow::anyhow!("provider down"));
            }
            for m in messages {
                if let Message::User { content, .. } = m {
                    *self.capture.lock().unwrap() = Some(content.as_text().to_string());
                }
            }
            Ok(LLMResponse {
                content: self.content.clone(),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    const VALID_SPEC_JSONL: &str = concat!(
        "{\"op\":\"add\",\"path\":\"/root\",\"value\":\"c1\"}\n",
        "{\"op\":\"add\",\"path\":\"/elements\",\"value\":{\"c1\":{\"type\":\"Card\",\"props\":{}}}}\n",
    );

    async fn seed_turn(sm: &SharedSessionManager, session_id: &str, question: &str, answer: &str) {
        let mut guard = sm.lock().await;
        guard.get_or_create(session_id).await.unwrap();
        guard
            .add_message(session_id, Message::user(question.to_string()))
            .await
            .unwrap();
        guard
            .add_message(
                session_id,
                Message::assistant(Some(answer.to_string()), None, None, None),
            )
            .await
            .unwrap();
    }

    fn build_sm(dir_session: std::path::PathBuf) -> SharedSessionManager {
        Arc::new(tokio::sync::Mutex::new(
            SessionManager::new(dir_session).unwrap(),
        ))
    }

    async fn recv_timeout(rx: &mut mpsc::Receiver<CardEvent>) -> Option<CardEvent> {
        tokio::time::timeout(Duration::from_millis(800), rx.recv())
            .await
            .ok()
            .flatten()
    }

    /// Drain every event the pipeline emits for one run, stopping at the
    /// silence that follows the terminal Completed/Failed event.
    async fn drain(rx: &mut mpsc::Receiver<CardEvent>) -> Vec<CardEvent> {
        let mut events = Vec::new();
        while let Some(e) = recv_timeout(rx).await {
            let terminal = matches!(e, CardEvent::Completed { .. } | CardEvent::Failed { .. });
            events.push(e);
            if terminal {
                break;
            }
        }
        events
    }

    /// Provider whose stream call never returns — models a slow generation
    /// holding a concurrency slot indefinitely.
    struct BlockingProvider;

    #[async_trait]
    impl Provider for BlockingProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            unreachable!("card generation must use chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
            _on_signal: &mut (dyn FnMut(StreamSignal) + Send),
        ) -> anyhow::Result<LLMResponse> {
            std::future::pending::<()>().await;
            unreachable!("never resumed")
        }
    }

    /// Provider that feeds the spec text in hand-chosen chunks, mimicking a
    /// real streaming endpoint (the split inside a line is deliberate).
    struct ChunkedStreamProvider {
        chunks: Vec<String>,
    }

    #[async_trait]
    impl Provider for ChunkedStreamProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            unreachable!("card generation must use chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
            on_signal: &mut (dyn FnMut(StreamSignal) + Send),
        ) -> anyhow::Result<LLMResponse> {
            let mut full = String::new();
            for c in &self.chunks {
                on_signal(StreamSignal::Content(c.clone()));
                full.push_str(c);
            }
            Ok(LLMResponse {
                content: Some(full),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    /// Drives one full eligible Run through the real listener entry point.
    #[tokio::test]
    async fn test_registered_run_emits_exactly_one_completed_card() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(
            &sm,
            "webui:chat1",
            "How much did I spend this week?",
            "You spent $120.",
        )
        .await;

        let capture = Arc::new(std::sync::Mutex::new(None));
        let provider = Arc::new(ScriptedProvider {
            content: Some(VALID_SPEC_JSONL.to_string()),
            fail: false,
            capture: capture.clone(),
        });
        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(CardService::new(
            provider,
            sm,
            CardStore::new(session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat1", "catalog prompt".into(), "hash".into())
            .unwrap();
        service.register_run("run-abc", "webui:chat1", "chat1");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat1".into(),
                run_id: "run-abc".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat1".into(),
                run_id: "run-abc".into(),
                result: String::new(),
            })
            .unwrap();

        let evt = recv_timeout(&mut card_rx)
            .await
            .expect("a Started card event");
        assert!(matches!(evt, CardEvent::Started { .. }));
        let rest = drain_rest(&mut card_rx).await;
        let completed: Vec<_> = rest
            .iter()
            .filter(|e| matches!(e, CardEvent::Completed { .. }))
            .collect();
        assert_eq!(completed.len(), 1, "exactly one Completed per run");
        match &completed[0] {
            CardEvent::Completed { run_id, spec, .. } => {
                assert_eq!(run_id, "run-abc");
                assert_eq!(spec["root"], json!("c1"));
            }
            _ => unreachable!(),
        }
        assert!(
            rest.iter().any(|e| matches!(e, CardEvent::Delta { .. })),
            "assembled ops ride a Delta before the authoritative Completed"
        );
        assert!(
            recv_timeout(&mut card_rx).await.is_none(),
            "nothing after the terminal event"
        );

        // The transcript handed to the model must contain this turn's question,
        // proving the cursor backed off to the user message, not past it.
        let cap = capture
            .lock()
            .unwrap()
            .clone()
            .expect("provider was called");
        assert!(cap.contains("How much did I spend this week?"));
    }

    /// Drain events after the first one, stopping at a terminal event.
    async fn drain_rest(rx: &mut mpsc::Receiver<CardEvent>) -> Vec<CardEvent> {
        let mut events = Vec::new();
        while let Some(e) = recv_timeout(rx).await {
            let terminal = matches!(e, CardEvent::Completed { .. } | CardEvent::Failed { .. });
            events.push(e);
            if terminal {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn test_streamed_deltas_appear_before_the_authoritative_completed() {
        // Seam: a chunked model answer must surface as incremental Delta
        // events (a line split across chunks still assembles, per SpecCompiler),
        // and the final Completed must carry the full authoritative spec.
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat6", "q", "a").await;

        let line2 = "{\"op\":\"add\",\"path\":\"/elements\",\"value\":{\"c1\":{\"type\":\"Card\",\"props\":{}}}}";
        let provider = Arc::new(ChunkedStreamProvider {
            chunks: vec![
                format!(
                    "{{\"op\":\"add\",\"path\":\"/root\",\"value\":\"c1\"}}\n{}",
                    &line2[..20]
                ),
                format!("{}\n", &line2[20..]),
            ],
        });

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(CardService::new(
            provider,
            sm,
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat6", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-stream", "webui:chat6", "chat6");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat6".into(),
                run_id: "run-stream".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat6".into(),
                run_id: "run-stream".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        let kinds: Vec<&str> = events
            .iter()
            .map(|e| match e {
                CardEvent::Started { .. } => "started",
                CardEvent::Delta { .. } => "delta",
                CardEvent::Completed { .. } => "completed",
                CardEvent::Failed { .. } => "failed",
            })
            .collect();
        assert_eq!(
            kinds,
            vec!["started", "delta", "delta", "completed"],
            "one Delta per assembled chunk-batch, then the authoritative spec"
        );
        if let CardEvent::Delta { ops, .. } = &events[1] {
            assert_eq!(ops.len(), 1, "first chunk completes exactly one line");
        }
        match &events[3] {
            CardEvent::Completed { spec, .. } => {
                assert_eq!(spec["root"], json!("c1"));
                assert_eq!(spec["elements"]["c1"]["type"], json!("Card"));
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    /// Three runs pile up at once; the gate admits two and skips the third
    /// outright — no queueing, no blocking, no event for the skipped run.
    #[tokio::test]
    async fn test_concurrency_gate_skips_the_third_simultaneous_run() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        for i in 1..=3 {
            seed_turn(&sm, &format!("webui:chat-c{i}"), "q", "a").await;
        }

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(BlockingProvider),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_policy(CardPolicy {
                max_concurrent: 2,
                ..Default::default()
            }),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        for i in 1..=3 {
            let chat = format!("c{i}");
            let session_id = format!("webui:chat-c{i}");
            service
                .register_catalog(&chat, "prompt".into(), "h".into())
                .unwrap();
            service.register_run(&format!("run-{i}"), &session_id, &chat);
            event_tx
                .send(AgentEvent::TaskStarted {
                    session_id: session_id.clone(),
                    run_id: format!("run-{i}"),
                })
                .unwrap();
            event_tx
                .send(AgentEvent::TaskCompleted {
                    session_id,
                    run_id: format!("run-{i}"),
                    result: String::new(),
                })
                .unwrap();
        }

        // Give every dispatch a chance to run, then take a full silence sample:
        // exactly two Starteds must ever appear.
        let mut started = 0;
        let mut others = Vec::new();
        while let Some(e) = recv_timeout(&mut card_rx).await {
            match e {
                CardEvent::Started { .. } => started += 1,
                other => others.push(other),
            }
        }
        assert_eq!(started, 2, "gate admits exactly max_concurrent runs");
        assert!(
            others.is_empty(),
            "skipped run produces no event at all: {others:?}"
        );
    }

    /// A generation call that outlives the policy timeout must end in Failed
    /// — the user sees only the body text, never a half-streamed card stuck
    /// open — and nothing may reach the sidecar.
    #[tokio::test]
    async fn test_slow_generation_times_out_and_fails_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat7", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(BlockingProvider),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_policy(CardPolicy {
                timeout: Duration::from_millis(80),
                ..Default::default()
            }),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat7", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-slow", "webui:chat7", "chat7");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat7".into(),
                run_id: "run-slow".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat7".into(),
                run_id: "run-slow".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        assert!(
            matches!(
                events.as_slice(),
                [CardEvent::Started { .. }, CardEvent::Failed { .. }]
            ),
            "timeout → Started then Failed: {events:?}"
        );
        let sidecar = session_dir.join("chat7.cards.jsonl");
        assert!(!sidecar.exists(), "a timed-out generation writes nothing");
    }

    /// Streaming provider that keeps the transport alive with `Activity`
    /// signals while it has no content yet (what a reasoning model's thinking
    /// chunks look like to us), then delivers the spec — or, with
    /// `stall_at_end`, goes permanently silent to model a stream that dies
    /// after some progress. Pins the no-progress timeout semantics.
    struct SlowThinkerProvider {
        tick: Duration,
        activity_before_content: u32,
        spec: String,
        stall_at_end: bool,
    }

    #[async_trait]
    impl Provider for SlowThinkerProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            unreachable!("card generation must use chat_stream")
        }

        async fn chat_stream(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
            on_signal: &mut (dyn FnMut(StreamSignal) + Send),
        ) -> anyhow::Result<LLMResponse> {
            for _ in 0..self.activity_before_content {
                tokio::time::sleep(self.tick).await;
                on_signal(StreamSignal::Activity);
            }
            if self.stall_at_end {
                std::future::pending::<()>().await;
            }
            on_signal(StreamSignal::Content(self.spec.clone()));
            Ok(LLMResponse {
                content: Some(self.spec.clone()),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    /// The policy timeout is a *no-progress* budget, not a wall-clock cap:
    /// a stream that keeps signalling liveness (reasoning chunks, keep-alives)
    /// may outlive the window and still get its card.
    #[tokio::test]
    async fn test_stream_activity_signals_extend_the_timeout_window() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat7b", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(SlowThinkerProvider {
                    tick: Duration::from_millis(15),
                    activity_before_content: 10,
                    spec: VALID_SPEC_JSONL.to_string(),
                    stall_at_end: false,
                }),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_policy(CardPolicy {
                timeout: Duration::from_millis(80),
                ..Default::default()
            }),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat7b", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-thinker", "webui:chat7b", "chat7b");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat7b".into(),
                run_id: "run-thinker".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat7b".into(),
                run_id: "run-thinker".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        let completed = events
            .iter()
            .filter(|e| matches!(e, CardEvent::Completed { .. }))
            .count();
        assert_eq!(
            completed, 1,
            "150ms of liveness signals with an 80ms no-progress window \
             must still complete: {events:?}"
        );
        assert!(
            !events.iter().any(|e| matches!(e, CardEvent::Failed { .. })),
            "a living stream must not be killed as stalled: {events:?}"
        );
    }

    /// Liveness signals defer the cutoff but do not lift it: a stream that
    /// goes permanently silent after some activity is still cut off once the
    /// no-progress window elapses with nothing arriving.
    #[tokio::test]
    async fn test_stream_stall_after_progress_still_fails() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat7c", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(SlowThinkerProvider {
                    tick: Duration::from_millis(15),
                    // 150ms of liveness: the window is crossed and survived.
                    activity_before_content: 10,
                    spec: VALID_SPEC_JSONL.to_string(),
                    stall_at_end: true,
                }),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_policy(CardPolicy {
                timeout: Duration::from_millis(80),
                ..Default::default()
            }),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat7c", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-stall", "webui:chat7c", "chat7c");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat7c".into(),
                run_id: "run-stall".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat7c".into(),
                run_id: "run-stall".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        assert!(
            matches!(
                events.as_slice(),
                [CardEvent::Started { .. }, CardEvent::Failed { .. }]
            ),
            "a stream that stalls after living must still end Failed: {events:?}"
        );
        let sidecar = session_dir.join("chat7c.cards.jsonl");
        assert!(
            !sidecar.exists(),
            "a stalled generation writes nothing to the sidecar"
        );
    }

    /// Process shutdown must cancel in-flight generations rather than leave
    /// them hanging: the task resolves as cancelled and emits nothing more.
    #[tokio::test]
    async fn test_shutdown_cancels_inflight_generation() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat8", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(CardService::new(
            Arc::new(BlockingProvider),
            sm,
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat8", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-hang", "webui:chat8", "chat8");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat8".into(),
                run_id: "run-hang".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat8".into(),
                run_id: "run-hang".into(),
                result: String::new(),
            })
            .unwrap();

        let first = recv_timeout(&mut card_rx)
            .await
            .expect("Started before shutdown");
        assert!(matches!(first, CardEvent::Started { .. }));

        let handles = service.shutdown();
        assert_eq!(handles.len(), 1, "the inflight task is tracked");
        for h in handles {
            let joined = tokio::time::timeout(Duration::from_millis(500), h)
                .await
                .expect("task resolves promptly after abort");
            assert!(joined.is_err(), "cancelled task surfaces as JoinError");
        }
        assert!(
            recv_timeout(&mut card_rx).await.is_none(),
            "a cancelled generation emits no terminal event"
        );

        // After shutdown, later Runs are inert too — nothing new spawns.
        service.register_run("run-after", "webui:chat8", "chat8");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat8".into(),
                run_id: "run-after".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat8".into(),
                run_id: "run-after".into(),
                result: String::new(),
            })
            .unwrap();
        assert!(
            recv_timeout(&mut card_rx).await.is_none(),
            "no generation starts after shutdown"
        );
    }

    /// Captures span ends (with the name resolved from span-start) so tests
    /// can assert on the `json_render.generate` instrumentation from outside.
    struct RecordingBackend {
        starts: std::sync::Mutex<HashMap<String, String>>,
        ends: std::sync::Mutex<Vec<RecordedSpan>>,
    }

    struct RecordedSpan {
        name: String,
        status: crate::trace::SpanStatus,
        attributes: std::collections::BTreeMap<String, serde_json::Value>,
    }

    impl RecordingBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                starts: std::sync::Mutex::new(HashMap::new()),
                ends: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn end_for(&self, name: &str) -> Option<RecordedSpan> {
            self.ends
                .lock()
                .unwrap()
                .iter()
                .find(|s| s.name == name)
                .map(|s| RecordedSpan {
                    name: s.name.clone(),
                    status: s.status.clone(),
                    attributes: s.attributes.clone(),
                })
        }
    }

    impl crate::trace::TraceBackend for RecordingBackend {
        fn on_span_start(
            &self,
            ctx: &crate::trace::SpanContext,
            name: &str,
            _start: std::time::SystemTime,
        ) {
            self.starts
                .lock()
                .unwrap()
                .insert(ctx.span_id.clone(), name.to_string());
        }

        fn on_span_end(
            &self,
            ctx: &crate::trace::SpanContext,
            _end: std::time::SystemTime,
            status: &crate::trace::SpanStatus,
            attributes: &std::collections::BTreeMap<String, serde_json::Value>,
        ) {
            let name = self
                .starts
                .lock()
                .unwrap()
                .get(&ctx.span_id)
                .cloned()
                .unwrap_or_default();
            self.ends.lock().unwrap().push(RecordedSpan {
                name,
                status: status.clone(),
                attributes: attributes.clone(),
            });
        }
    }

    #[tokio::test]
    async fn test_generation_span_records_patch_count_and_token_usage() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat9", "q", "a").await;

        let backend = RecordingBackend::new();
        let tracer = Arc::new(crate::trace::Tracer::new(vec![backend.clone()]));
        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(ScriptedProvider {
                    content: Some(VALID_SPEC_JSONL.to_string()),
                    fail: false,
                    capture: Arc::new(std::sync::Mutex::new(None)),
                }),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_tracer(tracer),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat9", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-span", "webui:chat9", "chat9");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat9".into(),
                run_id: "run-span".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat9".into(),
                run_id: "run-span".into(),
                result: String::new(),
            })
            .unwrap();
        let _ = drain(&mut card_rx).await;

        let span = backend
            .end_for("json_render.generate")
            .expect("a json_render.generate span");
        assert_eq!(span.status, crate::trace::SpanStatus::Ok);
        assert_eq!(
            span.attributes.get("json_render.patch_count"),
            Some(&serde_json::json!(2)),
            "both spec lines count as applied patches"
        );
        // The generation call's token usage is recorded on the span…
        assert!(span.attributes.contains_key("gen_ai.usage.input_tokens"));
        assert!(span.attributes.contains_key("gen_ai.usage.output_tokens"));
    }

    #[tokio::test]
    async fn test_failed_generation_leaves_error_span() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat10", "q", "a").await;

        let backend = RecordingBackend::new();
        let tracer = Arc::new(crate::trace::Tracer::new(vec![backend.clone()]));
        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(ScriptedProvider {
                    content: None,
                    fail: true,
                    capture: Arc::new(std::sync::Mutex::new(None)),
                }),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_tracer(tracer),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat10", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-err-span", "webui:chat10", "chat10");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat10".into(),
                run_id: "run-err-span".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat10".into(),
                run_id: "run-err-span".into(),
                result: String::new(),
            })
            .unwrap();
        let _ = drain(&mut card_rx).await;

        let span = backend
            .end_for("json_render.generate")
            .expect("failure path is traced too");
        assert!(
            matches!(&span.status, crate::trace::SpanStatus::Error(msg) if msg.contains("provider down")),
            "error status carries the reason: {:?}",
            span.status
        );
    }

    /// The ops escape hatch: with the policy switched off, an otherwise
    /// fully-eligible run produces nothing at all — no events, no call.
    #[tokio::test]
    async fn test_disabled_policy_produces_no_card() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat11", "q", "a").await;

        let capture = Arc::new(std::sync::Mutex::new(None));
        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(
            CardService::new(
                Arc::new(ScriptedProvider {
                    content: Some(VALID_SPEC_JSONL.to_string()),
                    fail: false,
                    capture: capture.clone(),
                }),
                sm,
                CardStore::new(&session_dir),
                card_tx,
            )
            .with_policy(CardPolicy {
                enabled: false,
                ..Default::default()
            }),
        );
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat11", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-off", "webui:chat11", "chat11");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat11".into(),
                run_id: "run-off".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat11".into(),
                run_id: "run-off".into(),
                result: String::new(),
            })
            .unwrap();

        assert!(
            recv_timeout(&mut card_rx).await.is_none(),
            "disabled policy emits no events"
        );
        assert!(
            capture.lock().unwrap().is_none(),
            "disabled policy never calls the provider"
        );
    }

    /// Streams a two-line spec whose title echoes the first `MARK-<n>`
    /// found in the request, and optionally stalls on a shared gate when the
    /// transcript says `SLOW`. The echo lets tests tie a spec back to the
    /// exact turn it was generated from.
    struct MarkEchoProvider {
        slow_gate: Arc<tokio::sync::Semaphore>,
    }

    impl MarkEchoProvider {
        fn echo_of(text: &str) -> String {
            let idx = text
                .find("MARK-")
                .expect("test transcript must contain MARK-n");
            text[idx + 5..idx + 6].to_string()
        }
    }

    #[async_trait]
    impl Provider for MarkEchoProvider {
        async fn chat(
            &self,
            _messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
        ) -> anyhow::Result<LLMResponse> {
            unreachable!("card generation must use chat_stream")
        }

        async fn chat_stream(
            &self,
            messages: &[&Message],
            _tools: Option<&[ToolDefinition]>,
            _recorder: Option<&dyn SpanRecorder>,
            on_signal: &mut (dyn FnMut(StreamSignal) + Send),
        ) -> anyhow::Result<LLMResponse> {
            let user_text = messages
                .iter()
                .find_map(|m| match m {
                    Message::User { content, .. } => Some(content.as_text().to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            if user_text.contains("SLOW") {
                let _permit = self.slow_gate.acquire().await.unwrap();
            }
            let mark = Self::echo_of(&user_text);
            on_signal(StreamSignal::Content(
                "{\"op\":\"add\",\"path\":\"/root\",\"value\":\"c1\"}\n".to_string(),
            ));
            on_signal(StreamSignal::Content(format!(
                "{{\"op\":\"add\",\"path\":\"/elements\",\"value\":{{\"c1\":{{\"type\":\"Card\",\"props\":{{\"title\":\"MARK-{mark}\"}}}}}}}}\n"
            )));
            Ok(LLMResponse {
                content: Some(user_text),
                tool_calls: None,
                finish_reason: FinishReason::Stop,
                usage: Usage::default(),
            })
        }
    }

    fn spec_title(spec: &serde_json::Value) -> String {
        spec["elements"]["c1"]["props"]["title"]
            .as_str()
            .unwrap_or("<none>")
            .to_string()
    }

    /// Wait until the listener resolved a Run's cursor. In the real system
    /// the runner writes the next turn's user message only after the current
    /// Run has started and completed, so `on_started` always sees its own
    /// turn's last user message. Tests that seed several turns up front must
    /// honour that ordering explicitly or the cursor would resolve against
    /// the wrong turn.
    async fn wait_cursor(service: &CardService, run_id: &str) {
        for _ in 0..200 {
            let resolved = service
                .runs
                .read()
                .map(|m| m.get(run_id).and_then(|t| t.cursor).is_some())
                .unwrap_or(false);
            if resolved {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("cursor for {run_id} never resolved");
    }

    /// Two rounds in one session, both generating at overlapping times: each
    /// card must carry its own round's content and run_id — never the other's.
    #[tokio::test]
    async fn test_two_rounds_in_one_session_do_not_cross_cards() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat12", "MARK-1 question", "answer one").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let provider = Arc::new(MarkEchoProvider {
            slow_gate: Arc::new(tokio::sync::Semaphore::new(0)),
        });
        let service = Arc::new(CardService::new(
            provider,
            sm.clone(),
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat12", "prompt".into(), "h".into())
            .unwrap();
        // Round 1 starts while the session has only its own user message.
        service.register_run("run-1", "webui:chat12", "chat12");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat12".into(),
                run_id: "run-1".into(),
            })
            .unwrap();
        wait_cursor(&service, "run-1").await;

        // Round 2 lands in the same session afterwards.
        seed_turn(&sm, "webui:chat12", "MARK-2 question", "answer two").await;
        service.register_run("run-2", "webui:chat12", "chat12");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat12".into(),
                run_id: "run-2".into(),
            })
            .unwrap();

        for i in 1..=2 {
            event_tx
                .send(AgentEvent::TaskCompleted {
                    session_id: "webui:chat12".into(),
                    run_id: format!("run-{i}"),
                    result: String::new(),
                })
                .unwrap();
        }

        let mut titles: HashMap<String, String> = HashMap::new();
        while let Some(e) = recv_timeout(&mut card_rx).await {
            if let CardEvent::Completed { run_id, spec, .. } = e {
                titles.insert(run_id, spec_title(&spec));
            }
        }
        assert_eq!(
            titles.get("run-1").map(String::as_str),
            Some("MARK-1"),
            "round 1's card shows round 1's content: {titles:?}"
        );
        assert_eq!(
            titles.get("run-2").map(String::as_str),
            Some("MARK-2"),
            "round 2's card shows round 2's content: {titles:?}"
        );
    }

    /// A user sending the next message must never cancel the previous turn's
    /// in-flight card: the stalled generation completes once its call returns.
    #[tokio::test]
    async fn test_next_message_does_not_cancel_inflight_card() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat13", "MARK-1 SLOW question", "a1").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let slow_gate = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = Arc::new(MarkEchoProvider {
            slow_gate: slow_gate.clone(),
        });
        let service = Arc::new(CardService::new(
            provider,
            sm.clone(),
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat13", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-slow1", "webui:chat13", "chat13");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat13".into(),
                run_id: "run-slow1".into(),
            })
            .unwrap();
        wait_cursor(&service, "run-slow1").await;
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat13".into(),
                run_id: "run-slow1".into(),
                result: String::new(),
            })
            .unwrap();
        let first = recv_timeout(&mut card_rx)
            .await
            .expect("Started for the inflight run");
        assert!(matches!(first, CardEvent::Started { .. }));

        // User sends the next message — the inflight card must survive it.
        seed_turn(&sm, "webui:chat13", "MARK-2 next question", "a2").await;
        service.register_run("run-next", "webui:chat13", "chat13");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat13".into(),
                run_id: "run-next".into(),
            })
            .unwrap();
        wait_cursor(&service, "run-next").await;
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat13".into(),
                run_id: "run-next".into(),
                result: String::new(),
            })
            .unwrap();

        // The second round completes while the first is still stalled.
        let mut got_next_completed = false;
        let mut got_failed = false;
        while let Some(e) = recv_timeout(&mut card_rx).await {
            match e {
                CardEvent::Completed { run_id, spec, .. } if run_id == "run-next" => {
                    assert_eq!(spec_title(&spec), "MARK-2");
                    got_next_completed = true;
                }
                CardEvent::Failed { .. } => got_failed = true,
                _ => {}
            }
        }
        assert!(got_next_completed, "second round generates normally");
        assert!(
            !got_failed,
            "inflight card is not cancelled by the next turn"
        );

        // Release the stalled first generation: it must complete, not vanish.
        slow_gate.add_permits(1);
        let mut first_done = None;
        while let Some(e) = recv_timeout(&mut card_rx).await {
            if let CardEvent::Completed { run_id, spec, .. } = e {
                first_done = Some((run_id, spec_title(&spec)));
            }
        }
        assert_eq!(
            first_done.as_ref().map(|(r, t)| (r.as_str(), t.as_str())),
            Some(("run-slow1", "MARK-1")),
            "the original card still arrives after its call returns"
        );
    }

    /// Cards are presentation-only: a full generation must leave the session
    /// itself byte-for-byte untouched — no appended messages, no persist.
    #[tokio::test]
    async fn test_card_generation_leaves_the_session_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat14", "q", "a").await;
        let before = sm.lock().await.get_messages("webui:chat14").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let service = Arc::new(CardService::new(
            scripted_valid(),
            sm.clone(),
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat14", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-clean", "webui:chat14", "chat14");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat14".into(),
                run_id: "run-clean".into(),
            })
            .unwrap();
        wait_cursor(&service, "run-clean").await;
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat14".into(),
                run_id: "run-clean".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, CardEvent::Completed { .. })),
            "a card was actually generated: {events:?}"
        );
        let after = sm.lock().await.get_messages("webui:chat14").await;
        assert_eq!(before.len(), after.len(), "no messages appended");
        let mut stray: Vec<_> = std::fs::read_dir(&session_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".jsonl") && !n.ends_with(".cards.jsonl"))
                    .collect()
            })
            .unwrap_or_default();
        stray.sort();
        assert!(
            stray.is_empty(),
            "generation never persists session state: {stray:?}"
        );
    }

    fn scripted_valid() -> Arc<ScriptedProvider> {
        Arc::new(ScriptedProvider {
            content: Some(VALID_SPEC_JSONL.to_string()),
            fail: false,
            capture: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    #[tokio::test]
    async fn test_unregistered_chat_produces_no_card() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat2", "hi", "hello").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let provider = Arc::new(ScriptedProvider {
            content: Some(VALID_SPEC_JSONL.to_string()),
            fail: false,
            capture: Arc::new(std::sync::Mutex::new(None)),
        });
        let service = Arc::new(CardService::new(
            provider,
            sm,
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        // Run is WebUI-eligible but the chat never registered a catalog.
        service.register_run("run-no-cat", "webui:chat2", "chat2");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat2".into(),
                run_id: "run-no-cat".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat2".into(),
                run_id: "run-no-cat".into(),
                result: String::new(),
            })
            .unwrap();

        assert!(recv_timeout(&mut card_rx).await.is_none());
    }

    #[tokio::test]
    async fn test_non_webui_run_produces_no_card_even_with_webui_session_id() {
        // cron/heartbeat reuse a user session for event routing but never
        // register through the WebUI inbound path — the eligibility gate must
        // reject them based on run_id, not the session_id in the event.
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat3", "scheduled?", "yes").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let provider = Arc::new(ScriptedProvider {
            content: Some(VALID_SPEC_JSONL.to_string()),
            fail: false,
            capture: Arc::new(std::sync::Mutex::new(None)),
        });
        let service = Arc::new(CardService::new(
            provider,
            sm,
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat3", "prompt".into(), "h".into())
            .unwrap();
        // No register_run for this system-minted run id.
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat3".into(),
                run_id: "system-run".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat3".into(),
                run_id: "system-run".into(),
                result: String::new(),
            })
            .unwrap();

        assert!(recv_timeout(&mut card_rx).await.is_none());
    }

    #[tokio::test]
    async fn test_empty_spec_marks_failed_and_writes_no_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat4", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let provider = Arc::new(ScriptedProvider {
            content: Some("not a patch stream at all".to_string()),
            fail: false,
            capture: Arc::new(std::sync::Mutex::new(None)),
        });
        let store = CardStore::new(&session_dir);
        let service = Arc::new(CardService::new(provider, sm, store, card_tx));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat4", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-empty", "webui:chat4", "chat4");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat4".into(),
                run_id: "run-empty".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat4".into(),
                run_id: "run-empty".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        assert!(
            matches!(
                events.as_slice(),
                [CardEvent::Started { .. }, CardEvent::Failed { .. }]
            ),
            "empty spec → Started then Failed, never a card: {events:?}"
        );
        let sidecar = session_dir.join("chat4.cards.jsonl");
        assert!(!sidecar.exists(), "no sidecar write for an empty spec");
    }

    #[tokio::test]
    async fn test_provider_error_emits_failed() {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = build_sm(session_dir.clone());
        seed_turn(&sm, "webui:chat5", "q", "a").await;

        let (card_tx, mut card_rx) = mpsc::channel(16);
        let (event_tx, _) = broadcast::channel(16);
        let provider = Arc::new(ScriptedProvider {
            content: None,
            fail: true,
            capture: Arc::new(std::sync::Mutex::new(None)),
        });
        let service = Arc::new(CardService::new(
            provider,
            sm,
            CardStore::new(&session_dir),
            card_tx,
        ));
        let _listener = spawn_listener(service.clone(), event_tx.subscribe());

        service
            .register_catalog("chat5", "prompt".into(), "h".into())
            .unwrap();
        service.register_run("run-err", "webui:chat5", "chat5");
        event_tx
            .send(AgentEvent::TaskStarted {
                session_id: "webui:chat5".into(),
                run_id: "run-err".into(),
            })
            .unwrap();
        event_tx
            .send(AgentEvent::TaskCompleted {
                session_id: "webui:chat5".into(),
                run_id: "run-err".into(),
                result: String::new(),
            })
            .unwrap();

        let events = drain(&mut card_rx).await;
        assert!(
            matches!(
                events.as_slice(),
                [CardEvent::Started { .. }, CardEvent::Failed { .. }]
            ),
            "provider error → Started then Failed: {events:?}"
        );
    }
}

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::sse::Event,
    routing::{get, post},
};
use serde::Serialize;
use tokio::sync::{broadcast, mpsc};

use super::{Channel, ChannelFactory};
use crate::card::{CardEvent, CardService};
use crate::embed;
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::{AgentEvent, SharedSessionManager, TaskHook, TaskState};
use crate::{debug, error, info};

struct AppState {
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<SsePayload>>>>>,
    inbound_tx: mpsc::Sender<BusRequest>,
    channel_id: String,
    session_manager: Option<SharedSessionManager>,
    index_html: String,
    traces_html: String,
    config_html: String,
    config_path: String,
    event_tx: Option<broadcast::Sender<AgentEvent>>,
    /// Enabled file-backend trace file paths (empty when tracing/file backend off).
    trace_paths: Vec<String>,
    /// Epoch ms when the current process reset its trace file(s); `None` when
    /// tracing is off or `reset_on_start` is false (no clean startup boundary).
    trace_startup_ms: Option<i64>,
    /// Card generation service (None when cards are not wired).
    card: Option<Arc<CardService>>,
}

/// SSE payload with event type discrimination.
#[derive(Clone, Debug)]
enum SsePayload {
    Message(String),
    AgentEvent(String),
    Card(String),
}

pub struct WebuiChannel {
    host: String,
    port: u16,
    channel_id: String,
    session_prefix: String, // "{channel_id}:" for fast session_id stripping
    message_bus: Option<Arc<MessageBus>>,
    session_manager: Option<SharedSessionManager>,
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<SsePayload>>>>>,
    shutdown_rx: Option<broadcast::Receiver<()>>,
    event_tx: Option<broadcast::Sender<AgentEvent>>,
    trace_paths: Vec<String>,
    trace_startup_ms: Option<i64>,
    config_path: String,
    card: Option<Arc<CardService>>,
}

impl WebuiChannel {
    pub fn from_config(config: &serde_json::Value) -> Result<Self> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0")
            .to_string();
        let port = config.get("port").and_then(|v| v.as_u64()).unwrap_or(8080) as u16;
        Ok(Self {
            host,
            port,
            channel_id: "webui".to_string(),
            session_prefix: "webui:".to_string(),
            message_bus: None,
            session_manager: None,
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            shutdown_rx: None,
            event_tx: None,
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            config_path: String::new(),
            card: None,
        })
    }

    pub fn set_message_bus(&mut self, mb: Arc<MessageBus>) {
        self.message_bus = Some(mb);
    }

    pub fn set_session_manager(&mut self, sm: SharedSessionManager) {
        self.session_manager = Some(sm);
    }

    pub fn set_shutdown_rx(&mut self, rx: broadcast::Receiver<()>) {
        self.shutdown_rx = Some(rx);
    }

    pub fn set_event_tx(&mut self, tx: broadcast::Sender<AgentEvent>) {
        self.event_tx = Some(tx);
    }

    pub fn set_trace_paths(&mut self, paths: Vec<String>) {
        self.trace_paths = paths;
    }

    pub fn set_trace_startup_ms(&mut self, startup_ms: Option<i64>) {
        self.trace_startup_ms = startup_ms;
    }

    pub fn set_config_path(&mut self, path: String) {
        self.config_path = path;
    }

    pub fn set_card(&mut self, card: Arc<CardService>) {
        self.card = Some(card);
    }

    /// Prune disconnected SSE subscribers for a chat and send payload to remaining.
    async fn prune_and_send(&mut self, chat_id: &str, payload: SsePayload) -> Result<()> {
        let senders = {
            let mut guard = self.chats.lock().await;
            guard.remove(chat_id)
        };
        let Some(senders) = senders else {
            // Nobody is listening on this chat's SSE lane: the payload (e.g. a
            // card Completed event) is dropped. Debug-traceable so "the card
            // generated but never appeared" can be told apart from "no card
            // was generated".
            let kind = match payload {
                SsePayload::Message(_) => "message",
                SsePayload::AgentEvent(_) => "agent_event",
                SsePayload::Card(_) => "card",
            };
            debug!(
                "[webui] no live SSE subscribers for chat {}, dropping {} payload",
                chat_id, kind
            );
            return Ok(());
        };

        let mut alive = Vec::new();
        for tx in senders {
            if tx.send(payload.clone()).await.is_ok() {
                alive.push(tx);
            }
        }

        if !alive.is_empty() {
            self.chats.lock().await.insert(chat_id.to_string(), alive);
        }
        Ok(())
    }

    fn chat_id_from(&self, session_id: &str) -> String {
        session_id
            .strip_prefix(&self.session_prefix)
            .unwrap_or("")
            .to_string()
    }

    pub fn start_server(&self) -> IoHandle {
        let session_id = self.session_id();
        let channel_name = self.name().to_string();
        let host = self.host.clone();
        let port = self.port;
        let inbound_tx = match &self.message_bus {
            Some(mb) => mb.inbound_tx(),
            None => {
                error!("[webui] No message bus configured");
                return IoHandle {
                    join_handle: None,
                    session_id,
                    channel_name,
                };
            }
        };
        let chats = self.chats.clone();
        let channel_id = self.channel_id.clone();
        let session_manager = self.session_manager.clone();
        let shutdown_rx = self.shutdown_rx.as_ref().map(|rx| rx.resubscribe());
        let event_tx = self.event_tx.clone();
        let trace_paths = self.trace_paths.clone();
        let trace_startup_ms = self.trace_startup_ms;
        let config_path = self.config_path.clone();
        let card = self.card.clone();

        let handle = tokio::spawn(async move {
            let state = AppState {
                chats,
                inbound_tx,
                channel_id,
                session_manager,
                index_html: embed::get_content_by_dest("webui/index.html")
                    .unwrap_or("<h1>SlimBot Gateway</h1>")
                    .to_string(),
                traces_html: embed::get_content_by_dest("webui/traces.html")
                    .unwrap_or("<h1>Trace Viewer</h1>")
                    .to_string(),
                config_html: embed::get_content_by_dest("webui/config.html")
                    .unwrap_or("<h1>Config Editor</h1>")
                    .to_string(),
                config_path,
                event_tx,
                trace_paths,
                trace_startup_ms,
                card,
            };

            let app = build_router(Arc::new(state));

            let listener = match tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("[webui] Failed to bind: {}", e);
                    return;
                }
            };
            if let Ok(addr) = listener.local_addr() {
                info!("[webui] Listening on http://{}", addr);
            }

            if let Some(mut rx) = shutdown_rx {
                tokio::select! {
                    _ = rx.recv() => {
                        info!("[webui] Shutdown signal received");
                    }
                    result = axum::serve(listener, app) => {
                        if let Err(e) = result {
                            error!("[webui] Server error: {}", e);
                        }
                    }
                }
            } else if let Err(e) = axum::serve(listener, app).await {
                error!("[webui] Server error: {}", e);
            }
        });

        IoHandle {
            join_handle: Some(handle),
            session_id,
            channel_name,
        }
    }
}

/// Build the axum router for the WebUI channel.
///
/// Extracted from `start_server` so HTTP-level tests can exercise the real
/// route table (handler-direct tests bypass route matching entirely).
fn build_router(state: Arc<AppState>) -> axum::Router {
    // Note: axum 0.7 captures use `:id` (not `{id}` — that is axum 0.8+
    // syntax and would register a literal `{id}` segment that never matches).
    axum::Router::new()
        .route("/", get(index_handler))
        .route("/chats", get(list_chats_handler))
        .route("/chats", post(create_chat_handler))
        .route("/sse", get(sse_handler))
        .route("/message", post(message_handler))
        .route("/session/*chat_id", get(session_history_handler))
        .route("/traces", get(traces_page_handler))
        .route("/traces/api/traces", get(traces_list_handler))
        .route("/traces/api/trace/:id", get(trace_detail_handler))
        .route("/config", get(config_page_handler))
        .route("/config/api/state", get(config_state_handler))
        .route("/config/api/update", post(config_update_handler))
        .route("/json-render/catalog", post(catalog_register_handler))
        .route("/common.css", get(common_css_handler))
        .with_state(state)
}

/// Chat info returned by the chat list endpoint.
#[derive(Debug, Serialize)]
struct ChatInfo {
    chat_id: String,
    message_count: usize,
    created_at_ms: i64,
}

async fn index_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("content-type", "text/html")],
        state.index_html.clone(),
    )
}

/// Serve the shared WebUI stylesheet (used by both the chat and trace pages).
async fn common_css_handler() -> (StatusCode, [(&'static str, &'static str); 1], String) {
    let css = embed::get_content_by_dest("webui/common.css").unwrap_or("");
    (
        StatusCode::OK,
        [("content-type", "text/css")],
        css.to_string(),
    )
}

/// Serve the standalone trace viewer page.
async fn traces_page_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("content-type", "text/html")],
        state.traces_html.clone(),
    )
}

/// List of trace summaries (newest first) plus viewer metadata.
#[derive(serde::Serialize)]
struct TracesListResponse {
    /// True when no file backend is configured / tracing is disabled.
    disabled: bool,
    /// True when a trace file was larger than the size guard (still read).
    too_large: bool,
    /// Trace file startup (epoch ms) — the Gateway's startup time when
    /// `reset_on_start` is on. None when no trace file exists yet.
    startup_ms: Option<i64>,
    traces: Vec<crate::trace_viewer::TraceSummary>,
}

/// Read + parse all trace files once, shared by the list and detail handlers.
/// Returns (spans, too_large).
fn load_trace_spans(paths: &[String]) -> (Vec<crate::trace_viewer::SpanRecord>, bool) {
    let read = crate::trace_viewer::read_trace_files(paths);
    if read.too_large {
        crate::warn_log!(
            "[webui] Trace file exceeded {} bytes; still reading",
            crate::trace_viewer::MAX_TRACE_FILE_BYTES
        );
    }
    (
        crate::trace_viewer::parse_span_lines(&read.content),
        read.too_large,
    )
}

/// `GET /traces/api/traces`: parse the trace file(s), group by trace id, and
/// return summaries sorted by start time descending.
async fn traces_list_handler(State(state): State<Arc<AppState>>) -> axum::Json<TracesListResponse> {
    if state.trace_paths.is_empty() {
        return axum::Json(TracesListResponse {
            disabled: true,
            too_large: false,
            startup_ms: None,
            traces: Vec::new(),
        });
    }
    let startup_ms = state.trace_startup_ms;
    let (spans, too_large) = load_trace_spans(&state.trace_paths);
    let traces = crate::trace_viewer::build_summaries(&spans);
    axum::Json(TracesListResponse {
        disabled: false,
        too_large,
        startup_ms,
        traces,
    })
}

/// `GET /traces/api/trace/{id}`: full span tree for one trace, or 404.
async fn trace_detail_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<axum::Json<crate::trace_viewer::TraceDetail>, StatusCode> {
    if state.trace_paths.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let (spans, _) = load_trace_spans(&state.trace_paths);
    match crate::trace_viewer::build_detail(&id, &spans) {
        Some(detail) => Ok(axum::Json(detail)),
        None => Err(StatusCode::NOT_FOUND),
    }
}

/// Serve the standalone config editor page.
async fn config_page_handler(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (
        StatusCode::OK,
        [("content-type", "text/html")],
        state.config_html.clone(),
    )
}

/// `GET /config/api/state`: schema + current values (secrets masked).
/// Reads the config file fresh so the editor always reflects disk state.
async fn config_state_handler(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let config = crate::config::Config::load_for_preview(&state.config_path).map_err(|e| {
        crate::error!("[webui] Failed to load config for editor: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let schema = crate::config_schema::serialize_schema(&config);
    Ok(axum::Json(serde_json::json!({ "schema": schema })))
}

/// `POST /config/api/update`: apply `[{op, path, value}]` patches and persist.
/// On validation failure the config is NOT written; per-field errors are returned.
async fn config_update_handler(
    State(state): State<Arc<AppState>>,
    body: String,
) -> Result<axum::Json<serde_json::Value>, StatusCode> {
    let ops: Vec<crate::config_schema::PatchOp> = serde_json::from_str(&body).map_err(|e| {
        crate::error!("[webui] Invalid config patch body: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    let mut config = crate::config::Config::load_for_preview(&state.config_path).map_err(|e| {
        crate::error!("[webui] Failed to load config for update: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let errors = crate::config_schema::apply_config_patch(&mut config, &ops).map_err(|e| {
        crate::error!("[webui] Config patch rejected: {}", e);
        StatusCode::BAD_REQUEST
    })?;
    if errors.is_empty() {
        config.save(&state.config_path).map_err(|e| {
            crate::error!("[webui] Failed to save config: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        crate::info!("[webui] Config updated via config editor");
    }
    Ok(axum::Json(serde_json::json!({
        "ok": errors.is_empty(),
        "errors": errors,
    })))
}

async fn list_chats_handler(State(state): State<Arc<AppState>>) -> axum::Json<Vec<ChatInfo>> {
    let prefix = format!("{}:", state.channel_id);
    let mut chats = Vec::new();
    if let Some(sm) = &state.session_manager {
        let guard = sm.lock().await;
        for (session_id, msg_count, created) in guard.list_persisted_sessions(&prefix) {
            let chat_id = session_id
                .strip_prefix(&prefix)
                .unwrap_or(&session_id)
                .to_string();
            chats.push(ChatInfo {
                chat_id,
                message_count: msg_count,
                created_at_ms: created,
            });
        }
    }
    axum::Json(chats)
}

async fn session_history_handler(
    State(state): State<Arc<AppState>>,
    Path(chat_id): Path<String>,
) -> axum::Json<Vec<crate::session::FrontendMessage>> {
    let session_id = format!("{}:{}", state.channel_id, chat_id);
    let mut messages = Vec::new();
    if let Some(sm) = &state.session_manager {
        let guard = sm.lock().await;
        if let Ok(msgs) = guard.load_session_messages(&session_id) {
            messages = msgs;
        }
    }
    // Replay: attach sidecar cards by exact anchor match. A card whose
    // anchor no longer exists (consolidation rewrote it away) simply does
    // not attach — never guessed by index, never appended to the end.
    if let Some(card) = &state.card {
        let records = card.store().load(&chat_id);
        if !records.is_empty() {
            let by_ts: std::collections::HashMap<&str, &serde_json::Value> = records
                .iter()
                .filter(|r| !r.ts.is_empty())
                .map(|r| (r.ts.as_str(), &r.spec))
                .collect();
            for m in messages.iter_mut() {
                if let crate::session::FrontendMessage::Assistant { ts, card, .. } = m
                    && let Some(spec) = by_ts.get(ts.as_str())
                {
                    *card = Some((*spec).clone());
                }
            }
        }
    }
    axum::Json(messages)
}

async fn create_chat_handler(State(state): State<Arc<AppState>>) -> axum::Json<String> {
    let chat_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let session_id = format!("{}:{}", state.channel_id, chat_id);
    if let Some(sm) = &state.session_manager {
        let mut guard = sm.lock().await;
        let _ = guard.get_or_create(&session_id).await;
    }
    axum::Json(chat_id)
}

async fn sse_handler(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
) -> axum::response::sse::Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>>
{
    let chat_id = params.get("chat_id").cloned().unwrap_or_default();

    let (tx, mut rx) = mpsc::channel::<SsePayload>(32);
    {
        let mut guard = state.chats.lock().await;
        guard.entry(chat_id).or_default().push(tx);
    }

    let stream = async_stream::stream! {
        while let Some(payload) = rx.recv().await {
            let event = match payload {
                SsePayload::Message(content) => Event::default().event("message").data(content),
                SsePayload::AgentEvent(json) => Event::default().event("agent_event").data(json),
                SsePayload::Card(json) => Event::default().event("card").data(json),
            };
            yield Ok(event);
        }
    };

    axum::response::sse::Sse::new(stream)
}

async fn message_handler(
    Query(params): Query<HashMap<String, String>>,
    State(state): State<Arc<AppState>>,
    body: String,
) -> StatusCode {
    let chat_id = match params.get("chat_id") {
        Some(id) => id.clone(),
        None => return StatusCode::BAD_REQUEST,
    };

    let session_id = format!("{}:{}", state.channel_id, chat_id);
    let mut hook = TaskHook::new(&session_id);
    if let Some(ref tx) = state.event_tx {
        hook = hook.with_events(tx.clone());
    }
    // Mark this Run as WebUI-originated so the card service may generate a card
    // for it. cron / heartbeat mint their Run ids elsewhere and never appear
    // here, so their runs produce no card even when their events reuse a WebUI
    // session id. Registration is fail-closed: no id here means no card.
    if let Some(card) = &state.card {
        card.register_run(hook.run_id(), &session_id, &chat_id);
    }
    let request = BusRequest {
        session_id,
        content: body,
        channel_inject: None,
        hook,
    };

    match state.inbound_tx.send(request).await {
        Ok(_) => StatusCode::OK,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

/// Body of a Catalog registration upload. The `prompt` is untrusted: it is
/// length-checked by the registry and must never be logged in full.
#[derive(serde::Deserialize)]
struct CatalogRegistration {
    chat_id: String,
    prompt: String,
    #[serde(default)]
    catalog_hash: String,
}

/// `POST /json-render/catalog`: register a chat's Catalog prompt (which also
/// declares the client can render cards). 204 on success, 400 on empty or
/// oversized prompt, 503 when cards are not wired.
async fn catalog_register_handler(State(state): State<Arc<AppState>>, body: String) -> StatusCode {
    let Some(card) = &state.card else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let reg: CatalogRegistration = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(_) => return StatusCode::BAD_REQUEST,
    };
    match card.register_catalog(&reg.chat_id, reg.prompt, reg.catalog_hash) {
        Ok(()) => StatusCode::NO_CONTENT,
        Err(_) => StatusCode::BAD_REQUEST,
    }
}

#[async_trait]
impl Channel for WebuiChannel {
    fn id(&self) -> &str {
        &self.channel_id
    }

    fn name(&self) -> &str {
        "WebUI"
    }

    fn chat_id(&self) -> &str {
        "webui_main"
    }

    async fn read_input(&mut self) -> Result<String> {
        Ok(String::new())
    }

    async fn write_output(&mut self, result: &BusResult) -> Result<()> {
        // Skip empty or whitespace-only content to avoid rendering empty bubbles
        if result.content.trim().is_empty() {
            return Ok(());
        }
        let chat_id = self.chat_id_from(&result.session_id);
        // Body payload is JSON so the frontend can attribute the text to a Run
        // and match a later card event to it by the shared `run_id`.
        let payload = serde_json::json!({
            "run_id": result.run_id,
            "content": result.content,
        });
        self.prune_and_send(&chat_id, SsePayload::Message(payload.to_string()))
            .await
    }

    async fn write_card(&mut self, event: &CardEvent) -> Result<()> {
        let chat_id = self.chat_id_from(event.session_id());
        let json = serde_json::to_string(event)?;
        self.prune_and_send(&chat_id, SsePayload::Card(json)).await
    }

    async fn write_status(&mut self, _session_id: &str, _state: &TaskState) -> Result<()> {
        Ok(())
    }

    async fn write_event(&mut self, event: &AgentEvent) -> Result<()> {
        let chat_id = self.chat_id_from(event.session_id());
        let json = serde_json::to_string(event)?;
        self.prune_and_send(&chat_id, SsePayload::AgentEvent(json))
            .await
    }

    async fn prepare_inject(&self) -> Result<String> {
        Ok(String::new())
    }

    fn start(&self, _inbound_tx: tokio::sync::mpsc::Sender<crate::message_bus::BusRequest>) {
        // Deprecated: use start_with_scheduler instead
    }

    fn start_with_scheduler(&self, _scheduler: &IoScheduler) -> IoHandle {
        self.start_server()
    }
}

pub struct WebuiChannelFactory {
    message_bus: Arc<MessageBus>,
    session_manager: SharedSessionManager,
    shutdown_tx: broadcast::Sender<()>,
    event_tx: broadcast::Sender<AgentEvent>,
    trace_paths: Vec<String>,
    trace_startup_ms: Option<i64>,
    config_path: String,
    card: Option<Arc<CardService>>,
}

impl WebuiChannelFactory {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        message_bus: Arc<MessageBus>,
        session_manager: SharedSessionManager,
        shutdown_tx: broadcast::Sender<()>,
        event_tx: broadcast::Sender<AgentEvent>,
        trace_paths: Vec<String>,
        trace_startup_ms: Option<i64>,
        config_path: String,
        card: Option<Arc<CardService>>,
    ) -> Self {
        Self {
            message_bus,
            session_manager,
            shutdown_tx,
            event_tx,
            trace_paths,
            trace_startup_ms,
            config_path,
            card,
        }
    }
}

impl ChannelFactory for WebuiChannelFactory {
    fn channel_type(&self) -> &str {
        "webui"
    }

    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        let mut ch = WebuiChannel::from_config(config)?;
        ch.set_message_bus(self.message_bus.clone());
        ch.set_session_manager(self.session_manager.clone());
        ch.set_shutdown_rx(self.shutdown_tx.subscribe());
        ch.set_event_tx(self.event_tx.clone());
        ch.set_trace_paths(self.trace_paths.clone());
        ch.set_trace_startup_ms(self.trace_startup_ms);
        ch.set_config_path(self.config_path.clone());
        if let Some(card) = &self.card {
            ch.set_card(card.clone());
        }
        Ok(Box::new(ch))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_webui_channel_from_config_defaults() {
        let config = json!({});
        let ch = WebuiChannel::from_config(&config).unwrap();
        assert_eq!(ch.host, "0.0.0.0");
        assert_eq!(ch.port, 8080);
        assert_eq!(ch.channel_id, "webui");
    }

    #[test]
    fn test_webui_channel_from_config_custom() {
        let config = json!({
            "host": "127.0.0.1",
            "port": 3000
        });
        let ch = WebuiChannel::from_config(&config).unwrap();
        assert_eq!(ch.host, "127.0.0.1");
        assert_eq!(ch.port, 3000);
    }

    #[test]
    fn test_webui_channel_trait_methods() {
        let config = json!({});
        let ch = WebuiChannel::from_config(&config).unwrap();
        assert_eq!(ch.id(), "webui");
        assert_eq!(ch.name(), "WebUI");
        assert_eq!(ch.chat_id(), "webui_main");
        assert_eq!(ch.session_id(), "webui:webui_main");
    }

    #[tokio::test]
    async fn test_webui_read_input_returns_empty() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();
        let result = ch.read_input().await.unwrap();
        assert_eq!(result, "");
    }

    #[tokio::test]
    async fn test_webui_prepare_inject_returns_empty() {
        let config = json!({});
        let ch = WebuiChannel::from_config(&config).unwrap();
        let result = ch.prepare_inject().await.unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_webui_channel_factory() {
        // Create dummy message bus and session manager for factory test
        let (mb, _receivers) = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        let (shutdown_tx, _) = broadcast::channel::<()>(1);
        let (event_tx, _) = broadcast::channel::<AgentEvent>(32);

        let factory = WebuiChannelFactory::new(
            mb,
            sm,
            shutdown_tx,
            event_tx,
            Vec::new(),
            None,
            String::new(),
            None,
        );
        assert_eq!(factory.channel_type(), "webui");

        let config = json!({"host": "127.0.0.1", "port": 9999});
        let channel = factory.create(&config).unwrap();
        assert_eq!(channel.id(), "webui");
        assert_eq!(channel.name(), "WebUI");
    }

    #[tokio::test]
    async fn test_webui_write_output_broadcasts_to_sse() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();

        // Register an SSE subscriber for chat "abc123"
        let mut rx = {
            let mut guard = ch.chats.lock().await;
            let (tx, rx) = mpsc::channel::<SsePayload>(32);
            guard.entry("abc123".to_string()).or_default().push(tx);
            rx
        };

        let result = BusResult {
            session_id: "webui:abc123".to_string(),
            run_id: "t1".to_string(),
            content: "hello from agent".to_string(),
        };

        ch.write_output(&result).await.unwrap();

        // Body payload is JSON carrying the run_id so the frontend can match a
        // later card event to this turn.
        let payload = rx.recv().await.expect("one SSE payload");
        match payload {
            SsePayload::Message(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(v["run_id"], json!("t1"));
                assert_eq!(v["content"], json!("hello from agent"));
            }
            other => panic!("expected Message payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_webui_write_card_broadcasts_card_event() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();
        let mut rx = {
            let mut guard = ch.chats.lock().await;
            let (tx, rx) = mpsc::channel::<SsePayload>(32);
            guard.entry("abc123".to_string()).or_default().push(tx);
            rx
        };

        let event = CardEvent::Completed {
            session_id: "webui:abc123".to_string(),
            run_id: "r7".to_string(),
            spec: json!({"root": "c", "elements": {"c": {"type": "Card"}}}),
        };
        ch.write_card(&event).await.unwrap();

        let payload = rx.recv().await.expect("card delivered to the chat lane");
        match payload {
            SsePayload::Card(json) => {
                let v: serde_json::Value = serde_json::from_str(&json).unwrap();
                assert_eq!(v["type"], json!("completed"));
                assert_eq!(v["run_id"], json!("r7"));
                assert_eq!(v["spec"]["root"], json!("c"));
            }
            other => panic!("expected Card payload, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_webui_write_output_no_subscriber() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();

        let result = BusResult {
            session_id: "webui:nobody".to_string(),
            run_id: "t2".to_string(),
            content: "no one listening".to_string(),
        };

        // Should not panic when no subscribers exist
        ch.write_output(&result).await.unwrap();
    }

    #[tokio::test]
    async fn test_webui_write_output_skips_empty_content() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();

        // Register an SSE subscriber with a receiver we can inspect
        let (tx, mut rx) = mpsc::channel::<SsePayload>(32);
        {
            let mut guard = ch.chats.lock().await;
            guard.entry("abc123".to_string()).or_default().push(tx);
        }

        // Empty content should be skipped
        let empty_result = BusResult {
            session_id: "webui:abc123".to_string(),
            run_id: "t3".to_string(),
            content: "".to_string(),
        };
        ch.write_output(&empty_result).await.unwrap();

        // Whitespace-only content should also be skipped
        let whitespace_result = BusResult {
            session_id: "webui:abc123".to_string(),
            run_id: "t4".to_string(),
            content: "   \n\t  ".to_string(),
        };
        ch.write_output(&whitespace_result).await.unwrap();

        // Nothing should have been sent to the subscriber
        let received = rx.try_recv();
        assert!(
            received.is_err(),
            "empty/whitespace content should not be sent to SSE subscribers"
        );
    }

    #[test]
    fn test_webui_start_server_without_message_bus() {
        let config = json!({});
        let ch = WebuiChannel::from_config(&config).unwrap();
        let handle = ch.start_server();
        // No message bus configured — should return IoHandle with no join_handle
        assert!(handle.join_handle.is_none());
    }

    #[tokio::test]
    async fn test_webui_list_chats_empty() {
        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
            event_tx: None,
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: None,
        });
        let result = list_chats_handler(State(state)).await;
        assert!(result.0.is_empty());
    }

    #[tokio::test]
    async fn test_webui_list_chats_returns_persisted_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("webui:chat1").await.unwrap();
            guard
                .add_message(
                    "webui:chat1",
                    crate::session::Message::user("hello".to_string()),
                )
                .await
                .unwrap();
            guard.persist("webui:chat1").await.unwrap();
        }

        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: Some(sm),
            index_html: String::new(),
            event_tx: None,
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: None,
        });
        let result = list_chats_handler(State(state)).await;
        assert_eq!(result.0.len(), 1);
        assert_eq!(result.0[0].chat_id, "chat1");
        assert_eq!(result.0[0].message_count, 1);
    }

    #[tokio::test]
    async fn test_webui_session_history_handler() {
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("webui:abc").await.unwrap();
            guard
                .add_message("webui:abc", crate::session::Message::user("hi".to_string()))
                .await
                .unwrap();
            guard
                .add_message(
                    "webui:abc",
                    crate::session::Message::assistant(Some("hello".to_string()), None, None, None),
                )
                .await
                .unwrap();
            guard.persist("webui:abc").await.unwrap();
        }

        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: Some(sm),
            index_html: String::new(),
            event_tx: None,
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: None,
        });
        let result = session_history_handler(State(state), Path("abc".to_string())).await;
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn test_webui_create_chat_handler_generates_id() {
        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
            event_tx: None,
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: None,
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async { create_chat_handler(State(state)).await });
        // Should return a non-empty chat_id
        assert!(!result.0.is_empty());
        // Should be 8 chars (uuid prefix)
        assert_eq!(result.0.len(), 8);
    }

    #[test]
    fn test_embed_get_content_by_dest_finds_webui_index() {
        // Regression: get_content looks up by name ("index.html") but we need
        // to find by destination path ("webui/index.html"). This verifies
        // get_content_by_dest returns the full HTML, not the fallback.
        let html = crate::embed::get_content_by_dest("webui/index.html");
        assert!(html.is_some(), "webui/index.html should be embedded");
        let html = html.unwrap();
        assert!(
            html.contains("<!DOCTYPE html>"),
            "should contain HTML doctype"
        );
        assert!(
            html.contains("EventSource"),
            "should contain SSE client code"
        );
        assert!(html.contains("/message"), "should contain message endpoint");
    }

    #[test]
    fn test_embed_get_content_by_dest_missing() {
        let result = crate::embed::get_content_by_dest("nonexistent/file.html");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_webui_server_starts_and_shuts_down() {
        let (mb, _receivers) = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let mut ch = WebuiChannel::from_config(&json!({"host": "127.0.0.1", "port": 0})).unwrap();
        ch.set_message_bus(mb);
        ch.set_session_manager(sm);
        ch.set_shutdown_rx(shutdown_tx.subscribe());

        let handle = ch.start_server();
        assert!(handle.join_handle.is_some());

        // Give server time to bind
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Send shutdown signal
        shutdown_tx.send(()).unwrap();

        // Server task should complete (not hang)
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            handle.join_handle.unwrap(),
        )
        .await;
        assert!(
            result.is_ok(),
            "server should exit within 3s of shutdown signal"
        );
    }

    #[tokio::test]
    async fn test_webui_server_starts_without_shutdown_rx() {
        // Server without shutdown_rx should still start (bind to port).
        // We can't easily test it shutting down, but verify it starts without panic.
        let (mb, _receivers) = MessageBus::new();
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));

        let mut ch = WebuiChannel::from_config(&json!({"host": "127.0.0.1", "port": 0})).unwrap();
        ch.set_message_bus(mb);
        ch.set_session_manager(sm);
        // No set_shutdown_rx call

        let handle = ch.start_server();
        assert!(handle.join_handle.is_some());

        // Brief wait then abort
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Drop the handle — server task will keep running until MB closes,
        // which is fine for this test.
        drop(handle);
    }

    // ── Trace handlers ──

    /// Build a minimal AppState for trace handler tests.
    fn trace_state(trace_paths: Vec<String>) -> Arc<AppState> {
        Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
            traces_html: "<h1>Trace Viewer</h1>".to_string(),
            config_html: String::new(),
            config_path: String::new(),
            event_tx: None,
            trace_paths,
            trace_startup_ms: None,
            card: None,
        })
    }

    /// Write a realistic two-span trace to a file and return its path.
    fn write_trace_file(dir: &std::path::Path, trace_id: &str, status: &str) -> String {
        let path = dir.join(format!("{}.jsonl", trace_id));
        let root = serde_json::json!({
            "trace_id": trace_id,
            "span_id": "root001",
            "parent_span_id": null,
            "name": "invoke_agent slimbot",
            "start": "2026-08-15T10:00:00.000000Z",
            "end": "2026-08-15T10:00:01.000000Z",
            "duration_us": 1000000,
            "status": status,
            "attributes": {"gen_ai.conversation.id": "webui:chat1"},
        });
        let child = serde_json::json!({
            "trace_id": trace_id,
            "span_id": "child01",
            "parent_span_id": "root001",
            "name": "chat",
            "start": "2026-08-15T10:00:00.100000Z",
            "end": "2026-08-15T10:00:00.500000Z",
            "duration_us": 400000,
            "status": "ok",
            "attributes": {"http.request.body": "{\"model\":\"gpt-4o\"}"},
        });
        let content = format!("{}\n{}", root, child);
        std::fs::write(&path, content).unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn test_webui_traces_page_returns_html() {
        let state = trace_state(Vec::new());
        let (status, headers, body) = traces_page_handler(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[0], ("content-type", "text/html"));
        assert!(body.contains("Trace Viewer"));
    }

    #[tokio::test]
    async fn test_webui_traces_list_disabled_when_no_paths() {
        let state = trace_state(Vec::new());
        let res = traces_list_handler(State(state)).await;
        assert!(res.0.disabled);
        assert!(res.0.traces.is_empty());
    }

    #[tokio::test]
    async fn test_webui_traces_list_returns_sorted_summaries() {
        let tmp = tempfile::tempdir().unwrap();
        let older = write_trace_file(tmp.path(), "trace-a", "ok");
        let newer = write_trace_file(tmp.path(), "trace-b", "error");
        let state = trace_state(vec![older, newer]);
        let res = traces_list_handler(State(state)).await;
        assert!(!res.0.disabled);
        assert_eq!(res.0.traces.len(), 2);
        // Newest first.
        assert_eq!(res.0.traces[0].full_trace_id, "trace-b");
        assert_eq!(res.0.traces[1].full_trace_id, "trace-a");
        assert_eq!(res.0.traces[0].status, "error");
        assert_eq!(res.0.traces[0].span_count, 2);
        assert_eq!(res.0.traces[0].root_name, "invoke_agent slimbot");
        assert!(!res.0.traces[0].running);
    }

    #[tokio::test]
    async fn test_webui_trace_detail_returns_span_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_trace_file(tmp.path(), "trace-d", "ok");
        let state = trace_state(vec![path]);
        let res = trace_detail_handler(State(state), Path("trace-d".to_string())).await;
        let detail = res.unwrap().0;
        assert_eq!(detail.trace_id, "trace-d");
        assert_eq!(detail.span_count, 2);
        let root = detail.root.unwrap();
        assert_eq!(root.span.name, "invoke_agent slimbot");
        assert_eq!(root.children.len(), 1);
        assert_eq!(root.children[0].span.name, "chat");
        assert_eq!(
            root.children[0].span.attributes["http.request.body"],
            "{\"model\":\"gpt-4o\"}"
        );
    }

    #[tokio::test]
    async fn test_webui_trace_detail_404_for_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_trace_file(tmp.path(), "trace-e", "ok");
        let state = trace_state(vec![path]);
        let res = trace_detail_handler(State(state), Path("missing".to_string())).await;
        assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn test_webui_trace_detail_404_when_disabled() {
        let state = trace_state(Vec::new());
        let res = trace_detail_handler(State(state), Path("anything".to_string())).await;
        assert_eq!(res.err(), Some(StatusCode::NOT_FOUND));
    }

    #[tokio::test]
    async fn test_webui_trace_detail_route_matches_full_trace_id() {
        // Regression: the detail endpoint must be reachable over HTTP with the
        // full trace id from the list. Handler-direct tests bypass route
        // matching, so a route table bug (e.g. `{id}` instead of `:id` under
        // axum 0.7) would slip through them and surface as a 404 here.
        let tmp = tempfile::tempdir().unwrap();
        let path = write_trace_file(tmp.path(), "r0ute-id-0123456789abcdef", "ok");
        let state = trace_state(vec![path]);

        let app = build_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::new();
        let url = format!(
            "http://127.0.0.1:{}/traces/api/trace/r0ute-id-0123456789abcdef",
            addr.port()
        );
        let mut last_status = None;
        for _ in 0..50 {
            if let Ok(res) = client.get(&url).send().await {
                last_status = Some(res.status());
                if res.status().is_success() {
                    let body: serde_json::Value = res.json().await.unwrap();
                    assert_eq!(body["full_trace_id"], "r0ute-id-0123456789abcdef");
                    assert_eq!(body["span_count"], 2);
                    return;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!(
            "trace detail route did not match full trace id (last HTTP {:?})",
            last_status
        );
    }

    // ── Config editor handlers ──

    /// Build a minimal AppState for config handler tests.
    fn config_state(config_path: String) -> Arc<AppState> {
        Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
            traces_html: String::new(),
            config_html: "<h1>Config Editor</h1>".to_string(),
            config_path,
            event_tx: None,
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: None,
        })
    }

    /// Write a minimal valid config file and return its path.
    fn write_config_file(dir: &std::path::Path) -> String {
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
                "agent": {"provider": "default", "max_iterations": 40},
                "providers": {"default": {"type": "openai", "api_key": "sk-test", "model": "gpt-4o"}},
                "tools": [{"name": "shell", "enabled": true}],
                "channels": {"webui": {"enabled": true, "host": "127.0.0.1", "port": 8080}}
            }"#,
        )
        .unwrap();
        path.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn test_webui_config_page_returns_html() {
        let state = config_state(String::new());
        let (status, headers, body) = config_page_handler(State(state)).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[0], ("content-type", "text/html"));
        assert!(body.contains("Config Editor"));
    }

    #[tokio::test]
    async fn test_webui_config_state_returns_schema_with_masked_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path());
        let state = config_state(path);
        let res = config_state_handler(State(state)).await.unwrap();
        let schema = &res.0["schema"];
        let sections = schema["sections"].as_array().unwrap();
        assert!(sections.iter().any(|s| s["name"] == "agent"));
        assert!(sections.iter().any(|s| s["name"] == "gateway.cron"));
        assert!(sections.iter().any(|s| s["name"] == "providers"));
        assert!(sections.iter().any(|s| s["name"] == "tools"));
        assert!(sections.iter().any(|s| s["name"] == "channels"));

        // api_key must be masked (null value) in provider items.
        let providers = sections.iter().find(|s| s["name"] == "providers").unwrap();
        let item = &providers["items"][0];
        assert_eq!(item["key"], "default");
        let fields = item["fields"].as_array().unwrap();
        let api_key = fields.iter().find(|f| f["name"] == "api_key").unwrap();
        assert_eq!(api_key["secret"], true);
        assert_eq!(api_key["value"], serde_json::Value::Null);
        let model = fields.iter().find(|f| f["name"] == "model").unwrap();
        assert_eq!(model["value"], "gpt-4o");
    }

    #[tokio::test]
    async fn test_webui_config_update_applies_patch_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path());
        let state = config_state(path.clone());
        let body = r#"[{"op":"set","path":"agent.max_iterations","value":55}]"#;
        let res = config_update_handler(State(state), body.to_string())
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        assert!(res.0["errors"].as_array().unwrap().is_empty());
        // Reload from disk: the persisted config reflects the patch.
        let reloaded = crate::config::Config::load_for_preview(&path).unwrap();
        assert_eq!(reloaded.agent.max_iterations, 55);
    }

    #[tokio::test]
    async fn test_webui_config_update_rejects_invalid_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path());
        let before = std::fs::read_to_string(&path).unwrap();
        let state = config_state(path.clone());
        let body = r#"[{"op":"set","path":"agent.provider","value":"ghost"}]"#;
        let res = config_update_handler(State(state), body.to_string())
            .await
            .unwrap();
        assert_eq!(res.0["ok"], false);
        let errors = res.0["errors"].as_array().unwrap();
        assert!(!errors.is_empty());
        assert_eq!(errors[0]["path"], "agent.provider");
        // The config file must be untouched when validation fails.
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn test_webui_config_update_bad_body_is_400() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path());
        let state = config_state(path);
        let res = config_update_handler(State(state), "not json".to_string()).await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
    }

    #[tokio::test]
    async fn test_webui_config_state_missing_file_is_500() {
        let state = config_state("/nonexistent/config.json".to_string());
        let res = config_state_handler(State(state)).await;
        assert_eq!(res.err(), Some(StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[tokio::test]
    async fn test_webui_config_update_unknown_path_is_400() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_config_file(tmp.path());
        let state = config_state(path);
        let body = r#"[{"op":"set","path":"agent.nope","value":1}]"#;
        let res = config_update_handler(State(state), body.to_string()).await;
        assert_eq!(res.err(), Some(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn test_embed_get_content_by_dest_finds_webui_config() {
        // The config editor page must be embedded and served at /config.
        let config = crate::embed::get_content_by_dest("webui/config.html");
        assert!(config.is_some(), "webui/config.html should be embedded");
        let html = config.unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("/config/api/state"), "should fetch schema");
        assert!(html.contains("/config/api/update"), "should post patches");
    }

    #[test]
    fn test_embed_get_content_by_dest_finds_webui_traces_and_common_css() {
        // The trace viewer page and shared CSS must be embedded.
        let traces = crate::embed::get_content_by_dest("webui/traces.html");
        assert!(traces.is_some(), "webui/traces.html should be embedded");
        assert!(traces.unwrap().contains("<!DOCTYPE html>"));

        let css = crate::embed::get_content_by_dest("webui/common.css");
        assert!(css.is_some(), "webui/common.css should be embedded");
        assert!(css.unwrap().contains("--accent"));
    }

    #[test]
    fn test_traces_html_guide_uses_real_chars_not_html_entities() {
        // Regression: renderNode() builds the span-tree indentation guide and
        // passes it through esc(), which HTML-escapes `&`. The guide therefore
        // must be real characters (non-breaking spaces), not HTML entity
        // strings like `&nbsp;` — those rendered literally as
        // `&nbsp;&nbsp;&nbsp;` text next to every child span in the tree.
        let traces = crate::embed::get_content_by_dest("webui/traces.html")
            .expect("webui/traces.html should be embedded");

        // Each nesting level indents with three non-breaking spaces.
        assert!(
            traces.contains("'\\u00A0\\u00A0\\u00A0'"),
            "renderNode guide must be real nbsp chars"
        );
        // Plain space indentation only: no box-drawing decoration and no
        // entity forms that esc() would double-escape.
        assert!(
            !traces.contains("\\u251C\\u2500")
                && !traces.contains("&nbsp;")
                && !traces.contains("&#9500;")
                && !traces.contains("&#9472;"),
            "span-tree guide must be plain nbsp indentation only"
        );
    }

    // ── Catalog registration endpoint (seam 1: WebUI HTTP) ──

    /// AppState wired with a live CardService, for catalog handler tests.
    fn catalog_state() -> (Arc<AppState>, Arc<CardService>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(session_dir.clone()).unwrap(),
        ));
        let (card_tx, _card_rx) = mpsc::channel::<CardEvent>(8);
        let service = Arc::new(CardService::new(
            Arc::new(crate::provider::MockProvider::new()),
            sm,
            crate::card::CardStore::new(session_dir),
            card_tx,
        ));
        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            event_tx: None,
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: Some(service.clone()),
        });
        (state, service, dir)
    }

    #[tokio::test]
    async fn test_catalog_registration_returns_204() {
        let (state, service, _dir) = catalog_state();
        let body =
            json!({ "chat_id": "c1", "prompt": "use Card/Text", "catalog_hash": "h" }).to_string();
        let status = catalog_register_handler(State(state), body).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(service.registry().get("c1").is_some());
    }

    #[tokio::test]
    async fn test_catalog_rejects_empty_prompt() {
        let (state, _service, _dir) = catalog_state();
        let body = json!({ "chat_id": "c1", "prompt": "   ", "catalog_hash": "h" }).to_string();
        assert_eq!(
            catalog_register_handler(State(state), body).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn test_catalog_rejects_oversize_prompt() {
        let (state, _service, _dir) = catalog_state();
        let big = "x".repeat(crate::card::MAX_PROMPT_BYTES + 1);
        let body = json!({ "chat_id": "c1", "prompt": big, "catalog_hash": "h" }).to_string();
        assert_eq!(
            catalog_register_handler(State(state), body).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn test_catalog_rejects_malformed_body() {
        let (state, _service, _dir) = catalog_state();
        assert_eq!(
            catalog_register_handler(State(state), "not json".to_string()).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn test_catalog_overwrites_same_chat() {
        let (state, service, _dir) = catalog_state();
        let b1 = json!({ "chat_id": "c1", "prompt": "first", "catalog_hash": "h1" }).to_string();
        let b2 = json!({ "chat_id": "c1", "prompt": "second", "catalog_hash": "h2" }).to_string();
        assert_eq!(
            catalog_register_handler(State(state.clone()), b1).await,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            catalog_register_handler(State(state), b2).await,
            StatusCode::NO_CONTENT
        );
        let reg = service.registry().get("c1").unwrap();
        assert_eq!(reg.prompt, "second");
        assert_eq!(reg.catalog_hash, "h2");
    }

    #[tokio::test]
    async fn test_catalog_503_when_cards_not_wired() {
        // `config_state` builds an AppState with `card: None`.
        let state = config_state(String::new());
        let body = json!({ "chat_id": "c1", "prompt": "p", "catalog_hash": "h" }).to_string();
        assert_eq!(
            catalog_register_handler(State(state), body).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    // ── Card replay via session history (seam 1) ──

    /// AppState wired with both a SessionManager and a CardService, so the
    /// history handler can attach sidecar cards to persisted messages.
    fn replay_state() -> (
        Arc<AppState>,
        Arc<CardService>,
        Arc<tokio::sync::Mutex<crate::session::SessionManager>>,
        tempfile::TempDir,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let session_dir = dir.path().join("sessions");
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(session_dir.clone()).unwrap(),
        ));
        let (card_tx, _card_rx) = mpsc::channel::<CardEvent>(8);
        let service = Arc::new(CardService::new(
            Arc::new(crate::provider::MockProvider::new()),
            sm.clone(),
            crate::card::CardStore::new(session_dir),
            card_tx,
        ));
        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().0.inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: Some(sm.clone()),
            index_html: String::new(),
            event_tx: None,
            traces_html: String::new(),
            config_html: String::new(),
            config_path: String::new(),
            trace_paths: Vec::new(),
            trace_startup_ms: None,
            card: Some(service.clone()),
        });
        (state, service, sm, dir)
    }

    #[tokio::test]
    async fn test_session_history_attaches_cards_by_exact_timestamp() {
        let (state, service, sm, _dir) = replay_state();
        let anchor_ts;
        {
            let mut guard = sm.lock().await;
            guard.get_or_create("webui:replay1").await.unwrap();
            guard
                .add_message(
                    "webui:replay1",
                    crate::session::Message::user("q?".to_string()),
                )
                .await
                .unwrap();
            guard
                .add_message(
                    "webui:replay1",
                    crate::session::Message::assistant(
                        Some("answer.".to_string()),
                        None,
                        None,
                        None,
                    ),
                )
                .await
                .unwrap();
            let msgs = guard.get_messages("webui:replay1").await;
            anchor_ts = msgs.last().unwrap().timestamp().to_string();
            guard.persist("webui:replay1").await.unwrap();
        }
        assert!(!anchor_ts.is_empty(), "add_message must stamp the anchor");

        let spec = json!({"root": "r", "elements": {"r": {"type": "Card", "props": {}}}});
        service
            .store()
            .append(
                "replay1",
                &crate::card::CardRecord {
                    ts: anchor_ts.clone(),
                    created_at_ms: 1,
                    spec: spec.clone(),
                },
            )
            .unwrap();
        // Orphan record whose anchor matches no message: must not attach.
        service
            .store()
            .append(
                "replay1",
                &crate::card::CardRecord {
                    ts: "ghost".to_string(),
                    created_at_ms: 2,
                    spec: json!({"root": "g", "elements": {}}),
                },
            )
            .unwrap();

        let out = session_history_handler(State(state), Path("replay1".to_string())).await;
        assert_eq!(out.0.len(), 2);
        match &out.0[0] {
            crate::session::FrontendMessage::User { ts, .. } => {
                assert!(!ts.is_empty(), "every message exposes its anchor")
            }
            other => panic!("expected user first, got {other:?}"),
        }
        match &out.0[1] {
            crate::session::FrontendMessage::Assistant { content, ts, card } => {
                assert_eq!(content, "answer.");
                assert_eq!(ts, &anchor_ts);
                assert_eq!(card.as_ref().unwrap(), &spec);
            }
            other => panic!("expected assistant carrying the card, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_session_history_unknown_chat_is_empty_not_error() {
        let (state, _service, _sm, _dir) = replay_state();
        let out = session_history_handler(State(state), Path("nope".to_string())).await;
        assert!(out.0.is_empty());
    }
}

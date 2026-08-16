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
use crate::embed;
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::{AgentEvent, SharedSessionManager, TaskHook, TaskState};
use crate::{error, info};

struct AppState {
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<SsePayload>>>>>,
    inbound_tx: mpsc::Sender<BusRequest>,
    channel_id: String,
    session_manager: Option<SharedSessionManager>,
    index_html: String,
    traces_html: String,
    event_tx: Option<broadcast::Sender<AgentEvent>>,
    /// Enabled file-backend trace file paths (empty when tracing/file backend off).
    trace_paths: Vec<String>,
    /// Epoch ms when the current process reset its trace file(s); `None` when
    /// tracing is off or `reset_on_start` is false (no clean startup boundary).
    trace_startup_ms: Option<i64>,
}

/// SSE payload with event type discrimination.
#[derive(Clone)]
enum SsePayload {
    Message(String),
    AgentEvent(String),
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

    /// Prune disconnected SSE subscribers for a chat and send payload to remaining.
    async fn prune_and_send(&mut self, chat_id: &str, payload: SsePayload) -> Result<()> {
        let senders = {
            let mut guard = self.chats.lock().await;
            guard.remove(chat_id)
        };
        let Some(senders) = senders else {
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

        let handle = tokio::spawn(async move {
            let index_html = embed::get_content_by_dest("webui/index.html")
                .unwrap_or("<h1>SlimBot Gateway</h1>")
                .to_string();
            let traces_html = embed::get_content_by_dest("webui/traces.html")
                .unwrap_or("<h1>Trace Viewer</h1>")
                .to_string();

            let state = AppState {
                chats,
                inbound_tx,
                channel_id,
                session_manager,
                index_html,
                traces_html,
                event_tx,
                trace_paths,
                trace_startup_ms,
            };

            let app = axum::Router::new()
                .route("/", get(index_handler))
                .route("/chats", get(list_chats_handler))
                .route("/chats", post(create_chat_handler))
                .route("/sse", get(sse_handler))
                .route("/message", post(message_handler))
                .route("/session/*chat_id", get(session_history_handler))
                .route("/traces", get(traces_page_handler))
                .route("/traces/api/traces", get(traces_list_handler))
                .route("/traces/api/trace/{id}", get(trace_detail_handler))
                .route("/common.css", get(common_css_handler))
                .with_state(Arc::new(state));

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
                SsePayload::Message(content) => Event::default().data(content),
                SsePayload::AgentEvent(json) => Event::default().event("agent_event").data(json),
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
        self.prune_and_send(&chat_id, SsePayload::Message(result.content.clone()))
            .await
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
}

impl WebuiChannelFactory {
    pub fn new(
        message_bus: Arc<MessageBus>,
        session_manager: SharedSessionManager,
        shutdown_tx: broadcast::Sender<()>,
        event_tx: broadcast::Sender<AgentEvent>,
        trace_paths: Vec<String>,
        trace_startup_ms: Option<i64>,
    ) -> Self {
        Self {
            message_bus,
            session_manager,
            shutdown_tx,
            event_tx,
            trace_paths,
            trace_startup_ms,
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

        let factory = WebuiChannelFactory::new(mb, sm, shutdown_tx, event_tx, Vec::new(), None);
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
        {
            let mut guard = ch.chats.lock().await;
            let (tx, _rx) = mpsc::channel::<SsePayload>(32);
            guard.entry("abc123".to_string()).or_default().push(tx);
        }

        let result = BusResult {
            session_id: "webui:abc123".to_string(),
            task_id: "t1".to_string(),
            content: "hello from agent".to_string(),
        };

        ch.write_output(&result).await.unwrap();
        // No panic — output was sent to registered SSE subscribers
    }

    #[tokio::test]
    async fn test_webui_write_output_no_subscriber() {
        let config = json!({});
        let mut ch = WebuiChannel::from_config(&config).unwrap();

        let result = BusResult {
            session_id: "webui:nobody".to_string(),
            task_id: "t2".to_string(),
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
            task_id: "t3".to_string(),
            content: "".to_string(),
        };
        ch.write_output(&empty_result).await.unwrap();

        // Whitespace-only content should also be skipped
        let whitespace_result = BusResult {
            session_id: "webui:abc123".to_string(),
            task_id: "t4".to_string(),
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
            trace_paths: Vec::new(),
            trace_startup_ms: None,
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
            trace_paths: Vec::new(),
            trace_startup_ms: None,
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
            trace_paths: Vec::new(),
            trace_startup_ms: None,
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
            trace_paths: Vec::new(),
            trace_startup_ms: None,
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
            event_tx: None,
            trace_paths,
            trace_startup_ms: None,
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
}

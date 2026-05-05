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
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::{SharedSessionManager, TaskHook, TaskState};
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::{error, info};
use crate::embed;

struct AppState {
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<String>>>>>,
    inbound_tx: mpsc::Sender<BusRequest>,
    channel_id: String,
    session_manager: Option<SharedSessionManager>,
    index_html: String,
}

pub struct WebuiChannel {
    host: String,
    port: u16,
    channel_id: String,
    message_bus: Option<Arc<MessageBus>>,
    session_manager: Option<SharedSessionManager>,
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<String>>>>>,
    shutdown_rx: Option<broadcast::Receiver<()>>,
}

impl WebuiChannel {
    pub fn from_config(config: &serde_json::Value) -> Result<Self> {
        let host = config
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0")
            .to_string();
        let port = config
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(8080) as u16;
        Ok(Self {
            host,
            port,
            channel_id: "webui".to_string(),
            message_bus: None,
            session_manager: None,
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            shutdown_rx: None,
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

        let handle = tokio::spawn(async move {
            let index_html =
                embed::get_content_by_dest("webui/index.html").unwrap_or("<h1>SlimBot Gateway</h1>").to_string();

            let state = AppState {
                chats,
                inbound_tx,
                channel_id,
                session_manager,
                index_html,
            };

            let app = axum::Router::new()
                .route("/", get(index_handler))
                .route("/chats", get(list_chats_handler))
                .route("/chats", post(create_chat_handler))
                .route("/sse", get(sse_handler))
                .route("/message", post(message_handler))
                .route("/session/*chat_id", get(session_history_handler))
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

async fn index_handler(State(state): State<Arc<AppState>>) -> (StatusCode, [(&'static str, &'static str); 1], String) {
    (StatusCode::OK, [("content-type", "text/html")], state.index_html.clone())
}

async fn list_chats_handler(State(state): State<Arc<AppState>>) -> axum::Json<Vec<ChatInfo>> {
    let prefix = format!("{}:", state.channel_id);
    let mut chats = Vec::new();
    if let Some(sm) = &state.session_manager {
        let guard = sm.lock().await;
        for (session_id, msg_count, created) in guard.list_persisted_sessions(&prefix) {
            let chat_id = session_id.strip_prefix(&prefix).unwrap_or(&session_id).to_string();
            chats.push(ChatInfo { chat_id, message_count: msg_count, created_at_ms: created });
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
) -> axum::response::sse::Sse<
    impl futures::Stream<Item = Result<Event, std::convert::Infallible>>,
> {
    let chat_id = params.get("chat_id").cloned().unwrap_or_default();

    let (tx, mut rx) = mpsc::channel::<String>(32);
    {
        let mut guard = state.chats.lock().await;
        guard.entry(chat_id).or_default().push(tx);
    }

    let stream = async_stream::stream! {
        while let Some(content) = rx.recv().await {
            yield Ok(Event::default().data(content));
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
    let hook = TaskHook::new(&session_id);
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
        let chat_id = result
            .session_id
            .strip_prefix("webui:")
            .unwrap_or("")
            .to_string();
        let guard = self.chats.lock().await;
        if let Some(senders) = guard.get(&chat_id) {
            for tx in senders {
                let _ = tx.send(result.content.clone()).await;
            }
        }
        Ok(())
    }

    async fn write_status(&mut self, _session_id: &str, _state: &TaskState) -> Result<()> {
        Ok(())
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
}

impl WebuiChannelFactory {
    pub fn new(
        message_bus: Arc<MessageBus>,
        session_manager: SharedSessionManager,
        shutdown_tx: broadcast::Sender<()>,
    ) -> Self {
        Self {
            message_bus,
            session_manager,
            shutdown_tx,
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
        let mb = Arc::new(MessageBus::new());
        let tmp = tempfile::tempdir().unwrap();
        let sm = Arc::new(tokio::sync::Mutex::new(
            crate::session::SessionManager::new(tmp.path().join("sessions")).unwrap(),
        ));
        let (shutdown_tx, _) = broadcast::channel::<()>(1);

        let factory = WebuiChannelFactory::new(mb, sm, shutdown_tx);
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
            let (tx, _rx) = mpsc::channel::<String>(32);
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
            inbound_tx: MessageBus::new().inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
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
            guard.add_message("webui:chat1", crate::session::Message::user("hello".to_string())).await.unwrap();
            guard.persist("webui:chat1").await.unwrap();
        }

        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: Some(sm),
            index_html: String::new(),
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
            guard.add_message("webui:abc", crate::session::Message::user("hi".to_string())).await.unwrap();
            guard.add_message("webui:abc", crate::session::Message::assistant(Some("hello".to_string()), None)).await.unwrap();
            guard.persist("webui:abc").await.unwrap();
        }

        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: Some(sm),
            index_html: String::new(),
        });
        let result = session_history_handler(State(state), Path("abc".to_string())).await;
        assert_eq!(result.0.len(), 2);
    }

    #[test]
    fn test_webui_create_chat_handler_generates_id() {
        let state = Arc::new(AppState {
            chats: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            inbound_tx: MessageBus::new().inbound_tx(),
            channel_id: "webui".to_string(),
            session_manager: None,
            index_html: String::new(),
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(async {
            create_chat_handler(State(state)).await
        });
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
        assert!(html.contains("<!DOCTYPE html>"), "should contain HTML doctype");
        assert!(html.contains("EventSource"), "should contain SSE client code");
        assert!(html.contains("/message"), "should contain message endpoint");
    }

    #[test]
    fn test_embed_get_content_by_dest_missing() {
        let result = crate::embed::get_content_by_dest("nonexistent/file.html");
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_webui_server_starts_and_shuts_down() {
        let mb = Arc::new(MessageBus::new());
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
        ).await;
        assert!(result.is_ok(), "server should exit within 3s of shutdown signal");
    }

    #[tokio::test]
    async fn test_webui_server_starts_without_shutdown_rx() {
        // Server without shutdown_rx should still start (bind to port).
        // We can't easily test it shutting down, but verify it starts without panic.
        let mb = Arc::new(MessageBus::new());
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
}

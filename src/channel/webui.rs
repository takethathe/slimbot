use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::sse::Event,
    routing::{get, post},
};
use tokio::sync::mpsc;

use super::{Channel, ChannelFactory};
use crate::message_bus::{BusRequest, BusResult, MessageBus};
use crate::session::{SharedSessionManager, TaskHook, TaskState};
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::{debug, error, info};
use crate::embed;

struct WebuiState {
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<String>>>>>,
    inbound_tx: mpsc::Sender<BusRequest>,
    channel_id: String,
}

pub struct WebuiChannel {
    host: String,
    port: u16,
    channel_id: String,
    message_bus: Option<Arc<MessageBus>>,
    session_manager: Option<SharedSessionManager>,
    chats: Arc<tokio::sync::Mutex<HashMap<String, Vec<mpsc::Sender<String>>>>>,
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
        })
    }

    pub fn set_message_bus(&mut self, mb: Arc<MessageBus>) {
        self.message_bus = Some(mb);
    }

    pub fn set_session_manager(&mut self, sm: SharedSessionManager) {
        self.session_manager = Some(sm);
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
        let channel_id_for_routes = channel_id.clone();
        let session_manager = self.session_manager.clone();

        let handle = tokio::spawn(async move {
            info!("[webui] Starting server on {}:{}", host, port);

            let state = Arc::new(WebuiState {
                chats,
                inbound_tx,
                channel_id,
            });

            let index_html =
                embed::get_content("webui/index.html").unwrap_or("<h1>SlimBot Gateway</h1>");

            let app = axum::Router::new()
                .route(
                    "/",
                    get(move || {
                        let html = index_html.to_string();
                        async move { (StatusCode::OK, [("content-type", "text/html")], html) }
                    }),
                )
                .route(
                    "/chats",
                    get({
                        let sm = session_manager.clone();
                        let cid = channel_id_for_routes.clone();
                        move || list_chats_handler(sm, cid)
                    }),
                )
                .route(
                    "/chats",
                    post({
                        let sm = session_manager.clone();
                        let cid = channel_id_for_routes.clone();
                        move || create_chat_handler(sm, cid)
                    }),
                )
                .route("/sse", get(sse_handler))
                .route("/message", post(message_handler))
                .with_state((state, session_manager));

            let listener = match tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("[webui] Failed to bind: {}", e);
                    return;
                }
            };
            if let Err(e) = axum::serve(listener, app).await {
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

async fn list_chats_handler(
    sm: Option<SharedSessionManager>,
    channel_id: String,
) -> axum::Json<Vec<String>> {
    let prefix = format!("{}:", channel_id);
    let mut chat_ids = Vec::new();
    if let Some(sm) = sm {
        let guard = sm.lock().await;
        for session_id in guard.list_session_ids(&prefix) {
            chat_ids.push(session_id);
        }
    }
    axum::Json(chat_ids)
}

async fn create_chat_handler(
    sm: Option<SharedSessionManager>,
    channel_id: String,
) -> axum::Json<String> {
    let chat_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let session_id = format!("{}:{}", channel_id, chat_id);
    if let Some(sm) = sm {
        let mut guard = sm.lock().await;
        let _ = guard.get_or_create(&session_id).await;
    }
    axum::Json(chat_id)
}

async fn sse_handler(
    Query(params): Query<HashMap<String, String>>,
    State((state, _sm)): State<(Arc<WebuiState>, Option<SharedSessionManager>)>,
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
    State((state, _sm)): State<(Arc<WebuiState>, Option<SharedSessionManager>)>,
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
}

impl WebuiChannelFactory {
    pub fn new(message_bus: Arc<MessageBus>, session_manager: SharedSessionManager) -> Self {
        Self {
            message_bus,
            session_manager,
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

        let factory = WebuiChannelFactory::new(mb, sm);
        assert_eq!(factory.channel_type(), "webui");

        let config = json!({"host": "127.0.0.1", "port": 9999});
        let channel = factory.create(&config).unwrap();
        assert_eq!(channel.id(), "webui");
        assert_eq!(channel.name(), "WebUI");
    }
}

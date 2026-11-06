//! Feishu/Lark channel — the first IM channel (ticket 27).
//!
//! A native `Channel` riding the pinned community crate
//! `larksuite-oapi-sdk-rs` (ADR 0017): an enterprise self-built app held open by
//! a single WebSocket long connection that the gateway owns. This file is the
//! first IM channel, so it keeps only the p2p text round trip — a message in
//! starts one ReAct run and the answer goes back on the same chat. Streaming
//! cards and media ride on top of this in ticket 28.
//!
//! The network is isolated behind two seams ([`FeishuTransport`] for the inbound
//! long connection, [`FeishuSender`] for the outbound send) so every decision
//! this channel makes — owner binding, group-off, session identity, the round
//! trip, and the privacy guardrails — is testable without Feishu credentials.
//!
//! Privacy (ADR 0017): the adapter never logs a message body, an `open_id`, a
//! `tenant_key`, the app secret, or a token. Logs carry only fixed words, chat
//! *counts*, and non-identifying state.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc};

use super::{Channel, ChannelFactory};
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::log::LogLevel;
use crate::message_bus::{BusRequest, BusResult};
use crate::session::{TaskHook, TaskState};
use crate::{debug, error, info};

/// The channel id and session prefix. A session id is `{CHANNEL_ID}:{chat_id}`
/// — two segments, one per conversation, with no per-user fork.
const CHANNEL_ID: &str = "feishu";

/// Chat types Feishu reports. Only a p2p DM is handled in v1; everything else
/// (notably `group`) is dropped entirely.
const CHAT_TYPE_P2P: &str = "p2p";

/// Outcome of deciding what to do with an inbound message. The identity and
/// scope decision is split out from the network so it can be tested directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InboundDecision {
    /// An owner DM: start a run on the owner's authority.
    StartRun,
    /// A DM from someone who is not the configured owner: ignore silently.
    IgnoreNonOwner,
    /// Group traffic: ignore entirely in v1.
    IgnoreGroup,
    /// An owner DM with no text to act on: ignore (media arrives in ticket 28).
    IgnoreEmpty,
}

/// A transport-agnostic view of one inbound message: just the fields the
/// decision and the run need, so no SDK type leaks past the seam.
#[derive(Debug, Clone)]
pub(crate) struct InboundMessage {
    pub chat_id: String,
    pub chat_type: String,
    pub sender_open_id: String,
    pub text: String,
    #[allow(dead_code)] // the reply target matters once cards stream (ticket 28)
    pub message_id: String,
}

/// Decide whether an inbound message should start a run, from the conversation
/// scope and the sender's identity alone. Owner binding is pure configuration
/// (`owner_open_id`): a DM whose sender is not the owner is ignored, and no
/// non-p2p conversation is handled at all in v1.
pub(crate) fn classify_inbound(
    chat_type: &str,
    sender_open_id: &str,
    owner_open_id: &str,
) -> InboundDecision {
    if chat_type != CHAT_TYPE_P2P {
        return InboundDecision::IgnoreGroup;
    }
    // An unset owner binds nobody; without a match a DM cannot carry the owner's
    // authority, so it is treated as a stranger rather than a free pass.
    if owner_open_id.is_empty() || sender_open_id != owner_open_id {
        return InboundDecision::IgnoreNonOwner;
    }
    InboundDecision::StartRun
}

/// The adapter's single logging mouth. Lines are fully formatted here before
/// reaching the global macros, so the PII guard can mirror the exact string
/// the logger would see into an in-memory capture under `cfg(test)` — without
/// touching the process-wide logger singleton (whose init order races with
/// other lib tests).
fn note(level: LogLevel, line: String) {
    #[cfg(test)]
    CAPTURED.lock().unwrap().push(line.clone());
    match level {
        LogLevel::Debug => debug!("{line}"),
        LogLevel::Info => info!("{line}"),
        LogLevel::Error => error!("{line}"),
        _ => {}
    }
}

/// Every line the adapter would have logged, drained on read. Test-only.
#[cfg(test)]
static CAPTURED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
fn captured_lines() -> Vec<String> {
    std::mem::take(&mut CAPTURED.lock().unwrap())
}

/// The failure class of an SDK error, for logging. `LarkError`'s Display can
/// carry server-rendered free text (an API `msg` may echo request parameters,
/// a token error its context), so only the variant — plus a numeric code,
/// which is never PII — is safe to surface (ADR 0017).
fn error_class(e: &larksuite_oapi_sdk_rs::LarkError) -> String {
    use larksuite_oapi_sdk_rs::LarkError as E;
    match e {
        E::Api(api) => format!("api code {}", api.code),
        E::OAuthTokenRejected { code, .. } => format!("oauth rejected code {code}"),
        E::DPoPBinding(_) | E::DPoPProof(_) => "dpop error".to_string(),
        E::IllegalParam(_) => "illegal param".to_string(),
        E::ClientTimeout(_) => "client timeout".to_string(),
        E::ServerTimeout(_) => "server timeout".to_string(),
        E::RateLimited(_) => "rate limited".to_string(),
        E::DialFailed(_) => "dial failed".to_string(),
        E::Http(_) => "http error".to_string(),
        E::Json(_) => "json error".to_string(),
        E::Token(_) => "token error".to_string(),
        E::MaxRetries => "max retries".to_string(),
        E::Event(_) => "event error".to_string(),
        E::Crypto(_) => "crypto error".to_string(),
        E::Registration(_) => "registration error".to_string(),
        E::ClientAssertion(_) => "assertion error".to_string(),
    }
}

/// Turns inbound messages into owner-authored bus requests. Shared between the
/// long-connection pump and the tests, so the routing decision is one place.
pub(crate) struct InboundRouter {
    owner_open_id: String,
    inbound_tx: mpsc::Sender<BusRequest>,
}

impl InboundRouter {
    fn new(owner_open_id: String, inbound_tx: mpsc::Sender<BusRequest>) -> Self {
        Self {
            owner_open_id,
            inbound_tx,
        }
    }

    /// Route one message. Only an owner DM with text becomes a run; anything
    /// else is a silent drop. The session id is two-segment so repeated turns
    /// on the same chat accumulate on one session (JSONL persistence).
    async fn handle(&self, msg: InboundMessage) -> InboundDecision {
        let decision = classify_inbound(&msg.chat_type, &msg.sender_open_id, &self.owner_open_id);
        match decision {
            InboundDecision::IgnoreGroup => {
                // The id of the group is itself an identifier; say only that a
                // group message arrived and was dropped.
                note(
                    LogLevel::Debug,
                    "[feishu] dropped a non-p2p message (group traffic is off in v1)".to_string(),
                );
                decision
            }
            InboundDecision::IgnoreNonOwner => {
                note(
                    LogLevel::Debug,
                    "[feishu] dropped a DM from a non-owner (no run started)".to_string(),
                );
                decision
            }
            InboundDecision::StartRun if msg.text.trim().is_empty() => {
                // Text is this ticket's only payload; an empty body (a bare
                // image or file) is handled when media lands in ticket 28.
                InboundDecision::IgnoreEmpty
            }
            InboundDecision::StartRun => {
                let session_id = format!("{CHANNEL_ID}:{}", msg.chat_id);
                let hook = TaskHook::new(&session_id);
                // The sender is bound by config, so this is the owner's message:
                // it carries the owner's authority for consequential actions.
                let request = BusRequest::owner(session_id, msg.text, hook);
                if let Err(e) = self.inbound_tx.send(request).await {
                    // The bus is gone; nothing to deliver, and the text is not
                    // logged even in the failure line (a SendError's Display
                    // carries only "channel closed", never the message).
                    note(
                        LogLevel::Error,
                        format!("[feishu] inbound send failed: {e}"),
                    );
                }
                decision
            }
            InboundDecision::IgnoreEmpty => decision,
        }
    }
}

/// The outbound half of the seam: post a finished answer back to a chat. The
/// real implementation talks to Feishu; tests record what was asked.
#[async_trait]
pub(crate) trait FeishuSender: Send + Sync {
    async fn send_text_to_chat(&self, chat_id: &str, text: &str) -> Result<()>;
}

/// The inbound half of the seam: open the single long connection and pump
/// messages through the router until shutdown. Held solely by the gateway
/// (ADR 0017), so exactly one connection exists per configured app.
pub(crate) trait FeishuTransport: Send + Sync {
    fn connect(&self, router: Arc<InboundRouter>) -> IoHandle;
}

/// Parsed `channels.feishu` configuration. The open `extra` map is flattened, so
/// unknown keys are simply ignored; a missing app id / secret / owner is a
/// configuration error surfaced at channel creation.
#[derive(Debug, Clone)]
pub(crate) struct FeishuConfig {
    app_id: String,
    app_secret: String,
    owner_open_id: String,
    base_url: Option<String>,
    verification_token: String,
    encrypt_key: String,
}

impl FeishuConfig {
    fn from_value(config: &serde_json::Value) -> Result<Self> {
        let field = |key: &str| -> Option<String> {
            config
                .get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        let app_id = field("app_id").unwrap_or_default();
        let app_secret = field("app_secret").unwrap_or_default();
        let owner_open_id = field("owner_open_id").unwrap_or_default();
        if app_id.trim().is_empty() {
            anyhow::bail!("channels.feishu.app_id is required");
        }
        if app_secret.trim().is_empty() {
            anyhow::bail!("channels.feishu.app_secret is required");
        }
        if owner_open_id.trim().is_empty() {
            anyhow::bail!(
                "channels.feishu.owner_open_id is required: without it no DM is the owner's and \
                 the channel can never start a run"
            );
        }
        Ok(Self {
            app_id,
            app_secret,
            owner_open_id,
            base_url: field("base_url").filter(|s| !s.trim().is_empty()),
            verification_token: field("verification_token").unwrap_or_default(),
            encrypt_key: field("encrypt_key").unwrap_or_default(),
        })
    }
}

/// The Feishu channel. It owns the routing decision and both network seams; it
/// never touches the SDK directly, which is walled off behind them.
pub(crate) struct FeishuChannel {
    chat_id: String,
    router: Arc<InboundRouter>,
    sender: Arc<dyn FeishuSender>,
    transport: Arc<dyn FeishuTransport>,
}

impl FeishuChannel {
    fn new(
        router: Arc<InboundRouter>,
        sender: Arc<dyn FeishuSender>,
        transport: Arc<dyn FeishuTransport>,
    ) -> Self {
        Self {
            // A placeholder for the trait's single-chat accessor; the real chat
            // comes from each session id. Routing never reads this.
            chat_id: "default".to_string(),
            router,
            sender,
            transport,
        }
    }

    /// The chat id a session id belongs to (`feishu:oc_xxx` → `oc_xxx`).
    fn chat_id_from(session_id: &str) -> &str {
        session_id
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or("")
    }
}

#[async_trait]
impl Channel for FeishuChannel {
    fn id(&self) -> &str {
        CHANNEL_ID
    }

    fn name(&self) -> &str {
        "Feishu"
    }

    fn chat_id(&self) -> &str {
        &self.chat_id
    }

    async fn read_input(&mut self) -> Result<String> {
        // Inbound arrives through the long connection, not a polled read.
        Ok(String::new())
    }

    async fn write_output(&mut self, result: &BusResult) -> Result<()> {
        if result.content.trim().is_empty() {
            return Ok(());
        }
        let chat_id = Self::chat_id_from(&result.session_id).to_string();
        // The content is the agent's answer; a send failure is reported without
        // echoing the body.
        self.sender
            .send_text_to_chat(&chat_id, &result.content)
            .await
    }

    async fn write_status(&mut self, _session_id: &str, _state: &TaskState) -> Result<()> {
        // Intermediate status is not surfaced in the text-only p2p round trip;
        // streaming updates land with ticket 28.
        Ok(())
    }

    async fn prepare_inject(&self) -> Result<String> {
        Ok(String::new())
    }

    fn start(&self, _inbound_tx: mpsc::Sender<BusRequest>) {
        // Superseded by start_with_scheduler.
    }

    fn start_with_scheduler(&self, _scheduler: &IoScheduler) -> IoHandle {
        // The single long connection, owned here and only here (ADR 0017).
        self.transport.connect(self.router.clone())
    }
}

/// Builds Feishu channels from the `channels.feishu` config entry.
pub struct FeishuChannelFactory {
    message_bus: Arc<crate::message_bus::MessageBus>,
    shutdown_tx: broadcast::Sender<()>,
}

impl FeishuChannelFactory {
    /// `shutdown_tx` is the gateway's channel-shutdown broadcast: the long
    /// connection observes it so `ChannelManager::shutdown` can join the
    /// connection task instead of timing out.
    pub fn new(
        message_bus: Arc<crate::message_bus::MessageBus>,
        shutdown_tx: broadcast::Sender<()>,
    ) -> Self {
        Self {
            message_bus,
            shutdown_tx,
        }
    }
}

impl ChannelFactory for FeishuChannelFactory {
    fn channel_type(&self) -> &str {
        CHANNEL_ID
    }

    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        let config = FeishuConfig::from_value(config)?;
        let router = Arc::new(InboundRouter::new(
            config.owner_open_id.clone(),
            self.message_bus.inbound_tx(),
        ));
        let sender: Arc<dyn FeishuSender> = Arc::new(SdkSender::new(&config)?);
        let transport: Arc<dyn FeishuTransport> =
            Arc::new(SdkTransport::new(config, self.shutdown_tx.clone()));
        Ok(Box::new(FeishuChannel::new(router, sender, transport)))
    }
}

// ── Real network implementations (isolated so the rest is test-able) ──

/// Outbound send through the SDK's channel messaging.
struct SdkSender {
    client: larksuite_oapi_sdk_rs::LarkClient,
}

impl SdkSender {
    fn new(config: &FeishuConfig) -> Result<Self> {
        Ok(Self {
            client: build_client(config)?,
        })
    }
}

#[async_trait]
impl FeishuSender for SdkSender {
    async fn send_text_to_chat(&self, chat_id: &str, text: &str) -> Result<()> {
        use larksuite_oapi_sdk_rs::channel::{ReceiveIdType, SendTarget};
        let target = SendTarget::new(ReceiveIdType::ChatId.as_str(), chat_id);
        self.client
            .channel_messaging()
            .send_text(
                &target,
                text,
                &larksuite_oapi_sdk_rs::RequestOption::default(),
            )
            .await
            .map_err(|e| {
                // Only the failure class is logged: the SDK's Display can
                // carry server free text that echoes request parameters.
                note(
                    LogLevel::Error,
                    format!("[feishu] outbound send failed: {}", error_class(&e)),
                );
                anyhow::anyhow!("feishu send failed")
            })
            .map(|_| ())
    }
}

/// Inbound long connection through the SDK channel runtime, pumping owner DMs
/// into the shared router. One call, one connection: the gateway is its only
/// holder, so no second process can double-process (ADR 0017).
struct SdkTransport {
    config: FeishuConfig,
    shutdown_tx: broadcast::Sender<()>,
}

impl SdkTransport {
    fn new(config: FeishuConfig, shutdown_tx: broadcast::Sender<()>) -> Self {
        Self {
            config,
            shutdown_tx,
        }
    }
}

impl FeishuTransport for SdkTransport {
    fn connect(&self, router: Arc<InboundRouter>) -> IoHandle {
        let config = self.config.clone();
        let mut shutdown_rx = self.shutdown_tx.subscribe();
        let session_id = format!("{CHANNEL_ID}:{}", "default");
        let channel_name = "Feishu".to_string();
        let join_handle = tokio::spawn(async move {
            // Subscribe up front: holding the receiver is what keeps this
            // task able to observe the broadcast for its whole life.
            run_connection(config, router, &mut shutdown_rx).await;
        });
        IoHandle {
            join_handle: Some(join_handle),
            session_id,
            channel_name,
        }
    }
}

/// Build a `LarkClient` from the (already secret-resolved) config. The Feishu
/// app credentials are read here and never logged. The request timeout is
/// tightened from the SDK's 30s default because outbound sends execute while
/// the ChannelManager holds the shared channels lock — a slow Feishu call must
/// not stall every other channel for minutes.
fn build_client(config: &FeishuConfig) -> Result<larksuite_oapi_sdk_rs::LarkClient> {
    let mut builder =
        larksuite_oapi_sdk_rs::LarkClient::builder(&config.app_id, &config.app_secret)
            .timeout(std::time::Duration::from_secs(10));
    if let Some(base_url) = &config.base_url {
        builder = builder.base_url(base_url.clone());
    }
    // A build failure is a configuration error (e.g. an unparsable base_url),
    // surfaced as Err — never a panic. The SDK error text is not echoed: it
    // is about our inputs, and the field names are enough.
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("cannot build the feishu client: {}", error_class(&e)))
}

/// Open the single connection and drive it until shutdown. The SDK's `start`
/// loops forever and reconnects on its own, so the task selects it against
/// the gateway's channel-shutdown broadcast: dropping the future closes the
/// underlying WebSocket and lets `ChannelManager::shutdown` join promptly.
async fn run_connection(
    config: FeishuConfig,
    router: Arc<InboundRouter>,
    shutdown_rx: &mut broadcast::Receiver<()>,
) {
    use larksuite_oapi_sdk_rs::channel::{Channel as SdkChannel, ChannelPolicy, DmMode};
    use larksuite_oapi_sdk_rs::event::EventDispatcher;

    let client = match build_client(&config) {
        Ok(client) => client,
        Err(e) => {
            note(LogLevel::Error, format!("[feishu] cannot start: {e}"));
            return;
        }
    };
    // The SDK policy is defense-in-depth over `classify_inbound`'s DM check:
    // owner-only DMs. Group suppression is `classify_inbound`'s authority
    // alone (the SDK's default group policy admits all groups, and group
    // traffic is dropped before it can start a run).
    let policy = ChannelPolicy::default()
        .dm_mode(DmMode::Allowlist)
        .allow_dm_sender(config.owner_open_id.clone());
    let dispatcher = EventDispatcher::new(
        config.verification_token.clone(),
        config.encrypt_key.clone(),
    );

    let pump = SdkChannel::builder(&client, dispatcher)
        .policy(policy)
        .on_message(move |msg| {
            let router = router.clone();
            async move {
                let sender_open_id = msg
                    .sender
                    .user_id
                    .as_ref()
                    .and_then(|u| u.open_id())
                    .unwrap_or("")
                    .to_string();
                let inbound = InboundMessage {
                    chat_id: msg.chat_id.clone(),
                    chat_type: msg.chat_type.clone(),
                    message_id: msg.message_id.clone(),
                    sender_open_id,
                    text: msg.text.clone().unwrap_or_default(),
                };
                router.handle(inbound).await;
                Ok(())
            }
        })
        .build();

    note(
        LogLevel::Info,
        "[feishu] opening the long connection".to_string(),
    );
    tokio::select! {
        _ = shutdown_rx.recv() => {
            // Dropping `pump` (and this future) tears down the WebSocket.
            note(LogLevel::Info, "[feishu] long connection closed on shutdown".to_string());
        }
        r = pump.start() => {
            if let Err(e) = r {
                // Class only, never the SDK's Display (see `error_class`).
                note(LogLevel::Error, format!("[feishu] long connection ended: {}", error_class(&e)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message_bus::MessageBus;

    /// A sender that records what it was asked to deliver.
    struct RecordingSender {
        sent: Arc<std::sync::Mutex<Vec<(String, String)>>>,
    }

    #[async_trait]
    impl FeishuSender for RecordingSender {
        async fn send_text_to_chat(&self, chat_id: &str, text: &str) -> Result<()> {
            self.sent
                .lock()
                .unwrap()
                .push((chat_id.to_string(), text.to_string()));
            Ok(())
        }
    }

    /// A transport that hands back a trivial handle, standing in for the real
    /// long connection so the single-holder wiring is testable offline.
    struct StubTransport;

    impl FeishuTransport for StubTransport {
        fn connect(&self, _router: Arc<InboundRouter>) -> IoHandle {
            IoHandle {
                join_handle: Some(tokio::spawn(async {})),
                session_id: "feishu:default".to_string(),
                channel_name: "Feishu".to_string(),
            }
        }
    }

    fn dm(chat_type: &str, sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            chat_id: "oc_conv1".to_string(),
            chat_type: chat_type.to_string(),
            sender_open_id: sender.to_string(),
            text: text.to_string(),
            message_id: "om_1".to_string(),
        }
    }

    // ── classify_inbound: identity and scope, nothing else ──

    #[test]
    fn test_classify_owner_dm_starts_a_run() {
        assert_eq!(
            classify_inbound("p2p", "ou_owner", "ou_owner"),
            InboundDecision::StartRun
        );
    }

    #[test]
    fn test_classify_non_owner_dm_is_ignored() {
        assert_eq!(
            classify_inbound("p2p", "ou_stranger", "ou_owner"),
            InboundDecision::IgnoreNonOwner
        );
    }

    #[test]
    fn test_classify_group_is_off_regardless_of_sender() {
        // Even the owner in a group is not handled in v1; group is off entirely.
        assert_eq!(
            classify_inbound("group", "ou_owner", "ou_owner"),
            InboundDecision::IgnoreGroup
        );
    }

    #[test]
    fn test_classify_unset_owner_binds_nobody() {
        // Without an owner binding a DM cannot carry the owner's authority.
        assert_eq!(
            classify_inbound("p2p", "ou_anyone", ""),
            InboundDecision::IgnoreNonOwner
        );
    }

    // ── handle: the routing that reaches the bus ──

    async fn route(msg: InboundMessage, owner: &str) -> (InboundDecision, Vec<BusRequest>) {
        let (mb, mut receivers) = MessageBus::new();
        let router = Arc::new(InboundRouter::new(owner.to_string(), mb.inbound_tx()));
        let decision = router.handle(msg).await;
        drop(router);
        let mut got = Vec::new();
        while let Ok(req) = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            receivers.inbound.recv(),
        )
        .await
        {
            match req {
                Some(r) => got.push(r),
                None => break,
            }
        }
        (decision, got)
    }

    #[tokio::test]
    async fn test_owner_dm_reaches_the_bus_with_two_segment_session_and_owner_authority() {
        let (decision, got) = route(
            dm("p2p", "ou_owner", "does that PR have updates?"),
            "ou_owner",
        )
        .await;
        assert_eq!(decision, InboundDecision::StartRun);
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].session_id, "feishu:oc_conv1",
            "session id is two-segment"
        );
        assert_eq!(got[0].content, "does that PR have updates?");
        assert!(
            got[0].author.may_authorize(),
            "a bound owner DM carries owner authority"
        );
    }

    #[tokio::test]
    async fn test_non_owner_dm_starts_no_run() {
        let (decision, got) = route(dm("p2p", "ou_stranger", "hi"), "ou_owner").await;
        assert_eq!(decision, InboundDecision::IgnoreNonOwner);
        assert!(got.is_empty(), "a non-owner DM never reaches the bus");
    }

    #[tokio::test]
    async fn test_group_message_starts_no_run() {
        let (decision, got) = route(dm("group", "ou_owner", "team ping"), "ou_owner").await;
        assert_eq!(decision, InboundDecision::IgnoreGroup);
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn test_owner_dm_without_text_is_ignored_and_starts_no_run() {
        let (decision, got) = route(dm("p2p", "ou_owner", "   "), "ou_owner").await;
        assert_eq!(decision, InboundDecision::IgnoreEmpty);
        assert!(got.is_empty());
    }

    #[tokio::test]
    async fn test_repeated_owner_turns_accumulate_on_one_session() {
        // Same chat, two turns → same session id, so the SessionManager folds
        // them into one conversation.
        let a = route(dm("p2p", "ou_owner", "first"), "ou_owner").await;
        let b = route(dm("p2p", "ou_owner", "second"), "ou_owner").await;
        assert_eq!(a.1[0].session_id, b.1[0].session_id);
    }

    // ── write_output: the answer goes back on the same chat ──

    #[tokio::test]
    async fn test_write_output_sends_answer_to_the_session_chat() {
        let (mb, _rx) = MessageBus::new();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender { sent: sent.clone() }),
            Arc::new(StubTransport),
        );
        let result = BusResult {
            session_id: "feishu:oc_conv1".to_string(),
            run_id: "r1".to_string(),
            content: "here is the answer".to_string(),
        };
        ch.write_output(&result).await.unwrap();
        assert_eq!(
            sent.lock().unwrap().as_slice(),
            &[("oc_conv1".to_string(), "here is the answer".to_string())]
        );
    }

    #[tokio::test]
    async fn test_write_output_drops_an_empty_answer_without_sending() {
        let (mb, _rx) = MessageBus::new();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender { sent: sent.clone() }),
            Arc::new(StubTransport),
        );
        let result = BusResult {
            session_id: "feishu:oc_conv1".to_string(),
            run_id: "r1".to_string(),
            content: "   ".to_string(),
        };
        ch.write_output(&result).await.unwrap();
        assert!(
            sent.lock().unwrap().is_empty(),
            "an empty answer sends nothing"
        );
    }

    // ── the long connection is opened only through start (single holder) ──

    #[tokio::test]
    async fn test_start_yields_exactly_one_connection_holder() {
        let (mb, _rx) = MessageBus::new();
        let ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender {
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            }),
            Arc::new(StubTransport),
        );
        let scheduler = IoScheduler::new(mb.inbound_tx());
        let handle = ch.start_with_scheduler(&scheduler);
        // The gateway owns the one handle that holds the connection; creation
        // alone (init) never opened one, so a second holder is impossible.
        assert!(handle.join_handle.is_some());
    }

    // ── config ──

    #[test]
    fn test_config_requires_app_id_secret_and_owner() {
        let missing_owner = serde_json::json!({"app_id": "cli", "app_secret": "s"});
        assert!(FeishuConfig::from_value(&missing_owner).is_err());
        let missing_secret = serde_json::json!({"app_id": "cli", "owner_open_id": "ou"});
        assert!(FeishuConfig::from_value(&missing_secret).is_err());
        let ok = serde_json::json!({"app_id": "cli", "app_secret": "s", "owner_open_id": "ou"});
        assert!(FeishuConfig::from_value(&ok).is_ok());
    }

    #[test]
    fn test_chat_id_from_session_is_two_segment_tail() {
        assert_eq!(FeishuChannel::chat_id_from("feishu:oc_conv1"), "oc_conv1");
        assert_eq!(FeishuChannel::chat_id_from("feishu:"), "");
        assert_eq!(FeishuChannel::chat_id_from("nocolon"), "");
    }

    // ── privacy guard: a full round trip logs no PII (ADR 0017) ──

    /// Drive an owner DM, a non-owner DM, a group message, and an answer back
    /// out, then assert the adapter's single logging mouth never saw an
    /// identifier/body marker. The mouth is `note`, so this checks the exact
    /// strings the global logger would receive without touching the logger
    /// singleton (which other lib tests initialize).
    #[tokio::test]
    async fn test_round_trip_logs_never_carry_pii() {
        let _ = captured_lines(); // drain anything an earlier test captured

        let (mb, _rx) = MessageBus::new();
        let router = Arc::new(InboundRouter::new(
            "ou-ZZOWNERMARKER".to_string(),
            mb.inbound_tx(),
        ));
        let marked = |chat_type: &str, sender: &str, text: &str| InboundMessage {
            chat_id: "oc-ZZCHATMARKER".to_string(),
            chat_type: chat_type.to_string(),
            sender_open_id: sender.to_string(),
            text: text.to_string(),
            message_id: "om-ZZMSGMARKER".to_string(),
        };
        router
            .handle(marked("p2p", "ou-ZZOWNERMARKER", "body-ZZBODYMARKER"))
            .await;
        router
            .handle(marked("p2p", "ou-ZZPEERMARKER", "peer-ZZPEERBODY"))
            .await;
        router
            .handle(marked("group", "ou-ZZOWNERMARKER", "team-ZZGROUPBODY"))
            .await;
        // The answer goes back on the same chat through the sender seam.
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ch = FeishuChannel::new(
            router,
            Arc::new(RecordingSender { sent: sent.clone() }),
            Arc::new(StubTransport),
        );
        ch.write_output(&BusResult {
            session_id: "feishu:oc-ZZCHATMARKER".to_string(),
            run_id: "r1".to_string(),
            content: "answer-ZZANSWERMARKER".to_string(),
        })
        .await
        .unwrap();
        assert_eq!(sent.lock().unwrap().len(), 1, "the round trip did deliver");

        let log_text = captured_lines().join("\n");
        // Capture proof: the fixed drop line for a non-owner DM is in there,
        // so the scan below is not passing on an empty capture.
        assert!(
            log_text.contains("dropped a DM from a non-owner"),
            "the ignore branches must log through `note`"
        );
        for marker in [
            "ZZBODYMARKER",
            "ZZPEERBODY",
            "ZZGROUPBODY",
            "ZZANSWERMARKER",
            "ZZOWNERMARKER",
            "ZZPEERMARKER",
            "ZZCHATMARKER",
            "ZZMSGMARKER",
        ] {
            assert!(!log_text.contains(marker), "logs must never carry {marker}");
        }
    }
}

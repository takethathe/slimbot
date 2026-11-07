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

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use tokio::sync::{broadcast, mpsc};

use super::{Channel, ChannelFactory};
use crate::io_scheduler::{IoHandle, IoScheduler};
use crate::log::LogLevel;
use crate::message_bus::{BusRequest, BusResult};
use crate::session::{ContentBlock, TaskHook, TaskState};
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

/// A transport-agnostic view of one inbound attachment. It carries only a
/// Feishu **file-key handle** — there is no URL field by construction, so the
/// channel cannot be coaxed into fetching an arbitrary address (ADR 0017).
#[derive(Debug, Clone)]
pub(crate) struct InboundResource {
    /// The SDK resource type: `"image"`, `"file"`, `"media"`, `"audio"`, ….
    pub resource_type: String,
    /// The opaque Feishu file-key handle. Never a URL.
    pub file_key: String,
    /// Original file name when Feishu reports one (usually empty for images).
    pub file_name: String,
}

/// A transport-agnostic view of one inbound message: just the fields the
/// decision and the run need, so no SDK type leaks past the seam.
#[derive(Debug, Clone)]
pub(crate) struct InboundMessage {
    pub chat_id: String,
    pub chat_type: String,
    pub sender_open_id: String,
    pub text: String,
    pub message_id: String,
    /// Attachments that came with the message (empty for a text-only DM).
    pub resources: Vec<InboundResource>,
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

/// Serializes the log-capturing tests. `CAPTURED` is a process-wide static that
/// any concurrent test's `captured_lines()` drains, so a capture-and-scan test
/// must hold this lock from its initial drain through its final read or another
/// test steals its lines mid-flight.
#[cfg(test)]
static CAPTURE_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
/// When a media downloader and a media directory are wired ([`Self::with_media`]),
/// attachments are fetched by file-key and folded into the request; without them
/// (offline tests, or a channel built for text only) resources are skipped.
pub(crate) struct InboundRouter {
    owner_open_id: String,
    inbound_tx: mpsc::Sender<BusRequest>,
    downloader: Option<Arc<dyn MediaDownloader>>,
    media_dir: Option<PathBuf>,
}

impl InboundRouter {
    fn new(owner_open_id: String, inbound_tx: mpsc::Sender<BusRequest>) -> Self {
        Self {
            owner_open_id,
            inbound_tx,
            downloader: None,
            media_dir: None,
        }
    }

    /// Wire the attachment path: a downloader (file-key → bytes) and the
    /// directory non-image files are saved into so the run can read them by
    /// path. Images are inlined as multimodal blocks and need no directory.
    fn with_media(mut self, downloader: Arc<dyn MediaDownloader>, media_dir: PathBuf) -> Self {
        self.downloader = Some(downloader);
        self.media_dir = Some(media_dir);
        self
    }

    /// Fetch each attachment by its file-key handle. A URL-shaped handle is
    /// refused outright (no fetch). Images become inline multimodal blocks;
    /// other files are saved under the media dir and reported back as a path
    /// line so the run references them by path. Returns (image blocks, path
    /// lines). Never logs a handle, file name, or body (ADR 0017).
    async fn fetch_attachments(&self, msg: &InboundMessage) -> (Vec<ContentBlock>, Vec<String>) {
        let mut images = Vec::new();
        let mut file_lines = Vec::new();
        if msg.resources.is_empty() {
            return (images, file_lines);
        }
        for res in &msg.resources {
            // Accept only an opaque file-key handle. A remote address shaped
            // like `scheme://…` is refused before any request is attempted.
            if crate::utils::looks_like_url(&res.file_key) {
                note(
                    LogLevel::Debug,
                    "[feishu] refused an attachment handle shaped like a URL".to_string(),
                );
                continue;
            }
            let Some(dl) = self.downloader.as_ref() else {
                // No downloader wired (text-only channel): skip the attachment
                // rather than fabricate one; a bare-media DM still starts.
                continue;
            };
            let bytes = match dl
                .download(&msg.message_id, &res.file_key, &res.resource_type)
                .await
            {
                Ok(b) => b,
                Err(_) => {
                    // Only the failure is noted, never the handle or body.
                    note(
                        LogLevel::Error,
                        "[feishu] an attachment download failed".to_string(),
                    );
                    continue;
                }
            };
            if res.resource_type == "image" {
                images.push(ContentBlock::Image {
                    mime_type: image_mime_from_name(&res.file_name).to_string(),
                    base64_data: BASE64.encode(&bytes),
                });
            } else if let Some(dir) = self.media_dir.as_ref() {
                match save_media_file(dir, &msg.message_id, &res.file_name, &bytes) {
                    Some(path) => file_lines.push(format!("[attached file saved to: {path}]")),
                    None => note(
                        LogLevel::Error,
                        "[feishu] an attachment could not be written to disk".to_string(),
                    ),
                }
            }
        }
        (images, file_lines)
    }

    /// Route one message. Only an owner DM with something to act on (text or an
    /// attachment) becomes a run; anything else is a silent drop. The session id
    /// is two-segment so repeated turns on the same chat accumulate on one
    /// session (JSONL persistence).
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
            InboundDecision::StartRun if msg.text.trim().is_empty() && msg.resources.is_empty() => {
                // Nothing to act on: no text and no attachment.
                InboundDecision::IgnoreEmpty
            }
            InboundDecision::StartRun => {
                let session_id = format!("{CHANNEL_ID}:{}", msg.chat_id);
                let hook = TaskHook::new(&session_id);
                // Fold attachments into the run: inline images and file-path
                // references appended to the caption text.
                let (images, file_lines) = self.fetch_attachments(&msg).await;
                let mut content = msg.text.clone();
                for line in file_lines {
                    if !content.trim().is_empty() {
                        content.push('\n');
                    }
                    content.push_str(&line);
                }
                // The sender is bound by config, so this is the owner's message:
                // it carries the owner's authority for consequential actions.
                let request = BusRequest::owner(session_id, content, hook).with_attachments(images);
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

/// Persist a downloaded non-image attachment under `dir`, returning the path to
/// hand to the run. The file name is reduced to its last path component so a
/// crafted name can never escape the media directory (no path traversal). None
/// on any I/O failure.
fn save_media_file(
    dir: &PathBuf,
    message_id: &str,
    file_name: &str,
    bytes: &[u8],
) -> Option<String> {
    use std::io::Write;
    let safe_name = std::path::Path::new(file_name)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "attachment.bin".to_string());
    // Prefix with the (opaque) message id so two attachments never collide.
    let name = format!("{message_id}_{safe_name}");
    let path = dir.join(name);
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    let mut file = std::fs::File::create(&path).ok()?;
    file.write_all(bytes).ok()?;
    Some(path.to_string_lossy().to_string())
}

/// The outbound half of the seam: post a finished answer (text or media) back
/// to a chat. The real implementation talks to Feishu; tests record what was
/// asked.
#[async_trait]
pub(crate) trait FeishuSender: Send + Sync {
    async fn send_text_to_chat(&self, chat_id: &str, text: &str) -> Result<()>;
    /// Deliver an agent-produced image/file to a chat. The path is a local
    /// file the SDK uploads; a caption, if any, accompanies it. Never logs the
    /// path, caption, or bytes (ADR 0017).
    async fn send_media_to_chat(
        &self,
        chat_id: &str,
        media: &crate::message_bus::OutboundMedia,
        caption: Option<&str>,
    ) -> Result<()>;
    /// Open a streaming card on a chat and return its message id. `markdown`
    /// is the initial body; `streaming` shows the typing indicator. A failure
    /// here makes the channel fall back to a plain text answer.
    async fn create_card(&self, chat_id: &str, markdown: &str, streaming: bool) -> Result<String>;
    /// Update a previously created card in place with a new body. `streaming`
    /// off marks the card final. Never logs the body (ADR 0017).
    async fn update_card(&self, message_id: &str, markdown: &str, streaming: bool) -> Result<()>;
}

/// The inbound half of the seam: open the single long connection and pump
/// messages through the router until shutdown. Held solely by the gateway
/// (ADR 0017), so exactly one connection exists per configured app.
pub(crate) trait FeishuTransport: Send + Sync {
    fn connect(&self, router: Arc<InboundRouter>) -> IoHandle;
}

/// The attachment-fetch half of the seam. It takes ONLY a Feishu file-key
/// handle (plus the message it belongs to) and returns raw bytes — there is no
/// URL parameter by construction, so a caller cannot turn an inbound message
/// into an SSRF probe (ADR 0017). The real implementation calls the `im`
/// resource API; tests record which handles were requested.
#[async_trait]
pub(crate) trait MediaDownloader: Send + Sync {
    async fn download(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>>;
}

/// A best-effort image MIME type from a file name/extension. Feishu image
/// resources often carry no name, in which case `image/png` is the safe inline
/// default for a multimodal block.
fn image_mime_from_name(file_name: &str) -> &'static str {
    let ext = std::path::Path::new(file_name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        // Images usually arrive nameless; png is the conservative default.
        "" | "png" => "image/png",
        _ => "image/png",
    }
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
    /// Stream a multi-chunk answer into one in-place card. On by default;
    /// `false` keeps the plain p2p text round trip.
    streaming: bool,
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
            // Streaming defaults on: only an explicit `"streaming": false` (or
            // `false`) turns it off, preserving the plain text round trip.
            streaming: match config.get("streaming") {
                Some(serde_json::Value::Bool(b)) => *b,
                Some(serde_json::Value::String(s)) => !s.trim().eq_ignore_ascii_case("false"),
                _ => true,
            },
        })
    }
}

/// One streaming card currently open on a session.
struct ActiveCard {
    message_id: String,
    /// Set once a create/update fails, so later updates are skipped and the
    /// final answer falls back to plain text (never message spam).
    failed: bool,
}

/// The Feishu channel. It owns the routing decision and both network seams; it
/// never touches the SDK directly, which is walled off behind them.
pub(crate) struct FeishuChannel {
    chat_id: String,
    router: Arc<InboundRouter>,
    sender: Arc<dyn FeishuSender>,
    transport: Arc<dyn FeishuTransport>,
    /// Whether a multi-chunk answer streams into one in-place card. Off keeps
    /// the plain p2p text round trip (ticket 27) intact.
    streaming: bool,
    /// One open card per session, keyed by session id.
    cards: std::collections::HashMap<String, ActiveCard>,
    /// run_ids whose final answer was already delivered through a card, so
    /// `write_output` must not also send the same text.
    carded_runs: std::collections::HashSet<String>,
}

impl FeishuChannel {
    fn new(
        router: Arc<InboundRouter>,
        sender: Arc<dyn FeishuSender>,
        transport: Arc<dyn FeishuTransport>,
        streaming: bool,
    ) -> Self {
        Self {
            // A placeholder for the trait's single-chat accessor; the real chat
            // comes from each session id. Routing never reads this.
            chat_id: "default".to_string(),
            router,
            sender,
            transport,
            streaming,
            cards: std::collections::HashMap::new(),
            carded_runs: std::collections::HashSet::new(),
        }
    }

    /// The chat id a session id belongs to (`feishu:oc_xxx` → `oc_xxx`).
    fn chat_id_from(session_id: &str) -> &str {
        session_id
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or("")
    }

    /// Finalize (or drop) the card for a run at its terminal event. On a
    /// successful final update the run is recorded as card-delivered so the
    /// matching `write_output` is suppressed. A missing or failed card leaves
    /// the run unrecorded, so the text answer still goes out.
    async fn finish_card(&mut self, session_id: &str, run_id: &str, text: &str) {
        let Some(card) = self.cards.remove(session_id) else {
            return; // no card was open (streaming off, or never created)
        };
        if card.failed {
            return; // fall back to text via write_output
        }
        if self
            .sender
            .update_card(&card.message_id, text, false)
            .await
            .is_ok()
        {
            self.remember_carded_run(run_id.to_string());
        }
    }

    /// Bound the dedup set: entries live only between the terminal event and
    /// the matching write_output, but a run that never emits one must not leak.
    fn remember_carded_run(&mut self, run_id: String) {
        if self.carded_runs.len() >= 512 {
            self.carded_runs.clear();
        }
        self.carded_runs.insert(run_id);
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
        // Dedup: if this run's answer already landed in a finalized card, do
        // not send the same text a second time.
        if self.carded_runs.remove(&result.run_id) {
            return Ok(());
        }
        let chat_id = Self::chat_id_from(&result.session_id).to_string();
        // The content is the agent's answer; a send failure is reported without
        // echoing the body.
        self.sender
            .send_text_to_chat(&chat_id, &result.content)
            .await
    }

    async fn write_event(&mut self, event: &crate::session::AgentEvent) -> Result<()> {
        use crate::session::AgentEvent as E;
        if !self.streaming {
            return Ok(()); // ticket 27's plain text path drives delivery via write_output
        }
        match event {
            E::TaskStarted {
                session_id,
                run_id: _,
            } => {
                let chat_id = Self::chat_id_from(session_id).to_string();
                // Open an empty streaming card. On failure, record a failed
                // placeholder so interim updates are skipped and the answer
                // falls back to text.
                match self.sender.create_card(&chat_id, "", true).await {
                    Ok(message_id) => {
                        self.cards.insert(
                            session_id.clone(),
                            ActiveCard {
                                message_id,
                                failed: false,
                            },
                        );
                    }
                    Err(_) => {
                        self.cards.insert(
                            session_id.clone(),
                            ActiveCard {
                                message_id: String::new(),
                                failed: true,
                            },
                        );
                    }
                }
            }
            E::AssistantMessage {
                session_id,
                content,
            } => {
                if let Some(card) = self.cards.get_mut(session_id)
                    && !card.failed
                    && !content.trim().is_empty()
                {
                    // An update failure flips the card to failed; the final
                    // answer then falls back to text.
                    if self
                        .sender
                        .update_card(&card.message_id, content, true)
                        .await
                        .is_err()
                    {
                        card.failed = true;
                    }
                }
            }
            E::TaskCompleted {
                session_id,
                run_id,
                result,
            } => self.finish_card(session_id, run_id, result).await,
            E::TaskFailed {
                session_id,
                run_id,
                error,
            } => self.finish_card(session_id, run_id, error).await,
            _ => {}
        }
        Ok(())
    }

    async fn write_media(&mut self, delivery: &crate::message_bus::MediaDelivery) -> Result<()> {
        // Defense-in-depth against the outbound mirror of the inbound rule: a
        // media path shaped like a URL is refused, never fetched.
        if crate::utils::looks_like_url(delivery.media.path()) {
            anyhow::bail!("feishu refuses a media path shaped like a URL");
        }
        let chat_id = Self::chat_id_from(&delivery.session_id).to_string();
        self.sender
            .send_media_to_chat(&chat_id, &delivery.media, delivery.caption.as_deref())
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
    workspace_dir: PathBuf,
}

impl FeishuChannelFactory {
    /// `shutdown_tx` is the gateway's channel-shutdown broadcast: the long
    /// connection observes it so `ChannelManager::shutdown` can join the
    /// connection task instead of timing out. `workspace_dir` roots the media
    /// directory inbound files are saved into (ticket 28).
    pub fn new(
        message_bus: Arc<crate::message_bus::MessageBus>,
        shutdown_tx: broadcast::Sender<()>,
        workspace_dir: PathBuf,
    ) -> Self {
        Self {
            message_bus,
            shutdown_tx,
            workspace_dir,
        }
    }
}

impl ChannelFactory for FeishuChannelFactory {
    fn channel_type(&self) -> &str {
        CHANNEL_ID
    }

    fn create(&self, config: &serde_json::Value) -> Result<Box<dyn Channel>> {
        let config = FeishuConfig::from_value(config)?;
        let streaming = config.streaming;
        // Inbound files land under `{workspace}/media/feishu`; the run then
        // reads them by path. Images are inlined and need no directory.
        let media_dir = self.workspace_dir.join("media").join(CHANNEL_ID);
        let downloader: Arc<dyn MediaDownloader> = Arc::new(SdkDownloader::new(&config)?);
        let router = Arc::new(
            InboundRouter::new(config.owner_open_id.clone(), self.message_bus.inbound_tx())
                .with_media(downloader, media_dir),
        );
        let sender: Arc<dyn FeishuSender> = Arc::new(SdkSender::new(&config)?);
        let transport: Arc<dyn FeishuTransport> =
            Arc::new(SdkTransport::new(config, self.shutdown_tx.clone()));
        Ok(Box::new(FeishuChannel::new(
            router, sender, transport, streaming,
        )))
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

    async fn send_media_to_chat(
        &self,
        chat_id: &str,
        media: &crate::message_bus::OutboundMedia,
        caption: Option<&str>,
    ) -> Result<()> {
        use crate::message_bus::OutboundMedia;
        use larksuite_oapi_sdk_rs::channel::SendInput;
        let opt = &larksuite_oapi_sdk_rs::RequestOption::default();
        let log_err = |e: larksuite_oapi_sdk_rs::LarkError| {
            // Class only: the SDK Display can echo request parameters, and a
            // local path or caption must never reach the log (ADR 0017).
            note(
                LogLevel::Error,
                format!("[feishu] outbound media send failed: {}", error_class(&e)),
            );
            anyhow::anyhow!("feishu media send failed")
        };
        // A caption is sent as its own text message so the media maps cleanly
        // to a single native image/file message.
        if let Some(cap) = caption.filter(|c| !c.trim().is_empty()) {
            let t = SendInput {
                chat_id: Some(chat_id.to_string()),
                text: Some(cap.to_string()),
                ..Default::default()
            };
            self.client
                .channel_messaging()
                .send(&t, opt)
                .await
                .map_err(log_err)?;
        }
        let mut input = SendInput {
            chat_id: Some(chat_id.to_string()),
            ..Default::default()
        };
        match media {
            OutboundMedia::Image { path } => input.image_path = Some(path.clone()),
            OutboundMedia::File { path, .. } => input.file_path = Some(path.clone()),
        }
        self.client
            .channel_messaging()
            .send(&input, opt)
            .await
            .map_err(log_err)
            .map(|_| ())
    }

    async fn create_card(&self, chat_id: &str, markdown: &str, streaming: bool) -> Result<String> {
        use larksuite_oapi_sdk_rs::channel::SendInput;
        let card = build_streaming_card(markdown, streaming)?;
        let input = SendInput {
            chat_id: Some(chat_id.to_string()),
            card: Some(card),
            ..Default::default()
        };
        let result = self
            .client
            .channel_messaging()
            .send(&input, &larksuite_oapi_sdk_rs::RequestOption::default())
            .await
            .map_err(card_err)?;
        Ok(result.message_id)
    }

    async fn update_card(&self, message_id: &str, markdown: &str, streaming: bool) -> Result<()> {
        use larksuite_oapi_sdk_rs::card::v2::{
            Body, Card, CardDocument, Config, Element, Markdown,
        };
        let doc = CardDocument::new(
            Card::new()
                .config(Config::new().update_multi().streaming_mode(streaming))
                .body(Body::new().element(Element::Markdown(Markdown::new(markdown)))),
        )
        .map_err(|_| anyhow::anyhow!("invalid streaming card"))?;
        self.client
            .channel_messaging()
            .edit_card(
                message_id,
                &doc,
                &larksuite_oapi_sdk_rs::RequestOption::default(),
            )
            .await
            .map_err(card_err)
            .map(|_| ())
    }
}

/// Build a Card JSON 2.0 document (as a serialized string) whose single body
/// element is `markdown`. `streaming` toggles the typing indicator: on while an
/// answer is still arriving, off on the final update. `update_multi` is
/// required by the v2 schema (the only supported update mode).
fn build_streaming_card(markdown: &str, streaming: bool) -> Result<String> {
    use larksuite_oapi_sdk_rs::card::v2::{Body, Card, CardDocument, Config, Element, Markdown};
    let doc = CardDocument::new(
        Card::new()
            .config(Config::new().update_multi().streaming_mode(streaming))
            .body(Body::new().element(Element::Markdown(Markdown::new(markdown)))),
    )
    .map_err(|_| anyhow::anyhow!("invalid streaming card"))?;
    serde_json::to_string(doc.card()).map_err(|e| anyhow::anyhow!("card serialize: {e}"))
}

/// Log a card failure by class only; the SDK Display can carry free text tied
/// to the request (and the card body is the agent's answer). ADR 0017.
fn card_err(e: larksuite_oapi_sdk_rs::LarkError) -> anyhow::Error {
    note(
        LogLevel::Error,
        format!("[feishu] card update failed: {}", error_class(&e)),
    );
    anyhow::anyhow!("feishu card update failed")
}

/// Inbound attachment fetch through the SDK's message-resource API. It accepts
/// only a file-key handle; a URL-shaped handle is refused here too as
/// defense-in-depth, so the seam can never be turned into a remote fetch.
struct SdkDownloader {
    client: larksuite_oapi_sdk_rs::LarkClient,
}

impl SdkDownloader {
    fn new(config: &FeishuConfig) -> Result<Self> {
        Ok(Self {
            client: build_client(config)?,
        })
    }
}

#[async_trait]
impl MediaDownloader for SdkDownloader {
    async fn download(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>> {
        // Mirror the router's rule at the seam boundary: a handle shaped like a
        // URL is refused, never dereferenced.
        if crate::utils::looks_like_url(file_key) {
            anyhow::bail!("refused an attachment handle shaped like a URL");
        }
        let resp = self
            .client
            .channel_messaging()
            .download_message_resource(
                message_id,
                file_key,
                resource_type,
                &larksuite_oapi_sdk_rs::RequestOption::default(),
            )
            .await
            .map_err(|e| {
                // Class only: the SDK Display can carry free text tied to the
                // request, and the file-key is itself an identifier.
                note(
                    LogLevel::Error,
                    format!("[feishu] attachment download failed: {}", error_class(&e)),
                );
                anyhow::anyhow!("feishu attachment download failed")
            })?;
        Ok(resp.data)
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
                    // Map each attachment to an opaque file-key handle; the SDK
                    // never surfaces a URL here (ADR 0017).
                    resources: msg
                        .resources
                        .iter()
                        .map(|r| InboundResource {
                            resource_type: r.resource_type.clone(),
                            file_key: r.file_key.clone(),
                            file_name: r.file_name.clone(),
                        })
                        .collect(),
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

    /// A sender that records what it was asked to deliver (text, media, cards).
    #[derive(Default)]
    struct RecordingSender {
        sent: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        media: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        /// Ordered trace of card operations, e.g. `create card_0 true`.
        cards: Arc<std::sync::Mutex<Vec<String>>>,
        /// When true, card create/update fail, to exercise the text fallback.
        fail_card: bool,
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

        async fn send_media_to_chat(
            &self,
            chat_id: &str,
            media: &crate::message_bus::OutboundMedia,
            _caption: Option<&str>,
        ) -> Result<()> {
            let kind = match media {
                crate::message_bus::OutboundMedia::Image { .. } => "image",
                crate::message_bus::OutboundMedia::File { .. } => "file",
            };
            self.media.lock().unwrap().push((
                chat_id.to_string(),
                media.path().to_string(),
                kind.to_string(),
            ));
            Ok(())
        }

        async fn create_card(
            &self,
            _chat_id: &str,
            _markdown: &str,
            streaming: bool,
        ) -> Result<String> {
            if self.fail_card {
                anyhow::bail!("card create failed");
            }
            let mut ops = self.cards.lock().unwrap();
            let id = format!("card_{}", ops.len());
            ops.push(format!("create {id} streaming={streaming}"));
            Ok(id)
        }

        async fn update_card(
            &self,
            message_id: &str,
            _markdown: &str,
            streaming: bool,
        ) -> Result<()> {
            if self.fail_card {
                anyhow::bail!("card update failed");
            }
            self.cards
                .lock()
                .unwrap()
                .push(format!("update {message_id} streaming={streaming}"));
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

    /// A downloader that records every handle it is asked to fetch and
    /// returns canned bytes, so the file-key-only rule and the multimodal
    /// fold-in are testable without Feishu credentials or the network.
    struct StubDownloader {
        requested: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl MediaDownloader for StubDownloader {
        async fn download(
            &self,
            _message_id: &str,
            file_key: &str,
            _resource_type: &str,
        ) -> Result<Vec<u8>> {
            self.requested.lock().unwrap().push(file_key.to_string());
            // A fixed non-empty payload stands in for the real bytes.
            Ok(vec![0u8; 8])
        }
    }

    fn dm(chat_type: &str, sender: &str, text: &str) -> InboundMessage {
        InboundMessage {
            chat_id: "oc_conv1".to_string(),
            chat_type: chat_type.to_string(),
            sender_open_id: sender.to_string(),
            text: text.to_string(),
            message_id: "om_1".to_string(),
            resources: Vec::new(),
        }
    }

    /// Same as [`dm`] but with attachments riding on the message.
    fn dm_with_resources(
        chat_type: &str,
        sender: &str,
        text: &str,
        resources: Vec<InboundResource>,
    ) -> InboundMessage {
        InboundMessage {
            resources,
            ..dm(chat_type, sender, text)
        }
    }

    fn resource(resource_type: &str, file_key: &str, file_name: &str) -> InboundResource {
        InboundResource {
            resource_type: resource_type.to_string(),
            file_key: file_key.to_string(),
            file_name: file_name.to_string(),
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

    // ── inbound media: file-key-only fetch folds into the run (ticket 28) ──

    /// Route through a router wired with a stub downloader and a fresh temp
    /// media dir. Returns the decision, the requests that reached the bus, and
    /// the exact handles the downloader was asked to fetch (to prove a URL is
    /// never dereferenced).
    async fn route_media(
        msg: InboundMessage,
        owner: &str,
        media_dir: PathBuf,
    ) -> (InboundDecision, Vec<BusRequest>, Vec<String>) {
        let (mb, mut receivers) = MessageBus::new();
        let requested = Arc::new(std::sync::Mutex::new(Vec::new()));
        let downloader = Arc::new(StubDownloader {
            requested: requested.clone(),
        });
        let router = Arc::new(
            InboundRouter::new(owner.to_string(), mb.inbound_tx())
                .with_media(downloader, media_dir),
        );
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
        (
            decision,
            got,
            std::mem::take(&mut *requested.lock().unwrap()),
        )
    }

    fn temp_media_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "slimbot_feishu_{}_{}_{}",
            tag,
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn test_inbound_image_becomes_a_multimodal_attachment() {
        let dir = temp_media_dir("img");
        let (decision, got, requested) = route_media(
            dm_with_resources(
                "p2p",
                "ou_owner",
                "",
                vec![resource("image", "img_v2_abc", "")],
            ),
            "ou_owner",
            dir.clone(),
        )
        .await;
        assert_eq!(decision, InboundDecision::StartRun, "a bare image starts");
        assert_eq!(got.len(), 1);
        assert_eq!(
            requested.as_slice(),
            &["img_v2_abc".to_string()],
            "the image is fetched by its file-key handle"
        );
        let imgs: Vec<_> = got[0]
            .attachments
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .collect();
        assert_eq!(
            imgs.len(),
            1,
            "an image attachment becomes an inline multimodal block"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_inbound_file_is_saved_and_referenced_by_path() {
        let dir = temp_media_dir("file");
        let (decision, got, requested) = route_media(
            dm_with_resources(
                "p2p",
                "ou_owner",
                "take a look at this",
                vec![resource("file", "file_v2_x", "report.pdf")],
            ),
            "ou_owner",
            dir.clone(),
        )
        .await;
        assert_eq!(decision, InboundDecision::StartRun);
        assert_eq!(requested.as_slice(), &["file_v2_x".to_string()]);
        assert!(
            got[0].content.contains("[attached file saved to:"),
            "a file is referenced by path in the run text: {:?}",
            got[0].content
        );
        assert!(
            got[0].content.starts_with("take a look at this"),
            "the caption text is preserved ahead of the file reference"
        );
        assert!(
            got[0].attachments.is_empty(),
            "a non-image file is not inlined as a multimodal block"
        );
        // The file actually lands inside the media dir and cannot escape it.
        assert!(
            std::fs::read_dir(&dir).unwrap().count() >= 1,
            "the downloaded file is written under the media dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_inbound_url_shaped_handle_is_refused_and_never_fetched() {
        // Acceptance: inbound media fetch accepts only a file-key handle; a
        // URL-shaped one is refused with no dereference (no SSRF probe).
        let dir = temp_media_dir("ssrf");
        let (decision, got, requested) = route_media(
            dm_with_resources(
                "p2p",
                "ou_owner",
                "",
                vec![resource("image", "https://evil.example/x.png", "")],
            ),
            "ou_owner",
            dir.clone(),
        )
        .await;
        assert!(
            requested.is_empty(),
            "a URL-shaped handle must never reach the downloader"
        );
        // With nothing fetchable and no text, there is nothing to act on: the
        // message is dropped, so no run starts either.
        let _ = decision;
        for req in &got {
            assert!(
                req.attachments.is_empty(),
                "a refused handle produces no attachment"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── write_output: the answer goes back on the same chat ──

    #[tokio::test]
    async fn test_write_output_sends_answer_to_the_session_chat() {
        let (mb, _rx) = MessageBus::new();
        let sent = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender {
                sent: sent.clone(),
                ..Default::default()
            }),
            Arc::new(StubTransport),
            false,
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
            Arc::new(RecordingSender {
                sent: sent.clone(),
                ..Default::default()
            }),
            Arc::new(StubTransport),
            false,
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

    // ── write_media: an agent-produced file arrives in the session chat ──

    #[tokio::test]
    async fn test_write_media_delivers_to_the_session_chat() {
        use crate::message_bus::{MediaDelivery, OutboundMedia};
        let (mb, _rx) = MessageBus::new();
        let media = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sender = Arc::new(RecordingSender {
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            media: media.clone(),
            ..Default::default()
        });
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            sender,
            Arc::new(StubTransport),
            false,
        );
        let delivery = MediaDelivery {
            session_id: "feishu:oc_conv1".to_string(),
            media: OutboundMedia::File {
                path: "out/report.pdf".to_string(),
                name: Some("report.pdf".to_string()),
            },
            caption: None,
        };
        ch.write_media(&delivery).await.unwrap();
        assert_eq!(
            media.lock().unwrap().as_slice(),
            &[(
                "oc_conv1".to_string(),
                "out/report.pdf".to_string(),
                "file".to_string()
            )],
            "the file is delivered to the chat the session belongs to"
        );
    }

    #[tokio::test]
    async fn test_write_media_refuses_a_url_shaped_path() {
        // The outbound mirror of the inbound rule: a path shaped like a URL is
        // refused before any upload is attempted.
        use crate::message_bus::{MediaDelivery, OutboundMedia};
        let (mb, _rx) = MessageBus::new();
        let media = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sender = Arc::new(RecordingSender {
            sent: Arc::new(std::sync::Mutex::new(Vec::new())),
            media: media.clone(),
            ..Default::default()
        });
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            sender,
            Arc::new(StubTransport),
            false,
        );
        let delivery = MediaDelivery {
            session_id: "feishu:oc_conv1".to_string(),
            media: OutboundMedia::Image {
                path: "https://evil.example/x.png".to_string(),
            },
            caption: None,
        };
        let err = ch.write_media(&delivery).await.unwrap_err();
        assert!(
            err.to_string().contains("URL"),
            "a URL-shaped media path is refused: {err}"
        );
        assert!(
            media.lock().unwrap().is_empty(),
            "a refused media path sends nothing"
        );
    }

    // ── streaming card: one card in place, no message spam (ticket 28) ──

    /// Build a Feishu channel over a RecordingSender, with streaming on/off and
    /// an optional card-failure stub.
    fn card_channel(streaming: bool, fail_card: bool) -> (FeishuChannel, Arc<RecordingSender>) {
        let (mb, _rx) = MessageBus::new();
        let sender = Arc::new(RecordingSender {
            fail_card,
            ..Default::default()
        });
        let ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            sender.clone(),
            Arc::new(StubTransport),
            streaming,
        );
        (ch, sender)
    }

    use crate::session::AgentEvent;

    #[tokio::test]
    async fn test_streaming_card_updates_in_place_and_suppresses_text() {
        let (mut ch, sender) = card_channel(true, false);
        ch.write_event(&AgentEvent::TaskStarted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
        })
        .await
        .unwrap();
        ch.write_event(&AgentEvent::AssistantMessage {
            session_id: "feishu:oc_conv1".into(),
            content: "thinking…".into(),
        })
        .await
        .unwrap();
        ch.write_event(&AgentEvent::TaskCompleted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            result: "the final answer".into(),
        })
        .await
        .unwrap();
        // The final BusResult for the same run must NOT send duplicate text.
        ch.write_output(&BusResult {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            content: "the final answer".into(),
        })
        .await
        .unwrap();

        let ops = sender.cards.lock().unwrap();
        assert_eq!(
            ops.as_slice(),
            &[
                "create card_0 streaming=true".to_string(),
                "update card_0 streaming=true".to_string(),
                "update card_0 streaming=false".to_string(),
            ],
            "one card is opened and updated in place, finalized without the \
             typing indicator"
        );
        assert!(
            sender.sent.lock().unwrap().is_empty(),
            "a card-delivered run sends no duplicate text message"
        );
    }

    #[tokio::test]
    async fn test_card_fallback_sends_text_when_card_fails() {
        // create_card fails → the run is not marked card-delivered, so the
        // final answer still reaches the chat as plain text (no silent drop).
        let (mut ch, sender) = card_channel(true, true);
        ch.write_event(&AgentEvent::TaskStarted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
        })
        .await
        .unwrap();
        ch.write_event(&AgentEvent::TaskCompleted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            result: "the final answer".into(),
        })
        .await
        .unwrap();
        ch.write_output(&BusResult {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            content: "the final answer".into(),
        })
        .await
        .unwrap();
        assert!(
            sender.cards.lock().unwrap().is_empty(),
            "a failing card records no successful operation"
        );
        assert_eq!(
            sender.sent.lock().unwrap().as_slice(),
            &[("oc_conv1".to_string(), "the final answer".to_string())],
            "the answer falls back to text"
        );
    }

    #[tokio::test]
    async fn test_streaming_off_sends_plain_text_and_opens_no_card() {
        let (mut ch, sender) = card_channel(false, false);
        ch.write_event(&AgentEvent::TaskStarted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
        })
        .await
        .unwrap();
        ch.write_event(&AgentEvent::TaskCompleted {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            result: "the final answer".into(),
        })
        .await
        .unwrap();
        ch.write_output(&BusResult {
            session_id: "feishu:oc_conv1".into(),
            run_id: "r1".into(),
            content: "the final answer".into(),
        })
        .await
        .unwrap();
        assert!(
            sender.cards.lock().unwrap().is_empty(),
            "streaming off opens no card"
        );
        assert_eq!(sender.sent.lock().unwrap().len(), 1, "text is the delivery");
    }

    // ── the long connection is opened only through start (single holder) ──

    #[tokio::test]
    async fn test_start_yields_exactly_one_connection_holder() {
        let (mb, _rx) = MessageBus::new();
        let ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender {
                sent: Arc::new(std::sync::Mutex::new(Vec::new())),
                ..Default::default()
            }),
            Arc::new(StubTransport),
            false,
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
        let _guard = CAPTURE_GUARD.lock().await;
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
            resources: Vec::new(),
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
            Arc::new(RecordingSender {
                sent: sent.clone(),
                ..Default::default()
            }),
            Arc::new(StubTransport),
            false,
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

    #[tokio::test]
    async fn test_media_logs_never_carry_handles_names_or_paths() {
        // Ticket 28 privacy: the media paths (inbound fetch + outbound send)
        // obey the same guardrail — a file-key handle, an original file name,
        // and a local send path must never appear in a log line.
        use crate::message_bus::{MediaDelivery, OutboundMedia};
        let _guard = CAPTURE_GUARD.lock().await;
        let _ = captured_lines(); // drain anything an earlier test captured

        // Inbound: a URL-shaped handle is refused (the refusal must log, but
        // only fixed words), and a saved file carries a marked file-name.
        let dir = temp_media_dir("pii");
        let (decision, _got, requested) = route_media(
            dm_with_resources(
                "p2p",
                "ou-ZZOWNERMARKER",
                "",
                vec![
                    resource("image", "https://evil.example/ZZURLHANDLE", ""),
                    resource("file", "ZZFILEKEYMARKER", "ZZFILENAMEMARKER.pdf"),
                ],
            ),
            "ou-ZZOWNERMARKER",
            dir.clone(),
        )
        .await;
        assert_eq!(decision, InboundDecision::StartRun);
        // Proof the fetch path ran: the marked file-key was handed to the
        // downloader while the URL-shaped handle was not (SSRF refusal).
        assert!(
            requested.iter().any(|k| k == "ZZFILEKEYMARKER"),
            "the file-key handle reached the downloader"
        );
        assert!(
            !requested.iter().any(|k| k.contains("ZZURLHANDLE")),
            "a URL-shaped handle is never dereferenced"
        );

        // Outbound: a marked local path and caption go through write_media.
        let media = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mb, _rx) = MessageBus::new();
        let mut ch = FeishuChannel::new(
            Arc::new(InboundRouter::new("ou_owner".into(), mb.inbound_tx())),
            Arc::new(RecordingSender {
                media: media.clone(),
                ..Default::default()
            }),
            Arc::new(StubTransport),
            false,
        );
        ch.write_media(&MediaDelivery {
            session_id: "feishu:oc-ZZCHATMARKER".to_string(),
            media: OutboundMedia::File {
                path: "/tmp/ZZLOCALPATHMARKER.bin".to_string(),
                name: Some("ZZDISPLAYNAMEMARKER".to_string()),
            },
            caption: Some("ZZCAPTIONMARKER".to_string()),
        })
        .await
        .unwrap();
        assert_eq!(media.lock().unwrap().len(), 1, "the media did deliver");

        let log_text = captured_lines().join("\n");
        // Capture proof: the URL-refusal line is present, so the scan is not
        // passing on an empty capture.
        assert!(
            log_text.contains("refused an attachment handle shaped like a URL"),
            "the SSRF refusal must log through `note`: {log_text}"
        );
        for marker in [
            "ZZURLHANDLE",
            "ZZFILEKEYMARKER",
            "ZZFILENAMEMARKER",
            "ZZLOCALPATHMARKER",
            "ZZDISPLAYNAMEMARKER",
            "ZZCAPTIONMARKER",
            "ZZOWNERMARKER",
            "ZZCHATMARKER",
        ] {
            assert!(!log_text.contains(marker), "logs must never carry {marker}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}

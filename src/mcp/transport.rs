//! The two v1 transports: a stdio child process and a streamable-HTTP endpoint.
//!
//! Both hide the same thing from the client: how a request becomes an answer.
//! stdio is newline-delimited JSON-RPC over the child's pipes with an ambient
//! reader task; streamable HTTP is one POST per request whose answer may come
//! back as plain JSON *or* as one SSE frame.
//!
//! Privacy rule shared by both: a server's own diagnostics never reach our logs
//! or error text (a GitHub server that prints its token on start-up is not our
//! bug to broadcast), and URLs are reported as host + path, never with a query
//! that could carry a credential.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, oneshot};

use super::rpc::{
    McpError, McpTransport, NotificationSink, notification_frame, request_frame, response_result,
};

/// Requests waiting for their answer: the reader task hands a framed result
/// back to whichever call owns the id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

/// Where a transport delivers a server→client notification. `None` until the
/// owning pool installs one, so a notification that arrives during the
/// handshake is dropped rather than buffered by the transport itself.
type SinkSlot = Arc<std::sync::Mutex<Option<NotificationSink>>>;

/// Deliver one parsed frame to the sink, if it is a notification and a sink is
/// installed. Shared by both transports so the "is this a notification" rule
/// has one definition: no id, a `method` string, and not a response.
fn deliver_notification(slot: &SinkSlot, frame: &Value) {
    if frame.get("id").is_some() || frame.get("result").is_some() || frame.get("error").is_some() {
        return;
    }
    let Some(method) = frame.get("method").and_then(Value::as_str) else {
        return;
    };
    let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));
    // Cloning the Arc out of the lock keeps the callback off the critical
    // section: a slow consumer must not block the reader task.
    let sink = slot.lock().ok().and_then(|guard| guard.clone());
    if let Some(sink) = sink {
        sink(method, params);
    }
}

/// stdio transport: spawn the server as a child process and speak
/// newline-delimited JSON-RPC over its stdin/stdout.
pub struct StdioTransport {
    writer: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    child: Arc<Mutex<Option<Child>>>,
    /// Shared with the reader task, which raises it on EOF.
    closed: Arc<AtomicBool>,
    /// Bytes the server wrote to stderr. Counted, never captured: it is the
    /// only hint that a server is complaining, and its content may be secret.
    stderr_bytes: Arc<AtomicUsize>,
    notifications: SinkSlot,
}

impl StdioTransport {
    /// Spawn `command` and take over its pipes. The returned transport owns the
    /// child: closing it kills the process.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Arc<Self>, McpError> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A server that outlives the transport is a leaked process, so the
            // child dies with the last handle even on an error path.
            .kill_on_drop(true)
            // The child does not inherit the gateway's environment: a server
            // must not pick up an unrelated provider key sitting in our process.
            // PATH and HOME come through because `npx`-style launchers need
            // them, plus exactly the variables the server config names.
            .env_clear();
        for (key, value) in std::env::vars().filter(|(k, _)| k == "PATH" || k == "HOME") {
            cmd.env(key, value);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("cannot spawn `{command}`: {e}")))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".to_string()))?;
        let stderr = child.stderr.take();

        let closed = Arc::new(AtomicBool::new(false));
        let transport = Arc::new(Self {
            writer: Arc::new(Mutex::new(stdin)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            child: Arc::new(Mutex::new(Some(child))),
            closed: closed.clone(),
            stderr_bytes: Arc::new(AtomicUsize::new(0)),
            notifications: Arc::new(std::sync::Mutex::new(None)),
        });

        route_reader(
            BufReader::new(stdout),
            transport.pending.clone(),
            transport.writer.clone(),
            closed,
            transport.notifications.clone(),
        );
        if let Some(stderr) = stderr {
            spawn_stderr_counter(stderr, transport.stderr_bytes.clone());
        }
        Ok(transport)
    }
}

/// Read stdout lines and route them. Split out so tests can drive it with any
/// async reader.
fn route_reader<R, W>(
    reader: BufReader<R>,
    pending: Pending,
    writer: Arc<Mutex<W>>,
    closed: Arc<AtomicBool>,
    notifications: SinkSlot,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                        continue; // Non-JSON noise on stdout is not ours to parse.
                    };
                    let id = frame.get("id").and_then(Value::as_u64);
                    let is_response = frame.get("result").is_some() || frame.get("error").is_some();
                    match (id, is_response) {
                        (Some(id), true) => {
                            let sender = pending.lock().await.remove(&id);
                            if let Some(sender) = sender {
                                let _ = sender.send(response_result(&frame));
                            }
                        }
                        (Some(id), false) => {
                            // A server→client request (sampling, roots, ping):
                            // refuse it, because waiting on one would hang us.
                            let reply = json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32601, "message": "Method not supported by slimbot"},
                            });
                            let mut w = writer.lock().await;
                            let _ = w.write_all(format!("{}\n", reply).as_bytes()).await;
                            let _ = w.flush().await;
                        }
                        // A notification from the server. It carries no id
                        // and expects no answer; the pool decides what it is
                        // worth, and drops it when nothing declared it.
                        _ => deliver_notification(&notifications, &frame),
                    }
                }
                Ok(None) | Err(_) => {
                    // EOF: the child is gone. Fail every waiter and mark closed.
                    closed.store(true, Ordering::Release);
                    let mut waiters = pending.lock().await;
                    for sender in std::mem::take(&mut *waiters).into_values() {
                        let _ = sender.send(Err(McpError::Transport(
                            "server process closed its output".to_string(),
                        )));
                    }
                    return;
                }
            }
        }
    });
}
fn spawn_stderr_counter(stderr: tokio::process::ChildStderr, counter: Arc<AtomicUsize>) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            // Count only. The line itself may carry a token.
            counter.fetch_add(line.len() + 1, Ordering::Relaxed);
        }
    });
}

/// Removes a request from the pending map when the caller stops waiting for it.
/// The reader task removes an answered request, so this covers the other half: a
/// request whose future was dropped (an outer timeout) must not leave an entry
/// behind for the rest of the connection's life.
struct PendingCleanup<'a> {
    id: u64,
    pending: &'a Pending,
}

impl Drop for PendingCleanup<'_> {
    fn drop(&mut self) {
        // Never blocks: a drop must not wait on a lock the reader task holds.
        if let Ok(mut guard) = self.pending.try_lock() {
            guard.remove(&self.id);
        }
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, id: u64, method: &str, params: Value) -> Result<Value, McpError> {
        if self.closed.load(Ordering::Acquire) {
            // The count is the only stderr signal we surface: a nonzero number
            // tells a launcher was complaining without echoing what it said (the
            // line may carry a token), and it beats claiming stderr was empty.
            let bytes = self.stderr_bytes.load(Ordering::Relaxed);
            return Err(McpError::Transport(format!(
                "server process is gone ({bytes} stderr byte(s), content not logged)"
            )));
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        // Our own timeout drops this future mid-await, so the entry is cleaned up
        // on the way out rather than left for a server that never answers.
        let _cleanup = PendingCleanup {
            id,
            pending: &self.pending,
        };
        let frame = request_frame(id, method, params);
        {
            let mut writer = self.writer.lock().await;
            if let Err(e) = writer.write_all(format!("{frame}\n").as_bytes()).await {
                return Err(McpError::Transport(format!("cannot write to stdin: {e}")));
            }
            let _ = writer.flush().await;
        }
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(McpError::Transport(
                "no answer before the pipe closed".into(),
            )),
        }
    }

    fn set_notification_sink(&self, sink: NotificationSink) {
        // The reader task is already running and reads the slot per frame, so
        // an installed sink takes effect on the next line the child writes.
        if let Ok(mut slot) = self.notifications.lock() {
            *slot = Some(sink);
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let frame = notification_frame(method, params);
        let mut writer = self.writer.lock().await;
        writer
            .write_all(format!("{frame}\n").as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("cannot write to stdin: {e}")))?;
        writer
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("cannot flush stdin: {e}")))
    }

    async fn close(&self) {
        self.closed.store(true, Ordering::Release);
        let mut guard = self.child.lock().await;
        if let Some(mut child) = guard.take() {
            let _ = child.kill().await;
        }
        let mut waiters = self.pending.lock().await;
        for sender in std::mem::take(&mut *waiters).into_values() {
            let _ = sender.send(Err(McpError::Transport("connection closed".into())));
        }
    }
}

/// Streamable-HTTP transport: one POST per JSON-RPC message, answering either
/// as `application/json` or as an SSE stream containing our frame.
pub struct HttpTransport {
    client: reqwest::Client,
    /// Full endpoint for requests; only host + path is ever reported.
    url: String,
    /// Static request headers (may carry a bearer token): never logged.
    headers: Vec<(String, String)>,
    session_id: Mutex<Option<String>>,
    protocol_version: Mutex<Option<String>>,
    notifications: SinkSlot,
}

impl std::fmt::Debug for HttpTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Headers are excluded on purpose: they hold the credential.
        f.debug_struct("HttpTransport")
            .field("endpoint", &self.safe_endpoint())
            .finish_non_exhaustive()
    }
}

impl HttpTransport {
    pub fn new(url: String, headers: Vec<(String, String)>) -> Arc<Self> {
        Arc::new(Self {
            client: reqwest::Client::new(),
            url,
            headers,
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            notifications: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// The URL with its query stripped, so an error text can name the endpoint
    /// without repeating a token that a server put in the query string. The port
    /// stays: it identifies the server (two loopback connectors are otherwise
    /// indistinguishable) and carries no credential.
    fn safe_endpoint(&self) -> String {
        match url::Url::parse(&self.url) {
            Ok(parsed) => {
                let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
                format!(
                    "{}://{}{}{}",
                    parsed.scheme(),
                    parsed.host_str().unwrap_or("?"),
                    port,
                    parsed.path()
                )
            }
            Err(_) => self.url.split('?').next().unwrap_or("?").to_string(),
        }
    }

    fn accept_headers(
        &self,
        session: Option<&str>,
        protocol: Option<&str>,
    ) -> Vec<(String, String)> {
        let mut out = vec![
            (
                "accept".to_string(),
                "application/json, text/event-stream".to_string(),
            ),
            ("content-type".to_string(), "application/json".to_string()),
        ];
        if let Some(id) = session {
            out.push(("mcp-session-id".to_string(), id.to_string()));
        }
        if let Some(v) = protocol {
            out.push(("mcp-protocol-version".to_string(), v.to_string()));
        }
        out
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn request(&self, id: u64, method: &str, params: Value) -> Result<Value, McpError> {
        let frame = request_frame(id, method, params);
        let session = self.session_id.lock().await.clone();
        let protocol = self.protocol_version.lock().await.clone();
        let mut request = self.client.post(&self.url);
        for (key, value) in self.headers.iter().chain(
            self.accept_headers(session.as_deref(), protocol.as_deref())
                .iter(),
        ) {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request
            .json(&frame)
            .send()
            .await
            .map_err(|e| self.transport_error("request", &e))?;
        let status = response.status();
        if !status.is_success() {
            // The body is not surfaced: an upstream proxy error page can embed
            // the request line, which may carry a credential.
            return Err(McpError::Transport(format!(
                "{} answered HTTP {status}",
                self.safe_endpoint()
            )));
        }
        if let Some(new_session) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().await = Some(new_session.to_string());
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let wanted = format!("response to {method}");
        let frame = if content_type.contains("text/event-stream") {
            self.read_sse_for(response, id).await.ok_or_else(|| {
                McpError::Protocol(format!("event stream carried no {wanted} for id {id}"))
            })?
        } else {
            response
                .json::<Value>()
                .await
                .map_err(|e| self.transport_error("decode", &e))?
        };
        if method == "initialize" {
            let result = response_result(&frame)?;
            if let Some(v) = result.get("protocolVersion").and_then(Value::as_str) {
                *self.protocol_version.lock().await = Some(v.to_string());
            }
            return Ok(result);
        }
        response_result(&frame)
    }

    fn set_notification_sink(&self, sink: NotificationSink) {
        if let Ok(mut slot) = self.notifications.lock() {
            *slot = Some(sink);
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        let frame = notification_frame(method, params);
        let session = self.session_id.lock().await.clone();
        let protocol = self.protocol_version.lock().await.clone();
        let mut request = self.client.post(&self.url);
        for (key, value) in self.headers.iter().chain(
            self.accept_headers(session.as_deref(), protocol.as_deref())
                .iter(),
        ) {
            request = request.header(key.as_str(), value.as_str());
        }
        let response = request
            .json(&frame)
            .send()
            .await
            .map_err(|e| self.transport_error("notify", &e))?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(McpError::Transport(format!(
                "{} answered HTTP {} to a notification",
                self.safe_endpoint(),
                response.status()
            )))
        }
    }

    async fn close(&self) {
        // Best-effort session termination: a server that never implemented it
        // answers 4xx, and a closed connection must not look like a failure.
        let session = self.session_id.lock().await.clone();
        let protocol = self.protocol_version.lock().await.clone();
        if let Some(id) = session {
            let mut request = self.client.delete(&self.url);
            for (key, value) in self
                .headers
                .iter()
                .chain(self.accept_headers(Some(&id), protocol.as_deref()).iter())
            {
                request = request.header(key.as_str(), value.as_str());
            }
            let _ = request.send().await;
        }
    }
}

impl HttpTransport {
    fn transport_error(&self, stage: &str, e: &reqwest::Error) -> McpError {
        // `reqwest::Error`'s Display can echo the URL; ours never does.
        McpError::Transport(format!(
            "{}: {stage} failed ({}{})",
            self.safe_endpoint(),
            if e.is_timeout() {
                "timeout"
            } else {
                "transport error"
            },
            ""
        ))
    }

    /// Consume SSE frames until one carries the response for `id`, then stop.
    ///
    /// Server→client notifications that the server interleaves into *this*
    /// response stream are handed to the sink on the way. HTTP here is one POST
    /// per request, so there is no standing stream a server can push on: a
    /// notification is only seen while some call is waiting, and whatever the
    /// server pushes between calls is never read. That is a property of the
    /// protocol language, not a gap this transport can close — stdio, which
    /// keeps a reader on the child's stdout, is where notifications are live.
    async fn read_sse_for(&self, response: reqwest::Response, id: u64) -> Option<Value> {
        // `bytes_stream` yields one `Bytes` chunk per read; the frame loop only
        // ever needs the byte slice, so the error type is the only thing named.
        let bytes = async_stream::try_stream! {
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                yield chunk?;
            }
        };
        read_sse_for_frames::<_, _, reqwest::Error>(bytes, id, self.notifications.clone()).await
    }
}

/// Consume SSE `data:` frames from `bytes` until one carries the response for
/// `id`. Frames without that id are server→client notifications and go to the
/// sink; a frame that is not JSON at all is skipped, since it is not ours.
///
/// Split from [`HttpTransport::read_sse_for`] so the frame handling is testable
/// without a socket: an in-memory byte stream drives the same loop a live
/// response would.
async fn read_sse_for_frames<S, B, E>(bytes: S, id: u64, notifications: SinkSlot) -> Option<Value>
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    let mut stream = Box::pin(bytes);
    let mut buffer = String::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { return None };
        buffer.push_str(&String::from_utf8_lossy(chunk.as_ref()));
        // Frames are separated by a blank line; data lines may be split.
        while let Some(edge) = buffer.find("\n\n") {
            let frame = buffer[..edge].to_string();
            buffer.drain(..edge + 2);
            let mut data = String::new();
            for line in frame.lines() {
                if let Some(payload) = line.strip_prefix("data:") {
                    data.push_str(payload.trim_start());
                }
            }
            if data.is_empty() {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if parsed.get("id").and_then(Value::as_u64) == Some(id) {
                return Some(parsed);
            }
            deliver_notification(&notifications, &parsed);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::rpc::McpClient;

    /// A real child process speaking MCP over stdio: the test binary re-invoked
    /// as a single-purpose server, so the transport is exercised without an
    /// external dependency. Outside that re-invocation the test does nothing.
    #[test]
    fn mcp_stdio_child_server() {
        // The parent re-runs the test binary with this exact name and this env
        // var set; a normal `cargo test` run returns immediately.
        if std::env::var("SLIMBOT_MCP_CHILD").is_err() {
            return;
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let stdin = tokio::io::stdin();
            let mut lines = BufReader::new(stdin).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(frame) = serde_json::from_str::<Value>(line.trim()) else {
                    continue;
                };
                let Some(id) = frame.get("id").and_then(Value::as_u64) else {
                    continue; // Notification (initialized): nothing to answer.
                };
                let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "initialize" => json!({
                        "protocolVersion": super::super::rpc::PROTOCOL_VERSION,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "fake-child", "version": "0"},
                    }),
                    "tools/list" => json!({"tools": [{
                        "name": "ping",
                        "description": "answer pong",
                        "annotations": {"readOnlyHint": true},
                    }]}),
                    "tools/call" => json!({"content": [{"type": "text", "text": "pong"}]}),
                    _ => Value::Null,
                };
                let reply = if result.is_null() {
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "nope"}})
                } else {
                    json!({"jsonrpc": "2.0", "id": id, "result": result})
                };
                // The leading newline ends whatever partial line the harness
                // printed (it writes `test <name> ... ` with no newline), so
                // this frame starts on a line of its own, as the protocol asks.
                println!("\n{reply}");
            }
        });
    }

    /// The harness path a child re-invocation needs: `--exact <full test name>`.
    const CHILD_TEST: &str = "mcp::transport::tests::mcp_stdio_child_server";

    #[test]
    fn test_stdio_transport_commits_and_reads_a_real_child() {
        let exe = std::env::current_exe().expect("test binary path");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let transport = StdioTransport::spawn(
                exe.to_str().unwrap(),
                &[
                    "--exact".into(),
                    CHILD_TEST.into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ],
                &[("SLIMBOT_MCP_CHILD".into(), "1".into())],
            )
            .await
            .expect("child must spawn");
            // Every step gets a deadline: a transport that loses a frame must
            // fail this test, not hang the suite for the length of a CI job.
            let budget = std::time::Duration::from_secs(20);
            let client = tokio::time::timeout(budget, McpClient::connect(transport.clone()))
                .await
                .expect("handshake over stdio must finish")
                .expect("handshake over stdio");
            let tools = tokio::time::timeout(budget, client.list_tools())
                .await
                .expect("tools/list must answer")
                .unwrap();
            assert_eq!(tools.len(), 1, "{tools:?}");
            assert_eq!(tools[0].name, "ping");
            let text = tokio::time::timeout(budget, client.call_tool("fake", "ping", &json!({})))
                .await
                .expect("tools/call must answer")
                .unwrap();
            assert!(text.contains("pong"), "{text}");
            // Closing kills the child; a later call fails cleanly, not hangs.
            client.close().await;
            let err = client
                .call_tool("fake", "ping", &json!({}))
                .await
                .unwrap_err();
            assert!(matches!(err, McpError::Transport(_)), "{err}");
        });
    }

    #[test]
    fn test_http_endpoint_reported_without_query_or_headers() {
        let transport = HttpTransport::new(
            "https://mcp.example.com:8443/rpc?token=super-secret".to_string(),
            vec![(
                "authorization".to_string(),
                "Bearer super-secret".to_string(),
            )],
        );
        let debug = format!("{transport:?}");
        assert!(
            debug.contains("https://mcp.example.com:8443/rpc"),
            "{debug}"
        );
        assert!(!debug.contains("super-secret"), "{debug}");
        assert!(
            !transport.safe_endpoint().contains("token"),
            "{}",
            transport.safe_endpoint()
        );
    }

    /// The sink a test installs: every delivered `(method, params)` in order.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Value)>>>;

    fn collecting_sink() -> (NotificationSink, Seen) {
        let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let sink: NotificationSink = Arc::new(move |method, params| {
            recorder
                .lock()
                .expect("recorder")
                .push((method.to_string(), params));
        });
        (sink, seen)
    }

    /// A slot filled before the reader starts, standing in for the pool
    /// installing its sink.
    fn filled_slot(sink: NotificationSink) -> SinkSlot {
        Arc::new(std::sync::Mutex::new(Some(sink)))
    }

    /// Let the reader task run until the recorder holds `n` entries.
    async fn wait_for_count(seen: &Seen, n: usize) -> Vec<(String, Value)> {
        for _ in 0..400 {
            let recorded = seen.lock().expect("recorder").clone();
            if recorded.len() >= n {
                return recorded;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("reader delivered {n} notification(s) short of the expectation");
    }

    /// Drain one pending request's channel, asserting the frame arrived.
    async fn answered(rx: oneshot::Receiver<Result<Value, McpError>>) -> Value {
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("the reader must answer id 7")
            .expect("the sender must still be alive");
        result.expect("id 7 carries a result")
    }

    #[tokio::test]
    async fn test_stdio_reader_forwards_server_notifications_to_the_sink() {
        // The reader sees every line the child writes. A frame with no id and a
        // method is a server push: it must reach the sink with its params, a
        // push without params must arrive as `{}`, and neither may disturb the
        // answers routed around them.
        let body = concat!(
            r#"{"jsonrpc":"2.0","method":"notifications/builds","params":{"n":1}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/empty"}"#,
            "\n",
        );
        let (reader, mut child) = tokio::io::duplex(4096);
        child
            .write_all(body.as_bytes())
            .await
            .expect("feed the reader");
        let (writer, _child_stdin) = tokio::io::duplex(64);
        let writer = Arc::new(Mutex::new(writer));

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);
        let (sink, seen) = collecting_sink();

        route_reader(
            BufReader::new(reader),
            pending,
            writer,
            Arc::new(AtomicBool::new(false)),
            filled_slot(sink),
        );

        assert_eq!(answered(rx).await, json!({"ok": true}));
        let recorded = wait_for_count(&seen, 2).await;
        assert_eq!(
            recorded,
            vec![
                ("notifications/builds".to_string(), json!({"n": 1})),
                ("notifications/empty".to_string(), json!({})),
            ],
            "notifications arrive in order, with params defaulted to {{}}"
        );
    }

    #[tokio::test]
    async fn test_stdio_reader_without_a_sink_drops_notifications_quietly() {
        // A transport with no sink installed is what a connection looks like
        // before the pool wires it: pushes are dropped, and the reader keeps
        // answering what it is asked.
        let (reader, mut child) = tokio::io::duplex(4096);
        child
            .write_all(
                concat!(
                    r#"{"jsonrpc":"2.0","method":"notifications/builds","params":{"n":1}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .await
            .expect("feed the reader");
        let (writer, _child_stdin) = tokio::io::duplex(64);
        let writer = Arc::new(Mutex::new(writer));

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending.lock().await.insert(7, tx);

        route_reader(
            BufReader::new(reader),
            pending,
            writer,
            Arc::new(AtomicBool::new(false)),
            Arc::new(std::sync::Mutex::new(None)),
        );

        assert_eq!(answered(rx).await, json!({"ok": true}));
    }

    #[tokio::test]
    async fn test_sse_stream_forwards_an_interleaved_notification() {
        // HTTP has no standing stream: a server push is only visible when the
        // server interleaves it into the response stream of a request we are
        // waiting on. That frame must reach the sink, and the answer we asked
        // for must still come back.
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/builds\",\"params\":{\"n\":1}}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n";
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(body.as_bytes().to_vec())];
        let (sink, seen) = collecting_sink();

        let frame = read_sse_for_frames(futures::stream::iter(chunks), 7, filled_slot(sink))
            .await
            .expect("the answer frame must be returned");

        assert_eq!(response_result(&frame).unwrap(), json!({"ok": true}));
        assert_eq!(
            *seen.lock().expect("recorder"),
            vec![("notifications/builds".to_string(), json!({"n": 1}))]
        );
    }

    #[tokio::test]
    async fn test_sse_stream_stops_at_the_answer_frame() {
        // Frames after our answer are not read: the connection is finished with
        // once the response for the id arrives, which is the whole shape of
        // "one request, one response" over HTTP.
        let body = "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/late\"}\n\n";
        let chunks: Vec<Result<Vec<u8>, std::io::Error>> = vec![Ok(body.as_bytes().to_vec())];
        let (sink, seen) = collecting_sink();

        let _ = read_sse_for_frames(futures::stream::iter(chunks), 7, filled_slot(sink)).await;
        assert!(
            seen.lock().expect("recorder").is_empty(),
            "nothing past the answer is consumed"
        );
    }

    #[tokio::test]
    async fn test_sse_frames_are_matched_by_id() {
        // Feed the parser two frames and expect only the one carrying id 7.
        let body = concat!(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\"}\n\n",
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"ok\":true}}\n\n",
        );
        let parsed: Vec<Value> = body
            .split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .filter_map(|frame| {
                let data = frame.lines().find_map(|l| l.strip_prefix("data:"))?;
                serde_json::from_str(data.trim()).ok()
            })
            .collect();
        let matched = parsed
            .iter()
            .find(|f| f.get("id").and_then(Value::as_u64) == Some(7));
        assert!(matched.is_some(), "{parsed:?}");
        assert_eq!(
            response_result(matched.unwrap()).unwrap(),
            json!({"ok": true})
        );
    }
}

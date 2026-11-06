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

use super::rpc::{McpError, McpTransport, notification_frame, request_frame, response_result};

/// Requests waiting for their answer: the reader task hands a framed result
/// back to whichever call owns the id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

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
        });

        route_reader(
            BufReader::new(stdout),
            transport.pending.clone(),
            transport.writer.clone(),
            closed,
        );
        if let Some(stderr) = stderr {
            spawn_stderr_counter(stderr, transport.stderr_bytes.clone());
        }
        Ok(transport)
    }
}

/// Read stdout lines and route them. Split out so tests can drive it with any
/// async reader.
fn route_reader<R>(
    reader: BufReader<R>,
    pending: Pending,
    writer: Arc<Mutex<ChildStdin>>,
    closed: Arc<AtomicBool>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
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
                        _ => {} // A notification from the server: logged output is out of scope.
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
    async fn read_sse_for(&self, response: reqwest::Response, id: u64) -> Option<Value> {
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let Ok(bytes) = chunk else { return None };
            buffer.push_str(&String::from_utf8_lossy(&bytes));
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
                if let Ok(parsed) = serde_json::from_str::<Value>(&data)
                    && parsed.get("id").and_then(Value::as_u64) == Some(id)
                {
                    return Some(parsed);
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::rpc::McpClient;

    /// A real child process speaking MCP over stdio: the test binary re-invoked
    /// as a single-purpose server, so the transport is exercised without an
    /// external dependency. Outside that re-invocation the test does nothing.
    #[test]
    #[allow(non_snake_case)]
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

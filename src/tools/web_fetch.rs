use std::net::IpAddr;
use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use regex::Regex;
use serde::Serialize;
use url::Url;

use crate::config::WebFetchConfig;
use crate::config::default_user_agent;
use crate::tool::Tool;
use crate::warn_log;

/// Maximum number of HTTP redirects to follow.
const MAX_REDIRECTS: usize = 5;

/// Maximum response body size in bytes (10 MB).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// Minimum allowed value for the max_chars parameter.
const MIN_MAX_CHARS: usize = 100;

// ── Static Regex patterns (compiled once) ──

static RE_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script[\s\S]*?</script>").unwrap());
static RE_STYLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<style[\s\S]*?</style>").unwrap());
static RE_TAGS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<[^>]+>").unwrap());
static RE_SPACES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static RE_BLANK_LINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

// ── SSRF protection ──

/// Check whether an IP address is private, loopback, link-local, or otherwise
/// not safe to access from a server-side fetcher.
fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // ULA
        }
    }
}

/// Resolve a hostname and check that none of the resulting IPs are private.
/// Returns Err if the host resolves to a private/loopback address.
fn validate_host_ip(host: &str) -> std::result::Result<(), String> {
    // Try to parse as IP literal first
    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(format!("Blocked: {} resolves to a private address", host));
        }
        return Ok(());
    }

    // For domain names, resolve via DNS
    match std::net::ToSocketAddrs::to_socket_addrs(&format!("{}:0", host)) {
        Ok(addrs) => {
            for addr in addrs {
                if is_private_ip(addr.ip()) {
                    return Err(format!(
                        "Blocked: {} resolves to private address {}",
                        host,
                        addr.ip()
                    ));
                }
            }
            Ok(())
        }
        Err(e) => Err(format!("DNS resolution failed for {}: {}", host, e)),
    }
}

// ── URL validation ──

/// Validate URL: only http/https allowed, domain must be present, and the
/// host must not resolve to a private/loopback IP (SSRF protection).
fn validate_url(url: &str) -> std::result::Result<(), String> {
    let parsed = Url::parse(url).map_err(|e| format!("Invalid URL: {}", e))?;

    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "Invalid URL: only http/https allowed, got '{}'",
            scheme
        ));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| "Invalid URL: missing domain".to_string())?;
    if host.is_empty() {
        return Err("Invalid URL: missing domain".to_string());
    }

    validate_host_ip(host)?;

    Ok(())
}

// ── HTML processing ──

/// Strip HTML tags and decode entities for plain-text extraction.
fn strip_html_tags(html: &str) -> String {
    // Remove script/style blocks entirely
    let without_scripts = RE_SCRIPT.replace_all(html, "");
    let without_styles = RE_STYLE.replace_all(&without_scripts, "");
    // Drop remaining tags
    let stripped = RE_TAGS.replace_all(&without_styles, "");
    // Decode common HTML entities (order matters: decode &lt;/&gt; first,
    // then &amp; last to prevent double-decoding)
    stripped
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Normalize whitespace: collapse runs of spaces/tabs, cap blank-line runs.
fn normalize_whitespace(text: &str) -> String {
    let collapsed = RE_SPACES.replace_all(text, " ");
    RE_BLANK_LINES
        .replace_all(&collapsed, "\n\n")
        .trim()
        .to_string()
}

// ── Char-boundary-safe truncation ──

/// Truncate a string at the nearest char boundary at or before `max_chars` bytes.
fn truncate_at_char_boundary(text: &str, max_chars: usize) -> &str {
    if text.len() <= max_chars {
        return text;
    }
    let end = text
        .char_indices()
        .map(|(i, _)| i)
        .take_while(|&i| i <= max_chars)
        .last()
        .unwrap_or(0);
    &text[..end]
}

// ── Result types ──

/// Output payload returned on a successful fetch.
#[derive(Serialize)]
struct WebFetchResult {
    url: String,
    #[serde(rename = "finalUrl")]
    final_url: String,
    status: u16,
    extractor: String,
    truncated: bool,
    length: usize,
    text: String,
}

/// Output payload returned when the fetch fails. Errors are returned as JSON
/// strings (not Result::Err) so the model can analyze and retry.
#[derive(Serialize)]
struct WebFetchError {
    error: String,
    url: String,
}

pub struct WebFetchTool {
    client: reqwest::Client,
    max_chars: usize,
    timeout_s: u64,
    user_agent: String,
}

impl WebFetchTool {
    pub fn new(config: Option<&WebFetchConfig>) -> Self {
        match config {
            Some(cfg) => Self {
                client: Self::build_client(cfg.timeout_s),
                max_chars: cfg.max_chars,
                timeout_s: cfg.timeout_s,
                user_agent: cfg.user_agent.clone(),
            },
            None => Self::default(),
        }
    }

    /// Build a reqwest client with timeout and SSRF-safe redirect policy.
    fn build_client(timeout_s: u64) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(timeout_s))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                // Validate each redirect target against SSRF rules
                let url = attempt.url();
                let host = url.host_str().unwrap_or("");
                if host.is_empty() {
                    return attempt.stop();
                }
                let scheme = url.scheme();
                if scheme != "http" && scheme != "https" {
                    return attempt.stop();
                }
                // Check if redirect target resolves to a private IP
                if validate_host_ip(host).is_err() {
                    return attempt.stop();
                }
                if attempt.previous().len() >= MAX_REDIRECTS {
                    attempt.stop()
                } else {
                    attempt.follow()
                }
            }))
            .build()
            .unwrap_or_default()
    }

    /// Check if a content type is acceptable for text extraction.
    fn is_acceptable_content_type(content_type: &str) -> bool {
        let ct = content_type.to_lowercase();
        ct.contains("text/html")
            || ct.contains("text/plain")
            || ct.contains("text/xml")
            || ct.contains("application/json")
            || ct.contains("application/xml")
            || ct.contains("application/xhtml+xml")
    }

    /// Fetch content from a URL. Returns (html, final_url, status_code).
    async fn fetch_html(&self, url: &str) -> std::result::Result<(String, String, u16), String> {
        let response = self
            .client
            .get(url)
            .header("User-Agent", &self.user_agent)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    format!("Request timeout after {}s", self.timeout_s)
                } else if e.is_connect() {
                    "Connection failed (DNS, network, or TLS error)".to_string()
                } else if e.is_redirect() {
                    format!(
                        "Redirect blocked or too many redirects (max {})",
                        MAX_REDIRECTS
                    )
                } else {
                    format!("Request failed: {}", e)
                }
            })?;

        let status = response.status().as_u16();
        let final_url = response.url().to_string();

        if !response.status().is_success() {
            return Err(format!(
                "HTTP {} {}",
                status,
                response.status().canonical_reason().unwrap_or("Error")
            ));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !Self::is_acceptable_content_type(&content_type) {
            return Err(format!("Unsupported content type: {}", content_type));
        }

        // Bounded read: reject oversized responses
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?;

        if bytes.len() > MAX_BODY_BYTES {
            return Err(format!(
                "Response body too large ({} bytes, max {} bytes)",
                bytes.len(),
                MAX_BODY_BYTES
            ));
        }

        let html = String::from_utf8_lossy(&bytes).into_owned();

        Ok((html, final_url, status))
    }

    /// Extract readable content from HTML. Returns (text, extractor_name).
    /// Falls back to raw HTML tag stripping when readability extraction fails.
    fn extract_content(&self, html: &str, extract_mode: &str) -> (String, String) {
        // Try readability-rust first for main-content extraction.
        let extracted: Option<(String, String)> =
            match readability_rust::Readability::new(html, None) {
                Ok(mut parser) => match parser.parse() {
                    Some(article) => {
                        let title = article.title.unwrap_or_default();
                        let content_html = article.content.unwrap_or_default();
                        if content_html.is_empty() {
                            None
                        } else {
                            let text = if extract_mode == "markdown" {
                                quick_html2md::html_to_markdown(&content_html)
                            } else {
                                normalize_whitespace(&strip_html_tags(&content_html))
                            };
                            let final_text = if title.is_empty() {
                                text
                            } else {
                                format!("# {}\n\n{}", title, text)
                            };
                            Some((final_text, "readability".to_string()))
                        }
                    }
                    None => None,
                },
                Err(_) => None,
            };

        match extracted {
            Some(result) => result,
            None => {
                // Fallback: convert the entire HTML document directly.
                let text = if extract_mode == "markdown" {
                    quick_html2md::html_to_markdown(html)
                } else {
                    normalize_whitespace(&strip_html_tags(html))
                };
                (text, "html".to_string())
            }
        }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        let timeout_s = 30u64;
        Self {
            client: Self::build_client(timeout_s),
            max_chars: 50000,
            timeout_s,
            user_agent: default_user_agent(),
        }
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }

    fn description(&self) -> &str {
        "Fetch a URL and extract readable content as markdown. Works for most web pages; may fail on login-walled or JS-heavy sites."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "URL to fetch (http or https)"
                },
                "max_chars": {
                    "type": "integer",
                    "description": "Maximum output length (default 50000)",
                    "default": 50000
                },
                "extract_mode": {
                    "type": "string",
                    "enum": ["markdown", "text"],
                    "description": "Output format (default markdown)",
                    "default": "markdown"
                }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, args: serde_json::Value) -> Result<String> {
        let url = args["url"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing required argument: url"))?;

        let max_chars = args["max_chars"]
            .as_u64()
            .map(|v| (v as usize).max(MIN_MAX_CHARS))
            .unwrap_or(self.max_chars);

        let extract_mode = args["extract_mode"].as_str().unwrap_or("markdown");

        // Validate URL (includes SSRF check)
        if let Err(e) = validate_url(url) {
            warn_log!("[web_fetch] Invalid URL '{}': {}", url, e);
            let error_result = WebFetchError {
                error: e,
                url: url.to_string(),
            };
            return Ok(serde_json::to_string(&error_result)?);
        }

        // Fetch HTML
        let (html, final_url, status) = match self.fetch_html(url).await {
            Ok(result) => result,
            Err(e) => {
                warn_log!("[web_fetch] Fetch failed for '{}': {}", url, e);
                let error_result = WebFetchError {
                    error: e,
                    url: url.to_string(),
                };
                return Ok(serde_json::to_string(&error_result)?);
            }
        };

        // Extract content
        let (text, extractor) = self.extract_content(&html, extract_mode);

        // Truncate if needed (char-boundary-safe)
        let truncated = text.len() > max_chars;
        let final_text = if truncated {
            truncate_at_char_boundary(&text, max_chars).to_string()
        } else {
            text
        };

        let result = WebFetchResult {
            url: url.to_string(),
            final_url,
            status,
            extractor,
            truncated,
            length: final_text.len(),
            text: final_text,
        };

        Ok(serde_json::to_string(&result)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_url_http_scheme() {
        assert!(validate_url("http://example.com").is_ok());
    }

    #[test]
    fn test_validate_url_https_scheme() {
        assert!(validate_url("https://example.com").is_ok());
    }

    #[test]
    fn test_validate_url_reject_ftp() {
        let result = validate_url("ftp://example.com");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("only http/https allowed"));
    }

    #[test]
    fn test_validate_url_reject_file() {
        let result = validate_url("file:///etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("only http/https allowed"));
    }

    #[test]
    fn test_validate_url_missing_domain() {
        let result = validate_url("http://");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_url_empty() {
        let result = validate_url("");
        assert!(result.is_err());
    }

    #[test]
    fn test_validate_url_with_path_and_query() {
        assert!(validate_url("https://example.com/article?id=42#section").is_ok());
    }

    #[test]
    fn test_validate_url_rejects_loopback() {
        assert!(validate_url("http://127.0.0.1/admin").is_err());
    }

    #[test]
    fn test_validate_url_rejects_private_ip() {
        assert!(validate_url("http://192.168.1.1/").is_err());
        assert!(validate_url("http://10.0.0.1/").is_err());
    }

    #[test]
    fn test_validate_url_rejects_link_local() {
        assert!(validate_url("http://169.254.169.254/metadata").is_err());
    }

    #[test]
    fn test_is_private_ip_loopback_v4() {
        assert!(is_private_ip("127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_private_v4() {
        assert!(is_private_ip("192.168.1.1".parse().unwrap()));
        assert!(is_private_ip("10.0.0.1".parse().unwrap()));
        assert!(is_private_ip("172.16.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_public_v4() {
        assert!(!is_private_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_private_ip("1.1.1.1".parse().unwrap()));
    }

    #[test]
    fn test_is_private_ip_loopback_v6() {
        assert!(is_private_ip("::1".parse().unwrap()));
    }

    #[test]
    fn test_extract_mode_markdown() {
        let tool = WebFetchTool::default();
        let html = r#"<html><body><article><h1>Title</h1><p>Content with <strong>bold</strong> text.</p></article></body></html>"#;
        let (text, extractor) = tool.extract_content(html, "markdown");
        assert!(!text.is_empty());
        assert_eq!(extractor, "readability");
    }

    #[test]
    fn test_extract_mode_text() {
        let tool = WebFetchTool::default();
        let html = r#"<html><body><article><h1>Title</h1><p>Content with <strong>bold</strong> text.</p></article></body></html>"#;
        let (text, extractor) = tool.extract_content(html, "text");
        assert!(!text.is_empty());
        assert!(!text.contains("<strong>"));
        assert!(!text.contains("<h1>"));
        assert_eq!(extractor, "readability");
    }

    #[test]
    fn test_extract_fallback_on_parse_failure() {
        let tool = WebFetchTool::default();
        // Readability requires article-like structure; empty body falls back
        let html = "<html><body><p>Some plain text without article structure</p></body></html>";
        let (text, extractor) = tool.extract_content(html, "text");
        // Should produce some text via either readability or fallback
        assert!(!text.is_empty());
        // Either extractor is fine; just verify the function doesn't panic
    }

    #[test]
    fn test_strip_html_tags_removes_scripts() {
        let html = r#"<p>Before</p><script>alert('x')</script><p>After</p>"#;
        let text = strip_html_tags(html);
        assert!(!text.contains("alert"));
        assert!(!text.contains("<script>"));
        assert!(text.contains("Before"));
        assert!(text.contains("After"));
    }

    #[test]
    fn test_strip_html_entity_decode_order() {
        // &amp;lt; should decode to &lt; (not to <)
        let html = "&amp;lt;script&amp;gt;";
        let text = strip_html_tags(html);
        assert_eq!(text, "&lt;script&gt;");
    }

    #[test]
    fn test_normalize_whitespace_collapses_spaces() {
        let text = "hello    world\n\n\n\nend";
        let normalized = normalize_whitespace(text);
        assert!(!normalized.contains("    "));
        assert!(!normalized.contains("\n\n\n"));
    }

    #[test]
    fn test_truncate_at_char_boundary_ascii() {
        let text = "Hello, World!";
        assert_eq!(truncate_at_char_boundary(text, 5), "Hello");
    }

    #[test]
    fn test_truncate_at_char_boundary_multibyte() {
        let text = "你好世界Hello";
        // Each CJK char is 3 bytes. At max_chars=7, we should get "你好" (6 bytes)
        let result = truncate_at_char_boundary(text, 7);
        assert_eq!(result, "你好");
        // Should NOT panic
    }

    #[test]
    fn test_truncate_at_char_boundary_emoji() {
        let text = "🎉🎊🎈";
        // Each emoji is 4 bytes. At max_chars=5, we should get "🎉" (4 bytes)
        let result = truncate_at_char_boundary(text, 5);
        assert_eq!(result, "🎉");
    }

    #[test]
    fn test_truncate_no_panic_on_zero() {
        let text = "Hello";
        assert_eq!(truncate_at_char_boundary(text, 0), "");
    }

    #[test]
    fn test_truncate_long_content() {
        let tool = WebFetchTool::default();
        let paragraph = "<p>abcdefghij</p>".repeat(1000);
        let html = format!("<html><body><article>{}</article></body></html>", paragraph);
        let (text, _) = tool.extract_content(&html, "text");
        let max_chars = 1000;
        let truncated = text.len() > max_chars;
        let final_text = if truncated {
            truncate_at_char_boundary(&text, max_chars).to_string()
        } else {
            text
        };
        assert!(truncated);
        assert!(final_text.len() <= max_chars);
    }

    #[test]
    fn test_no_truncate_short_content() {
        let tool = WebFetchTool::default();
        let html = r#"<html><body><article><p>Short content</p></article></body></html>"#;
        let (text, _) = tool.extract_content(html, "text");
        let max_chars = 50000;
        let truncated = text.len() > max_chars;
        assert!(!truncated);
    }

    #[tokio::test]
    async fn test_execute_missing_url_arg() {
        let tool = WebFetchTool::default();
        let result = tool.execute(serde_json::json!({})).await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Missing required argument")
        );
    }

    #[tokio::test]
    async fn test_execute_invalid_url_returns_error_json() {
        let tool = WebFetchTool::default();
        let result = tool
            .execute(serde_json::json!({"url": "ftp://example.com"}))
            .await
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(
            parsed["error"]
                .as_str()
                .unwrap()
                .contains("only http/https allowed")
        );
        assert_eq!(parsed["url"].as_str().unwrap(), "ftp://example.com");
    }

    #[tokio::test]
    async fn test_execute_empty_url_returns_error_json() {
        let tool = WebFetchTool::default();
        let result = tool.execute(serde_json::json!({"url": ""})).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert!(parsed["error"].as_str().unwrap().contains("Invalid URL"));
    }

    #[test]
    fn test_acceptable_content_types() {
        assert!(WebFetchTool::is_acceptable_content_type("text/html; charset=utf-8"));
        assert!(WebFetchTool::is_acceptable_content_type("text/plain"));
        assert!(WebFetchTool::is_acceptable_content_type("application/json"));
        assert!(WebFetchTool::is_acceptable_content_type("application/xml"));
        assert!(WebFetchTool::is_acceptable_content_type("application/xhtml+xml"));
        assert!(!WebFetchTool::is_acceptable_content_type("image/png"));
        assert!(!WebFetchTool::is_acceptable_content_type("application/octet-stream"));
    }
}

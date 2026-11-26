// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin mcp`: the stdio bridge (tab "MCP", Transports, stdio). One JSON-RPC message
//! per line in, one per line out; each message goes to the daemon over its Unix socket as
//! `POST /mcp` (or `/mcp/plugin`), or with `--url` to a remote server. It holds no tool
//! logic.

use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, header};
use knoggin_core::Sensitive;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::client::{Client, SendError};

/// How long the bridge keeps trying after it started a daemon.
const START_WAIT: Duration = Duration::from_secs(10);
/// The JSON-RPC internal error.
const INTERNAL: i64 = -32603;
/// The JSON-RPC parse error.
const PARSE: i64 = -32700;
/// A current-revision request carries its version in `params._meta` under this key.
const META_VERSION: &str = "io.modelcontextprotocol/protocolVersion";

/// Which tool set the bridge serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum ToolSet {
    Full,
    Plugin,
}

impl ToolSet {
    fn path(self) -> &'static str {
        match self {
            Self::Full => "/mcp",
            Self::Plugin => "/mcp/plugin",
        }
    }
}

/// Where the messages go.
pub(crate) enum Target {
    /// The local daemon. `start` starts one when the socket does not answer.
    Local {
        client: Client,
        start: Box<dyn Fn() -> anyhow::Result<()> + Send + Sync>,
    },
    /// A remote server with an API key.
    Remote {
        http: reqwest::Client,
        url: String,
        key: Sensitive<String>,
    },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Local { .. } => f.write_str("Target::Local"),
            Self::Remote { .. } => f.write_str("Target::Remote"),
        }
    }
}

/// Checks a `--url`: `https`, or plain `http` only when the host is written as `localhost`,
/// an address in `127.0.0.0/8` or `[::1]`. The name is never resolved to decide this.
pub(crate) fn check_url(url: &str) -> Result<(), String> {
    let refused =
        || format!("refusing {url}: an API key is only sent over https, or over http to localhost");
    let parsed = reqwest::Url::parse(url).map_err(|_| format!("{url} is not a URL"))?;
    match parsed.scheme() {
        "https" => Ok(()),
        "http" if loopback_host(&parsed) => Ok(()),
        _ => Err(refused()),
    }
}

/// Whether a URL's host is written as `localhost`, an address in `127.0.0.0/8` or `[::1]`.
pub(crate) fn loopback_host(url: &reqwest::Url) -> bool {
    match url.host_str() {
        Some("localhost") => true,
        Some(h) => h
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

/// The namespace of the nearest `.knoggin.toml` from `dir` upward. A file that does not
/// parse, or names an invalid namespace, is ignored with a warning.
pub(crate) fn find_namespace(dir: &Path) -> Option<String> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        namespace: String,
    }
    let path = dir
        .ancestors()
        .map(|d| d.join(".knoggin.toml"))
        .find(|p| p.is_file())?;
    let parsed = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| toml::from_str::<File>(&t).ok());
    match parsed {
        Some(f)
            if knoggin_core::model::limits::validate_namespace_name("namespace", &f.namespace)
                .is_ok() =>
        {
            Some(f.namespace)
        }
        _ => {
            tracing::warn!(
                kind = "knoggin_toml_ignored",
                "a .knoggin.toml was ignored: it must hold one valid `namespace`"
            );
            None
        }
    }
}

/// A header value, wrapped as `=?base64?...?=` when it is not plain visible ASCII.
fn mcp_header(value: &str) -> Option<HeaderValue> {
    if value.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        HeaderValue::from_str(value).ok()
    } else {
        let encoded = base64::engine::general_purpose::STANDARD.encode(value.as_bytes());
        HeaderValue::from_str(&format!("=?base64?{encoded}?=")).ok()
    }
}

/// The bridge's state across messages.
pub(crate) struct Bridge {
    target: Target,
    tool_set: ToolSet,
    namespace: Option<String>,
    /// The `Mcp-Session-Id` of an older revision's `initialize`.
    session: Option<HeaderValue>,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("target", &self.target)
            .field("tool_set", &self.tool_set)
            .finish_non_exhaustive()
    }
}

/// One answer from the other side.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

fn rpc_error(id: &Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(d) = data {
        error["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

impl Bridge {
    pub(crate) fn new(target: Target, tool_set: ToolSet, namespace: Option<String>) -> Self {
        Self {
            target,
            tool_set,
            namespace,
            session: None,
        }
    }

    /// Reads messages until standard input closes.
    // ponytail: one message at a time; a slow tool call holds up the next message. Spawn
    // per message (keeping `initialize` first) if agents need calls in parallel.
    pub(crate) async fn run(
        &mut self,
        input: impl AsyncBufRead + Unpin,
        mut output: impl AsyncWrite + Unpin,
    ) -> std::io::Result<()> {
        let mut lines = input.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = self.handle(&line).await {
                let mut text = reply.to_string();
                text.push('\n');
                output.write_all(text.as_bytes()).await?;
                output.flush().await?;
            }
        }
        Ok(())
    }

    /// The line to write for one message, if any.
    pub(crate) async fn handle(&mut self, line: &str) -> Option<Value> {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return Some(rpc_error(&Value::Null, PARSE, "Parse error", None));
        };
        let id = message.get("id").cloned();
        let is_request = message.is_object() && id.is_some();
        let rpc_id = id.unwrap_or(Value::Null);
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let mut headers = vec![
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (
                header::ACCEPT,
                HeaderValue::from_static("application/json, text/event-stream"),
            ),
        ];
        let version = message
            .pointer("/params/_meta")
            .and_then(|m| m.get(META_VERSION))
            .and_then(Value::as_str);
        if let Some(version) = version {
            // The current revision: the three headers come from the message itself.
            let mut add = |name: &'static str, v: &str| {
                if let Some(v) = mcp_header(v) {
                    headers.push((HeaderName::from_static(name), v));
                }
            };
            add("mcp-protocol-version", version);
            add("mcp-method", method);
            if let Some(name) = message.pointer("/params/name").and_then(Value::as_str) {
                add("mcp-name", name);
            }
        } else if let Some(sid) = &self.session
            && method != "initialize"
        {
            headers.push((HeaderName::from_static("mcp-session-id"), sid.clone()));
        }
        if let Some(ns) = self
            .namespace
            .as_deref()
            .and_then(|n| HeaderValue::from_str(n).ok())
        {
            headers.push((HeaderName::from_static("knoggin-namespace"), ns));
        }
        let answer = match self.send(&headers, Bytes::from(line.to_owned())).await {
            Ok(a) => a,
            Err(message) => {
                return is_request.then(|| rpc_error(&rpc_id, INTERNAL, &message, None));
            }
        };
        if method == "initialize"
            && let Some(sid) = answer.headers.get("mcp-session-id")
        {
            self.session = Some(sid.clone());
        }
        if answer.status == StatusCode::ACCEPTED && answer.body.is_empty() {
            return None;
        }
        let body: Value = serde_json::from_slice(&answer.body).unwrap_or(Value::Null);
        if body.get("jsonrpc").is_some() {
            return Some(body);
        }
        // One of the edge's own errors (`unauthenticated`, `store_locked`, ...): every call
        // still gets an answer.
        if !is_request {
            return None;
        }
        let body = body.get("error").unwrap_or(&body);
        let text = body.get("message").and_then(Value::as_str).map_or_else(
            || format!("the server answered {}", answer.status),
            str::to_owned,
        );
        let mut data = json!({ "code": body.get("code").cloned().unwrap_or(Value::Null) });
        if let Some(reason) = body.get("reason") {
            data["reason"] = reason.clone();
        }
        Some(rpc_error(&rpc_id, INTERNAL, &text, Some(data)))
    }

    async fn send(
        &self,
        headers: &[(HeaderName, HeaderValue)],
        body: Bytes,
    ) -> Result<Answer, String> {
        match &self.target {
            Target::Local { client, start } => {
                let path = self.tool_set.path();
                let first = client
                    .call(Method::POST, path, headers, Some(body.clone()))
                    .await;
                let reply = match first {
                    Err(SendError::NotRunning) => {
                        if let Err(e) = start() {
                            return Err(not_started(&format!("{e:#}")));
                        }
                        let deadline = tokio::time::Instant::now() + START_WAIT;
                        loop {
                            match client
                                .call(Method::POST, path, headers, Some(body.clone()))
                                .await
                            {
                                Err(SendError::NotRunning)
                                    if tokio::time::Instant::now() < deadline =>
                                {
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                                other => break other,
                            }
                        }
                    }
                    other => other,
                };
                match reply {
                    Ok(r) => Ok(Answer {
                        status: r.status,
                        headers: r.headers,
                        body: r.body,
                    }),
                    Err(SendError::NotRunning) => Err(not_started("it did not answer in time")),
                    Err(SendError::NotPrivate(m) | SendError::Io(m)) => Err(m),
                }
            }
            Target::Remote { http, url, key } => {
                let mut request = http
                    .post(url)
                    .bearer_auth(key.expose())
                    .body(body)
                    .timeout(Duration::from_secs(60));
                for (k, v) in headers {
                    request = request.header(k, v);
                }
                let r = request
                    .send()
                    .await
                    .map_err(|e| format!("could not reach {url}: {e}"))?;
                let status = r.status();
                let headers = r.headers().clone();
                let body = r
                    .bytes()
                    .await
                    .map_err(|e| format!("reading the answer: {e}"))?;
                Ok(Answer {
                    status,
                    headers,
                    body,
                })
            }
        }
    }
}

fn not_started(why: &str) -> String {
    format!(
        "Knoggin is not running and could not be started ({why}). Start it with `knoggin start`, \
         then try again; `knoggin status` shows what is wrong"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_http_url_is_refused_unless_the_host_is_written_as_loopback() {
        assert!(check_url("https://memory.example.com/mcp").is_ok());
        assert!(check_url("http://localhost:7690/mcp").is_ok());
        assert!(check_url("http://127.3.4.5:7690/mcp").is_ok());
        assert!(check_url("http://[::1]:7690/mcp").is_ok());
        assert!(check_url("http://memory.example.com/mcp").is_err());
        assert!(check_url("http://localhost.example.com/mcp").is_err());
        assert!(check_url("http://10.0.0.1/mcp").is_err());
        assert!(check_url("ftp://localhost/mcp").is_err());
    }

    #[test]
    fn the_namespace_comes_from_the_nearest_knoggin_toml() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(find_namespace(&deep), None);
        std::fs::write(d.path().join("a/.knoggin.toml"), "namespace = \"work\"\n").unwrap();
        assert_eq!(find_namespace(&deep).as_deref(), Some("work"));
        std::fs::write(d.path().join("a/b/.knoggin.toml"), "namespace = 3\n").unwrap();
        assert_eq!(find_namespace(&deep), None);
    }

    #[test]
    fn non_ascii_header_values_are_wrapped() {
        assert_eq!(mcp_header("tools/call").unwrap(), "tools/call");
        assert_eq!(mcp_header("é").unwrap(), "=?base64?w6k=?=");
    }
}

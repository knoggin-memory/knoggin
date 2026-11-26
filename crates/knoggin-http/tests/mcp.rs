// SPDX-License-Identifier: AGPL-3.0-only
//! The MCP endpoint through `store_layer` with an inserted context (tab "MCP", Tests that
//! must exist): the protocol and transport rules ported from recalld, both tool sets,
//! every tool, access recording, the client name, idempotency shared with REST.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use axum::body::Body;
use http::{Method, Request, StatusCode};
use knoggin_http::error::Code;
use knoggin_http::{Permissions, RequestContext};
use serde_json::{Value, json};

use support::{Env, Resp};

const RW: Permissions = Permissions::READ_WRITE;
const RO: Permissions = Permissions::READ;
const CURRENT: &str = "2026-07-28";

/// One MCP client of one endpoint and one revision.
struct Client<'a> {
    env: &'a Env,
    ctx: RequestContext,
    path: &'static str,
    /// The session of the older revisions; `None` speaks the current one.
    session: Option<String>,
    name: String,
}

impl<'a> Client<'a> {
    /// A current-revision client.
    fn modern(env: &'a Env, ctx: RequestContext, path: &'static str) -> Self {
        Self {
            env,
            ctx,
            path,
            session: None,
            name: "TestClient".into(),
        }
    }

    /// An older-revision client after `initialize`.
    async fn legacy(env: &'a Env, ctx: RequestContext, path: &'static str, name: &str) -> Self {
        let mut c = Self {
            env,
            ctx,
            path,
            session: None,
            name: name.into(),
        };
        let r = c.raw(&initialize(1, "2025-06-18", name), &[]).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        c.session = Some(r.headers["mcp-session-id"].to_str().unwrap().to_owned());
        c
    }

    /// Posts a body with extra headers and the session, if any.
    async fn raw(&self, body: &Value, headers: &[(&str, &str)]) -> Resp {
        let mut b = Request::builder()
            .method(Method::POST)
            .uri(self.path)
            .header("content-type", "application/json");
        if let Some(s) = &self.session {
            b = b.header("mcp-session-id", s);
        }
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut req = b.body(Body::from(body.to_string())).unwrap();
        req.extensions_mut().insert(self.ctx.clone());
        self.env.send(req).await
    }

    /// One request, with `_meta` and the three headers on the current revision.
    async fn request(&self, method: &str, mut params: Value) -> Resp {
        let mut headers = Vec::new();
        let name = params["name"].as_str().map(str::to_owned);
        if self.session.is_none() {
            params["_meta"] = json!({
                "io.modelcontextprotocol/protocolVersion": CURRENT,
                "io.modelcontextprotocol/clientInfo": { "name": self.name, "version": "1.0" },
                "io.modelcontextprotocol/clientCapabilities": {},
            });
            headers.push(("mcp-method", method.to_owned()));
            headers.push(("mcp-protocol-version", CURRENT.to_owned()));
            if let Some(n) = name {
                headers.push(("mcp-name", n));
            }
        }
        let h: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let body = json!({ "jsonrpc": "2.0", "id": 7, "method": method, "params": params });
        self.raw(&body, &h).await
    }

    /// `tools/call`; returns the tool result.
    async fn call(&self, tool: &str, args: Value) -> Value {
        let r = self
            .request("tools/call", json!({ "name": tool, "arguments": args }))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        assert!(r.json["error"].is_null(), "{}", r.json);
        r.json["result"].clone()
    }

    /// A call that must succeed; returns its JSON value.
    async fn ok(&self, tool: &str, args: Value) -> Value {
        let result = self.call(tool, args).await;
        assert!(result["isError"].is_null(), "{tool}: {result}");
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        if self.session.is_none() {
            assert_eq!(result["structuredContent"], text, "{tool}");
        } else {
            assert!(result.get("structuredContent").is_none(), "{tool}");
        }
        text
    }

    /// A call that must fail as a tool; returns the error code and field.
    async fn fails(&self, tool: &str, args: Value) -> (Code, Option<String>) {
        let result = self.call(tool, args).await;
        assert_eq!(result["isError"], true, "{tool}: {result}");
        let e: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert!(e["error"]["message"].is_string(), "{e}");
        let keys: Vec<_> = e["error"].as_object().unwrap().keys().cloned().collect();
        for k in &keys {
            assert!(
                ["code", "message", "field", "reason"].contains(&k.as_str()),
                "{e}"
            );
        }
        (
            serde_json::from_value(e["error"]["code"].clone()).unwrap(),
            e["error"]["field"].as_str().map(str::to_owned),
        )
    }

    async fn tools(&self) -> Vec<Value> {
        let r = self.request("tools/list", json!({})).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.json);
        r.json["result"]["tools"].as_array().unwrap().clone()
    }
}

fn initialize(id: i64, version: &str, name: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
        "protocolVersion": version, "capabilities": {},
        "clientInfo": { "name": name, "version": "1.0" } } })
}

fn plain(id: i64, method: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method })
}

/// Every log line of the process, at every level, from every thread.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Installs the capture as the global subscriber (nextest runs each test in its own
/// process, so no other test sees it).
fn capture_logs() -> Logs {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    logs
}

fn id_of(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

// ─── Transport and protocol (ported from recalld) ─────────────────────────────

#[tokio::test]
async fn current_revision_is_stateless() {
    let env = Env::new(true).await;
    let mut c = Client::modern(&env, env.t.ctx(RW), "/mcp");
    let r = c.request("tools/list", json!({})).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.headers.get("mcp-session-id").is_none());
    assert_eq!(r.json["result"]["resultType"], "complete");
    assert_eq!(r.json["result"]["cacheScope"], "private");
    assert_eq!(r.json["result"]["ttlMs"], 3_600_000);
    assert_eq!(
        r.json["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "knoggin"
    );
    // An inbound session ID is ignored, even an unknown one.
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": {
        "io.modelcontextprotocol/protocolVersion": CURRENT,
        "io.modelcontextprotocol/clientCapabilities": {} } } });
    c.session = None;
    let r = c
        .raw(
            &body,
            &[
                ("mcp-method", "tools/list"),
                ("mcp-protocol-version", CURRENT),
                ("mcp-session-id", "does-not-exist"),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.headers.get("mcp-session-id").is_none());
}

#[tokio::test]
async fn current_revision_headers_must_match_the_body() {
    let env = Env::new(true).await;
    let c = Client::modern(&env, env.t.ctx(RW), "/mcp");
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion": CURRENT,
        "io.modelcontextprotocol/clientCapabilities": {} });
    let list =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": meta } });
    let call = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "_meta": meta, "name": "list_namespaces", "arguments": {} } });
    let enc = format!("=?base64?{}?=", "bGlzdF9uYW1lc3BhY2Vz"); // list_namespaces
    type Case<'a> = (&'a Value, Vec<(&'a str, &'a str)>, &'a str);
    let cases: Vec<Case<'_>> = vec![
        (&list, vec![("mcp-protocol-version", CURRENT)], "Mcp-Method"),
        (
            &list,
            vec![
                ("mcp-method", "tools/call"),
                ("mcp-protocol-version", CURRENT),
            ],
            "does not match body method",
        ),
        (
            &list,
            vec![("mcp-method", "tools/list")],
            "MCP-Protocol-Version",
        ),
        (
            &call,
            vec![
                ("mcp-method", "tools/call"),
                ("mcp-protocol-version", CURRENT),
                ("mcp-name", "recall_memories"),
            ],
            "params.name",
        ),
        (
            &call,
            vec![
                ("mcp-method", "tools/call"),
                ("mcp-protocol-version", CURRENT),
                ("mcp-name", "=?base64?!!!?="),
            ],
            "malformed base64",
        ),
    ];
    for (body, headers, needle) in cases {
        let r = c.raw(body, &headers).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.json);
        assert_eq!(r.json["error"]["code"], -32020, "{}", r.json);
        assert!(
            r.json["error"]["message"]
                .as_str()
                .unwrap()
                .contains(needle),
            "{needle}: {}",
            r.json
        );
    }
    let r = c
        .raw(
            &call,
            &[
                ("mcp-method", "tools/call"),
                ("mcp-protocol-version", CURRENT),
                ("mcp-name", &enc),
            ],
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json["result"]["isError"].is_null(), "{}", r.json);
    // Unsupported version: -32022 with the supported list.
    let bad = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": { "_meta": {
        "io.modelcontextprotocol/protocolVersion": "2027-01-01",
        "io.modelcontextprotocol/clientCapabilities": {} } } });
    let r = c
        .raw(
            &bad,
            &[
                ("mcp-method", "tools/list"),
                ("mcp-protocol-version", "2027-01-01"),
            ],
        )
        .await;
    assert_eq!(r.json["error"]["code"], -32022);
    assert_eq!(r.json["error"]["data"]["requested"], "2027-01-01");
    assert_eq!(r.json["error"]["data"]["supported"], json!([CURRENT]));
    // A notification needs no headers and gets 202 with an empty body.
    let note = json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "_meta": meta } });
    let r = c.raw(&note, &[]).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert!(r.bytes.is_empty());
}

#[tokio::test]
async fn older_revisions_use_sessions() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let raw = Client::modern(&env, ctx.clone(), "/mcp");
    for v in ["2025-06-18", "2025-03-26", "2024-11-05"] {
        let r = raw.raw(&initialize(1, v, "c"), &[]).await;
        assert_eq!(r.json["result"]["protocolVersion"], v);
        assert_eq!(
            r.json["result"]["capabilities"],
            json!({ "tools": { "listChanged": false } })
        );
        assert_eq!(r.json["result"]["serverInfo"]["name"], "knoggin");
        assert!(r.json["result"]["instructions"].is_string());
    }
    for v in ["1999-01-01", CURRENT] {
        let r = raw.raw(&initialize(1, v, "c"), &[]).await;
        assert_eq!(r.json["result"]["protocolVersion"], "2025-06-18");
    }
    let c = Client::legacy(&env, ctx.clone(), "/mcp", "LegacyClient").await;
    let sid = c.session.clone().unwrap();
    assert_eq!(sid.len(), 64);
    assert_eq!(c.tools().await.len(), 18);
    // A repeat initialize with a live session keeps its ID.
    let r = c.raw(&initialize(2, "2025-06-18", "x"), &[]).await;
    assert_eq!(r.headers["mcp-session-id"], sid.as_str());
    // A failed initialize makes no session.
    let r = raw
        .raw(
            &json!({ "jsonrpc": "2.0", "id": 3, "method": "initialize",
                "params": { "protocolVersion": 7 } }),
            &[],
        )
        .await;
    assert_eq!(r.json["error"]["code"], -32602);
    assert!(r.headers.get("mcp-session-id").is_none());
    // Without a session or `_meta`: -32602 naming both ways in, with the real ID.
    let r = raw.raw(&plain(4242, "tools/list"), &[]).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.json["id"], 4242);
    assert_eq!(r.json["error"]["code"], -32602);
    let m = r.json["error"]["message"].as_str().unwrap();
    assert!(m.contains("io.modelcontextprotocol/protocolVersion") && m.contains("initialize"));
    // An unknown session: 404 with -32600.
    let r = raw
        .raw(&plain(9, "tools/list"), &[("mcp-session-id", "ghost")])
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.json["id"], 9);
    assert_eq!(r.json["error"]["code"], -32600);
    assert_eq!(
        r.json["error"]["message"],
        "Unknown or expired MCP session; send 'initialize' again"
    );
    // Another key, another store or the other endpoint: the same 404.
    let other_store = env.t.new_store("App").await;
    for (ctx, path) in [
        (env.t.ctx(RW), "/mcp"),
        (env.t.ctx_for(other_store, RW), "/mcp"),
        (ctx.clone(), "/mcp/plugin"),
    ] {
        let mut other = Client::modern(&env, ctx, path);
        other.session = Some(sid.clone());
        let r = other.raw(&plain(5, "tools/list"), &[]).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{}", r.json);
        assert_eq!(r.json["error"]["code"], -32600);
    }
    // DELETE ends it: 200, then 404.
    let delete = |sid: Option<&str>, ctx: &RequestContext| {
        let mut b = Request::builder().method(Method::DELETE).uri("/mcp");
        if let Some(s) = sid {
            b = b.header("mcp-session-id", s);
        }
        let mut req = b.body(Body::empty()).unwrap();
        req.extensions_mut().insert(ctx.clone());
        req
    };
    assert_eq!(
        env.send(delete(Some(&sid), &env.t.ctx(RW))).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        env.send(delete(Some(&sid), &ctx)).await.status,
        StatusCode::OK
    );
    let r = env.send(delete(Some(&sid), &ctx)).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(r.error(), Code::NotFound);
    let r = env.send(delete(None, &ctx)).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.headers["allow"], "POST, DELETE");
    let r = c.raw(&plain(6, "tools/list"), &[]).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn get_parse_errors_batches_and_methods() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    for path in ["/mcp", "/mcp/plugin"] {
        let mut req = Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(Body::empty())
            .unwrap();
        req.extensions_mut().insert(ctx.clone());
        let r = env.send(req).await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(r.headers["allow"], "POST, DELETE");
        let c = Client::modern(&env, ctx.clone(), path);
        let mut req = Request::builder()
            .method(Method::POST)
            .uri(path)
            .body(Body::from("{not json"))
            .unwrap();
        req.extensions_mut().insert(ctx.clone());
        let r = env.send(req).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(r.json["error"]["code"], -32700);
        assert!(r.json["id"].is_null());
        // A batch is refused on both revisions.
        let r = c.raw(&json!([plain(1, "ping")]), &[]).await;
        assert_eq!(r.json["error"]["code"], -32600, "{}", r.json);
        let l = Client::legacy(&env, ctx.clone(), path, "c").await;
        let r = l
            .raw(
                &json!([plain(1, "tools/list"), plain(2, "tools/list")]),
                &[],
            )
            .await;
        assert_eq!(r.json["error"]["code"], -32600, "{}", r.json);
        // Discover and ping need neither `_meta` nor a session.
        let r = c.raw(&plain(1, "server/discover"), &[]).await;
        assert_eq!(r.json["result"]["supportedVersions"], json!([CURRENT]));
        assert_eq!(r.json["result"]["cacheScope"], "public");
        assert_eq!(
            r.json["result"]["capabilities"],
            json!({ "tools": { "listChanged": false } })
        );
        assert!(r.headers.get("mcp-session-id").is_none());
        let r = c.raw(&plain(1, "ping"), &[]).await;
        assert_eq!(r.json["result"]["resultType"], "complete");
        assert!(r.json["result"].get("ttlMs").is_none());
        // Removed and unknown methods.
        for m in [
            "ping",
            "initialize",
            "resources/list",
            "prompts/list",
            "resources/templates/list",
        ] {
            let r = c.request(m, json!({})).await;
            assert_eq!(r.json["error"]["code"], -32601, "{m}: {}", r.json);
        }
        for m in ["resources/list", "prompts/list", "resources/templates/list"] {
            let r = l.raw(&plain(3, m), &[]).await;
            assert_eq!(r.json["error"]["code"], -32601, "{m}");
        }
        let r = l.raw(&plain(3, "ping"), &[]).await;
        assert_eq!(r.json["result"]["resultType"], "complete");
        // A legacy tools/call result has no cache hints and the envelope.
        let r = l
            .raw(
                &json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call",
                    "params": { "name": "list_namespaces" } }),
                &[],
            )
            .await;
        assert_eq!(r.json["result"]["resultType"], "complete");
        assert!(r.json["result"].get("cacheScope").is_none());
        assert!(r.json["result"]["isError"].is_null(), "{}", r.json);
        // tools/call without a name is -32602.
        let r = l
            .raw(
                &json!({ "jsonrpc": "2.0", "id": 8, "method": "tools/call", "params": {} }),
                &[],
            )
            .await;
        assert_eq!(r.json["error"]["code"], -32602);
        // A notification on the old revision is 202 too.
        let r = l
            .raw(
                &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
                &[],
            )
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED);
    }
    // A route without a context is the store layer's `internal`.
    let req = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .body(Body::from(plain(1, "ping").to_string()))
        .unwrap();
    let r = env.send(req).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.json["error"]["code"], "internal");
}

#[tokio::test]
async fn instructions_per_endpoint() {
    let env = Env::new(true).await;
    for (path, needle) in [
        (
            "/mcp",
            "Save something when the user asks you to remember it",
        ),
        (
            "/mcp/plugin",
            "Before saving anything, show the user the exact text",
        ),
    ] {
        let c = Client::modern(&env, env.t.ctx(RW), path);
        let r = c.raw(&plain(1, "server/discover"), &[]).await;
        let text = r.json["result"]["instructions"].as_str().unwrap();
        assert!(text.contains(needle), "{text}");
        assert!(text.contains("by running knoggin open."), "{text}");
        assert!(
            text.ends_with("Source code: https://github.com/knoggin-memory/knoggin."),
            "{text}"
        );
        let r = c.raw(&initialize(1, "2025-06-18", "c"), &[]).await;
        assert_eq!(r.json["result"]["instructions"], text);
    }
}

// ─── Tool sets ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn tool_lists_and_hints() {
    let env = Env::new(true).await;
    // (name, RO full, RO plugin, D, I full, I plugin, write)
    let table: [(&str, bool, bool, bool, bool, bool, bool); 18] = [
        ("get_context", true, true, false, true, true, false),
        ("recall_memories", false, true, false, false, true, false),
        ("get_memory", false, true, false, false, true, false),
        (
            "find_similar_memories",
            false,
            true,
            false,
            false,
            true,
            false,
        ),
        ("list_memories", true, true, false, true, true, false),
        ("list_namespaces", true, true, false, true, true, false),
        ("list_tags", true, true, false, true, true, false),
        ("namespace_stats", true, true, false, true, true, false),
        ("store_memory", false, false, false, false, false, true),
        ("store_memories", false, false, false, false, false, true),
        ("supersede_memory", false, false, true, false, false, true),
        ("reinforce_memory", false, false, false, false, false, true),
        ("move_memories", false, false, false, true, true, true),
        ("create_namespace", false, false, false, false, false, true),
        (
            "update_namespace_policy",
            false,
            false,
            true,
            true,
            true,
            true,
        ),
        ("forget_memory", false, false, true, true, true, true),
        ("forget_memories", false, false, true, true, true, true),
        ("delete_namespace", false, false, true, false, false, true),
    ];
    let plugin = [
        "get_context",
        "recall_memories",
        "get_memory",
        "find_similar_memories",
        "list_memories",
        "list_namespaces",
        "store_memory",
        "supersede_memory",
        "reinforce_memory",
        "forget_memory",
    ];
    for (path, perms, want) in [
        ("/mcp", RW, 18),
        ("/mcp/plugin", RW, 10),
        ("/mcp", RO, 8),
        ("/mcp/plugin", RO, 6),
    ] {
        let modern = Client::modern(&env, env.t.ctx(perms), path).tools().await;
        let legacy = Client::legacy(&env, env.t.ctx(perms), path, "c")
            .await
            .tools()
            .await;
        assert_eq!(modern.len(), want, "{path}");
        assert_eq!(legacy.len(), want, "{path}");
        let on_plugin = path == "/mcp/plugin";
        for (tool, old) in modern.iter().zip(&legacy) {
            let name = tool["name"].as_str().unwrap();
            let row = table.iter().find(|r| r.0 == name).unwrap();
            assert!(!on_plugin || plugin.contains(&name), "{name}");
            assert!(perms.write || !row.6, "{name}");
            let a = &tool["annotations"];
            let (ro, i) = if on_plugin {
                (row.2, row.5)
            } else {
                (row.1, row.4)
            };
            assert_eq!(a["readOnlyHint"], ro, "{path} {name}");
            assert_eq!(a["destructiveHint"], row.3, "{name}");
            assert_eq!(a["idempotentHint"], i, "{path} {name}");
            assert_eq!(a["openWorldHint"], false, "{name}");
            assert!(tool["title"].is_string() && tool["description"].is_string());
            assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
            assert_eq!(tool["outputSchema"]["type"], "object", "{name}");
            assert!(old.get("outputSchema").is_none(), "{name}");
            assert_eq!(old["inputSchema"], tool["inputSchema"]);
            let always = ["get_context", "recall_memories", "store_memory"].contains(&name);
            assert_eq!(
                tool["_meta"]["anthropic/alwaysLoad"] == true,
                always,
                "{name}"
            );
            let props = tool["inputSchema"]["properties"].as_object();
            let has = |p: &str| props.is_some_and(|m| m.contains_key(p));
            if ["recall_memories", "get_memory", "find_similar_memories"].contains(&name) {
                assert_eq!(has("recordAccess"), !on_plugin, "{path} {name}");
            }
            assert_eq!(has("idempotencyKey"), row.6, "{name}");
            assert!(!has("createdAt"), "{name}");
        }
    }
    // The two plugin descriptions that differ.
    let tools = Client::modern(&env, env.t.ctx(RW), "/mcp/plugin")
        .tools()
        .await;
    let desc = |n: &str| {
        tools.iter().find(|t| t["name"] == n).unwrap()["description"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(
        desc("store_memory"),
        "Save one memory: a short summary and optional full text. Before you call this, show the user the exact text and wait for a yes; then tell the user what you saved. Never save passwords, keys or other secrets. This only adds; it never replaces an existing memory."
    );
    assert_eq!(
        desc("supersede_memory"),
        "Replace an existing memory with a corrected or newer version. Before you call this, show the user the exact text and wait for a yes; then tell the user what you saved. The old memory is kept but no longer returned by recall."
    );
    assert_eq!(
        desc("get_memory"),
        "Fetch one memory by ID, with its full text."
    );
}

/// Every documented argument is accepted by the parser and an undocumented one refused.
#[tokio::test]
async fn schemas_match_the_parsers() {
    let env = Env::new(true).await;
    for path in ["/mcp", "/mcp/plugin"] {
        let c = Client::modern(&env, env.t.ctx(RW), path);
        for tool in c.tools().await {
            let name = tool["name"].as_str().unwrap();
            let props = tool["inputSchema"]["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            assert_eq!(tool["inputSchema"]["additionalProperties"], false, "{name}");
            for prop in props.keys() {
                let result = c.call(name, json!({ prop: null })).await;
                let text = result["content"][0]["text"].as_str().unwrap();
                assert!(
                    !text.contains("unknown field"),
                    "{path} {name} {prop}: {text}"
                );
            }
            let (code, field) = c.fails(name, json!({ "undocumented": 1 })).await;
            assert_eq!(code, Code::InvalidArgument, "{name}");
            assert_eq!(field.as_deref(), Some("undocumented"), "{name}");
            if path == "/mcp/plugin" {
                let (code, field) = c.fails(name, json!({ "recordAccess": false })).await;
                assert_eq!(
                    (code, field.as_deref()),
                    (Code::InvalidArgument, Some("recordAccess"))
                );
            }
        }
    }
}

// ─── The tools ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn every_tool_success_bounds_and_errors() {
    let env = Env::new(true).await;
    let c = Client::modern(&env, env.t.ctx(RW), "/mcp");
    let ro = Client::modern(&env, env.t.ctx(RO), "/mcp");
    let a = c
        .ok(
            "store_memory",
            json!({ "summary": "Maya prefers window seats", "tags": ["travel"],
                "entities": ["Maya"], "fullText": "Said so in May.", "checkDuplicates": true }),
        )
        .await;
    assert_eq!(a["phase"], "full");
    let b = c
        .ok("store_memory", json!({ "summary": "Maya flies to Lisbon" }))
        .await;
    let (a, b) = (id_of(&a), id_of(&b));
    let missing = "0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e10";

    // Success of every tool.
    let ctx = c
        .ok("get_context", json!({ "topic": "Maya", "limit": 5 }))
        .await;
    assert_eq!(ctx["defaultNamespace"], "default");
    assert!(ctx["model"].is_object() && ctx.get("webUrl").is_some());
    let r = c
        .ok("recall_memories", json!({ "query": "window seats" }))
        .await;
    assert!(r["count"].as_u64().unwrap() >= 1, "{r}");
    let r = c
        .ok(
            "recall_memories",
            json!({ "query": "Maya", "detail": "full", "limit": 2,
            "fullTextCount": 1, "depth": 1, "minStrength": 0.1, "tags": [], "entities": ["Maya"],
            "timeRangeStart": 0, "timeRangeEnd": "2100-01-01T00:00:00Z", "recordAccess": false }),
        )
        .await;
    assert!(r["memories"][0]["score"].is_number(), "{r}");
    let m = c
        .ok(
            "get_memory",
            json!({ "id": a, "includeHistory": true, "namespace": "default" }),
        )
        .await;
    assert_eq!(m["fullText"], "Said so in May.");
    assert!(m["accessHistory"].is_array());
    let l = c
        .ok(
            "list_memories",
            json!({ "tags": ["travel"], "phase": "full",
            "createdAfter": "2000-01-01T00:00:00Z", "includeFullText": true, "limit": 10 }),
        )
        .await;
    assert_eq!(l["items"].as_array().unwrap().len(), 1);
    assert!(l.get("nextCursor").is_some());
    let n = c.ok("list_namespaces", json!({})).await;
    assert_eq!(n["items"][0]["name"], "default");
    let t = c.ok("list_tags", json!({ "prefix": "tr" })).await;
    assert_eq!(t["items"], json!([{ "tag": "travel", "count": 1 }]));
    let s = c.ok("namespace_stats", json!({})).await;
    assert_eq!(s["memoryCount"], 2);
    let batch = c
        .ok(
            "store_memories",
            json!({ "memories": [{ "summary": "one" }, { "summary": "" }] }),
        )
        .await;
    assert_eq!(
        (batch["stored"].clone(), batch["errors"].clone()),
        (json!(1), json!(1))
    );
    let all_bad = c
        .call("store_memories", json!({ "memories": [{ "summary": "" }] }))
        .await;
    assert_eq!(all_bad["isError"], true, "{all_bad}");
    let sup = c
        .ok(
            "supersede_memory",
            json!({ "id": b, "summary": "Maya flies to Porto" }),
        )
        .await;
    assert_eq!(sup["supersedes"], b.as_str());
    let rf = c
        .ok("reinforce_memory", json!({ "id": a, "quality": 4 }))
        .await;
    assert_eq!(rf["id"], a.as_str());
    let ns = c
        .ok(
            "create_namespace",
            json!({ "name": "work", "initialStability": 5.0,
            "decayRateMultiplier": null }),
        )
        .await;
    assert_eq!(ns["name"], "work");
    let mv = c
        .ok(
            "move_memories",
            json!({ "ids": [a], "destinationNamespace": "work",
            "sourceNamespace": "default" }),
        )
        .await;
    assert_eq!(mv["moved"], 1);
    let policy = json!({ "initialStability": 3.7145, "defaultDifficulty": 5.0,
        "phaseThresholds": { "fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05 },
        "permastoreThreshold": 1500.0, "decayRateMultiplier": 2.0 });
    let p = c
        .ok(
            "update_namespace_policy",
            json!({ "name": "work", "policy": policy, "dryRun": true }),
        )
        .await;
    assert!(p["wouldChange"].is_object(), "{p}");
    let p = c
        .ok(
            "update_namespace_policy",
            json!({ "name": "work", "policy": policy }),
        )
        .await;
    assert_eq!(p["policy"]["decayRateMultiplier"], 2.0);
    let f = c
        .ok(
            "forget_memories",
            json!({ "ids": [a, missing], "dryRun": true }),
        )
        .await;
    assert_eq!(f["deleted"], 1);
    let f = c.ok("forget_memory", json!({ "id": missing })).await;
    assert_eq!(f["deleted"], false);
    let d = c
        .ok(
            "delete_namespace",
            json!({ "name": "work", "force": true, "dryRun": true }),
        )
        .await;
    assert_eq!(d["deleted"], false);
    assert_eq!(d["memoriesDeleted"], 1);
    let d = c
        .ok("delete_namespace", json!({ "name": "work", "force": true }))
        .await;
    assert_eq!(d["deleted"], true);
    let f = c
        .ok(
            "forget_memory",
            json!({ "id": sup["id"], "namespace": "default" }),
        )
        .await;
    assert_eq!(f["deleted"], true);
    let f = c.ok("forget_memories", json!({ "ids": [missing] })).await;
    assert_eq!(f["deleted"], 0);
    // Similar needs a vector: wait for the queue.
    let one = id_of(&batch["results"][0]["memory"]);
    let mut similar = None;
    for _ in 0..200 {
        let r = c
            .call("find_similar_memories", json!({ "id": one, "limit": 5 }))
            .await;
        if r["isError"].is_null() {
            similar = Some(r);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(similar.unwrap()["structuredContent"]["items"].is_array());
    c.ok(
        "find_similar_memories",
        json!({ "id": one, "minScore": 0.5, "recordAccess": false }),
    )
    .await;

    // Bounds, wrong types, unknown and missing arguments.
    let x = json!("x".repeat(4097));
    let cases: Vec<(&str, Value, Code, Option<&str>)> = vec![
        (
            "get_context",
            json!({ "limit": 0 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "get_context",
            json!({ "limit": 26 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "get_context",
            json!({ "limit": "5" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "get_context",
            json!({ "namespace": "nope" }),
            Code::NotFound,
            None,
        ),
        (
            "recall_memories",
            json!({}),
            Code::InvalidArgument,
            Some("query"),
        ),
        (
            "recall_memories",
            json!({ "query": x }),
            Code::InvalidArgument,
            Some("query"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "limit": 0 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "limit": 101 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "limit": 3, "fullTextCount": 4 }),
            Code::InvalidArgument,
            Some("fullTextCount"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "depth": 4 }),
            Code::InvalidArgument,
            Some("depth"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "minStrength": 1.5 }),
            Code::InvalidArgument,
            Some("minStrength"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "minStrength": -0.1 }),
            Code::InvalidArgument,
            Some("minStrength"),
        ),
        // Without `limit` the bound is the default limit, 25.
        (
            "recall_memories",
            json!({ "query": "q", "fullTextCount": 101 }),
            Code::InvalidArgument,
            Some("fullTextCount"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "timeRangeStart": 0 }),
            Code::InvalidArgument,
            None,
        ),
        (
            "recall_memories",
            json!({ "query": "q", "timeRangeStart": -1, "timeRangeEnd": 5 }),
            Code::InvalidArgument,
            Some("timeRangeStart"),
        ),
        (
            "recall_memories",
            json!({ "query": "q", "detail": "huge" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "recall_memories",
            json!({ "query": "q", "recordAccess": 1 }),
            Code::InvalidArgument,
            None,
        ),
        ("get_memory", json!({}), Code::InvalidArgument, Some("id")),
        (
            "get_memory",
            json!({ "id": "not-a-uuid" }),
            Code::InvalidArgument,
            None,
        ),
        ("get_memory", json!({ "id": missing }), Code::NotFound, None),
        (
            "find_similar_memories",
            json!({ "id": one, "limit": 0 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "find_similar_memories",
            json!({ "id": one, "limit": 101 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "find_similar_memories",
            json!({ "id": one, "minScore": 2 }),
            Code::InvalidArgument,
            Some("minScore"),
        ),
        (
            "find_similar_memories",
            json!({ "id": one, "minScore": -0.1 }),
            Code::InvalidArgument,
            Some("minScore"),
        ),
        (
            "find_similar_memories",
            json!({}),
            Code::InvalidArgument,
            Some("id"),
        ),
        (
            "list_memories",
            json!({ "limit": 201 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "list_memories",
            json!({ "limit": 0 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "list_memories",
            json!({ "phase": "liquid" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "list_memories",
            json!({ "createdAfter": "yesterday" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "list_memories",
            json!({ "cursor": "!!" }),
            Code::InvalidArgument,
            Some("cursor"),
        ),
        (
            "list_namespaces",
            json!({ "limit": 1 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "list_tags",
            json!({ "limit": 201 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "list_tags",
            json!({ "limit": 0 }),
            Code::InvalidArgument,
            Some("limit"),
        ),
        (
            "namespace_stats",
            json!({ "namespace": "nope" }),
            Code::NotFound,
            None,
        ),
        (
            "store_memory",
            json!({}),
            Code::InvalidArgument,
            Some("summary"),
        ),
        (
            "store_memory",
            json!({ "summary": "x".repeat(2001) }),
            Code::InvalidArgument,
            Some("summary"),
        ),
        (
            "store_memory",
            json!({ "summary": "s", "createdAt": "2020-01-01T00:00:00Z" }),
            Code::InvalidArgument,
            Some("createdAt"),
        ),
        (
            "store_memory",
            json!({ "summary": "s", "tags": "travel" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "store_memory",
            json!({ "summary": "s", "namespace": "nope" }),
            Code::NotFound,
            None,
        ),
        (
            "store_memory",
            json!({ "summary": "s", "idempotencyKey": "" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "store_memories",
            json!({ "memories": [] }),
            Code::InvalidArgument,
            Some("memories"),
        ),
        (
            "store_memories",
            json!({ "memories": vec![json!({ "summary": "s" }); 101] }),
            Code::InvalidArgument,
            Some("memories"),
        ),
        (
            "store_memories",
            json!({ "memories": [{ "summary": "s", "checkDuplicates": true }] }),
            Code::InvalidArgument,
            None,
        ),
        (
            "store_memories",
            json!({}),
            Code::InvalidArgument,
            Some("memories"),
        ),
        (
            "supersede_memory",
            json!({ "id": missing, "summary": "s" }),
            Code::NotFound,
            None,
        ),
        (
            "supersede_memory",
            json!({ "id": missing }),
            Code::InvalidArgument,
            Some("summary"),
        ),
        (
            "supersede_memory",
            json!({ "id": missing, "summary": "s", "namespace": "default" }),
            Code::InvalidArgument,
            Some("namespace"),
        ),
        (
            "reinforce_memory",
            json!({ "id": one, "quality": 0 }),
            Code::InvalidArgument,
            Some("quality"),
        ),
        (
            "reinforce_memory",
            json!({ "id": one, "quality": 5 }),
            Code::InvalidArgument,
            Some("quality"),
        ),
        (
            "reinforce_memory",
            json!({}),
            Code::InvalidArgument,
            Some("id"),
        ),
        (
            "move_memories",
            json!({ "ids": [], "destinationNamespace": "default" }),
            Code::InvalidArgument,
            Some("ids"),
        ),
        (
            "move_memories",
            json!({ "ids": [one, one], "destinationNamespace": "default" }),
            Code::InvalidArgument,
            Some("ids"),
        ),
        (
            "move_memories",
            json!({ "ids": [one] }),
            Code::InvalidArgument,
            Some("destinationNamespace"),
        ),
        (
            "move_memories",
            json!({ "ids": [one], "destinationNamespace": "nope" }),
            Code::NotFound,
            None,
        ),
        (
            "create_namespace",
            json!({}),
            Code::InvalidArgument,
            Some("name"),
        ),
        (
            "create_namespace",
            json!({ "name": "no spaces" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "create_namespace",
            json!({ "name": "default" }),
            Code::AlreadyExists,
            None,
        ),
        (
            "update_namespace_policy",
            json!({ "name": "default" }),
            Code::InvalidArgument,
            Some("policy"),
        ),
        (
            "update_namespace_policy",
            json!({ "name": "default", "policy": { "initialStability": 1.0 } }),
            Code::InvalidArgument,
            None,
        ),
        (
            "update_namespace_policy",
            json!({ "name": "nope", "policy": policy }),
            Code::NotFound,
            None,
        ),
        (
            "forget_memory",
            json!({}),
            Code::InvalidArgument,
            Some("id"),
        ),
        (
            "forget_memories",
            json!({ "ids": [] }),
            Code::InvalidArgument,
            Some("ids"),
        ),
        (
            "forget_memories",
            json!({ "ids": vec![json!(missing); 101] }),
            Code::InvalidArgument,
            Some("ids"),
        ),
        (
            "delete_namespace",
            json!({}),
            Code::InvalidArgument,
            Some("name"),
        ),
        (
            "delete_namespace",
            json!({ "name": "default" }),
            Code::InvalidArgument,
            None,
        ),
        (
            "delete_namespace",
            json!({ "name": "nope" }),
            Code::NotFound,
            None,
        ),
        ("no_such_tool", json!({}), Code::NotFound, None),
    ];
    for (tool, args, code, field) in cases {
        let (got, f) = c.fails(tool, args.clone()).await;
        assert_eq!(got, code, "{tool} {args}");
        if let Some(field) = field {
            assert_eq!(f.as_deref(), Some(field), "{tool} {args}");
        }
    }
    // `arguments` must be an object; a missing one means `{}`.
    let r = c
        .request(
            "tools/call",
            json!({ "name": "list_namespaces", "arguments": [1] }),
        )
        .await;
    assert_eq!(r.json["result"]["isError"], true);
    let r = c
        .request("tools/call", json!({ "name": "list_namespaces" }))
        .await;
    assert!(r.json["result"]["isError"].is_null(), "{}", r.json);
    // Optional `null` is "not given".
    c.ok(
        "list_memories",
        json!({ "namespace": null, "limit": null, "tags": null }),
    )
    .await;
    // The read-only credential: every writing tool is permission_denied, on both endpoints.
    for tool in [
        "store_memory",
        "store_memories",
        "supersede_memory",
        "reinforce_memory",
        "move_memories",
        "create_namespace",
        "update_namespace_policy",
        "forget_memory",
        "forget_memories",
        "delete_namespace",
    ] {
        let (code, _) = ro.fails(tool, json!({})).await;
        assert_eq!(code, Code::PermissionDenied, "{tool}");
    }
    let ro_plugin = Client::modern(&env, env.t.ctx(RO), "/mcp/plugin");
    for tool in [
        "store_memory",
        "supersede_memory",
        "reinforce_memory",
        "forget_memory",
    ] {
        let (code, _) = ro_plugin.fails(tool, json!({})).await;
        assert_eq!(code, Code::PermissionDenied, "/mcp/plugin {tool}");
    }
    for (tool, args) in [
        ("get_context", json!({})),
        ("recall_memories", json!({ "query": "Maya" })),
        ("get_memory", json!({ "id": one })),
        ("find_similar_memories", json!({ "id": one })),
        ("list_memories", json!({})),
        ("list_namespaces", json!({})),
        ("list_tags", json!({})),
        ("namespace_stats", json!({})),
    ] {
        ro.ok(tool, args.clone()).await;
        if !["list_tags", "namespace_stats"].contains(&tool) {
            ro_plugin.ok(tool, args).await;
        }
    }
}

#[tokio::test]
async fn a_full_batch_of_100_is_accepted_on_mcp() {
    // The model stays unloaded, so 6.7 MB of text is not embedded while other tests run.
    let env = Env::new(false).await;
    let c = Client::modern(&env, env.t.ctx(RW), "/mcp");
    let item = json!({ "summary": "s".repeat(2000), "fullText": "f".repeat(65_536) });
    let r = c
        .ok("store_memories", json!({ "memories": vec![item; 100] }))
        .await;
    assert_eq!(r["stored"], 100, "{}", r["results"][0]);
}

// ─── Access recording, the client's name, idempotency ─────────────────────────

async fn counts(env: &Env, id: &str) -> (Value, Value) {
    let m = env.t.ctx(RW);
    let r = env
        .call(
            Some(&m),
            Method::GET,
            &format!("/v1/memories/{id}?recordAccess=false"),
            None,
            &[],
        )
        .await;
    (r.json["accessCount"].clone(), r.json["retrievedAt"].clone())
}

async fn read(c: &Client<'_>, id: &str) {
    c.ok("recall_memories", json!({ "query": "window seats" }))
        .await;
    c.ok("get_memory", json!({ "id": id })).await;
    c.ok("get_context", json!({ "topic": "window seats" }))
        .await;
}

#[tokio::test]
async fn access_recording_per_endpoint() {
    let env = Env::new(true).await;
    let rw = Client::modern(&env, env.t.ctx(RW), "/mcp");
    let id = id_of(
        &rw.ok(
            "store_memory",
            json!({ "summary": "Maya prefers window seats" }),
        )
        .await,
    );
    for c in [
        Client::modern(&env, env.t.ctx(RW), "/mcp/plugin"),
        Client::legacy(&env, env.t.ctx(RW), "/mcp/plugin", "c").await,
        Client::modern(&env, env.t.ctx(RO), "/mcp"),
        Client::modern(&env, env.t.ctx(RO), "/mcp/plugin"),
    ] {
        read(&c, &id).await;
        assert_eq!(
            counts(&env, &id).await,
            (json!(0), Value::Null),
            "{}",
            c.path
        );
    }
    read(&rw, &id).await;
    let (count, at) = counts(&env, &id).await;
    assert_eq!(count, 2, "recall and get record; get_context does not");
    assert!(at.is_string());
}

#[tokio::test]
async fn the_client_name_reaches_the_memory_and_no_log_line() {
    let logs = capture_logs();
    let env = Env::new(true).await;
    // 62 bytes, then a 4-byte character across byte 64.
    let modern_name = format!("{}🦀-marker-client-name", "m".repeat(62));
    let legacy_name = "legacy-marker-client-name";
    let mut m = Client::modern(&env, env.t.ctx(RW), "/mcp");
    m.name = modern_name.clone();
    let l = Client::legacy(&env, env.t.ctx(RW), "/mcp/plugin", legacy_name).await;
    let a = m
        .ok(
            "store_memory",
            json!({ "summary": "from the new revision" }),
        )
        .await;
    let b = l
        .ok(
            "store_memory",
            json!({ "summary": "from the old revision" }),
        )
        .await;
    let ctx = env.t.ctx(RW);
    for (saved, want) in [(a, "m".repeat(62)), (b, legacy_name.to_owned())] {
        let r = env
            .call(
                Some(&ctx),
                Method::GET,
                &format!("/v1/memories/{}", id_of(&saved)),
                None,
                &[],
            )
            .await;
        assert_eq!(r.json["client"], want.as_str());
    }
    let seen: Vec<String> = env
        .t
        .host
        .clients(env.t.store)
        .into_iter()
        .map(|c| c.0)
        .collect();
    assert_eq!(seen, [legacy_name.to_owned(), "m".repeat(62)]);
    m.ok("recall_memories", json!({ "query": "revision" }))
        .await;
    let text = logs.text();
    assert!(text.contains("tool call"), "the capture works: {text}");
    assert!(!text.contains("marker-client"), "{text}");
    assert!(!text.contains(&"m".repeat(62)), "{text}");
}

#[tokio::test]
async fn the_same_key_over_rest_and_mcp_saves_once() {
    let env = Env::new(true).await;
    let ctx = env.t.ctx(RW);
    let rest = env
        .call(
            Some(&ctx),
            Method::POST,
            "/v1/memories",
            Some(json!({ "summary": "once", "tags": ["t"] })),
            &[("idempotency-key", "shared-1")],
        )
        .await;
    assert_eq!(rest.status, StatusCode::CREATED);
    let c = Client::modern(&env, ctx.clone(), "/mcp");
    let result = c
        .call(
            "store_memory",
            json!({ "summary": "once", "tags": ["t"], "idempotencyKey": "shared-1" }),
        )
        .await;
    assert_eq!(
        result["content"][0]["text"].as_str().unwrap().as_bytes(),
        rest.bytes.as_slice(),
        "the replayed answer is the REST body"
    );
    let l = Client::legacy(&env, ctx.clone(), "/mcp", "c").await;
    let again = l
        .ok(
            "store_memory",
            json!({ "summary": "once", "tags": ["t"], "idempotencyKey": "shared-1",
                "namespace": "default", "checkDuplicates": false }),
        )
        .await;
    assert_eq!(again, rest.json);
    let list = c.ok("list_memories", json!({})).await;
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let (code, _) = c
        .fails(
            "store_memory",
            json!({ "summary": "other", "idempotencyKey": "shared-1" }),
        )
        .await;
    assert_eq!(code, Code::IdempotencyConflict);
    // A key used on MCP first replays over REST.
    let first = c
        .ok(
            "reinforce_memory",
            json!({ "id": id_of(&rest.json), "idempotencyKey": "shared-2" }),
        )
        .await;
    let r = env
        .call(
            Some(&ctx),
            Method::POST,
            &format!("/v1/memories/{}/reinforce", id_of(&rest.json)),
            Some(json!({ "quality": 3 })),
            &[("idempotency-key", "shared-2")],
        )
        .await;
    assert_eq!(r.headers["idempotent-replayed"], "true");
    assert_eq!(r.json, first);
}

#[tokio::test]
async fn store_errors_are_tool_errors() {
    let env = Env::new(true).await;
    // An unknown store opens as deleted: a tool error, not a JSON-RPC error.
    let ctx = env
        .t
        .ctx_for(knoggin_core::StoreId::from_uuid(uuid::Uuid::now_v7()), RW);
    let c = Client::modern(&env, ctx, "/mcp");
    let (code, _) = c.fails("list_namespaces", json!({})).await;
    assert_eq!(code, Code::StoreDeleting);
}

// ─── The body limit and the request timeout ───────────────────────────────────

/// A POST of `body` to `path` with a read-write context.
fn post(env: &Env, path: &str, body: Body) -> Request<Body> {
    let mut req = Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .unwrap();
    req.extensions_mut().insert(env.t.ctx(RW));
    req
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    // Ported from recalld's `oversized_body_is_rejected`, on both endpoints, for a body
    // with a length and for a streamed one.
    let mut config = knoggin_http::testing::fake_config();
    config.limits.body_bytes = 512;
    let env = Env::with_config(config, false).await;
    for path in ["/mcp", "/mcp/plugin"] {
        let r = env
            .send(post(&env, path, Body::from(vec![b'x'; 4096])))
            .await;
        assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE, "{path}");
        assert_eq!(r.error(), Code::PayloadTooLarge, "{path}");
        let chunks = futures_util::stream::iter(
            (0..8).map(|_| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![b'x'; 128]))),
        );
        let r = env.send(post(&env, path, Body::from_stream(chunks))).await;
        assert_eq!(r.error(), Code::PayloadTooLarge, "{path} streamed");
        // A body within the limit is read.
        let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }).to_string();
        let r = env.send(post(&env, path, Body::from(ping))).await;
        assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.json);
    }
}

#[tokio::test]
async fn a_request_past_the_timeout_is_a_timeout_error() {
    // Real time: the host's embedding threads are OS threads, so paused time is avoided.
    let mut config = knoggin_http::testing::fake_config();
    config.limits.request_timeout_ms = 100;
    let env = Env::with_config(config, false).await;
    for path in ["/mcp", "/mcp/plugin"] {
        // A body that never ends: the request cannot finish in time.
        let never = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
        let started = std::time::Instant::now();
        let r = env.send(post(&env, path, Body::from_stream(never))).await;
        assert_eq!(r.status, StatusCode::GATEWAY_TIMEOUT, "{path}");
        assert_eq!(r.error(), Code::Timeout, "{path}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{path}"
        );
    }
}

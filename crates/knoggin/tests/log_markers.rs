// SPDX-License-Identifier: AGPL-3.0-only
//! The log-marker test (tab "Security", Logs; tab "Engineering rules", rule 3): marker
//! strings in every kind of sensitive value that milestone 3 has, every route and every
//! tool run at log level `trace`, then all log output and the metrics page are searched,
//! and the data folder too. The web UI's read models and its event stream are in it, and
//! a second test covers sign-in (milestone 5).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::io::{Read, Write};

use serde_json::{Value, json};
use support::{Home, bridge, tool_json};

const SUMMARY: &str = "mkrsummaryq1";
const FULL: &str = "mkrfulltextq2";
const TAG: &str = "mkrtagq3";
const ENTITY: &str = "mkrentityq4";
const TOPIC: &str = "mkrtopicq5";
const EMOTION: &str = "mkremotionq6";
const NAMESPACE: &str = "mkrnamespaceq7";
const QUERY: &str = "mkrqueryq8";
const APP: &str = "mkrappq9";
const KEY_NAME: &str = "mkrkeynameq10";
const CLIENT: &str = "mkrclientq11";

fn modern(id: i64, method: &str, mut params: Value) -> Value {
    params["_meta"] = json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": CLIENT, "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {},
    });
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn tool(id: i64, name: &str, args: Value) -> Value {
    modern(id, "tools/call", json!({ "name": name, "arguments": args }))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A raw request to an `http://host:port` origin with a matching `Host`.
fn web(
    origin: &str,
    method: &str,
    path: &str,
    auth: Option<&str>,
    body: Option<&Value>,
) -> (u16, Value) {
    let authority = origin.trim_start_matches("http://");
    let mut s = std::net::TcpStream::connect(authority).unwrap();
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head =
        format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    if let Some(a) = auth {
        head.push_str(&format!("Authorization: Bearer {a}\r\n"));
    }
    if !body.is_empty() {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    s.write_all(format!("{head}\r\n{body}").as_bytes()).unwrap();
    let mut raw = String::new();
    s.read_to_string(&mut raw).unwrap();
    let status = raw[9..12].parse().unwrap();
    let json = raw
        .split("\r\n\r\n")
        .nth(1)
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    (status, json)
}

/// A key in lower-case hex and in base64 (standard and URL-safe, padded and not).
fn encodings(key: &[u8]) -> Vec<String> {
    use base64::Engine as _;
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    let hex = key.iter().map(|b| format!("{b:02x}")).collect();
    vec![
        hex,
        STANDARD.encode(key),
        STANDARD_NO_PAD.encode(key),
        URL_SAFE.encode(key),
        URL_SAFE_NO_PAD.encode(key),
    ]
}

/// The master keys from the key file, and the data key of every container file under the
/// data folder, unwrapped with them.
fn key_bytes(home: &Home) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    use base64::Engine as _;
    use knoggin_crypto::{KeyProvider as _, LocalKeyProvider, MasterKeySet, container};
    let text = std::fs::read(home.root().join("master.key")).unwrap();
    let set: Value = serde_json::from_slice(&text).unwrap();
    let master = set["keys"]
        .as_object()
        .unwrap()
        .values()
        .map(|k| {
            base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(k.as_str().unwrap())
                .unwrap()
        })
        .collect();
    let provider = LocalKeyProvider::new(MasterKeySet::from_json(&text).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut data = Vec::new();
    for (path, bytes) in home.data_bytes() {
        if path.extension().is_some_and(|e| e == "kgs") {
            let (header, _) = container::read_header(&bytes).unwrap();
            let key = rt
                .block_on(provider.unwrap(header.store_id, &header.wrapped_key()))
                .unwrap();
            data.push(key.expose().to_vec());
        }
    }
    (master, data)
}

#[test]
fn no_marker_reaches_a_log_the_metrics_page_or_the_data_folder() {
    let metrics = free_port();
    let home = Home::new()
        .with("KNOGGIN_LOG__LEVEL", "trace")
        .with("KNOGGIN_METRICS__BIND", &format!("127.0.0.1:{metrics}"));
    let mut d = home.daemon();
    let mut stderr = String::new();
    let mut tokens: Vec<String> = Vec::new();
    // Each call must get the status it is expected to get: a success path proves itself.
    let want = |r: &support::Resp, status: u16, what: &str| {
        assert_eq!(r.status, status, "{what}: {}", r.text());
    };

    // Store routes on the personal store, over the socket.
    let s = |m: &str, p: &str, b: Option<Value>| {
        home.sock(m, p, &[("Knoggin-Namespace", NAMESPACE)], b.as_ref())
    };
    want(
        &s("POST", "/v1/namespaces", Some(json!({ "name": NAMESPACE }))),
        201,
        "create namespace",
    );
    want(
        &s(
            "POST",
            "/v1/namespaces",
            Some(json!({ "name": format!("{NAMESPACE}d") })),
        ),
        201,
        "create namespace",
    );
    let memory = json!({ "summary": format!("{SUMMARY} about {QUERY}"), "fullText": FULL, "tags": [TAG],
        "entities": [ENTITY], "topics": [TOPIC], "emotions": [EMOTION] });
    let r = s("POST", "/v1/memories", Some(memory.clone()));
    want(&r, 201, "save");
    let id = r.json()["id"].as_str().unwrap().to_owned();
    let r = s("POST", "/v1/memories", Some(memory.clone()));
    want(&r, 201, "save");
    let third = r.json()["id"].as_str().unwrap().to_owned();
    let r = s(
        "POST",
        "/v1/memories/batch",
        Some(json!({ "memories": [memory.clone(), memory.clone()] })),
    );
    want(&r, 200, "batch");
    let other = r.json()["results"][0]["memory"]["id"]
        .as_str()
        .unwrap_or(&id)
        .to_owned();
    for (m, p, b, status) in [
        ("GET", format!("/v1/memories?tag={TAG}"), None, 200),
        ("GET", format!("/v1/memories/{id}"), None, 200),
        (
            "POST",
            format!("/v1/memories/{id}/reinforce"),
            Some(json!({ "quality": 3 })),
            200,
        ),
        (
            "POST",
            format!("/v1/memories/{id}/similar"),
            Some(json!({})),
            200,
        ),
        (
            "POST",
            "/v1/recall".into(),
            Some(json!({ "query": QUERY, "entities": [ENTITY], "tags": [TAG] })),
            200,
        ),
        ("GET", format!("/v1/context?topic={QUERY}"), None, 200),
        ("GET", "/v1/namespaces".into(), None, 200),
        (
            "GET",
            format!("/v1/namespaces/{NAMESPACE}/stats"),
            None,
            200,
        ),
        ("GET", format!("/v1/tags?prefix={TAG}"), None, 200),
        (
            "POST",
            format!("/v1/memories/{other}/supersede"),
            Some(json!({ "summary": format!("{SUMMARY} again") })),
            201,
        ),
        (
            "POST",
            "/v1/memories/move".into(),
            Some(json!({ "ids": [other], "destinationNamespace": "default" })),
            200,
        ),
        (
            "PUT",
            format!("/v1/namespaces/{NAMESPACE}/policy"),
            Some(json!({
            "initialStability": 3.7145, "defaultDifficulty": 5.0,
            "phaseThresholds": { "fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05 },
            "permastoreThreshold": 1500.0, "decayRateMultiplier": null })),
            200,
        ),
        // A call that fails, with markers in it.
        (
            "POST",
            "/v1/memories".into(),
            Some(json!({ "summary": "", "tags": [format!("{TAG}!bad")] })),
            400,
        ),
        ("GET", "/v1/export".into(), None, 200),
        (
            "POST",
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [other] })),
            200,
        ),
        ("DELETE", format!("/v1/memories/{third}"), None, 200),
        ("DELETE", format!("/v1/namespaces/{NAMESPACE}d"), None, 200),
    ] {
        want(&s(m, &p, b), status, &format!("{m} {p}"));
    }
    let line = json!({ "type": "memory", "id": "0193a1f2-7c3e-7b1a-9f00-2b6f3c1d9e11", "namespace": NAMESPACE,
        "createdAt": "2026-01-01T00:00:00.000Z", "phase": "full", "summary": SUMMARY, "fullText": FULL, "tags": [TAG] });
    let body = format!(
        "{{\"type\":\"header\",\"format\":\"knoggin-export\",\"version\":1}}\n{line}\n{{\"type\":\"bogus\",\"x\":\"{SUMMARY}\"}}\n"
    );
    let r = home.sock(
        "POST",
        "/v1/import",
        &[("Content-Type", "application/x-ndjson")],
        None,
    );
    want(&r, 400, "empty import");
    let mut raw = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
    raw.write_all(
        format!(
            "POST /v1/import HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .as_bytes(),
    )
    .unwrap();
    let mut answer = String::new();
    raw.read_to_string(&mut answer).unwrap();
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");

    // The web UI's read models, with markers in their parameters, and an event stream
    // that carries events naming the marker namespace.
    let mut stream = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
    stream
        .write_all(b"GET /v1/ui/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap();
    seen.extend_from_slice(&buf[..n]);
    assert!(
        seen.starts_with(b"HTTP/1.1 200"),
        "{}",
        String::from_utf8_lossy(&seen)
    );
    want(
        &s("POST", "/v1/memories", Some(memory.clone())),
        201,
        "save while streaming",
    );
    want(
        &s("POST", "/v1/recall", Some(json!({ "query": QUERY }))),
        200,
        "recall while streaming",
    );
    let carried = |seen: &[u8]| {
        let text = String::from_utf8_lossy(seen);
        text.contains(&format!("\"namespace\":\"{NAMESPACE}\""))
            && text.contains("event: store")
            && text.contains("event: recall")
    };
    while !carried(&seen) {
        let n = stream
            .read(&mut buf)
            .expect("the stream carried no marker event");
        assert!(n > 0, "the stream ended");
        seen.extend_from_slice(&buf[..n]);
    }
    let streamed = String::from_utf8_lossy(&seen).into_owned();
    assert!(
        streamed.contains("event: store") && streamed.contains("event: recall"),
        "{streamed}"
    );
    drop(stream);
    for (p, status) in [
        (format!("/v1/ui/graph?namespace={NAMESPACE}"), 200),
        (
            format!(
                "/v1/ui/memories?namespace={NAMESPACE}&tag={TAG}&entity={ENTITY}&client={CLIENT}&sort=summary"
            ),
            200,
        ),
        (format!("/v1/ui/memories?id={id}"), 200),
        (format!("/v1/ui/facets?namespace={NAMESPACE}"), 200),
        (format!("/v1/ui/memories/{id}/history"), 200),
        (format!("/v1/ui/memories/{id}/neighbors?depth=2"), 200),
        (
            format!("/v1/ui/search?q={QUERY}&namespace={NAMESPACE}"),
            200,
        ),
        (format!("/v1/ui/search?q={QUERY}"), 200),
        (format!("/v1/ui/activity?namespace={NAMESPACE}"), 200),
        (format!("/v1/ui/health?namespace={NAMESPACE}"), 200),
        ("/v1/ui/system".into(), 200),
        // Calls that fail, with markers in them.
        (format!("/v1/ui/health?namespace={NAMESPACE}x"), 404),
        (format!("/v1/ui/memories?tag={TAG}&{SUMMARY}=1"), 400),
        (format!("/v1/ui/search?q={QUERY}&limit=99"), 400),
    ] {
        want(&s("GET", &p, None), status, &p);
    }
    // A page of the web UI (503 when this build has no `ui/dist`).
    let page = home
        .sock("GET", &format!("/map?{SUMMARY}"), &[], None)
        .status;
    assert!(page == 200 || page == 503, "UI page: {page}");

    // Control routes.
    for p in [
        "/v1/session",
        "/v1/openapi.json",
        "/v1/stores",
        "/v1/admin/status",
        "/v1/users",
        "/healthz",
        "/readyz",
    ] {
        want(&home.sock("GET", p, &[], None), 200, p);
    }
    // The one user's routes. The daemon has no other user, so removing one is refused:
    // an admin cannot remove themselves.
    let user = home.sock("GET", "/v1/users", &[], None).json()["items"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    want(
        &home.sock("GET", &format!("/v1/users/{user}/stores"), &[], None),
        200,
        "user stores",
    );
    want(
        &home.sock("DELETE", &format!("/v1/users/{user}"), &[], None),
        400,
        "remove user",
    );
    let r = home.sock("POST", "/v1/stores", &[], Some(&json!({ "name": APP })));
    want(&r, 201, "create app");
    let app = r.json();
    let app_id = app["id"].as_str().unwrap().to_owned();
    want(
        &home.sock(
            "PATCH",
            &format!("/v1/stores/{app_id}"),
            &[],
            Some(&json!({ "name": format!("{APP}x") })),
        ),
        200,
        "rename",
    );
    want(
        &home.sock(
            "POST",
            "/v1/stores",
            &[],
            Some(&json!({ "name": format!("{APP}x") })),
        ),
        409,
        "duplicate app",
    );
    let r = home.sock(
        "POST",
        &format!("/v1/stores/{app_id}/keys"),
        &[],
        Some(&json!({ "name": KEY_NAME })),
    );
    want(&r, 201, "create key");
    let key = r.json();
    let secret = key["secret"].as_str().unwrap().to_owned();
    tokens.push(secret.clone());
    let key_id = key["key"]["id"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {secret}");
    want(
        &home.tcp(
            "POST",
            "/v1/memories",
            &[("Authorization", &auth)],
            Some(&memory),
        ),
        201,
        "key save",
    );
    want(
        &home.tcp("GET", "/v1/session", &[("Authorization", &auth)], None),
        200,
        "key session",
    );
    want(
        &home.tcp(
            "GET",
            "/v1/memories",
            &[("Authorization", "Bearer knoggin_live_mkrbadtokenq12")],
            None,
        ),
        401,
        "bad key",
    );
    tokens.push("mkrbadtokenq12".into());
    let r = home.sock(
        "POST",
        &format!("/v1/stores/{app_id}/keys/{key_id}/rotate"),
        &[],
        Some(&json!({})),
    );
    want(&r, 201, "rotate key");
    let rotated = r.json();
    tokens.push(rotated["secret"].as_str().unwrap().to_owned());
    let new_id = rotated["key"]["id"].as_str().unwrap().to_owned();
    for (m, p, status) in [
        ("GET", format!("/v1/stores/{app_id}"), 200),
        ("GET", format!("/v1/stores/{app_id}/keys"), 200),
        ("GET", format!("/v1/stores/{app_id}/status"), 200),
        // The snapshot list exists on a server only.
        ("GET", format!("/v1/admin/stores/{app_id}/snapshots"), 404),
        ("DELETE", format!("/v1/stores/{app_id}/keys/{new_id}"), 204),
        ("DELETE", format!("/v1/stores/{app_id}"), 200),
        ("POST", format!("/v1/stores/{app_id}/undelete"), 200),
    ] {
        want(&home.sock(m, &p, &[], None), status, &format!("{m} {p}"));
    }

    // The local web session: a one-time code over the socket, a session on the web listener.
    let r = home.sock("POST", "/v1/local/browser-code", &[], None);
    want(&r, 200, "browser code");
    let url = r.json()["url"].as_str().unwrap().to_owned();
    let (origin, code) = url.split_once("/?t=").unwrap();
    tokens.push(code.to_owned());
    let (st, session) = web(
        origin,
        "POST",
        "/v1/local/session",
        None,
        Some(&json!({ "code": code })),
    );
    assert_eq!(st, 200, "{session}");
    let token = session["sessionToken"].as_str().unwrap().to_owned();
    tokens.push(token.clone());
    assert_eq!(web(origin, "GET", "/v1/stores", Some(&token), None).0, 200);
    assert_eq!(
        web(
            origin,
            "GET",
            "/v1/session",
            Some(&format!("{token}x")),
            None
        )
        .0,
        200
    );
    assert_eq!(
        web(origin, "POST", "/auth/logout", Some(&token), None).0,
        204
    );
    let (st, _) = web(origin, "DELETE", "/v1/account", None, None);
    assert!(st >= 400);
    let delete_account = || {
        home.sock(
            "DELETE",
            "/v1/account",
            &[],
            Some(&json!({ "confirm": "delete my data" })),
        )
    };
    // The user undelete is an admin route, which a deleted account cannot call (410), and
    // the one user cannot be removed by an admin; so on the daemon it can only refuse.
    let undelete_user = || home.sock("POST", &format!("/v1/users/{user}/undelete"), &[], None);
    want(&delete_account(), 200, "delete account");
    want(&undelete_user(), 410, "undelete user while deleted");
    want(
        &home.sock("POST", "/v1/account/undelete", &[], None),
        200,
        "undelete account",
    );
    want(&undelete_user(), 409, "undelete a user that is not deleted");

    // The MCP endpoint over HTTP: no stream, and the older revision with its session.
    let mcp_headers = [("Accept", "application/json, text/event-stream")];
    want(
        &home.sock("GET", "/mcp", &mcp_headers, None),
        405,
        "GET /mcp",
    );
    want(
        &home.sock("DELETE", "/mcp", &mcp_headers, None),
        405,
        "DELETE /mcp without a session",
    );
    let init = json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": CLIENT, "version": "1" } } });
    let r = home.sock("POST", "/mcp", &mcp_headers, Some(&init));
    want(&r, 200, "legacy initialize");
    let sid = r
        .headers
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("mcp-session-id")
                .then(|| v.trim().to_owned())
        })
        .unwrap();
    tokens.push(sid.clone());
    let session = [mcp_headers[0], ("Mcp-Session-Id", sid.as_str())];
    let call = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": { "name": "store_memory", "arguments": memory.clone() } });
    let r = home.sock("POST", "/mcp", &session, Some(&call));
    want(&r, 200, "legacy tool call");
    assert!(r.json()["result"]["isError"].is_null(), "{}", r.text());
    want(
        &home.sock("DELETE", "/mcp", &session, None),
        200,
        "DELETE /mcp",
    );

    // Every tool, over the bridge, with the client's name as a marker.
    let (out, err) = bridge(&home, &[], &[tool(1, "store_memory", memory.clone())], &[]);
    stderr.push_str(&err);
    let a = tool_json(&out[0])["id"].as_str().unwrap().to_owned();
    let (out, err) = bridge(&home, &[], &[tool(1, "store_memory", memory.clone())], &[]);
    stderr.push_str(&err);
    let b = tool_json(&out[0])["id"].as_str().unwrap().to_owned();
    let calls = [
        tool(2, "store_memories", json!({ "memories": [memory.clone()] })),
        tool(3, "get_context", json!({ "topic": QUERY })),
        tool(
            4,
            "recall_memories",
            json!({ "query": QUERY, "entities": [ENTITY] }),
        ),
        tool(5, "get_memory", json!({ "id": a })),
        tool(6, "list_memories", json!({ "tags": [TAG] })),
        tool(7, "list_namespaces", json!({})),
        tool(8, "list_tags", json!({ "prefix": "mkr" })),
        tool(9, "namespace_stats", json!({ "namespace": NAMESPACE })),
        tool(10, "find_similar_memories", json!({ "id": a })),
        tool(
            11,
            "supersede_memory",
            json!({ "id": b, "summary": format!("{SUMMARY} later") }),
        ),
        tool(12, "reinforce_memory", json!({ "id": a, "quality": 4 })),
        tool(
            13,
            "create_namespace",
            json!({ "name": format!("{NAMESPACE}b") }),
        ),
        tool(
            14,
            "move_memories",
            json!({ "ids": [a], "destinationNamespace": format!("{NAMESPACE}b") }),
        ),
        tool(
            15,
            "update_namespace_policy",
            json!({ "name": format!("{NAMESPACE}b"), "policy": {
            "initialStability": 3.7145, "defaultDifficulty": 5.0,
            "phaseThresholds": { "fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05 },
            "permastoreThreshold": 1500.0, "decayRateMultiplier": 2.0 } }),
        ),
        tool(16, "forget_memories", json!({ "ids": [a] })),
        tool(17, "forget_memory", json!({ "id": b })),
        tool(
            18,
            "delete_namespace",
            json!({ "name": format!("{NAMESPACE}b"), "force": true }),
        ),
        tool(
            19,
            "store_memory",
            json!({ "summary": "", "tags": [format!("{TAG}!")] }),
        ),
        modern(20, "tools/list", json!({})),
    ];
    let (out, err) = bridge(&home, &["--tool-set", "plugin"], &calls, &[]);
    stderr.push_str(&err);
    assert_eq!(out.len(), calls.len());
    let (out, err) = bridge(&home, &[], &calls, &[]);
    stderr.push_str(&err);
    assert_eq!(out.len(), calls.len());
    for (o, c) in out.iter().zip(&calls) {
        assert!(o["error"].is_null(), "{c}: {o}");
    }

    // The CLI's own output to standard error, with markers in its arguments.
    for args in [
        vec!["apps", "create", APP],
        vec!["keys", "create", "--name", KEY_NAME],
        vec!["reembed"],
    ] {
        stderr.push_str(&home.run(&args).stderr);
    }

    let page = {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", metrics)).unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        t
    };
    assert!(page.contains("knoggin_requests_total"), "{page}");
    assert_eq!(d.stop(), 0);

    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert!(
        log.contains("route=/v1/memories"),
        "trace logging did not run"
    );
    let all = format!("{}\n{log}\n{stderr}\n{page}", d.stderr());
    // The master key set and every data key (of each snapshot and of the state file), in
    // hex and in base64: none of them may be in a log or the metrics page.
    let (master, data) = key_bytes(&home);
    assert!(
        !master.is_empty() && data.len() >= 2,
        "{} data keys",
        data.len()
    );
    for key in master.iter().chain(&data) {
        for form in encodings(key) {
            assert!(
                !all.contains(&form),
                "a key is in the logs or the metrics page"
            );
        }
    }
    for key in &master {
        for (path, bytes) in home.data_bytes() {
            for form in encodings(key) {
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(&form),
                    "the master key is in {}",
                    path.display()
                );
            }
        }
    }
    let mut markers: Vec<String> = [
        SUMMARY, FULL, TAG, ENTITY, TOPIC, EMOTION, NAMESPACE, QUERY, APP, KEY_NAME, CLIENT,
    ]
    .iter()
    .map(|m| (*m).to_owned())
    .collect();
    markers.extend(tokens);
    for m in &markers {
        assert!(
            !all.contains(m.as_str()),
            "the marker {m} is in the logs or the metrics page"
        );
    }
    for (path, bytes) in home.data_bytes() {
        let text = String::from_utf8_lossy(&bytes);
        for m in &markers {
            assert!(
                !text.contains(m.as_str()),
                "the marker {m} is in {}",
                path.display()
            );
        }
    }
}

/// A header of a raw answer.
fn header_of(r: &support::Resp, name: &str) -> Vec<String> {
    r.headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_owned())
        })
        .collect()
}

/// The value of cookie `name` in an answer's `Set-Cookie` headers.
fn set_cookie(r: &support::Resp, name: &str) -> String {
    header_of(r, "set-cookie")
        .into_iter()
        .find_map(|c| {
            let v = c.strip_prefix(&format!("{name}="))?.split(';').next()?;
            (!v.is_empty()).then(|| v.to_owned())
        })
        .unwrap_or_else(|| panic!("no {name} cookie in {}", r.headers))
}

/// Milestone 5: a daemon with sign-in on against the test issuer, at log level `trace`.
/// Markers sit in the sign-in subject, the email and the name; every secret of the flow
/// (state, nonce, PKCE verifier and challenge, authorization code, the sealed login
/// cookie, the session value, the ID and access tokens the exchange returned, minted
/// tokens, good and bad, and the web client secret) is then searched for in the logs and
/// the metrics page. The client secret goes in the token request's form.
#[test]
fn no_sign_in_secret_or_identity_reaches_a_log() {
    sign_in_markers(false);
}

/// The same with an issuer that takes only `client_secret_basic`, so the client secret
/// travels in an `Authorization: Basic` header; its base64 form is searched for too.
#[test]
fn no_sign_in_secret_reaches_a_log_with_client_secret_basic() {
    sign_in_markers(true);
}

fn sign_in_markers(basic: bool) {
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use knoggin_http::testing::issuer::{TEST_SECRET, TestIssuer};

    const SUB: &str = "mkrsubjectq20";
    const NAME: &str = "mkrnameq21";
    const CLIENT_ID: &str = "web-client";
    let email = format!("{SUB}@test.example");
    let rt = tokio::runtime::Runtime::new().unwrap();
    let issuer = rt.block_on(TestIssuer::start(true)).unwrap();
    if basic {
        issuer.require_basic();
    }
    let port = free_port();
    let metrics = free_port();
    let public = format!("http://127.0.0.1:{port}");
    let home = Home::new()
        .with("KNOGGIN_LOG__LEVEL", "trace")
        .with("KNOGGIN_METRICS__BIND", &format!("127.0.0.1:{metrics}"))
        .with("KNOGGIN_SERVER__BIND", &format!("127.0.0.1:{port}"))
        .with("KNOGGIN_SERVER__PUBLIC_URL", &public)
        .with("KNOGGIN_AUTH__MODE", "oauth")
        .with("KNOGGIN_AUTH__ISSUER", issuer.url())
        .with("KNOGGIN_AUTH__WEB_CLIENT_ID", CLIENT_ID)
        .with("KNOGGIN_AUTH__WEB_CLIENT_SECRET", TEST_SECRET)
        .with("KNOGGIN_AUTH__ALLOWED_EMAILS", &email);
    let mut d = home.daemon();
    let mut secrets: Vec<String> = Vec::new();
    let want = |r: &support::Resp, status: u16, what: &str| {
        assert_eq!(r.status, status, "{what}: {} {}", r.headers, r.text());
    };

    // Web sign-in, step by step.
    let r = home.tcp(
        "GET",
        &format!("/auth/login?return_to=/map&login_hint={SUB}"),
        &[],
        None,
    );
    want(&r, 303, "login");
    let login = set_cookie(&r, "knoggin_login");
    secrets.push(login.clone());
    let to = header_of(&r, "location").remove(0);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let back = rt.block_on(async { client.get(&to).send().await }).unwrap();
    let callback = back.headers()["location"].to_str().unwrap().to_owned();
    let path = callback.strip_prefix(&public).unwrap().to_owned();
    let cookie = format!("knoggin_login={login}");
    let r = home.tcp("GET", &path, &[("Cookie", &cookie)], None);
    want(&r, 303, "callback");
    let session = set_cookie(&r, "knoggin_session");
    secrets.push(session.clone());
    // The same callback again fails at the exchange (the code works once).
    let r = home.tcp("GET", &path, &[("Cookie", &cookie)], None);
    want(&r, 303, "replayed callback");
    let browser = format!("knoggin_session={session}");
    let ui = [("Cookie", browser.as_str()), ("X-Knoggin-UI", "1")];
    let r = home.tcp("GET", "/v1/session", &ui, None);
    want(&r, 200, "cookie session");
    assert_eq!(r.json()["user"]["email"], email.as_str(), "{}", r.text());
    want(
        &home.tcp("GET", "/v1/stores", &ui, None),
        200,
        "cookie stores",
    );
    want(
        &home.tcp(
            "POST",
            "/v1/stores",
            &[("Cookie", &browser)],
            Some(&json!({ "name": "x" })),
        ),
        403,
        "write without the UI header",
    );
    want(
        &home.tcp(
            "POST",
            "/v1/stores/00000000-0000-0000-0000-000000000000/keys",
            &ui,
            Some(&json!({ "name": "k" })),
        ),
        403,
        "a key from a cookie session",
    );

    // Access tokens: good, of another person, and broken.
    let mut claims = issuer.claims(SUB, &format!("{public}/mcp"));
    claims["name"] = json!(NAME);
    let token = issuer.mint(&claims);
    let auth = format!("Bearer {token}");
    let memory = json!({ "summary": "from a token" });
    want(
        &home.tcp(
            "POST",
            "/v1/memories",
            &[("Authorization", &auth)],
            Some(&memory),
        ),
        201,
        "token save",
    );
    want(
        &home.tcp("GET", "/v1/session", &[("Authorization", &auth)], None),
        200,
        "token session",
    );
    want(
        &home.tcp("GET", "/v1/stores", &[("Authorization", &auth)], None),
        403,
        "token control",
    );
    let mcp = [
        ("Authorization", auth.as_str()),
        ("Accept", "application/json, text/event-stream"),
    ];
    let r = home.tcp(
        "POST",
        "/mcp",
        &mcp,
        Some(
            &json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": { "name": "agent", "version": "1" } } }),
        ),
    );
    want(&r, 200, "token MCP");
    let mut other = issuer.claims("mkrstrangerq22", &format!("{public}/mcp"));
    other["name"] = json!(NAME);
    let stranger = format!("Bearer {}", issuer.mint(&other));
    want(
        &home.tcp(
            "GET",
            "/v1/namespaces",
            &[("Authorization", &stranger)],
            None,
        ),
        403,
        "not allowed",
    );
    let mut expired = claims.clone();
    expired["exp"] = json!(1);
    let expired = format!("Bearer {}", issuer.mint(&expired));
    let r = home.tcp(
        "GET",
        "/v1/namespaces",
        &[("Authorization", &expired)],
        None,
    );
    want(&r, 401, "expired token");
    assert!(header_of(&r, "www-authenticate")[0].contains("invalid_token"));
    let broken = format!("{token}mkrbrokenq23");
    want(
        &home.tcp(
            "POST",
            "/mcp/plugin",
            &[("Authorization", &format!("Bearer {broken}"))],
            Some(&json!({})),
        ),
        401,
        "broken token",
    );
    secrets.extend([token, broken, stranger, expired]);
    want(
        &home.tcp(
            "GET",
            "/.well-known/oauth-protected-resource/mcp",
            &[],
            None,
        ),
        200,
        "metadata",
    );
    want(&home.tcp("POST", "/auth/logout", &ui, None), 204, "logout");
    want(
        &home.tcp("GET", "/v1/stores", &ui, None),
        401,
        "after logout",
    );

    let page = {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", metrics)).unwrap();
        s.write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut t = String::new();
        s.read_to_string(&mut t).unwrap();
        t
    };
    assert_eq!(d.stop(), 0);
    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert!(
        log.contains("route=/v1/memories"),
        "trace logging did not run"
    );
    // The refusals were logged, by kind only.
    assert!(log.contains("token_refused") && log.contains("sign_in_failed"));
    let all = format!("{}\n{log}\n{page}", d.stderr());
    let issued = issuer.secrets();
    // state, nonce, challenge, code, verifier, the ID token and the access token.
    assert!(issued.len() >= 7, "{} secrets", issued.len());
    secrets.extend(issued);
    // The web client secret, as the form carries it and as `client_secret_basic` does
    // (the ID and the secret need no form-encoding here), with and without padding.
    let pair = STANDARD.encode(format!("{CLIENT_ID}:{TEST_SECRET}"));
    let alone = STANDARD.encode(TEST_SECRET);
    secrets.extend([
        TEST_SECRET.to_owned(),
        pair.trim_end_matches('=').to_owned(),
        alone.trim_end_matches('=').to_owned(),
        SUB.to_owned(),
        NAME.to_owned(),
        email.clone(),
        "mkrstrangerq22".into(),
    ]);
    for s in &secrets {
        assert!(
            !all.contains(s.as_str()),
            "a sign-in secret or marker is in the logs: {s:.12}..."
        );
    }
    for (path, bytes) in home.data_bytes() {
        let text = String::from_utf8_lossy(&bytes);
        for m in [SUB, NAME, email.as_str()] {
            assert!(!text.contains(m), "the marker {m} is in {}", path.display());
        }
    }
}

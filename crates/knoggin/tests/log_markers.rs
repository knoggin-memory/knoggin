// SPDX-License-Identifier: AGPL-3.0-only
//! The log-marker test (tab "Security", Logs; tab "Engineering rules", rule 3): marker
//! strings in every kind of sensitive value that milestone 3 has, every route and every
//! tool run at log level `trace`, then all log output and the metrics page are searched,
//! and the data folder too. Sign-in tokens (milestone 5) and `/v1/ui/*` (milestone 4) are
//! not in it yet.
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

#[test]
fn no_marker_reaches_a_log_the_metrics_page_or_the_data_folder() {
    let metrics = free_port();
    let home = Home::new()
        .with("KNOGGIN_LOG__LEVEL", "trace")
        .with("KNOGGIN_METRICS__BIND", &format!("127.0.0.1:{metrics}"));
    let mut d = home.daemon();
    let mut stderr = String::new();
    let mut tokens: Vec<String> = Vec::new();
    let ok = |r: &support::Resp, what: &str| {
        assert!(r.status < 500, "{what}: {} {}", r.status, r.text())
    };

    // Store routes on the personal store, over the socket.
    let s = |m: &str, p: &str, b: Option<Value>| {
        home.sock(m, p, &[("Knoggin-Namespace", NAMESPACE)], b.as_ref())
    };
    ok(
        &s("POST", "/v1/namespaces", Some(json!({ "name": NAMESPACE }))),
        "create namespace",
    );
    let memory = json!({ "summary": format!("{SUMMARY} about {QUERY}"), "fullText": FULL, "tags": [TAG],
        "entities": [ENTITY], "topics": [TOPIC], "emotions": [EMOTION] });
    let r = s("POST", "/v1/memories", Some(memory.clone()));
    ok(&r, "save");
    let id = r.json()["id"].as_str().unwrap().to_owned();
    let r = s(
        "POST",
        "/v1/memories/batch",
        Some(json!({ "memories": [memory.clone(), memory.clone()] })),
    );
    ok(&r, "batch");
    let other = r.json()["results"][0]["memory"]["id"]
        .as_str()
        .unwrap_or(&id)
        .to_owned();
    for (m, p, b) in [
        ("GET", format!("/v1/memories?tag={TAG}"), None),
        ("GET", format!("/v1/memories/{id}"), None),
        (
            "POST",
            format!("/v1/memories/{id}/reinforce"),
            Some(json!({ "quality": 3 })),
        ),
        (
            "POST",
            format!("/v1/memories/{id}/similar"),
            Some(json!({})),
        ),
        (
            "POST",
            "/v1/recall".into(),
            Some(json!({ "query": QUERY, "entities": [ENTITY], "tags": [TAG] })),
        ),
        ("GET", format!("/v1/context?topic={QUERY}"), None),
        ("GET", "/v1/namespaces".into(), None),
        ("GET", format!("/v1/namespaces/{NAMESPACE}/stats"), None),
        ("GET", format!("/v1/tags?prefix={TAG}"), None),
        (
            "POST",
            format!("/v1/memories/{other}/supersede"),
            Some(json!({ "summary": format!("{SUMMARY} again") })),
        ),
        (
            "POST",
            "/v1/memories/move".into(),
            Some(json!({ "ids": [other], "destinationNamespace": "default" })),
        ),
        (
            "PUT",
            format!("/v1/namespaces/{NAMESPACE}/policy"),
            Some(json!({ "policy": {
            "initialStability": 3.7145, "defaultDifficulty": 5.0,
            "phaseThresholds": { "fullToSummary": 0.7, "summaryToGhost": 0.3, "ghostToDelete": 0.05 },
            "permastoreThreshold": 1500.0, "decayRateMultiplier": null } })),
        ),
        (
            "POST",
            "/v1/memories".into(),
            Some(json!({ "summary": "", "tags": [format!("{TAG}!bad")] })),
        ),
        ("GET", "/v1/export".into(), None),
        (
            "POST",
            "/v1/memories/forget".into(),
            Some(json!({ "ids": [other] })),
        ),
    ] {
        ok(&s(m, &p, b), &p);
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
    ok(&r, "empty import");
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
        ok(&home.sock("GET", p, &[], None), p);
    }
    let app = home
        .sock("POST", "/v1/stores", &[], Some(&json!({ "name": APP })))
        .json();
    let app_id = app["id"].as_str().unwrap().to_owned();
    ok(
        &home.sock(
            "PATCH",
            &format!("/v1/stores/{app_id}"),
            &[],
            Some(&json!({ "name": format!("{APP}x") })),
        ),
        "rename",
    );
    ok(
        &home.sock(
            "POST",
            "/v1/stores",
            &[],
            Some(&json!({ "name": format!("{APP}x") })),
        ),
        "duplicate app",
    );
    let key = home
        .sock(
            "POST",
            &format!("/v1/stores/{app_id}/keys"),
            &[],
            Some(&json!({ "name": KEY_NAME })),
        )
        .json();
    let secret = key["secret"].as_str().unwrap().to_owned();
    tokens.push(secret.clone());
    let key_id = key["key"]["id"].as_str().unwrap().to_owned();
    let auth = format!("Bearer {secret}");
    ok(
        &home.tcp(
            "POST",
            "/v1/memories",
            &[("Authorization", &auth)],
            Some(&memory),
        ),
        "key save",
    );
    ok(
        &home.tcp("GET", "/v1/session", &[("Authorization", &auth)], None),
        "key session",
    );
    ok(
        &home.tcp(
            "GET",
            "/v1/memories",
            &[("Authorization", "Bearer knoggin_live_mkrbadtokenq12")],
            None,
        ),
        "bad key",
    );
    tokens.push("mkrbadtokenq12".into());
    let rotated = home
        .sock(
            "POST",
            &format!("/v1/stores/{app_id}/keys/{key_id}/rotate"),
            &[],
            Some(&json!({})),
        )
        .json();
    tokens.push(rotated["secret"].as_str().unwrap().to_owned());
    let new_id = rotated["key"]["id"].as_str().unwrap().to_owned();
    for (m, p) in [
        ("GET", format!("/v1/stores/{app_id}")),
        ("GET", format!("/v1/stores/{app_id}/keys")),
        ("GET", format!("/v1/stores/{app_id}/status")),
        ("GET", format!("/v1/admin/stores/{app_id}/snapshots")),
        ("DELETE", format!("/v1/stores/{app_id}/keys/{new_id}")),
        ("DELETE", format!("/v1/stores/{app_id}")),
        ("POST", format!("/v1/stores/{app_id}/undelete")),
    ] {
        ok(&home.sock(m, &p, &[], None), &p);
    }

    // The local web session: a one-time code over the socket, a session on the web listener.
    let url = home
        .sock("POST", "/v1/local/browser-code", &[], None)
        .json()["url"]
        .as_str()
        .unwrap()
        .to_owned();
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
    ok(
        &home.sock(
            "DELETE",
            "/v1/account",
            &[],
            Some(&json!({ "confirm": "delete my data" })),
        ),
        "delete account",
    );
    ok(
        &home.sock("POST", "/v1/account/undelete", &[], None),
        "undelete account",
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

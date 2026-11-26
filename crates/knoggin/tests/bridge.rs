// SPDX-License-Identifier: AGPL-3.0-only
//! `knoggin mcp`, the stdio bridge (tab "MCP", Tests that must exist): scripted sessions over
//! pipes on both revisions and both tool sets, a daemon that is not yet running, no TCP
//! connection, `KNOGGIN_API_KEY`, and the answers it makes up itself.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use serde_json::{Value, json};
use support::{Home, Started, bridge, call, modern, tool_json};

fn legacy_init(id: i64) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": { "name": "legacy-bridge-test", "version": "1" } } })
}

fn legacy(id: i64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn tool_names(answer: &Value) -> Vec<String> {
    answer["result"]["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("{answer}"))
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn both_revisions_and_both_tool_sets_starting_the_daemon() {
    let home = Home::new();
    let _guard = Started(&home);
    // No daemon yet: the bridge starts one and keeps trying.
    let script = [
        modern(1, "tools/list", json!({})),
        call(
            2,
            "store_memory",
            json!({ "summary": "Bridged over the socket" }),
        ),
        call(3, "recall_memories", json!({ "query": "bridged socket" })),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    ];
    let (out, err) = bridge(&home, &[], &script, &[]);
    assert_eq!(out.len(), 3, "{err}");
    assert_eq!(tool_names(&out[0]).len(), 18);
    assert_eq!(tool_json(&out[1])["phase"], "full", "{}", out[1]);
    assert_eq!(
        tool_json(&out[2])["memories"][0]["summary"],
        "Bridged over the socket",
        "{}",
        out[2]
    );
    assert_eq!(home.json(&["status"])["running"], true);

    // The older revision: the session from `initialize` is kept and sent.
    let script = [
        legacy_init(1),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        legacy(2, "tools/list", json!({})),
        legacy(
            3,
            "tools/call",
            json!({ "name": "recall_memories", "arguments": { "query": "bridged" } }),
        ),
    ];
    let (out, err) = bridge(&home, &["--tool-set", "plugin"], &script, &[]);
    assert_eq!(out.len(), 3, "{err}");
    assert_eq!(out[0]["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(tool_names(&out[1]).len(), 10);
    assert_eq!(
        tool_json(&out[2])["memories"][0]["summary"],
        "Bridged over the socket"
    );
    let (out, _) = bridge(
        &home,
        &[],
        &[legacy_init(1), legacy(2, "tools/list", json!({}))],
        &[],
    );
    assert_eq!(tool_names(&out[1]).len(), 18);
    let (out, _) = bridge(
        &home,
        &["--tool-set", "plugin"],
        &[modern(1, "tools/list", json!({}))],
        &[],
    );
    assert_eq!(tool_names(&out[0]).len(), 10);

    // A JSON array is refused on both revisions; a line that is not JSON is -32700.
    let (out, _) = bridge(&home, &[], &[json!([modern(1, "ping", json!({}))])], &[]);
    assert_eq!(out[0]["error"]["code"], -32600, "{}", out[0]);
    let (out, _) = bridge(
        &home,
        &[],
        &[legacy_init(1), json!([legacy(2, "ping", json!({}))])],
        &[],
    );
    assert_eq!(out[1]["error"]["code"], -32600, "{}", out[1]);
    let mut child = home.cmd(&["mcp"]);
    let o = {
        use std::io::Write;
        let mut p = child
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        p.stdin
            .take()
            .unwrap()
            .write_all(b"this is not json\n")
            .unwrap();
        p.wait_with_output().unwrap()
    };
    let line: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(line["error"]["code"], -32700);
    assert_eq!(line["id"], Value::Null);

    // 100 memories of full size are accepted on /mcp.
    let item = json!({ "summary": "s".repeat(2000), "fullText": "f".repeat(65_536) });
    let (out, err) = bridge(
        &home,
        &[],
        &[call(
            1,
            "store_memories",
            json!({ "memories": vec![item; 100] }),
        )],
        &[],
    );
    assert_eq!(tool_json(&out[0])["stored"], 100, "{err}");
}

#[test]
fn an_api_key_reaches_its_store_and_a_bad_one_gets_a_json_rpc_answer() {
    let home = Home::new();
    let mut d = home.daemon();
    home.json(&["apps", "create", "Agent"]);
    let secret = home.json(&["keys", "create", "--app", "agent"])["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let key = [("KNOGGIN_API_KEY", secret.as_str())];
    let (out, err) = bridge(
        &home,
        &[],
        &[call(
            1,
            "store_memory",
            json!({ "summary": "In the agent app" }),
        )],
        &key,
    );
    assert!(out[0]["result"]["isError"].is_null(), "{err}{}", out[0]);
    let personal = home.sock("GET", "/v1/memories", &[], None).json();
    assert_eq!(personal["items"], json!([]));
    let (out, _) = bridge(&home, &[], &[call(1, "list_memories", json!({}))], &key);
    assert_eq!(
        tool_json(&out[0])["items"][0]["summary"],
        "In the agent app"
    );
    // An edge error is a JSON-RPC error with the request's ID.
    let bad = format!("knoggin_live_{}", "B".repeat(43));
    let (out, _) = bridge(
        &home,
        &[],
        &[call(9, "list_memories", json!({}))],
        &[("KNOGGIN_API_KEY", &bad)],
    );
    assert_eq!(out[0]["id"], 9);
    assert_eq!(out[0]["error"]["code"], -32603);
    assert_eq!(out[0]["error"]["data"]["code"], "unauthenticated");
    d.stop();
}

#[test]
fn no_tcp_is_opened_and_a_daemon_that_cannot_start_gets_an_answer() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let home = Home::new().with("KNOGGIN_SERVER__BIND", &format!("127.0.0.1:{port}"));
    let (out, _) = bridge(&home, &[], &[modern(4, "ping", json!({}))], &[]);
    assert_eq!(out[0]["id"], 4);
    assert_eq!(out[0]["error"]["code"], -32603);
    assert!(
        out[0]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("knoggin start"),
        "{}",
        out[0]
    );
    assert!(
        listener.accept().is_err(),
        "the bridge or its daemon connected to the TCP port"
    );
    // A remote address must be https unless it is written as loopback.
    let o = home.run(&["mcp", "--url", "http://memory.example.com/mcp"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("https"), "{}", o.stderr);
    let o = home.run(&["mcp", "--url", "https://memory.example.com/mcp"]);
    assert!(o.stderr.contains("KNOGGIN_API_KEY"), "{}", o.stderr);
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The local commands against a daemon in a temporary home, with their exit codes (tab
//! "Config, CLI, local install", Tests that must exist: Commands, recalld import).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::path::PathBuf;

use serde_json::{Value, json};
use support::Home;

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/recalld-data")
}

#[test]
fn without_a_daemon_commands_exit_3_and_send_nothing_to_the_usual_port() {
    // Another program holds the port the daemon would use.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let home = Home::new().with("KNOGGIN_SERVER__BIND", &format!("127.0.0.1:{port}"));
    for args in [
        vec!["status"],
        vec!["stop"],
        vec!["apps", "list"],
        vec!["keys", "list"],
        vec!["reembed"],
        vec!["healthcheck"],
    ] {
        let o = home.run(&args);
        let want = if args == ["healthcheck"] { 1 } else { 3 };
        assert_eq!(o.code, want, "{args:?}: {}{}", o.stdout, o.stderr);
    }
    let o = home.run(&["apps", "list"]);
    assert!(o.stderr.contains("knoggin start"), "{}", o.stderr);
    // `start` runs a daemon, which cannot take the port and says so.
    let o = home.run(&["start"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("daemon.log"), "{}", o.stderr);
    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert!(log.contains("server.bind"), "{log}");
    assert!(
        listener.accept().is_err(),
        "a command connected to the TCP port"
    );
}

#[test]
fn apps_keys_export_import_and_reembed() {
    let home = Home::new();
    let mut d = home.daemon();
    assert_eq!(home.json(&["apps", "create", "Notes"])["name"], "Notes");
    assert_ne!(home.run(&["apps", "create", "notes"]).code, 0);
    let list = home.json(&["apps", "list"]);
    assert_eq!(list["items"].as_array().unwrap().len(), 2);

    let created = home.json(&[
        "keys",
        "create",
        "--app",
        "NOTES",
        "--name",
        "laptop",
        "--read-only",
        "--expires-days",
        "30",
    ]);
    assert!(
        created["secret"]
            .as_str()
            .unwrap()
            .starts_with("knoggin_live_")
    );
    assert_eq!(created["key"]["permissions"], json!(["memory:read"]));
    let keys = home.json(&["keys", "list", "--app", "notes"]);
    assert_eq!(keys["items"][0]["name"], "laptop");
    let id = keys["items"][0]["id"].as_str().unwrap().to_owned();
    assert_eq!(home.run(&["keys", "revoke", &id, "--app", "notes"]).code, 0);
    let keys = home.json(&["keys", "list", "--app", "notes"]);
    assert!(keys["items"][0]["revokedAt"].is_string());
    assert_eq!(home.json(&["keys", "create"])["key"]["name"], "cli");
    assert_ne!(home.run(&["keys", "list", "--app", "nope"]).code, 0);

    for s in ["first memory", "second memory"] {
        let r = home.sock("POST", "/v1/memories", &[], Some(&json!({ "summary": s })));
        assert_eq!(r.status, 201);
    }
    let file = home.dir.path().join("personal.jsonl");
    let f = file.to_str().unwrap();
    assert_eq!(home.run(&["export", "--out", f]).code, 0);
    // An existing path is refused.
    assert_ne!(home.run(&["export", "--out", f]).code, 0);
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.starts_with("{\"type\":\"header\""), "{text}");
    let imported = home.json(&["import", f, "--app", "notes"]);
    assert_eq!(imported["errors"], json!([]));
    assert!(imported["imported"].as_u64().unwrap() >= 3, "{imported}");
    // Twice changes nothing; with `error` the memories are error lines.
    let again = home.json(&["import", f, "--app", "notes"]);
    assert_eq!(again["imported"], 0, "{again}");
    let o = home.run(&["import", f, "--app", "notes", "--on-conflict", "error"]);
    assert_eq!(o.code, 1, "{}", o.stdout);
    let notes = home.json(&["apps", "list"]);
    let id = notes["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Notes")
        .unwrap()["id"]
        .clone();
    let listed = home.sock(
        "GET",
        "/v1/memories",
        &[("Knoggin-Store", id.as_str().unwrap())],
        None,
    );
    assert_eq!(listed.json()["items"].as_array().unwrap().len(), 2);

    let r = home.json(&["reembed", "--app", "notes"]);
    assert_eq!(r["embedding"]["pending"], 0, "{r}");
    assert_eq!(home.run(&["healthcheck"]).code, 0);
    assert_eq!(
        home.json(&["status"])["stores"].as_array().unwrap().len(),
        2
    );

    let deleted = home.json(&["apps", "delete", "notes"]);
    assert_eq!(deleted["status"], "deleted");

    // The key commands refuse while the daemon runs.
    let out = home.dir.path().join("k.json");
    assert_ne!(
        home.run(&["key", "export", "--out", out.to_str().unwrap()])
            .code,
        0
    );
    assert_ne!(home.run(&["key", "rotate"]).code, 0);
    d.stop();

    // Rotation re-wraps every key; the next start reads everything with the new key.
    let before: Value =
        serde_json::from_slice(&std::fs::read(home.data().join("key-source.json")).unwrap())
            .unwrap();
    let r = home.json(&["key", "rotate"]);
    assert_ne!(r["kekId"], before["kekId"]);
    assert_eq!(r["stores"], 2);
    let mut d = home.daemon();
    let listed = home.sock("GET", "/v1/memories", &[], None);
    assert_eq!(
        listed.json()["items"].as_array().unwrap().len(),
        2,
        "{}",
        listed.text()
    );
    d.stop();
}

#[test]
fn import_recalld_prints_the_expected_counts() {
    let home = Home::new();
    let mut d = home.daemon();
    let o = home.run(&["import", "recalld", fixture().to_str().unwrap(), "--json"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let expected: Value =
        serde_json::from_slice(&std::fs::read(fixture().join("expected-counts.json")).unwrap())
            .unwrap();
    assert_eq!(o.json(), expected);
    // The copies are gone.
    let left: Vec<_> = std::fs::read_dir(home.root().join("run"))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("import-"))
        .collect();
    assert!(left.is_empty());
    d.stop();
}

#[test]
fn config_check_model_verify_and_test_mode() {
    let home = Home::new().with("KNOGGIN_AUTH__WEB_CLIENT_SECRET", "secret-marker-123");
    let o = home.run(&["config", "check"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(
        o.stdout.contains("auth.web_client_secret = [set]"),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("server.bind = \"127.0.0.1:0\""),
        "{}",
        o.stdout
    );
    assert!(!o.stdout.contains("secret-marker-123"));
    let v = home.json(&["config", "check", "--role", "router"]);
    assert_eq!(v["auth.mode"], "\"oauth\"");
    let bad = Home::new().with("KNOGGIN_TEST_MODE", "yes");
    assert_eq!(bad.run(&["config", "check"]).code, 1);
    let unknown = Home::new();
    std::fs::create_dir_all(unknown.root()).unwrap();
    std::fs::write(unknown.root().join("config.toml"), "[server]\nbnd = 1\n").unwrap();
    let o = unknown.run(&["config", "check"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("bnd"), "{}", o.stderr);
    // No model files: verify reports them and exits 1, without loading anything.
    let o = home.run(&["model", "verify", "--json"]);
    assert_eq!(o.code, 1);
    assert_eq!(o.json()["graph"], "missing");
    // Wrong usage is 2.
    assert_eq!(home.run(&["keys", "create", "--expires-days", "7"]).code, 2);
    assert_eq!(home.run(&["frobnicate"]).code, 2);
}

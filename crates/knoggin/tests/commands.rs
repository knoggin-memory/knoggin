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
    // The detached daemon's standard error is its log file: the error line it wrote there
    // is the raw `knoggin: ...` line, once (the panic hook writes the same way).
    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert!(log.contains("server.bind"), "{log}");
    assert_eq!(
        log.lines()
            .filter(|l| l.starts_with("knoggin: ") && l.contains("server.bind"))
            .count(),
        1,
        "{log}"
    );
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
    // Real JSON values: strings, booleans, numbers and arrays.
    assert_eq!(v["auth.mode"], "oauth");
    assert_eq!(v["model.download"], true);
    assert_eq!(v["retention.days"], 7);
    assert_eq!(v["auth.audiences"], json!([]));
    assert_eq!(v["auth.web_client_secret"], "[set]");
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

/// A fake `open` and `xdg-open` first on `PATH`, which only note the file they are given,
/// so that no test opens a real browser.
fn with_fake_opener(home: Home) -> (Home, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let bin = home.dir.path().join("bin");
    let noted = home.dir.path().join("opened");
    std::fs::create_dir_all(&bin).unwrap();
    for name in ["open", "xdg-open"] {
        let script = bin.join(name);
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho \"$1\" >> '{}'\n", noted.display()),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    (home.with("PATH", &path), noted)
}

#[test]
fn open_starts_the_daemon_and_opens_a_private_page() {
    use std::os::unix::fs::PermissionsExt;
    let (home, noted) = with_fake_opener(Home::new());
    let _stop = support::Started(&home);
    // `--print-url` starts the daemon and only prints the address.
    let o = home.run(&["open", "--print-url"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let url = o.stdout.trim().to_owned();
    assert!(url.starts_with("http://") && url.contains("/?t="), "{url}");
    assert!(!noted.exists(), "--print-url opened something");
    // Without it the address goes into a private file under run/, and that file is opened.
    let o = home.run(&["open"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let opened = std::fs::read_to_string(&noted).unwrap();
    let page = PathBuf::from(opened.trim());
    assert_eq!(page.parent().unwrap(), home.root().join("run"));
    let name = page.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        name.starts_with("open-") && name.ends_with(".html"),
        "{name}"
    );
    assert_eq!(
        std::fs::metadata(&page).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let html = std::fs::read_to_string(&page).unwrap();
    assert!(html.contains("/?t="), "{html}");
    assert!(!html.contains(&url), "a one-time code was used twice");
    assert!(!o.stdout.contains("/?t=") && !o.stderr.contains("/?t="));
}

#[test]
fn open_exits_4_and_opens_nothing_when_the_install_is_locked() {
    let (home, noted) = with_fake_opener(Home::new());
    let mut d = home.daemon();
    d.stop();
    std::fs::remove_file(home.root().join("master.key")).unwrap();
    let _stop = support::Started(&home);
    for args in [vec!["open", "--print-url"], vec!["open"]] {
        let o = home.run(&args);
        assert_eq!(o.code, 4, "{args:?}: {}{}", o.stdout, o.stderr);
        assert!(o.stderr.contains("master_key_missing"), "{}", o.stderr);
    }
    assert!(!noted.exists(), "a locked install opened a page");
}

#[test]
fn key_import_loads_a_backup_into_a_copied_data_folder() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new();
    let mut d = home.daemon();
    let r = home.sock(
        "POST",
        "/v1/memories",
        &[],
        Some(&json!({ "summary": "opened again" })),
    );
    assert_eq!(r.status, 201);
    let backup = home.dir.path().join("backup.json");
    let b = backup.to_str().unwrap();
    // Refused while the daemon runs.
    assert_ne!(home.run(&["key", "import", b]).code, 0);
    d.stop();
    assert_eq!(home.run(&["key", "export", "--out", b]).code, 0);
    // The data folder on a new machine: no key, no record.
    std::fs::remove_file(home.root().join("master.key")).unwrap();
    std::fs::remove_file(home.data().join("key-source.json")).unwrap();
    // A backup that others can write is refused.
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert_ne!(home.run(&["key", "import", b]).code, 0);
    std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(home.json(&["key", "import", b])["source"], "key_file");
    assert!(home.data().join("key-source.json").exists());
    let mut d = home.daemon();
    let r = home.sock("GET", "/v1/memories", &[], None);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"][0]["summary"], "opened again");
    d.stop();
}

#[test]
fn without_test_mode_the_daemon_uses_the_real_model() {
    let home = Home::new();
    let models = home.dir.path().join("models");
    std::fs::create_dir_all(&models).unwrap();
    // The real model's files are missing and downloads are off, so nothing is fetched,
    // loaded or written; the state says so under the real model's identity.
    let mut d = home.daemon_with(|c| {
        c.env_remove("KNOGGIN_TEST_MODE")
            .env("KNOGGIN_MODEL__DOWNLOAD", "false")
            .env("KNOGGIN_MODEL__DIR", &models);
    });
    let model = home.json(&["status"])["model"].clone();
    assert_eq!(model["state"], "unavailable", "{model}");
    assert_eq!(model["reason"], "files_missing", "{model}");
    let identity = model["identity"].as_str().unwrap();
    assert!(identity.starts_with("embeddinggemma"), "{model}");
    assert!(!d.stderr().contains("test mode is on"));
    assert_eq!(std::fs::read_dir(&models).unwrap().count(), 0);
    d.stop();
}

#[test]
fn a_detached_daemon_logs_each_line_once() {
    let home = Home::new();
    let _stop = support::Started(&home);
    assert_eq!(home.run(&["start"]).code, 0);
    assert_eq!(home.run(&["stop"]).code, 0);
    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert_eq!(log.matches("the daemon is ready").count(), 1, "{log}");
}

#[test]
fn start_passes_the_log_level_and_not_the_api_key() {
    let home = Home::new().with("KNOGGIN_API_KEY", "knoggin_live_mkrapikey");
    let _stop = support::Started(&home);
    let o = home.run(&["start", "--log-level", "warn"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let env = std::process::Command::new("ps")
        .args(["eww", "-o", "command=", "-p", &home.pid().to_string()])
        .output()
        .unwrap();
    let env = String::from_utf8_lossy(&env.stdout).into_owned();
    assert!(
        env.contains("KNOGGIN_HOME="),
        "ps shows no environment: {env}"
    );
    assert!(env.contains("--log-level warn"), "{env}");
    assert!(!env.contains("KNOGGIN_API_KEY"), "{env}");
    assert_eq!(home.run(&["stop"]).code, 0);
    let log = std::fs::read_to_string(home.root().join("logs/daemon.log")).unwrap();
    assert!(
        !log.contains("the daemon is ready"),
        "an info line at warn: {log}"
    );
}

#[test]
fn model_fetch_writes_nothing_into_model_dir_when_downloads_are_off() {
    let home = Home::new().with("KNOGGIN_MODEL__DOWNLOAD", "false");
    let o = home.run(&["model", "fetch"]);
    assert_eq!(o.code, 1, "{}{}", o.stdout, o.stderr);
    assert!(o.stderr.contains("model.download"), "{}", o.stderr);
    assert!(!home.root().join("models").exists());
}

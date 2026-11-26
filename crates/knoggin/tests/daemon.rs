// SPDX-License-Identifier: AGPL-3.0-only
//! The daemon's start-up, locked mode and shut-down (tab "Config, CLI, local install",
//! Tests that must exist: Daemon), each in a temporary home in test mode.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::json;
use support::Home;

fn state_files(home: &Home) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(home.data().join("state"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    v.sort();
    v
}

fn snapshot_files(home: &Home) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for store in std::fs::read_dir(home.data().join("stores"))
        .unwrap()
        .flatten()
    {
        for f in std::fs::read_dir(store.path()).unwrap().flatten() {
            out.push(f.path());
        }
    }
    out.sort();
    out
}

/// Every file under the data folder with its bytes: "nothing written" compares two of these.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        if e.path().is_dir() {
            out.extend(tree(&e.path()));
        } else {
            out.push((e.path(), std::fs::read(e.path()).unwrap()));
        }
    }
    out.sort();
    out
}

/// A home with one saved memory, stopped cleanly.
fn used_home() -> Home {
    let home = Home::new();
    let mut d = home.daemon();
    let r = home.sock(
        "POST",
        "/v1/memories",
        &[],
        Some(&json!({ "summary": "kept across restarts" })),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    assert_eq!(d.stop(), 0, "{}", d.stderr());
    home
}

fn assert_locked(home: &Home, reason: &str) {
    for (method, path, body) in [
        ("GET", "/v1/memories", None),
        ("GET", "/v1/stores", None),
        ("POST", "/v1/recall", Some(json!({ "query": "x" }))),
        (
            "POST",
            "/mcp",
            Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" })),
        ),
    ] {
        let r = home.sock(method, path, &[], body.as_ref());
        assert_eq!(
            r.json()["error"]["code"],
            "store_locked",
            "{path}: {}",
            r.text()
        );
        assert_eq!(r.json()["error"]["reason"], reason, "{path}");
    }
    let r = home.sock("GET", "/v1/session", &[], None);
    assert!(r.json()["error"]["docsUrl"].is_string(), "{}", r.text());
    assert_eq!(home.sock("GET", "/healthz", &[], None).status, 200);
    assert_eq!(
        home.sock("GET", "/readyz", &[], None).json()["status"],
        "ready"
    );
    assert_eq!(home.run(&["status"]).code, 4);
}

#[test]
fn a_first_run_makes_the_key_the_state_and_the_personal_store() {
    let home = Home::new();
    let mut d = home.daemon();
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(home.data().join("key-source.json")).unwrap())
            .unwrap();
    assert_eq!(record["source"], "key_file");
    assert!(record["kekId"].as_str().unwrap().starts_with("local:"));
    let key = home.root().join("master.key");
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!state_files(&home).is_empty());
    let stores = home.sock("GET", "/v1/stores", &[], None).json();
    assert_eq!(stores["items"][0]["kind"], "personal");
    assert_eq!(stores["items"][0]["name"], "Personal");
    assert!(
        d.stderr()
            .contains("Losing every copy of the master key loses the data"),
        "{}",
        d.stderr()
    );
    // The socket has mode 0600, and the folders 0700.
    let sock = std::fs::metadata(home.socket()).unwrap();
    assert!(sock.file_type().is_socket());
    assert_eq!(sock.permissions().mode() & 0o777, 0o600);
    for dir in [home.root(), home.root().join("run"), home.data()] {
        assert_eq!(
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    let lock = std::fs::metadata(home.root().join("run/daemon.lock")).unwrap();
    assert_eq!(lock.permissions().mode() & 0o777, 0o600);
    assert_eq!(d.stop(), 0);
}

#[test]
fn an_interrupted_first_run_carries_on() {
    let home = Home::new();
    let mut d = home.daemon();
    let kek = std::fs::read(home.data().join("key-source.json")).unwrap();
    d.stop();
    // The key and its record exist, but no state file and no snapshot.
    for f in state_files(&home) {
        std::fs::remove_file(f).unwrap();
    }
    assert!(snapshot_files(&home).is_empty());
    let mut d = home.daemon();
    assert_eq!(
        std::fs::read(home.data().join("key-source.json")).unwrap(),
        kek
    );
    let stores = home.sock("GET", "/v1/stores", &[], None);
    assert_eq!(stores.status, 200, "{}", stores.text());
    d.stop();
}

#[test]
fn a_missing_key_locks_the_install_and_writes_nothing() {
    let home = used_home();
    std::fs::remove_file(home.root().join("master.key")).unwrap();
    let before = tree(&home.data());
    let mut d = home.daemon();
    assert_locked(&home, "master_key_missing");
    assert_eq!(d.stop(), 0);
    assert_eq!(
        tree(&home.data()),
        before,
        "the locked daemon wrote into the data folder"
    );
}

#[test]
fn an_unreadable_state_file_locks_without_falling_back_until_use_previous() {
    let home = used_home();
    let files = state_files(&home);
    assert!(files.len() >= 2, "{files:?}");
    std::fs::write(files.last().unwrap(), b"not a state file").unwrap();
    let before = tree(&home.data());
    let mut d = home.daemon();
    assert_locked(&home, "state_unreadable");
    // `use-previous` refuses while the daemon runs.
    assert_ne!(home.run(&["state", "use-previous", "--yes"]).code, 0);
    assert_eq!(d.stop(), 0);
    assert_eq!(tree(&home.data()), before);
    let o = home.run(&["state", "use-previous", "--yes"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    assert!(o.stdout.contains("undone"), "{}", o.stdout);
    let mut d = home.daemon();
    let r = home.sock("GET", "/v1/memories", &[], None);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(r.json()["items"][0]["summary"], "kept across restarts");
    d.stop();
}

#[test]
fn snapshots_without_a_state_file_lock_as_state_missing() {
    let home = used_home();
    assert!(!snapshot_files(&home).is_empty());
    for f in state_files(&home) {
        std::fs::remove_file(f).unwrap();
    }
    let mut d = home.daemon();
    assert_locked(&home, "state_missing");
    d.stop();
    assert!(
        state_files(&home).is_empty(),
        "an empty install was made beside the stores"
    );
}

#[test]
fn a_second_daemon_says_one_is_running() {
    let home = Home::new();
    let mut d = home.daemon();
    let o = home.run(&["daemon"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("already running"), "{}", o.stderr);
    d.stop();
}

#[test]
fn a_stale_socket_is_replaced_and_a_crash_leaves_no_lock_behind() {
    let home = Home::new();
    std::fs::create_dir_all(home.root().join("run")).unwrap();
    for dir in [home.root(), home.root().join("run")] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::write(home.socket(), b"left over").unwrap();
    let mut d = home.daemon();
    assert!(
        std::fs::metadata(home.socket())
            .unwrap()
            .file_type()
            .is_socket()
    );
    d.kill();
    // The socket file of the crashed daemon is still there; the next start replaces it.
    let mut d = home.daemon();
    assert_eq!(home.sock("GET", "/healthz", &[], None).status, 200);
    d.stop();
}

#[test]
fn a_clean_stop_leaves_a_readable_snapshot_and_no_lock() {
    let home = used_home();
    let snaps = snapshot_files(&home);
    assert!(!snaps.is_empty());
    assert!(!home.socket().exists());
    // The key commands run only when no daemon holds the lock.
    let out = home.dir.path().join("key.json");
    assert_eq!(
        home.run(&["key", "export", "--out", out.to_str().unwrap()])
            .code,
        0
    );
    let export = home.dir.path().join("store.jsonl");
    let o = home.run(&[
        "admin",
        "snapshot",
        "export",
        snaps.last().unwrap().to_str().unwrap(),
        "--out",
        export.to_str().unwrap(),
    ]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let text = std::fs::read_to_string(&export).unwrap();
    assert!(text.contains("kept across restarts"));
    assert_eq!(
        std::fs::metadata(&export).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let i = home.json(&[
        "admin",
        "snapshot",
        "inspect",
        snaps.last().unwrap().to_str().unwrap(),
    ]);
    assert_eq!(i["containerFormat"], 1);
}

#[test]
fn a_home_that_others_can_write_is_refused() {
    let home = Home::new();
    std::fs::create_dir_all(home.root()).unwrap();
    std::fs::set_permissions(home.root(), std::fs::Permissions::from_mode(0o770)).unwrap();
    let o = home.run(&["daemon"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("cannot write"), "{}", o.stderr);
    let home = Home::new().with("KNOGGIN_SERVER__BIND", "0.0.0.0:0");
    let o = home.run(&["daemon"]);
    assert_ne!(o.code, 0);
    assert!(o.stderr.contains("loopback"), "{}", o.stderr);
}

/// CPU seconds a process has used, from `ps -o time=` (`[[dd-]hh:]mm:ss[.cc]`).
fn cpu_seconds(pid: u32) -> f64 {
    let out = std::process::Command::new("ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let (days, rest) = text.split_once('-').unwrap_or(("0", &text));
    let mut secs = days.parse::<f64>().unwrap() * 86_400.0;
    for (i, part) in rest.rsplit(':').enumerate() {
        secs += part.parse::<f64>().unwrap() * 60f64.powi(i as i32);
    }
    secs
}

#[test]
fn an_idle_daemon_uses_no_cpu() {
    let home = Home::new();
    let mut d = home.daemon();
    let pid = d.child.as_ref().unwrap().id();
    // Let start-up finish, then measure five quiet seconds. A busy loop uses about five.
    std::thread::sleep(std::time::Duration::from_secs(2));
    let before = cpu_seconds(pid);
    std::thread::sleep(std::time::Duration::from_secs(5));
    let used = cpu_seconds(pid) - before;
    assert!(
        used < 1.5,
        "the idle daemon used {used:.2} s of CPU in 5 s"
    );
    assert_eq!(d.stop(), 0);
}

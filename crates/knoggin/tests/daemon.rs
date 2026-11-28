// SPDX-License-Identifier: AGPL-3.0-only
//! The daemon's start-up, locked mode and shut-down, each in a temporary home in test
//! mode.
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

/// Every API route: each path and method of `docs/openapi.json` (path parameters filled
/// with an ID), and the MCP endpoints.
fn api_routes() -> Vec<(String, String)> {
    let doc: serde_json::Value = serde_json::from_slice(
        &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi.json"))
            .unwrap(),
    )
    .unwrap();
    let mut out = Vec::new();
    for (path, item) in doc["paths"].as_object().unwrap() {
        if path == "/healthz" || path == "/readyz" {
            continue;
        }
        let filled = path
            .split('/')
            .map(|seg| {
                if seg.starts_with('{') {
                    "01a21617-0000-7000-8000-000000000001"
                } else {
                    seg
                }
            })
            .collect::<Vec<_>>()
            .join("/");
        for method in item.as_object().unwrap().keys() {
            out.push((method.to_uppercase(), filled.clone()));
        }
    }
    for path in ["/mcp", "/mcp/plugin"] {
        for method in ["GET", "POST", "DELETE"] {
            out.push((method.into(), path.into()));
        }
    }
    assert!(out.len() > 40, "{}", out.len());
    out
}

fn assert_locked(home: &Home, reason: &str) {
    for (method, path) in api_routes() {
        let body = (method != "GET").then(|| json!({}));
        let r = home.sock(&method, &path, &[], body.as_ref());
        assert_eq!(
            r.json()["error"]["code"],
            "store_locked",
            "{method} {path}: {}",
            r.text()
        );
        assert_eq!(r.json()["error"]["reason"], reason, "{method} {path}");
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

/// Runs `knoggin daemon`, which must refuse to start: exit code 1 within 20 seconds.
fn refused(home: &Home) -> String {
    let err = home.dir.path().join("refused.err");
    let mut child = home
        .cmd(&["daemon"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&err).unwrap())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the daemon started: {}",
                std::fs::read_to_string(&err).unwrap()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    let stderr = std::fs::read_to_string(&err).unwrap();
    assert_eq!(status.code(), Some(1), "{stderr}");
    stderr
}

fn chmod(path: &Path, mode: u32) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

/// The start-up checks of the settings, folders and key file: each refusal, on the process.
#[test]
fn start_up_refuses_unsafe_settings_folders_and_key_files() {
    // A folder that group or others can write: home, run, data, state, stores, key folder.
    for (dir, mode) in [
        ("", 0o770),
        ("run", 0o777),
        ("data", 0o777),
        ("data/state", 0o770),
        ("data/stores", 0o707),
        ("keys", 0o777),
    ] {
        let home = Home::new();
        let key = home.root().join("keys/master.key");
        let home = home.with("KNOGGIN_LOCAL__MASTER_KEY_FILE", key.to_str().unwrap());
        chmod(&home.root(), 0o700);
        chmod(&home.root().join(dir), mode);
        let e = refused(&home);
        assert!(e.contains("cannot write"), "{dir}: {e}");
    }
    // A key file with any permission bit for group or others.
    let home = Home::new();
    let mut d = home.daemon();
    d.stop();
    let key = home.root().join("master.key");
    for mode in [0o640, 0o604, 0o660] {
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(mode)).unwrap();
        let e = refused(&home);
        assert!(e.contains("no permission for group"), "{mode:o}: {e}");
    }
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    // The key file inside the data folder.
    let other = Home::new();
    let inside = other.data().join("master.key");
    let e = refused(&other.with("KNOGGIN_LOCAL__MASTER_KEY_FILE", inside.to_str().unwrap()));
    assert!(e.contains("inside local.data_dir"), "{e}");
    // A socket path longer than 100 bytes.
    let long = Home::new();
    let root = long.dir.path().join("h".repeat(100));
    let e = refused(&long.with("KNOGGIN_HOME", root.to_str().unwrap()));
    assert!(e.contains("longer than"), "{e}");
    // A bind that is not loopback, in both sign-in modes.
    for mode in ["none", "oauth"] {
        let home = Home::new()
            .with("KNOGGIN_SERVER__BIND", "0.0.0.0:0")
            .with("KNOGGIN_AUTH__MODE", mode);
        assert!(refused(&home).contains("loopback"), "{mode}");
    }
    // Sign-in on: allowed emails required, domains refused, an https public address.
    let oauth = || {
        Home::new()
            .with("KNOGGIN_AUTH__MODE", "oauth")
            .with("KNOGGIN_SERVER__PUBLIC_URL", "http://127.0.0.1:7690")
    };
    assert!(refused(&oauth()).contains("allowed_emails"));
    let e = refused(
        &oauth()
            .with("KNOGGIN_AUTH__ALLOWED_EMAILS", "me@example.com")
            .with("KNOGGIN_AUTH__ALLOWED_EMAIL_DOMAINS", "example.com"),
    );
    assert!(e.contains("allowed_email_domains"), "{e}");
    let e = refused(
        &Home::new()
            .with("KNOGGIN_AUTH__MODE", "oauth")
            .with("KNOGGIN_AUTH__ALLOWED_EMAILS", "me@example.com")
            .with(
                "KNOGGIN_SERVER__PUBLIC_URL",
                "http://localhost.example:7690",
            ),
    );
    assert!(e.contains("public_url"), "{e}");
    // Test mode and a public address that is not loopback.
    let e = refused(&Home::new().with("KNOGGIN_SERVER__PUBLIC_URL", "https://memory.example.com"));
    assert!(e.contains("test mode refuses"), "{e}");
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
    assert!(used < 1.5, "the idle daemon used {used:.2} s of CPU in 5 s");
    assert_eq!(d.stop(), 0);
}

/// `GET /healthz` on the socket; `None` while nothing answers.
fn healthz_now(home: &Home) -> Option<u16> {
    use std::io::{Read, Write};
    let mut s = std::os::unix::net::UnixStream::connect(home.socket()).ok()?;
    s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .ok()?;
    let mut raw = String::new();
    s.read_to_string(&mut raw).ok()?;
    raw.get(9..12)?.parse().ok()
}

#[test]
fn the_lock_file_is_complete_before_the_daemon_answers() {
    let home = Home::new();
    // As soon as `/healthz` answers, the lock file holds the PID and the address.
    for _ in 0..5 {
        let mut child = home
            .cmd(&["daemon"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if healthz_now(&home) == Some(200) {
                let lock = std::fs::read(home.root().join("run/daemon.lock")).unwrap();
                let v: serde_json::Value = serde_json::from_slice(&lock)
                    .unwrap_or_else(|_| panic!("lock file: {:?}", String::from_utf8_lossy(&lock)));
                assert!(!v["address"].as_str().unwrap().is_empty());
                assert_eq!(v["pid"].as_u64().unwrap(), u64::from(child.id()));
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no answer");
            assert!(child.try_wait().unwrap().is_none(), "the daemon exited");
        }
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(child.id().to_string())
            .status();
        assert_eq!(child.wait().unwrap().code(), Some(0));
    }
    // `knoggin start` prints the address it reads from there, never an empty one.
    let _guard = support::Started(&home);
    for _ in 0..3 {
        let o = home.run(&["start", "--json"]);
        assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
        let address = o.json()["address"].as_str().unwrap().to_owned();
        assert!(!address.is_empty());
        assert_eq!(address, home.address());
        assert_eq!(home.run(&["stop"]).code, 0);
    }
}

#[test]
fn an_open_event_stream_does_not_hold_up_a_stop() {
    use std::io::{Read, Write};
    let home = Home::new();
    let mut d = home.daemon();
    let r = home.sock(
        "POST",
        "/v1/memories",
        &[],
        Some(&json!({ "summary": "open" })),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    let mut stream = std::os::unix::net::UnixStream::connect(home.socket()).unwrap();
    stream
        .write_all(b"GET /v1/ui/events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(20)))
        .unwrap();
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).unwrap();
    assert!(buf[..n].starts_with(b"HTTP/1.1 200"));
    // The stream ends as soon as the stop begins, so the drain does not wait 30 seconds.
    let started = std::time::Instant::now();
    assert_eq!(home.run(&["stop"]).code, 0);
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(10),
        "stop took {took:?}"
    );
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert_eq!(d.stop(), 0);
    assert!(!d.stderr().contains("drain_timeout"), "{}", d.stderr());
}

/// A home in sign-in mode against `issuer`, on a fixed port that is also the public URL.
fn oauth_home(issuer: &str) -> (Home, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let home = Home::new()
        .with("KNOGGIN_SERVER__BIND", &format!("127.0.0.1:{port}"))
        .with(
            "KNOGGIN_SERVER__PUBLIC_URL",
            &format!("http://127.0.0.1:{port}"),
        )
        .with("KNOGGIN_AUTH__MODE", "oauth")
        .with("KNOGGIN_AUTH__ISSUER", issuer)
        .with("KNOGGIN_AUTH__WEB_CLIENT_ID", "web-client")
        .with("KNOGGIN_AUTH__WEB_CLIENT_SECRET", "test-secret")
        .with("KNOGGIN_AUTH__ALLOWED_EMAILS", "alice@test.example");
    (home, port)
}

/// Polls `f` for up to 10 seconds.
fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !f() {
        assert!(std::time::Instant::now() < deadline, "{what}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Discovery runs in the background: the daemon serves the socket at once whatever the
/// identity provider does, and the sign-in routes answer `unavailable` until it works.
#[test]
fn sign_in_discovery_at_start() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let issuer = rt
        .block_on(knoggin_http::testing::issuer::TestIssuer::start(true))
        .unwrap();
    // A discovery document that names another issuer: logged with both values, and
    // sign-in stays off.
    let (home, _) = oauth_home(&format!("{}/", issuer.url()));
    let mut d = home.daemon();
    assert_eq!(home.sock("GET", "/v1/stores", &[], None).status, 200);
    eventually("the mismatch is logged", || {
        let e = d.stderr();
        e.contains("is not auth.issuer") && e.contains(&format!("{:?}", issuer.url()))
    });
    assert_eq!(home.tcp("GET", "/auth/login", &[], None).status, 503);
    assert_eq!(d.stop(), 0);
    // An unreachable provider: the daemon serves the socket and answers `unavailable` on
    // the sign-in routes.
    let gone = format!("http://127.0.0.1:{}", {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    });
    let (home, _) = oauth_home(&gone);
    let mut d = home.daemon();
    assert_eq!(home.sock("GET", "/v1/stores", &[], None).status, 200);
    assert_eq!(home.tcp("GET", "/auth/login", &[], None).status, 503);
    assert_eq!(d.stop(), 0);
    // A provider that takes the connection and never answers (each discovery fetch waits
    // 5 seconds for it) does not delay the start.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let (home, _) = oauth_home(&format!("http://{}", silent.local_addr().unwrap()));
    let started = std::time::Instant::now();
    let mut d = home.daemon();
    assert_eq!(home.sock("GET", "/v1/stores", &[], None).status, 200);
    let took = started.elapsed();
    assert!(
        took < std::time::Duration::from_secs(5),
        "start took {took:?}"
    );
    assert_eq!(home.tcp("GET", "/auth/login", &[], None).status, 503);
    assert_eq!(d.stop(), 0);
    drop(silent);
    // A provider that answers: the sign-in route sends the browser to it.
    let (home, _) = oauth_home(issuer.url());
    let mut d = home.daemon();
    eventually("discovery works", || {
        home.tcp("GET", "/auth/login", &[], None).status == 303
    });
    let r = home.tcp("GET", "/auth/login", &[], None);
    assert_eq!(r.status, 303, "{}", r.text());
    assert!(r.headers.contains(&format!("{}/authorize?", issuer.url())));
    assert_eq!(d.stop(), 0);
}

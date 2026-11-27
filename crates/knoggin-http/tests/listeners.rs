// SPDX-License-Identifier: AGPL-3.0-only
//! The daemon's listeners over real sockets: the Unix socket and its modes, the path limit,
//! a stale socket file, the loopback pair, a TCP request without a credential, and the
//! first-byte timeout.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use knoggin_http::edge::Arrival;
use knoggin_http::edge::listen::{ListenConfig, serve_tcp, serve_unix};
use knoggin_http::local::{bind_loopback, bind_socket, check_private_dir, prepare_run_dir};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};
use tokio::sync::oneshot;

use support::edge::EdgeEnv;

/// A short home under `/tmp`: socket paths are limited to about 104 bytes.
fn short_home() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("kg")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    mut s: S,
    request: &str,
) -> String {
    s.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

fn get(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n")
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn the_socket_serves_its_owner_with_the_stated_modes() {
    let env = EdgeEnv::new().await;
    let home = short_home();
    let run = prepare_run_dir(home.path()).unwrap();
    assert_eq!(mode(&run), 0o700);
    // A stale socket file from a crashed daemon is replaced.
    std::fs::write(run.join("daemon.sock"), b"stale").unwrap();
    let listener = bind_socket(&run).unwrap();
    assert_eq!(mode(&run.join("daemon.sock")), 0o600);
    let (stop, stopped) = oneshot::channel::<()>();
    let server = tokio::spawn(serve_unix(
        listener,
        env.app.clone(),
        ListenConfig::DEFAULT,
        async {
            let _ = stopped.await;
        },
    ));
    let stream = UnixStream::connect(run.join("daemon.sock")).await.unwrap();
    let answer = exchange(stream, &get("/v1/session", "localhost")).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    assert!(answer.contains("\"kind\":\"local\""), "{answer}");
    stop.send(()).unwrap();
    server.await.unwrap();
}

#[test]
fn a_socket_path_that_is_too_long_is_refused() {
    let home = short_home();
    let deep = home.path().join("x".repeat(90));
    std::fs::create_dir(&deep).unwrap();
    let e = bind_socket(&deep).unwrap_err();
    assert!(e.to_string().contains("KNOGGIN_HOME"), "{e}");
}

#[test]
fn a_home_that_others_can_write_is_refused() {
    let home = short_home();
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(prepare_run_dir(home.path()).is_err());
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o720)).unwrap();
    assert!(check_private_dir(home.path()).is_err());
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let run = prepare_run_dir(home.path()).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o770)).unwrap();
    assert!(prepare_run_dir(home.path()).is_err());
}

#[tokio::test]
async fn a_tcp_request_without_a_credential_is_unauthenticated() {
    let env = EdgeEnv::new().await;
    let listeners = bind_loopback(0).unwrap();
    let port = listeners[0].local_addr().unwrap().port();
    let mut servers = Vec::new();
    let (stop, _) = tokio::sync::broadcast::channel::<()>(1);
    for l in listeners {
        let mut rx = stop.subscribe();
        servers.push(tokio::spawn(serve_tcp(
            l,
            env.app.clone(),
            Arrival::Fixed { port },
            ListenConfig::DEFAULT,
            async move {
                let _ = rx.recv().await;
            },
        )));
    }
    let host = format!("127.0.0.1:{port}");
    let stream = TcpStream::connect(&host).await.unwrap();
    let answer = exchange(stream, &get("/v1/stores", &host)).await;
    assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
    assert!(answer.contains("\"unauthenticated\""));
    let stream = TcpStream::connect(&host).await.unwrap();
    let answer = exchange(stream, &get("/healthz", &host)).await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    if servers.len() == 2 {
        let v6 = format!("[::1]:{port}");
        let stream = TcpStream::connect(&v6).await.unwrap();
        let answer = exchange(stream, &get("/v1/stores", &v6)).await;
        assert!(answer.starts_with("HTTP/1.1 401"), "{answer}");
    }
    stop.send(()).unwrap();
    for s in servers {
        s.await.unwrap();
    }
}

#[test]
fn another_program_on_the_ipv6_loopback_port_stops_the_bind() {
    let Ok(taken) = std::net::TcpListener::bind("[::1]:0") else {
        eprintln!("skipped: this machine has no IPv6 loopback");
        return;
    };
    let port = taken.local_addr().unwrap().port();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _guard = runtime.enter();
    match bind_loopback(port) {
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse),
        Ok(_) => panic!("bound a port another program holds on [::1]"),
    }
    // Port 0 finds one that is free on both.
    let both = bind_loopback(0).unwrap();
    assert_eq!(both.len(), 2);
    assert_eq!(
        both[0].local_addr().unwrap().port(),
        both[1].local_addr().unwrap().port()
    );
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_sends_nothing_is_closed() {
    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
    let config = ListenConfig {
        header_read_timeout: Duration::from_secs(30),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let home = short_home();
    let run = prepare_run_dir(home.path()).unwrap();
    let socket = bind_socket(&run).unwrap();
    let (stop, _) = tokio::sync::broadcast::channel::<()>(1);
    let (mut a, mut b) = (stop.subscribe(), stop.subscribe());
    let tcp = tokio::spawn(serve_tcp(
        listener,
        app.clone(),
        Arrival::Fixed { port: addr.port() },
        config,
        async move {
            let _ = a.recv().await;
        },
    ));
    let unix = tokio::spawn(serve_unix(socket, app, config, async move {
        let _ = b.recv().await;
    }));
    let started = tokio::time::Instant::now();
    let mut idle = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(
        idle.read(&mut buf).await.unwrap(),
        0,
        "the server closed the silent connection"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    let started = tokio::time::Instant::now();
    let mut idle = UnixStream::connect(run.join("daemon.sock")).await.unwrap();
    assert_eq!(
        idle.read(&mut buf).await.unwrap(),
        0,
        "the socket closed the silent connection"
    );
    assert!(started.elapsed() >= Duration::from_secs(30));
    // A connection that does send is served.
    let answer = exchange(
        UnixStream::connect(run.join("daemon.sock")).await.unwrap(),
        &get("/healthz", "x"),
    )
    .await;
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    stop.send(()).unwrap();
    tcp.await.unwrap();
    unix.await.unwrap();
}

// SPDX-License-Identifier: AGPL-3.0-only
//! The sign-in tests that name the router: an access token's reach on a server, the browser checks
//! on the router, and the router's start-up guards, against the test issuer, with routers started
//! through `router::run_router` and real containers behind them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod fleet_support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fleet_support::{Deployment, Logs, json, save};
use knoggin_http::edge::AuthMode;
use reqwest::Method;

#[tokio::test]
async fn an_access_token_reaches_memories_and_the_session_route_only() {
    let d = Deployment::new(true).await;
    let r = d.router("0.1.0").await;
    // An admin by email still gets no control or admin route with a token.
    let token = d.token(&r.url, "admin");
    assert_eq!(save(&r, Some(&token), "through a token").await.0, 201);
    let (status, v) = json(
        r.req(Method::GET, "/v1/session")
            .bearer_auth(&token)
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, v["signedIn"].as_bool()), (200, Some(true)));
    assert_eq!(v["credential"]["kind"], "oauth");
    for (m, path) in [
        (Method::GET, "/v1/stores"),
        (Method::POST, "/v1/stores"),
        (Method::GET, "/v1/admin/status"),
        (Method::GET, "/v1/users"),
        (Method::DELETE, "/v1/account"),
    ] {
        let mut b = r.req(m.clone(), path).bearer_auth(&token);
        if m == Method::POST {
            b = b.json(&serde_json::json!({ "name": "x" }));
        }
        let (status, v) = json(b.send().await.unwrap()).await;
        assert_eq!(
            (status, v["error"]["code"].as_str()),
            (403, Some("permission_denied")),
            "{m} {path}"
        );
    }
    // Without a token: the challenge, and no container is woken.
    let launched = d.launcher.handles().len();
    let answer = r.req(Method::GET, "/v1/memories").send().await.unwrap();
    assert_eq!(answer.status(), 401);
    assert!(answer.headers().contains_key("www-authenticate"));
    assert_eq!(d.launcher.handles().len(), launched);
}

#[tokio::test]
async fn the_browser_checks_hold_on_the_router() {
    let d = Deployment::new(true).await;
    let r = d.router("0.1.0").await;
    let token = d.token(&r.url, "carol");
    let get = || r.req(Method::GET, "/v1/memories").bearer_auth(&token);
    let code = |resp: reqwest::Response| async move { json(resp).await.1["error"]["code"].clone() };
    assert_eq!(
        code(
            get()
                .header("origin", "https://evil.example")
                .send()
                .await
                .unwrap()
        )
        .await,
        "permission_denied"
    );
    assert_eq!(
        code(
            get()
                .header("sec-fetch-site", "cross-site")
                .send()
                .await
                .unwrap()
        )
        .await,
        "permission_denied"
    );
    assert_eq!(
        code(get().header("host", "evil.example").send().await.unwrap()).await,
        "permission_denied"
    );
    for name in [
        "knoggin-permissions",
        "Knoggin-User-Id",
        "KNOGGIN-STORE-ID",
        "knoggin-credential-kind",
    ] {
        assert_eq!(
            code(get().header(name, "x").send().await.unwrap()).await,
            "invalid_argument",
            "{name}"
        );
    }
    let twice = get()
        .header("knoggin-user-id", "a")
        .header("knoggin-user-id", "b")
        .send()
        .await
        .unwrap();
    assert_eq!(code(twice).await, "invalid_argument");
    let options = r.req(Method::OPTIONS, "/v1/memories").send().await.unwrap();
    assert_eq!(options.status(), 403);
    let form = r
        .req(Method::POST, "/v1/memories")
        .bearer_auth(&token)
        .header("content-type", "application/x-www-form-urlencoded")
        .body("summary=x")
        .send()
        .await
        .unwrap();
    assert_eq!(code(form).await, "invalid_argument");
    // Health checks answer whatever the Host says.
    let health = r
        .req(Method::GET, "/healthz")
        .header("host", "10.9.9.9")
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    // Every answer carries the browser headers.
    let ok = get().send().await.unwrap();
    assert_eq!(ok.status(), 200);
    assert_eq!(ok.headers()["x-content-type-options"], "nosniff");
    assert_eq!(ok.headers()["x-frame-options"], "DENY");
    assert_eq!(ok.headers()["cache-control"], "no-store");
}

#[tokio::test]
async fn the_routers_start_up_guards() {
    let d = Deployment::new(true).await;
    let r = d.router("0.1.0").await;
    assert_eq!(save(&r, Some(&d.token(&r.url, "dave")), "x").await.0, 201);
    let logs = Logs::capture();
    // Each refusal exits 1 and logs the guard that refused.
    let refused = |result: Result<_, i32>, why: &str| {
        assert_eq!(result.err(), Some(1), "{why}");
        let log = logs.take();
        assert!(log.contains("the router refuses to start"), "{log}");
        assert!(log.contains(why), "expected {why:?} in {log}");
    };
    // Sign-in off over a database that holds another user than the local one.
    let none = d
        .router_with("0.1.0", |t| {
            t.edge.auth_mode = AuthMode::None;
            t.edge.oauth = None;
        })
        .await;
    refused(
        none,
        "sign-in is off, but the state database holds 1 users besides the local one",
    );
    // An `http` public address outside test mode, in both modes.
    for oauth in [true, false] {
        let plain = d
            .router_with("0.1.0", |t| {
                t.edge.test_mode = false;
                if !oauth {
                    t.edge.auth_mode = AuthMode::None;
                    t.edge.oauth = None;
                }
            })
            .await;
        refused(plain, "server.public_url must be https");
    }
    // One host: no trusted proxy, or one inside the user network.
    let bare = d
        .router_with("0.1.0", |t| t.settings.trusted_proxies.clear())
        .await;
    refused(bare, "on one host server.trusted_proxies must name");
    let overlap = d
        .router_with("0.1.0", |t| {
            t.settings.trusted_proxies = vec!["127.0.0.9/32".parse().unwrap()];
        })
        .await;
    refused(
        overlap,
        "server.trusted_proxies must not overlap containers.user_net_cidr",
    );
    // Both admin emails and an email claim.
    let both = d
        .router_with("0.1.0", |t| {
            t.edge.oauth.as_mut().unwrap().email_claim = Some("email".into());
        })
        .await;
    refused(
        both,
        "auth.admin_emails and auth.email_claim cannot both be set",
    );
}

#[tokio::test]
async fn a_router_with_sign_in_off_warns_at_every_start() {
    let d = Deployment::new(false).await;
    let logs = Logs::capture();
    let _r = d.router("0.1.0").await;
    let log = logs.take();
    assert!(
        log.contains("WARN")
            && log.contains("anyone who can reach this server can read and write its memories"),
        "{log}"
    );
}

#[tokio::test]
async fn a_router_that_cannot_reach_its_provider_exits_1_after_the_limit_and_never_listens() {
    // The limit is 60 seconds; this test shortens it to 2.
    const LIMIT: Duration = Duration::from_secs(2);
    let d = Deployment::new(true).await;
    let logs = Logs::capture();
    let public = Arc::new(Mutex::new(None));
    let seen = public.clone();
    let started = Instant::now();
    let probe = async {
        // While discovery is tried, nothing listens yet: no connection is accepted (it is
        // refused, or on macOS left unanswered).
        let mut tries = 0;
        while started.elapsed() < LIMIT - Duration::from_millis(500) {
            let addr = *seen.lock().unwrap();
            if let Some(addr) = addr {
                let connect = tokio::net::TcpStream::connect(addr);
                let accepted = tokio::time::timeout(Duration::from_millis(200), connect).await;
                assert!(
                    !matches!(accepted, Ok(Ok(_))),
                    "the public listener accepted before the start-up steps were done"
                );
                tries += 1;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        tries
    };
    let (result, tries) = tokio::join!(
        d.router_with("0.1.0", |t| {
            t.edge.oauth.as_mut().unwrap().issuer = "http://127.0.0.1:1".into();
            t.timing.discovery = LIMIT;
            *public.lock().unwrap() = Some(t.public);
        }),
        probe
    );
    assert_eq!(result.err(), Some(1));
    assert!(tries >= 3, "{tries}");
    let took = started.elapsed();
    assert!(
        took >= LIMIT && took < LIMIT + Duration::from_secs(3),
        "{took:?}"
    );
    let log = logs.take();
    assert!(
        log.contains("the identity provider could not be reached in 2 seconds"),
        "{log}"
    );
}

#[tokio::test]
async fn a_router_with_sign_in_off_starts_and_serves_the_one_user() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    assert_eq!(save(&r, None, "open").await.0, 201);
    let (status, v) = json(r.req(Method::GET, "/v1/session").send().await.unwrap()).await;
    assert_eq!((status, v["auth"].as_str()), (200, Some("none")));
}

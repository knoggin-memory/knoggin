// SPDX-License-Identifier: AGPL-3.0-only
//! The sign-in tests that name the router (tab "Sign-in and credentials", Tests that must
//! exist): an access token's reach on a server, the browser checks on the router, and
//! the router's start-up guards, against the test issuer, with routers started through
//! `router::run_router` and real containers behind them.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod fleet_support;

use fleet_support::{Deployment, json, save};
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
    // Sign-in off over a database that holds another user than the local one.
    let none = d
        .router_with("0.1.0", |t| {
            t.edge.auth_mode = AuthMode::None;
            t.edge.oauth = None;
        })
        .await;
    assert_eq!(none.err(), Some(1));
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
        assert_eq!(plain.err(), Some(1));
    }
    // One host: no trusted proxy, or one inside the user network.
    let bare = d
        .router_with("0.1.0", |t| t.settings.trusted_proxies.clear())
        .await;
    assert_eq!(bare.err(), Some(1));
    let overlap = d
        .router_with("0.1.0", |t| {
            t.settings.trusted_proxies = vec!["127.0.0.9/32".parse().unwrap()];
        })
        .await;
    assert_eq!(overlap.err(), Some(1));
    // Both admin emails and an email claim.
    let both = d
        .router_with("0.1.0", |t| {
            t.edge.oauth.as_mut().unwrap().email_claim = Some("email".into());
        })
        .await;
    assert_eq!(both.err(), Some(1));
}

#[tokio::test]
async fn a_router_with_sign_in_off_starts_and_serves_the_one_user() {
    let d = Deployment::new(false).await;
    let r = d.router("0.1.0").await;
    assert_eq!(save(&r, None, "open").await.0, 201);
    let (status, v) = json(r.req(Method::GET, "/v1/session").send().await.unwrap()).await;
    assert_eq!((status, v["auth"].as_str()), (200, Some("none")));
}

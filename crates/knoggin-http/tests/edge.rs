// SPDX-License-Identifier: AGPL-3.0-only
//! The edge layer of the local daemon in one process: the credential check with each
//! credential kind, the control routes, the browser checks, rate limits and the mode
//! routes (tabs "Sign-in and credentials" and "REST API", Tests that must exist).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::sync::Arc;

use http::{Method, StatusCode};
use knoggin_core::StoreId;
use knoggin_core::time::MILLIS_PER_DAY;
use knoggin_http::edge::{EdgeParts, EdgeState, edge_layer};
use knoggin_http::error::Code;
use knoggin_http::local::{LocalSessions, erase_step, locked_router};
use knoggin_http::{LockReason, store_layer};
use serde_json::{Value, json};
use uuid::Uuid;

use support::edge::{EdgeEnv, PORT, WEB, fixed, tight, web};

const SAVE: &str = "/v1/memories";

fn save_body(summary: &str) -> Option<Value> {
    Some(json!({ "summary": summary }))
}

fn uuid(v: &Value) -> Uuid {
    v.as_str().unwrap().parse().unwrap()
}

#[tokio::test]
async fn the_socket_needs_no_credential_and_reaches_everything() {
    let env = EdgeEnv::new().await;
    let r = env.local(Method::GET, "/v1/session", None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["signedIn"], true);
    assert_eq!(r.json["mode"], "local");
    assert_eq!(r.json["auth"], "none");
    assert_eq!(r.json["credential"]["kind"], "local");
    assert_eq!(r.json["user"]["isAdmin"], true);
    assert_eq!(r.json["user"]["email"], Value::Null);
    assert_eq!(r.json["publicUrl"], format!("http://127.0.0.1:{PORT}"));
    assert_eq!(r.json["mcpUrl"], format!("http://127.0.0.1:{PORT}/mcp"));
    assert_eq!(r.json["personalStoreId"], env.personal.to_string());
    assert_eq!(r.json["webSignIn"], false);
    assert_eq!(r.json["idleSeconds"], Value::Null);
    assert_eq!(
        r.json["docsUrl"],
        "https://github.com/knoggin-memory/knoggin/blob/v0.1.0/docs"
    );
    let saved = env
        .local(Method::POST, SAVE, save_body("over the socket"))
        .await;
    assert_eq!(saved.status, StatusCode::CREATED, "{}", saved.json);
    let id = saved.json["id"].as_str().unwrap();
    let got = env
        .local(Method::GET, &format!("/v1/memories/{id}"), None)
        .await;
    assert_eq!(got.json["savedBy"], "local");
    for uri in ["/v1/stores", "/v1/admin/status", "/v1/users"] {
        let r = env.local(Method::GET, uri, None).await;
        assert_eq!(r.status, StatusCode::OK, "{uri}: {}", r.json);
    }
    // A request ID on every answer, version 7.
    let rid: Uuid = r.headers["knoggin-request-id"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(rid.get_version_num(), 7);
    assert_eq!(r.headers["cache-control"], "no-store");
    assert_eq!(r.headers["x-content-type-options"], "nosniff");
    assert_eq!(r.headers["x-frame-options"], "DENY");
    assert_eq!(r.headers["referrer-policy"], "no-referrer");
}

#[tokio::test]
async fn tcp_without_a_credential_is_unauthenticated() {
    let env = EdgeEnv::new().await;
    for (method, uri) in [
        (Method::POST, SAVE),
        (Method::GET, "/v1/memories"),
        (Method::GET, "/v1/stores"),
        (Method::GET, "/v1/admin/status"),
        (Method::DELETE, "/v1/account"),
        (Method::POST, "/v1/local/browser-code"),
    ] {
        for arrival in [fixed(), web()] {
            let r = env.call(arrival, method.clone(), uri, None, &[]).await;
            assert_eq!(r.error(), Code::Unauthenticated, "{method} {uri}");
        }
    }
    let r = env
        .call(fixed(), Method::GET, "/v1/session", None, &[])
        .await;
    assert_eq!(r.json["signedIn"], false);
    assert!(r.json.get("signedOutReason").is_none());
    assert_eq!(r.json["user"], Value::Null);
    assert_eq!(r.json["credential"], Value::Null);
    let health = env.call(fixed(), Method::GET, "/healthz", None, &[]).await;
    assert_eq!(health.json, json!({ "status": "ok", "version": "0.1.0" }));
    let ready = env.call(fixed(), Method::GET, "/readyz", None, &[]).await;
    assert_eq!(ready.json, json!({ "status": "ready" }));
    env.edge.set_ready(false);
    let ready = env.call(fixed(), Method::GET, "/readyz", None, &[]).await;
    assert_eq!(
        (ready.status, ready.json.clone()),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "status": "starting" })
        )
    );
    let doc = env
        .call(fixed(), Method::GET, "/v1/openapi.json", None, &[])
        .await;
    assert!(doc.json["openapi"].as_str().unwrap().starts_with("3.1"));
}

#[tokio::test]
async fn api_key_lifecycle() {
    let env = EdgeEnv::new().await;
    let (secret, key) = env
        .key(env.personal, json!(["memory:read", "memory:write"]), 90)
        .await;
    let random = secret.strip_prefix("knoggin_live_").unwrap();
    assert_eq!(random.len(), 43);
    assert_eq!(key["prefix"], &random[..8]);
    assert_eq!(key["lastUsedOn"], Value::Null);
    let id = uuid(&key["id"]);
    // Listed without its secret.
    let list = env
        .local(
            Method::GET,
            &format!("/v1/stores/{}/keys", env.personal),
            None,
        )
        .await;
    assert_eq!(list.json["items"][0]["id"], key["id"]);
    assert!(!String::from_utf8_lossy(&list.bytes).contains(random));
    // Valid on the fixed listener: saves, with its own `savedBy`, and marks the day used.
    let r = env
        .bearer(fixed(), &secret, Method::POST, SAVE, save_body("by key"))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let got = env
        .bearer(
            fixed(),
            &secret,
            Method::GET,
            &format!("/v1/memories/{}", r.json["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(got.json["savedBy"], format!("api_key:{id}"));
    assert!(got.headers.get("knoggin-key-expires").is_none());
    let list = env
        .local(
            Method::GET,
            &format!("/v1/stores/{}/keys", env.personal),
            None,
        )
        .await;
    assert!(list.json["items"][0]["lastUsedOn"].is_string());
    // The session route with a key.
    let s = env
        .bearer(fixed(), &secret, Method::GET, "/v1/session", None)
        .await;
    assert_eq!(s.json["credential"]["kind"], "api_key");
    assert_eq!(s.json["credential"]["storeId"], env.personal.to_string());
    assert_eq!(s.json["user"], json!({ "id": env.user.to_string() }));
    // Wrong shape and unknown keys.
    for bad in [
        "knoggin_live_short".to_owned(),
        format!("knoggin_live_{}", "A".repeat(43)),
        format!("knoggin_live_{}!", &random[..42]),
        "Basic".to_owned(),
    ] {
        let r = env
            .bearer(fixed(), &bad, Method::GET, "/v1/memories", None)
            .await;
        assert_eq!(r.error(), Code::Unauthenticated, "{bad}");
    }
    let r = env
        .call(
            fixed(),
            Method::GET,
            "/v1/memories",
            None,
            &[("authorization", "Basic abc")],
        )
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    // Rotation: the old key fails, the new one works and lives 90 days.
    let rotated = env
        .local(
            Method::POST,
            &format!("/v1/stores/{}/keys/{id}/rotate", env.personal),
            None,
        )
        .await;
    assert_eq!(rotated.status, StatusCode::CREATED, "{}", rotated.json);
    let new_secret = rotated.json["secret"].as_str().unwrap().to_owned();
    assert_eq!(rotated.json["key"]["name"], "laptop");
    let created =
        knoggin_core::Timestamp::parse_rfc3339(rotated.json["key"]["createdAt"].as_str().unwrap())
            .unwrap();
    let expires =
        knoggin_core::Timestamp::parse_rfc3339(rotated.json["key"]["expiresAt"].as_str().unwrap())
            .unwrap();
    assert_eq!(expires.millis() - created.millis(), 90 * MILLIS_PER_DAY);
    let old = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(old.error(), Code::Unauthenticated);
    let again = env
        .local(
            Method::POST,
            &format!("/v1/stores/{}/keys/{id}/rotate", env.personal),
            None,
        )
        .await;
    assert_eq!(again.error(), Code::Conflict, "already revoked");
    let new = env
        .bearer(fixed(), &new_secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(new.status, StatusCode::OK);
    // Revoked: refused on its next request.
    let new_id = rotated.json["key"]["id"].as_str().unwrap();
    let revoke = env
        .local(
            Method::DELETE,
            &format!("/v1/stores/{}/keys/{new_id}", env.personal),
            None,
        )
        .await;
    assert_eq!(revoke.status, StatusCode::NO_CONTENT);
    let r = env
        .bearer(fixed(), &new_secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    let s = env
        .bearer(fixed(), &new_secret, Method::GET, "/v1/session", None)
        .await;
    assert_eq!(
        (
            s.json["signedIn"].clone(),
            s.json["signedOutReason"].clone()
        ),
        (json!(false), json!("expired"))
    );
}

#[tokio::test]
async fn key_expiry_and_the_warning_header() {
    let env = EdgeEnv::new().await;
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 30).await;
    env.clock.advance(15 * MILLIS_PER_DAY);
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert!(r.headers.get("knoggin-key-expires").is_none());
    env.clock.advance(2 * MILLIS_PER_DAY);
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    let header = r.headers["knoggin-key-expires"].to_str().unwrap();
    knoggin_core::Timestamp::parse_rfc3339(header).unwrap();
    // Every answer to a request with that key carries it: the session route, a control
    // route that refuses the key, a store route that refuses the request, and MCP.
    let ping = json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" });
    let personal = env.personal.to_string();
    for (method, uri, body, extra) in [
        (Method::GET, "/v1/session", None, None),
        (Method::GET, "/v1/stores", None, None),
        (
            Method::GET,
            "/v1/memories",
            None,
            Some(("knoggin-store", personal.as_str())),
        ),
        (Method::POST, "/mcp", Some(ping), None),
    ] {
        let auth = format!("Bearer {secret}");
        let mut headers = vec![("authorization", auth.as_str())];
        headers.extend(extra);
        let r = env.call(fixed(), method, uri, body, &headers).await;
        assert_eq!(
            r.headers
                .get("knoggin-key-expires")
                .map(|v| v.to_str().unwrap()),
            Some(header),
            "{uri} {}",
            r.json
        );
    }
    env.clock.advance(14 * MILLIS_PER_DAY);
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(r.error(), Code::Unauthenticated, "expired");
    // Bounds of the request.
    for body in [
        json!({ "name": "x", "expiresInDays": 7 }),
        json!({ "name": "", }),
        json!({ "name": "x".repeat(65) }),
        json!({ "name": "x", "permissions": ["memory:write"] }),
        json!({ "name": "x", "unknown": 1 }),
    ] {
        let r = env
            .local(
                Method::POST,
                &format!("/v1/stores/{}/keys", env.personal),
                Some(body.clone()),
            )
            .await;
        assert_eq!(r.error(), Code::InvalidArgument, "{body}");
    }
}

#[tokio::test]
async fn a_read_only_key_cannot_write_or_gain_write_through_headers() {
    let env = EdgeEnv::new().await;
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 90).await;
    let r = env
        .bearer(fixed(), &secret, Method::POST, SAVE, save_body("nope"))
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    let auth = format!("Bearer {secret}");
    for name in [
        "knoggin-permissions",
        "Knoggin-Permissions",
        "KNOGGIN-USER-ID",
        "knoggin-store-id",
        "Knoggin-Credential-Kind",
        "knoggin-request-id",
    ] {
        for arrival in [fixed(), knoggin_http::edge::Arrival::Socket] {
            let r = env
                .call(
                    arrival,
                    Method::POST,
                    SAVE,
                    save_body("nope"),
                    &[("authorization", &auth), (name, "memory:read,memory:write")],
                )
                .await;
            assert_eq!(r.error(), Code::InvalidArgument, "{name}");
        }
    }
    // Sent twice.
    let mut request = http::Request::builder()
        .method(Method::GET)
        .uri("/v1/memories")
        .header("host", format!("127.0.0.1:{PORT}"))
        .header("authorization", &auth)
        .header("knoggin-permissions", "memory:read")
        .header("knoggin-permissions", "memory:read,memory:write")
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(fixed());
    assert_eq!(env.send(request).await.error(), Code::InvalidArgument);
}

#[tokio::test]
async fn a_key_reaches_its_own_store_only_on_every_listener() {
    let env = EdgeEnv::new().await;
    let app = env.app("Work").await;
    let (secret, _) = env
        .key(app, json!(["memory:read", "memory:write"]), 90)
        .await;
    let auth = format!("Bearer {secret}");
    // Over the socket a key still names its own store.
    let r = env
        .call(
            knoggin_http::edge::Arrival::Socket,
            Method::POST,
            SAVE,
            save_body("in work"),
            &[("authorization", &auth)],
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let mine = env.local(Method::GET, "/v1/memories", None).await;
    assert_eq!(
        mine.json["items"].as_array().unwrap().len(),
        0,
        "the personal store is untouched"
    );
    let theirs = env
        .call(
            knoggin_http::edge::Arrival::Socket,
            Method::GET,
            "/v1/memories",
            None,
            &[("knoggin-store", &app.to_string())],
        )
        .await;
    assert_eq!(theirs.json["items"].as_array().unwrap().len(), 1);
    // A key with Knoggin-Store is refused, even naming its own store.
    for store in [app, env.personal] {
        let r = env
            .call(
                fixed(),
                Method::GET,
                "/v1/memories",
                None,
                &[
                    ("authorization", &auth),
                    ("knoggin-store", &store.to_string()),
                ],
            )
            .await;
        assert_eq!(r.error(), Code::InvalidArgument);
    }
    // Control routes with a key: permission_denied, the session route aside.
    for (method, uri) in control_routes(&env, app) {
        let r = env
            .call(
                fixed(),
                method.clone(),
                &uri,
                Some(json!({})),
                &[("authorization", &auth)],
            )
            .await;
        assert_eq!(r.error(), Code::PermissionDenied, "{method} {uri}");
    }
}

fn control_routes(env: &EdgeEnv, app: StoreId) -> Vec<(Method, String)> {
    let key = Uuid::now_v7();
    let user = env.user;
    vec![
        (Method::GET, "/v1/stores".into()),
        (Method::POST, "/v1/stores".into()),
        (Method::GET, format!("/v1/stores/{app}")),
        (Method::PATCH, format!("/v1/stores/{app}")),
        (Method::DELETE, format!("/v1/stores/{app}")),
        (Method::POST, format!("/v1/stores/{app}/undelete")),
        (Method::GET, format!("/v1/stores/{app}/status")),
        (Method::GET, format!("/v1/stores/{app}/keys")),
        (Method::POST, format!("/v1/stores/{app}/keys")),
        (Method::POST, format!("/v1/stores/{app}/keys/{key}/rotate")),
        (Method::DELETE, format!("/v1/stores/{app}/keys/{key}")),
        (Method::DELETE, "/v1/account".into()),
        (Method::POST, "/v1/account/undelete".into()),
        (Method::GET, "/v1/users".into()),
        (Method::GET, format!("/v1/users/{user}/stores")),
        (Method::DELETE, format!("/v1/users/{user}")),
        (Method::POST, format!("/v1/users/{user}/undelete")),
        (Method::GET, "/v1/admin/status".into()),
        (Method::GET, format!("/v1/admin/stores/{app}/snapshots")),
        (Method::POST, "/v1/local/browser-code".into()),
        (Method::POST, "/auth/logout".into()),
    ]
}

#[tokio::test]
async fn a_deleted_store_stops_its_keys_until_the_undo() {
    let env = EdgeEnv::new().await;
    let app = env.app("Work").await;
    let (secret, _) = env
        .key(app, json!(["memory:read", "memory:write"]), 90)
        .await;
    let ok = env
        .bearer(fixed(), &secret, Method::POST, SAVE, save_body("before"))
        .await;
    assert_eq!(ok.status, StatusCode::CREATED);
    let deleted = env
        .local(Method::DELETE, &format!("/v1/stores/{app}"), None)
        .await;
    assert_eq!(deleted.json["status"], "deleted", "{}", deleted.json);
    assert!(deleted.json["deleteAfter"].is_string());
    assert!(!env.host.is_open(app), "dropped from memory at once");
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(r.error(), Code::StoreDeleting);
    let r = env
        .local(
            Method::POST,
            &format!("/v1/stores/{app}/keys"),
            Some(json!({ "name": "late" })),
        )
        .await;
    assert_eq!(
        r.error(),
        Code::StoreDeleting,
        "creation on a deleted store"
    );
    // The key list and a revocation still work on a deleted store.
    let list = env
        .local(Method::GET, &format!("/v1/stores/{app}/keys"), None)
        .await;
    assert_eq!(list.status, StatusCode::OK);
    let undone = env
        .local(Method::POST, &format!("/v1/stores/{app}/undelete"), None)
        .await;
    assert_eq!(undone.json["status"], "active", "{}", undone.json);
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "works again after the undo");
    let again = env
        .local(Method::POST, &format!("/v1/stores/{app}/undelete"), None)
        .await;
    assert_eq!(again.error(), Code::Conflict);
    // After the undo period an undo is refused.
    env.local(Method::DELETE, &format!("/v1/stores/{app}"), None)
        .await;
    env.clock.advance(7 * MILLIS_PER_DAY + 1);
    let late = env
        .local(Method::POST, &format!("/v1/stores/{app}/undelete"), None)
        .await;
    assert_eq!(late.error(), Code::Conflict);
}

#[tokio::test]
async fn stores_create_list_rename_and_refuse() {
    let env = EdgeEnv::new().await;
    env.app("zeta").await;
    env.app("Alpha").await;
    let list = env.local(Method::GET, "/v1/stores", None).await;
    let names: Vec<&str> = list.json["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Personal", "Alpha", "zeta"]);
    assert_eq!(list.json["items"][0]["kind"], "personal");
    let dup = env
        .local(
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "  ALPHA " })),
        )
        .await;
    assert_eq!(dup.error(), Code::AlreadyExists);
    assert_eq!(
        dup.json["error"]["message"],
        "An app with this name already exists."
    );
    let dup = env
        .local(
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": "personal" })),
        )
        .await;
    assert_eq!(dup.error(), Code::AlreadyExists);
    for bad in [
        json!({ "name": "  " }),
        json!({ "name": "a\u{7}b" }),
        json!({ "name": "x".repeat(65) }),
        json!({ "name": "x", "kind": "app" }),
    ] {
        let r = env
            .local(Method::POST, "/v1/stores", Some(bad.clone()))
            .await;
        assert_eq!(r.error(), Code::InvalidArgument, "{bad}");
    }
    let made = env
        .local(
            Method::POST,
            "/v1/stores",
            Some(json!({ "name": " Notes " })),
        )
        .await;
    assert_eq!(made.json["name"], "Notes");
    assert_eq!(made.json["kind"], "app");
    let id = made.json["id"].as_str().unwrap();
    let renamed = env
        .local(
            Method::PATCH,
            &format!("/v1/stores/{id}"),
            Some(json!({ "name": "Journal" })),
        )
        .await;
    assert_eq!(renamed.json["name"], "Journal", "{}", renamed.json);
    let taken = env
        .local(
            Method::PATCH,
            &format!("/v1/stores/{id}"),
            Some(json!({ "name": "zeta" })),
        )
        .await;
    assert_eq!(taken.error(), Code::AlreadyExists);
    let personal = env
        .local(
            Method::DELETE,
            &format!("/v1/stores/{}", env.personal),
            None,
        )
        .await;
    assert_eq!(personal.error(), Code::InvalidArgument);
    // Another user's store is not_found, never permission_denied.
    let (_, theirs) = env.other_user("bob").await;
    for (method, uri) in [
        (Method::GET, format!("/v1/stores/{theirs}")),
        (Method::DELETE, format!("/v1/stores/{theirs}")),
        (Method::GET, format!("/v1/stores/{theirs}/keys")),
        (Method::GET, format!("/v1/stores/{theirs}/status")),
        (Method::GET, format!("/v1/stores/{}", Uuid::now_v7())),
    ] {
        let r = env.local(method.clone(), &uri, None).await;
        assert_eq!(r.error(), Code::NotFound, "{method} {uri}");
    }
    let r = env
        .call(
            knoggin_http::edge::Arrival::Socket,
            Method::GET,
            "/v1/memories",
            None,
            &[("knoggin-store", &theirs.to_string())],
        )
        .await;
    assert_eq!(r.error(), Code::NotFound, "Knoggin-Store of another user");
    let filtered = env
        .local(Method::GET, "/v1/stores?includeDeleted=false&x=1", None)
        .await;
    assert_eq!(
        filtered.error(),
        Code::InvalidArgument,
        "an unknown query parameter"
    );
}

#[tokio::test]
async fn account_deletion_undo_and_the_erase_step() {
    let env = EdgeEnv::new().await;
    let app = env.app("Work").await;
    let r = env
        .local(
            Method::DELETE,
            "/v1/account",
            Some(json!({ "confirm": "yes" })),
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    let r = env
        .local(
            Method::DELETE,
            "/v1/account",
            Some(json!({ "confirm": "delete my data" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    assert!(r.json["deleteAfter"].is_string());
    // Only the routes of an account being deleted work now.
    let blocked = env.local(Method::POST, SAVE, save_body("late")).await;
    assert_eq!(blocked.error(), Code::StoreDeleting);
    let blocked = env.local(Method::GET, "/v1/stores", None).await;
    assert_eq!(blocked.error(), Code::StoreDeleting);
    let code = env
        .local(Method::POST, "/v1/local/browser-code", None)
        .await;
    assert_eq!(code.status, StatusCode::OK, "knoggin open still works");
    let s = env.local(Method::GET, "/v1/session", None).await;
    assert_eq!(s.json["signedIn"], true);
    assert!(s.json["user"]["deletedAt"].is_string());
    let again = env
        .local(
            Method::DELETE,
            "/v1/account",
            Some(json!({ "confirm": "delete my data" })),
        )
        .await;
    assert_eq!(
        again.json["deletedAt"], r.json["deletedAt"],
        "a repeat keeps the first time"
    );
    // Undo brings back the stores deleted with the account.
    let undone = env.local(Method::POST, "/v1/account/undelete", None).await;
    assert_eq!(
        undone.json["user"]["deletedAt"],
        Value::Null,
        "{}",
        undone.json
    );
    let stores = env.local(Method::GET, "/v1/stores", None).await;
    assert!(
        stores.json["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["status"] == "active")
    );
    let not_deleted = env.local(Method::POST, "/v1/account/undelete", None).await;
    assert_eq!(not_deleted.error(), Code::Conflict);
    // Delete again and let the erase step run after the undo period.
    let saved = env.local(Method::POST, SAVE, save_body("gone soon")).await;
    assert_eq!(saved.status, StatusCode::CREATED);
    env.local(
        Method::DELETE,
        "/v1/account",
        Some(json!({ "confirm": "delete my data" })),
    )
    .await;
    env.clock.advance(7 * MILLIS_PER_DAY + 9 * 60 * 1000);
    let report = erase_step(env.db.clone(), &env.host, &env.backend, env.clock.now(), 7)
        .await
        .unwrap();
    assert_eq!(
        report.stores_erased, 0,
        "not before 10 minutes past the period"
    );
    env.clock.advance(2 * 60 * 1000);
    let report = erase_step(env.db.clone(), &env.host, &env.backend, env.clock.now(), 7)
        .await
        .unwrap();
    assert_eq!(
        (report.stores_erased, report.users_erased),
        (2, 0),
        "the local user stays"
    );
    assert!(!env.backend.store_dir(app).exists());
    // The next request makes a new, empty personal store and clears the mark.
    let s = env.local(Method::GET, "/v1/session", None).await;
    assert_eq!(s.json["user"]["deletedAt"], Value::Null, "{}", s.json);
    assert_ne!(s.json["personalStoreId"], env.personal.to_string());
    let list = env.local(Method::GET, "/v1/memories", None).await;
    assert_eq!(list.json["items"].as_array().unwrap().len(), 0);
    let stores = env.local(Method::GET, "/v1/stores", None).await;
    assert_eq!(stores.json["items"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn admin_removal_of_another_user() {
    let env = EdgeEnv::new().await;
    let me = env
        .local(Method::DELETE, &format!("/v1/users/{}", env.user), None)
        .await;
    assert_eq!(me.error(), Code::InvalidArgument, "not yourself");
    let (bob, bob_store) = env.other_user("bob").await;
    let users = env.local(Method::GET, "/v1/users?limit=1", None).await;
    assert_eq!(users.json["items"].as_array().unwrap().len(), 1);
    let cursor = users.json["nextCursor"].as_str().unwrap().to_owned();
    let page2 = env
        .local(
            Method::GET,
            &format!("/v1/users?limit=1&cursor={cursor}"),
            None,
        )
        .await;
    assert_eq!(
        page2.json["items"].as_array().unwrap().len(),
        1,
        "{}",
        page2.json
    );
    assert_eq!(page2.json["items"][0]["container"]["state"], "parked");
    // A key of Bob's store, made directly.
    let (secret, hash, prefix) = knoggin_crypto::token::new_api_key().unwrap();
    knoggin_state::ApiKeys::new(env.db.clone(), env.fields.clone())
        .create(
            bob,
            bob_store,
            knoggin_state::NewApiKey {
                id: Uuid::now_v7(),
                hash,
                prefix,
                key_expires_ms: env.clock.now() + 90 * MILLIS_PER_DAY,
            },
            knoggin_core::Sensitive::new("k".into()),
            vec![knoggin_state::record::Permission::Read],
            env.clock.now(),
        )
        .await
        .unwrap();
    assert_eq!(
        env.bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
            .await
            .status,
        StatusCode::OK
    );
    let stores = env
        .local(Method::GET, &format!("/v1/users/{bob}/stores"), None)
        .await;
    assert_eq!(stores.json["items"][0]["id"], bob_store.to_string());
    let removed = env
        .local(Method::DELETE, &format!("/v1/users/{bob}"), None)
        .await;
    assert_eq!(removed.json["deletedBy"], "admin", "{}", removed.json);
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(
        (r.error(), r.json["error"]["reason"].clone()),
        (Code::PermissionDenied, json!("account_removed"))
    );
    let s = env
        .bearer(fixed(), &secret, Method::GET, "/v1/session", None)
        .await;
    assert_eq!(s.json["signedOutReason"], "account_removed");
    let back = env
        .local(Method::POST, &format!("/v1/users/{bob}/undelete"), None)
        .await;
    assert_eq!(back.json["deletedAt"], Value::Null, "{}", back.json);
    assert_eq!(
        env.bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn browser_checks_on_both_tcp_listeners() {
    let env = EdgeEnv::new().await;
    let (secret, _) = env
        .key(env.personal, json!(["memory:read", "memory:write"]), 90)
        .await;
    let auth = format!("Bearer {secret}");
    let ok = |r: &support::Resp| assert_eq!(r.status, StatusCode::OK, "{}", r.json);
    for host in [
        "127.0.0.1:7690",
        "localhost:7690",
        "[::1]:7690",
        "LOCALHOST.:7690",
    ] {
        ok(&env
            .call(
                fixed(),
                Method::GET,
                "/v1/memories",
                None,
                &[("authorization", &auth), ("host", host)],
            )
            .await);
    }
    for host in [
        "127.0.0.1",
        "127.0.0.1:7691",
        "evil.example:7690",
        "127.0.0.1.evil:7690",
    ] {
        let r = env
            .call(
                fixed(),
                Method::GET,
                "/v1/memories",
                None,
                &[("authorization", &auth), ("host", host)],
            )
            .await;
        assert_eq!(r.error(), Code::PermissionDenied, "{host}");
    }
    // The web listener takes this run's origin only.
    for host in ["127.0.0.1:7690", "localhost:50123", "kabc.localhost:50123"] {
        let r = env
            .call(
                web(),
                Method::GET,
                "/v1/memories",
                None,
                &[("authorization", &auth), ("host", host)],
            )
            .await;
        assert_eq!(r.error(), Code::PermissionDenied, "{host}");
    }
    ok(&env
        .call(
            web(),
            Method::GET,
            "/v1/memories",
            None,
            &[("authorization", &auth)],
        )
        .await);
    // Health answers whatever the Host says.
    for uri in ["/healthz", "/readyz"] {
        ok(&env
            .call(
                fixed(),
                Method::GET,
                uri,
                None,
                &[("host", "anything.example")],
            )
            .await);
    }
    // Origin.
    ok(&env
        .call(
            fixed(),
            Method::GET,
            "/v1/memories",
            None,
            &[
                ("authorization", &auth),
                ("origin", "http://127.0.0.1:7690"),
            ],
        )
        .await);
    ok(&env
        .call(
            web(),
            Method::GET,
            "/v1/memories",
            None,
            &[
                ("authorization", &auth),
                ("origin", "http://127.0.0.1:50123"),
            ],
        )
        .await);
    for (arrival, origin) in [
        (fixed(), "http://evil.example"),
        (fixed(), "null"),
        (fixed(), "http://127.0.0.1:50123"),
        (web(), "http://127.0.0.1:7690"),
        (web(), "https://127.0.0.1:50123"),
    ] {
        let r = env
            .call(
                arrival,
                Method::GET,
                "/v1/memories",
                None,
                &[("authorization", &auth), ("origin", origin)],
            )
            .await;
        assert_eq!(r.error(), Code::PermissionDenied, "{origin}");
    }
    // Fetch site.
    ok(&env
        .call(
            fixed(),
            Method::GET,
            "/v1/memories",
            None,
            &[("authorization", &auth), ("sec-fetch-site", "same-origin")],
        )
        .await);
    let r = env
        .call(
            fixed(),
            Method::GET,
            "/v1/memories",
            None,
            &[("authorization", &auth), ("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    // Preflight.
    let r = env
        .call(fixed(), Method::OPTIONS, "/v1/memories", None, &[])
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    assert!(r.headers.get("access-control-allow-origin").is_none());
    // A form-encoded body.
    let mut request = http::Request::builder()
        .method(Method::POST)
        .uri(SAVE)
        .header("host", "127.0.0.1:7690")
        .header("authorization", &auth)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from("summary=x"))
        .unwrap();
    request.extensions_mut().insert(fixed());
    assert_eq!(env.send(request).await.error(), Code::InvalidArgument);
    // The socket has no browser checks.
    ok(&env
        .call(
            knoggin_http::edge::Arrival::Socket,
            Method::GET,
            "/v1/memories",
            None,
            &[("host", "evil.example"), ("origin", "null")],
        )
        .await);
}

#[tokio::test]
async fn local_web_sessions() {
    let env = EdgeEnv::new().await;
    let code = env
        .local(Method::POST, "/v1/local/browser-code", None)
        .await;
    assert_eq!(code.status, StatusCode::OK, "{}", code.json);
    let url = code.json["url"].as_str().unwrap();
    let code_text = url
        .strip_prefix(&format!("http://{WEB}/?t="))
        .unwrap()
        .to_owned();
    assert_eq!(code_text.len(), 43);
    assert!(code.json["expiresAt"].is_string());
    // The code is given on the socket only.
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 90).await;
    let r = env
        .bearer(
            fixed(),
            &secret,
            Method::POST,
            "/v1/local/browser-code",
            None,
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    // The session is made on the web listener only, without a credential, once per code.
    let on_fixed = env
        .call(
            fixed(),
            Method::POST,
            "/v1/local/session",
            Some(json!({ "code": code_text })),
            &[],
        )
        .await;
    assert_eq!(on_fixed.error(), Code::NotFound);
    let made = env
        .call(
            web(),
            Method::POST,
            "/v1/local/session",
            Some(json!({ "code": code_text })),
            &[],
        )
        .await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.json);
    assert!(made.headers.get("set-cookie").is_none(), "no cookie");
    let token = made.json["sessionToken"].as_str().unwrap().to_owned();
    assert!(token.starts_with("knoggin_session_"));
    let twice = env
        .call(
            web(),
            Method::POST,
            "/v1/local/session",
            Some(json!({ "code": code_text })),
            &[],
        )
        .await;
    assert_eq!(twice.error(), Code::Unauthenticated);
    // The token is a bearer value on the web listener, reaching everything.
    let s = env
        .bearer(web(), &token, Method::GET, "/v1/session", None)
        .await;
    assert_eq!(s.json["credential"]["kind"], "session");
    assert_eq!(s.json["user"]["email"], Value::Null);
    assert_eq!(
        env.bearer(web(), &token, Method::GET, "/v1/stores", None)
            .await
            .status,
        StatusCode::OK
    );
    let saved = env
        .bearer(
            web(),
            &token,
            Method::POST,
            SAVE,
            save_body("from the page"),
        )
        .await;
    assert_eq!(saved.status, StatusCode::CREATED);
    // Refused on the fixed listener and on the socket, for logout too.
    for arrival in [fixed(), knoggin_http::edge::Arrival::Socket] {
        let r = env
            .bearer(arrival.clone(), &token, Method::GET, "/v1/stores", None)
            .await;
        assert_eq!(r.error(), Code::Unauthenticated);
    }
    let r = env
        .bearer(fixed(), &token, Method::POST, "/auth/logout", None)
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
    // Another daemon run knows nothing of it.
    let restarted = EdgeState::new(
        knoggin_http::edge::EdgeConfig::local(format!("http://127.0.0.1:{PORT}"), "0.1.0".into()),
        EdgeParts {
            db: env.db.clone(),
            field_keys: env.fields.clone(),
            keys: env.keys.clone(),
            clock: env.clock.clock(),
            host: Some(env.host.clone()),
            local: Some(Arc::new(LocalSessions::new(format!("http://{WEB}")))),
        },
    )
    .await
    .unwrap();
    let app = edge_layer(
        restarted,
        store_layer(env.host.clone()),
        axum::Router::new(),
    );
    let mut request = http::Request::builder()
        .uri("/v1/stores")
        .header("host", WEB)
        .header("authorization", format!("Bearer {token}"))
        .body(axum::body::Body::empty())
        .unwrap();
    request.extensions_mut().insert(web());
    assert_eq!(
        support::edge::send(&app, request).await.error(),
        Code::Unauthenticated
    );
    // Logout on the web listener ends it.
    let out = env
        .bearer(web(), &token, Method::POST, "/auth/logout", None)
        .await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    let r = env
        .bearer(web(), &token, Method::GET, "/v1/stores", None)
        .await;
    assert_eq!(r.error(), Code::Unauthenticated);
}

/// A one-time code from the socket, as `knoggin open` gets it.
async fn browser_code(env: &EdgeEnv) -> String {
    let r = env
        .local(Method::POST, "/v1/local/browser-code", None)
        .await;
    let url = r.json["url"].as_str().unwrap();
    url.strip_prefix(&format!("http://{WEB}/?t="))
        .unwrap()
        .to_owned()
}

/// Spends `code` on the web listener.
async fn redeem(env: &EdgeEnv, code: &str) -> support::Resp {
    env.call(
        web(),
        Method::POST,
        "/v1/local/session",
        Some(json!({ "code": code })),
        &[],
    )
    .await
}

#[tokio::test]
async fn local_web_sessions_count_time_on_the_wall_clock() {
    // The edge's clock, not a monotonic one that stops while the machine sleeps.
    let env = EdgeEnv::new().await;
    let code = browser_code(&env).await;
    env.clock.advance(60 * 1000);
    assert_eq!(
        redeem(&env, &code).await.error(),
        Code::Unauthenticated,
        "a code lives 60 seconds"
    );
    let code = browser_code(&env).await;
    env.clock.advance(59 * 1000);
    let made = redeem(&env, &code).await;
    assert_eq!(made.status, StatusCode::OK, "{}", made.json);
    let token = made.json["sessionToken"].as_str().unwrap().to_owned();
    env.clock.advance(MILLIS_PER_DAY - 1000);
    let r = env
        .bearer(web(), &token, Method::GET, "/v1/stores", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "a request keeps it alive");
    env.clock.advance(MILLIS_PER_DAY);
    let r = env
        .bearer(web(), &token, Method::GET, "/v1/stores", None)
        .await;
    assert_eq!(r.error(), Code::Unauthenticated, "24 idle hours end it");
}

#[tokio::test]
async fn rate_limits() {
    let env = EdgeEnv::with(tight).await;
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 90).await;
    for _ in 0..2 {
        assert_eq!(
            env.bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
                .await
                .status,
            StatusCode::OK
        );
    }
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/memories", None)
        .await;
    assert_eq!(r.error(), Code::RateLimited);
    assert!(r.headers.contains_key("retry-after"));
    // The socket takes from no bucket.
    for _ in 0..5 {
        assert_eq!(
            env.local(Method::GET, "/v1/memories", None).await.status,
            StatusCode::OK
        );
    }
    // Requests that do not pass the check share the fixed listener's bucket: here 3.
    let bad = format!("knoggin_live_{}", "A".repeat(43));
    assert_eq!(
        env.call(fixed(), Method::GET, "/healthz", None, &[])
            .await
            .status,
        StatusCode::OK
    );
    assert_eq!(
        env.bearer(fixed(), &bad, Method::GET, "/v1/memories", None)
            .await
            .error(),
        Code::Unauthenticated
    );
    assert_eq!(
        env.call(fixed(), Method::GET, "/v1/memories", None, &[])
            .await
            .error(),
        Code::Unauthenticated
    );
    assert_eq!(
        env.call(fixed(), Method::GET, "/healthz", None, &[])
            .await
            .error(),
        Code::RateLimited
    );
    assert_eq!(
        env.bearer(fixed(), &bad, Method::GET, "/v1/memories", None)
            .await
            .error(),
        Code::RateLimited
    );
    // The web listener has no such bucket.
    assert_eq!(
        env.call(web(), Method::GET, "/healthz", None, &[])
            .await
            .status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn each_bucket_runs_before_the_checks_after_it() {
    let env = EdgeEnv::with(tight).await;
    // Step 1 before step 2: a request the browser checks refuse still takes from the fixed
    // bucket (3 here), and an empty bucket answers before those checks.
    let cross_origin = [("origin", "http://evil.example")];
    for _ in 0..3 {
        let r = env
            .call(fixed(), Method::GET, "/healthz", None, &cross_origin)
            .await;
        assert_eq!(r.error(), Code::PermissionDenied);
    }
    let r = env
        .call(fixed(), Method::GET, "/healthz", None, &cross_origin)
        .await;
    assert_eq!(r.error(), Code::RateLimited);
    // Step 12 before step 13: an API key on a control route spends its bucket (2 here).
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 90).await;
    for _ in 0..2 {
        let r = env
            .bearer(fixed(), &secret, Method::GET, "/v1/stores", None)
            .await;
        assert_eq!(r.error(), Code::PermissionDenied);
    }
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/stores", None)
        .await;
    assert_eq!(r.error(), Code::RateLimited);
}

#[tokio::test]
async fn a_cookie_does_not_get_around_the_fixed_listeners_bucket() {
    // Mode `none` ignores a cookie, so a request with only a cookie is charged at step 1.
    let env = EdgeEnv::with(tight).await;
    let cookie = [("cookie", "knoggin_session=whatever")];
    for _ in 0..3 {
        let r = env
            .call(fixed(), Method::GET, "/healthz", None, &cookie)
            .await;
        assert_eq!(r.status, StatusCode::OK);
    }
    for uri in ["/healthz", "/v1/memories", "/v1/stores"] {
        let r = env.call(fixed(), Method::GET, uri, None, &cookie).await;
        assert_eq!(r.error(), Code::RateLimited, "{uri}");
    }
}

#[tokio::test]
async fn export_with_the_store_parameter_and_the_caps() {
    let env = EdgeEnv::new().await;
    let app = env.app("Work notes").await;
    let r = env
        .local(Method::GET, &format!("/v1/export?store={app}"), None)
        .await;
    assert_eq!(
        r.status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&r.bytes)
    );
    let date = knoggin_core::Timestamp::from_millis(env.clock.now())
        .utc_date()
        .unwrap();
    assert_eq!(
        r.headers["content-disposition"],
        format!("attachment; filename=\"knoggin-Work_notes-{date}.jsonl\"")
    );
    let (secret, _) = env.key(app, json!(["memory:read"]), 90).await;
    let r = env
        .bearer(fixed(), &secret, Method::GET, "/v1/export", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "a key exports its own store");
    let r = env
        .bearer(
            fixed(),
            &secret,
            Method::GET,
            &format!("/v1/export?store={app}"),
            None,
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
    let (_, theirs) = env.other_user("bob").await;
    let r = env
        .local(Method::GET, &format!("/v1/export?store={theirs}"), None)
        .await;
    assert_eq!(r.error(), Code::NotFound);
    let r = env
        .call(
            knoggin_http::edge::Arrival::Socket,
            Method::GET,
            &format!("/v1/export?store={app}"),
            None,
            &[("knoggin-store", &app.to_string())],
        )
        .await;
    assert_eq!(r.error(), Code::InvalidArgument);
    // Two exports held open; the third is rate_limited.
    let open = |store: StoreId| {
        let mut request = http::Request::builder()
            .uri(format!("/v1/export?store={store}"))
            .body(axum::body::Body::empty())
            .unwrap();
        request
            .extensions_mut()
            .insert(knoggin_http::edge::Arrival::Socket);
        tower::ServiceExt::oneshot(env.app.clone(), request)
    };
    let a = open(app).await.unwrap();
    let b = open(env.personal).await.unwrap();
    assert_eq!((a.status(), b.status()), (StatusCode::OK, StatusCode::OK));
    let r = env.local(Method::GET, "/v1/export", None).await;
    assert_eq!(r.error(), Code::RateLimited);
    drop((a, b));
    let r = env.local(Method::GET, "/v1/export", None).await;
    assert_eq!(r.status, StatusCode::OK);
}

#[tokio::test]
async fn mode_routes_of_the_daemon() {
    let env = EdgeEnv::new().await;
    let s = env
        .local(
            Method::GET,
            &format!("/v1/stores/{}/status", env.personal),
            None,
        )
        .await;
    assert_eq!(s.json["state"], "parked", "{}", s.json);
    assert_eq!(s.json["model"]["state"], "ready");
    env.local(Method::POST, SAVE, save_body("open it")).await;
    let s = env
        .local(
            Method::GET,
            &format!("/v1/stores/{}/status", env.personal),
            None,
        )
        .await;
    assert_eq!(s.json["state"], "ready");
    assert_eq!(s.json["full"], false);
    assert!(s.json["embedding"]["total"].is_number());
    let a = env.local(Method::GET, "/v1/admin/status", None).await;
    assert_eq!(a.json["mode"], "local");
    assert_eq!(
        a.json["backends"],
        json!({ "stateDb": "local_file", "objectStorage": "local_dir", "keyProvider": "key_file", "launcher": "none" })
    );
    assert_eq!(a.json["users"], json!({ "total": 1, "deleting": 0 }));
    for field in [
        "storage",
        "containers",
        "pool",
        "rollout",
        "retireBefore",
        "imageDigest",
    ] {
        assert_eq!(a.json[field], Value::Null, "{field}");
    }
    assert!(a.json["dataDir"].is_string());
    let snaps = env
        .local(
            Method::GET,
            &format!("/v1/admin/stores/{}/snapshots", env.personal),
            None,
        )
        .await;
    assert_eq!(snaps.error(), Code::NotFound);
    // The Store shape reads the open store's count.
    let store = env
        .local(Method::GET, &format!("/v1/stores/{}", env.personal), None)
        .await;
    assert_eq!(store.json["memoryCount"], 1, "{}", store.json);
    let unknown = env.local(Method::GET, "/v1/nothing-here", None).await;
    assert_eq!(unknown.error(), Code::NotFound);
    let wrong = env.local(Method::PUT, "/v1/stores", None).await;
    assert_eq!(
        (wrong.status, wrong.json["error"]["code"].clone()),
        (StatusCode::METHOD_NOT_ALLOWED, json!("invalid_argument"))
    );
}

#[tokio::test]
async fn the_locked_router_answers_store_locked() {
    let app = locked_router(
        LockReason::MasterKeyMissing,
        "0.1.0".into(),
        "https://docs.example/docs".into(),
    );
    let call = |uri: &str| {
        let mut request = http::Request::builder()
            .uri(uri)
            .header("host", "127.0.0.1:7690")
            .body(axum::body::Body::empty())
            .unwrap();
        request.extensions_mut().insert(fixed());
        support::edge::send(&app, request)
    };
    let s = call("/v1/session").await;
    assert_eq!(s.error(), Code::StoreLocked);
    assert_eq!(s.json["error"]["reason"], "master_key_missing");
    assert_eq!(s.json["error"]["docsUrl"], "https://docs.example/docs");
    let m = call("/v1/memories").await;
    assert_eq!(m.error(), Code::StoreLocked);
    assert!(m.json["error"].get("docsUrl").is_none());
    assert_eq!(call("/healthz").await.json["status"], "ok");
    assert_eq!(call("/readyz").await.json, json!({ "status": "ready" }));
}

#[tokio::test]
async fn a_request_without_its_listeners_tag_is_internal() {
    let env = EdgeEnv::new().await;
    let request = http::Request::builder()
        .uri("/healthz")
        .body(axum::body::Body::empty())
        .unwrap();
    assert_eq!(env.send(request).await.error(), Code::Internal);
}

#[tokio::test]
async fn the_mcp_endpoint_sits_behind_the_credential_check() {
    let env = EdgeEnv::new().await;
    let body = Some(json!({ "jsonrpc": "2.0", "id": 1, "method": "ping" }));
    for uri in ["/mcp", "/mcp/plugin"] {
        let r = env
            .call(fixed(), Method::POST, uri, body.clone(), &[])
            .await;
        assert_eq!(r.error(), Code::Unauthenticated, "{uri}");
    }
    let (secret, _) = env.key(env.personal, json!(["memory:read"]), 90).await;
    let r = env
        .bearer(fixed(), &secret, Method::POST, "/mcp", body.clone())
        .await;
    assert_ne!(
        r.status,
        StatusCode::UNAUTHORIZED,
        "{}",
        String::from_utf8_lossy(&r.bytes)
    );
    let r = env
        .call(
            fixed(),
            Method::POST,
            "/mcp",
            body,
            &[
                ("authorization", &format!("Bearer {secret}")),
                ("origin", "http://evil.example"),
            ],
        )
        .await;
    assert_eq!(r.error(), Code::PermissionDenied);
}

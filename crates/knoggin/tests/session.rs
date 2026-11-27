// SPDX-License-Identifier: AGPL-3.0-only
//! The local install end to end, as one scripted session in a temporary home, in test
//! mode.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr
)]

mod support;

use std::time::Duration;

use serde_json::json;
use support::{Home, Started, bridge, call, tool_json};

fn summaries(list: &serde_json::Value) -> Vec<String> {
    list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["summary"].as_str().unwrap_or("").to_owned())
        .collect()
}

#[test]
fn the_local_session() {
    let home = Home::new();

    // Start the daemon.
    let o = home.run(&["start"]);
    assert_eq!(o.code, 0, "{}{}", o.stdout, o.stderr);
    let _guard = Started(&home);
    assert!(o.stdout.contains("knoggin open"), "{}", o.stdout);

    // A TCP request without a credential is refused, and the command line works over the socket.
    let r = home.tcp("GET", "/v1/memories", &[], None);
    assert_eq!(r.status, 401, "{}", r.text());
    assert_eq!(r.json()["error"]["code"], "unauthenticated");
    let s = home.json(&["status"]);
    assert_eq!(s["running"], true);
    assert_eq!(s["masterKeySource"], "key_file");
    assert_eq!(s["stores"][0]["kind"], "personal");

    // Save over REST.
    let r = home.sock(
        "POST",
        "/v1/memories",
        &[],
        Some(
            &json!({ "summary": "Maya prefers window seats on long flights", "tags": ["travel"] }),
        ),
    );
    assert_eq!(r.status, 201, "{}", r.text());

    // Recall over `knoggin mcp`.
    let (out, err) = bridge(
        &home,
        &[],
        &[call(
            1,
            "recall_memories",
            json!({ "query": "window seats" }),
        )],
        &[],
    );
    assert_eq!(out.len(), 1, "{err}");
    let recall = tool_json(&out[0]);
    assert_eq!(
        recall["memories"][0]["summary"], "Maya prefers window seats on long flights",
        "{recall}"
    );

    // Create an app and a key.
    let app = home.json(&["apps", "create", "Work"]);
    assert_eq!(app["kind"], "app");
    let key = home.json(&["keys", "create", "--app", "work"]);
    let secret = key["secret"].as_str().unwrap().to_owned();
    let bearer = format!("Bearer {secret}");
    let auth = [("Authorization", bearer.as_str())];

    // Read the app with the key, and not without it.
    let r = home.tcp(
        "POST",
        "/v1/memories",
        &auth,
        Some(&json!({ "summary": "The work app's first memory" })),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    let r = home.tcp("GET", "/v1/memories", &auth, None);
    assert_eq!(r.status, 200, "{}", r.text());
    assert_eq!(summaries(&r.json()), ["The work app's first memory"]);
    assert_eq!(home.tcp("GET", "/v1/memories", &[], None).status, 401);
    let wrong = format!("Bearer knoggin_live_{}", "A".repeat(43));
    assert_eq!(
        home.tcp("GET", "/v1/memories", &[("Authorization", &wrong)], None)
            .status,
        401
    );

    // Stop and start, and the data is there.
    assert_eq!(home.run(&["stop"]).code, 0);
    assert_eq!(home.run(&["status"]).code, 3);
    assert_eq!(home.run(&["start"]).code, 0);
    let r = home.sock("GET", "/v1/memories", &[], None);
    assert_eq!(
        summaries(&r.json()),
        ["Maya prefers window seats on long flights"]
    );
    let r = home.tcp("GET", "/v1/memories", &auth, None);
    assert_eq!(summaries(&r.json()), ["The work app's first memory"]);

    // Kill the daemon five seconds after a write, and the store opens with that write present.
    let r = home.sock(
        "POST",
        "/v1/memories",
        &[],
        Some(&json!({ "summary": "Written just before the crash" })),
    );
    assert_eq!(r.status, 201, "{}", r.text());
    std::thread::sleep(Duration::from_secs(5));
    let pid = home.pid();
    assert!(
        std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status()
            .unwrap()
            .success()
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while home.run(&["status"]).code != 3 {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon did not die"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(home.run(&["start"]).code, 0);
    let r = home.sock("GET", "/v1/memories", &[], None);
    let mut got = summaries(&r.json());
    got.sort();
    assert_eq!(
        got,
        [
            "Maya prefers window seats on long flights",
            "Written just before the crash"
        ]
    );
}

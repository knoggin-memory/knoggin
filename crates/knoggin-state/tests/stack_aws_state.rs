// SPDX-License-Identifier: AGPL-3.0-only
//! The state conformance suite against `DynamoDb` on a moto container, plus what only
//! DynamoDB shows: the stored attributes (the document as JSON text, `expires_ms` and the
//! time-to-live `expires` in seconds rounded up) and the read-side expiry check on an item
//! that DynamoDB still holds. No AWS account: dummy credentials and moto's address are set
//! here, on the client, and nowhere else.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType,
    ScalarAttributeType, TimeToLiveSpecification,
};
use knoggin_state::testing::{TestClock, conformance};
use knoggin_state::{Cond, DynamoDb, Item, StateDb, Write};
use serde_json::{Value, json};
use testcontainers::GenericImage;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;

const TABLE: &str = "knoggin-state";

/// `<repository>@sha256:<digest>` from `tests/images.toml`.
fn pinned_image(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/images.toml");
    let file: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    file["images"][name].as_str().unwrap().to_owned()
}

#[allow(deprecated, reason = "the dated version the binary uses")]
fn behavior_version() -> BehaviorVersion {
    BehaviorVersion::v2026_11_17()
}

/// A client config for moto alone: its address, a region and dummy credentials.
fn moto_config(endpoint: &str) -> aws_sdk_dynamodb::config::Builder {
    aws_sdk_dynamodb::config::Builder::new()
        .behavior_version(behavior_version())
        .region(Region::new("us-east-1"))
        .credentials_provider(Credentials::new("testing", "testing", None, None, "moto"))
        .endpoint_url(endpoint)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs docker"]
async fn conformance_on_dynamodb() {
    let image = pinned_image("moto");
    // testcontainers joins name and tag with `:`, which turns this back into `<repo>@sha256:<hex>`.
    let (name, digest) = image.rsplit_once(':').unwrap();
    let container = GenericImage::new(name, digest)
        .with_exposed_port(5000.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Running on"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let raw = aws_sdk_dynamodb::Client::from_conf(moto_config(&endpoint).build());

    // The table as `deploy/aws` makes it: on demand, `pk` and `sk`, time-to-live on `expires`.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let made = raw
            .create_table()
            .table_name(TABLE)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(attr("pk"))
            .attribute_definitions(attr("sk"))
            .key_schema(key("pk", KeyType::Hash))
            .key_schema(key("sk", KeyType::Range))
            .send()
            .await;
        match made {
            Ok(_) => break,
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for moto: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("moto did not start: {e:?}"),
        }
    }
    raw.update_time_to_live()
        .table_name(TABLE)
        .time_to_live_specification(
            TimeToLiveSpecification::builder()
                .attribute_name("expires")
                .enabled(true)
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();

    let clock = TestClock::new();
    let db = Arc::new(DynamoDb::new(moto_config(&endpoint), TABLE, clock.clock()));
    conformance(db.clone(), &clock).await;

    // What is stored: the document as JSON text, `expires_ms` and `expires` (seconds,
    // rounded up), and nothing else.
    let now = clock.now();
    let doc = json!({ "s": "Grüße", "n": [1, null] });
    db.write(vec![Write::put(
        Item::new("attrs", "-", doc.clone()).expiring(Some(now + 1_500)),
        Cond::Absent,
    )])
    .await
    .unwrap();
    let stored = raw_item(&raw, "attrs", "-").await.unwrap();
    let mut names: Vec<&str> = stored.keys().map(String::as_str).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        ["doc", "expires", "expires_ms", "pk", "sk", "version"]
    );
    let text = stored["doc"].as_s().unwrap();
    assert_eq!(serde_json::from_str::<Value>(text).unwrap(), doc);
    assert_eq!(
        stored["expires_ms"].as_n().unwrap(),
        &(now + 1_500).to_string()
    );
    assert_eq!(
        stored["expires"].as_n().unwrap(),
        &(now / 1000 + 2).to_string()
    );
    assert_eq!(stored["version"].as_n().unwrap(), "1");
    // A put without an expiry removes both expiry attributes.
    db.write(vec![Write::put(
        Item::new("attrs", "-", doc),
        Cond::Version(1),
    )])
    .await
    .unwrap();
    let stored = raw_item(&raw, "attrs", "-").await.unwrap();
    assert!(!stored.contains_key("expires_ms") && !stored.contains_key("expires"));
    assert_eq!(stored["version"].as_n().unwrap(), "2");

    // DynamoDB removes expired items late: one it still holds is gone for every read.
    let now = clock.now();
    db.write(vec![Write::put(
        Item::new("late", "a", json!({})).expiring(Some(now + 10)),
        Cond::Absent,
    )])
    .await
    .unwrap();
    clock.advance(10);
    assert!(raw_item(&raw, "late", "a").await.is_some());
    assert!(db.get("late", "a").await.unwrap().is_none());
    assert!(db.query("late", "", 10, None).await.unwrap().is_empty());

    container.rm().await.unwrap();
}

fn attr(name: &str) -> AttributeDefinition {
    AttributeDefinition::builder()
        .attribute_name(name)
        .attribute_type(ScalarAttributeType::S)
        .build()
        .unwrap()
}

fn key(name: &str, kind: KeyType) -> KeySchemaElement {
    KeySchemaElement::builder()
        .attribute_name(name)
        .key_type(kind)
        .build()
        .unwrap()
}

async fn raw_item(
    raw: &aws_sdk_dynamodb::Client,
    pk: &str,
    sk: &str,
) -> Option<std::collections::HashMap<String, AttributeValue>> {
    raw.get_item()
        .table_name(TABLE)
        .key("pk", AttributeValue::S(pk.into()))
        .key("sk", AttributeValue::S(sk.into()))
        .consistent_read(true)
        .send()
        .await
        .unwrap()
        .item
}

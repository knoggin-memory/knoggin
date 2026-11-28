// SPDX-License-Identifier: AGPL-3.0-only
//! The binary's AWS wiring against a moto container: `knoggin admin selftest` with
//! `state.backend = "dynamodb"` and `keys.provider = "kms"` reads and writes the table
//! (the schema record) and makes and unwraps a data key. The child process finds moto
//! through `AWS_ENDPOINT_URL` alone, with dummy
//! keys, no shared config or credentials file and no instance metadata.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

mod support;

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use support::Home;
use testcontainers::GenericImage;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;

/// `<repository>@sha256:<digest>` from `tests/images.toml`.
fn pinned_image(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/images.toml");
    let file: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    file["images"][name].as_str().unwrap().to_owned()
}

/// One JSON call to moto; moto takes the service from the credential scope and checks no
/// signature.
async fn call(
    http: &reqwest::Client,
    endpoint: &str,
    service: &str,
    target: &str,
    body: Value,
) -> Value {
    let version = if service == "dynamodb" { "1.0" } else { "1.1" };
    let answer = http
        .post(endpoint)
        .header(
            "Authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential=testing/20261126/us-east-1/{service}/aws4_request, \
                 SignedHeaders=host, Signature=0"
            ),
        )
        .header("X-Amz-Target", target)
        .header("Content-Type", format!("application/x-amz-json-{version}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert!(
        answer.status().is_success(),
        "{target}: {}",
        answer.status()
    );
    answer.json().await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs docker"]
async fn admin_selftest_on_dynamodb_and_kms() {
    let image = pinned_image("moto");
    let (name, digest) = image.rsplit_once(':').unwrap();
    let container = GenericImage::new(name, digest)
        .with_exposed_port(5000.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Running on"))
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5000).await.unwrap();
    let endpoint = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while http
        .get(format!("{endpoint}/moto-api/"))
        .send()
        .await
        .is_err()
    {
        assert!(Instant::now() < deadline, "moto did not start");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    call(
        &http,
        &endpoint,
        "dynamodb",
        "DynamoDB_20120810.CreateTable",
        json!({
            "TableName": "knoggin-state",
            "BillingMode": "PAY_PER_REQUEST",
            "AttributeDefinitions": [
                { "AttributeName": "pk", "AttributeType": "S" },
                { "AttributeName": "sk", "AttributeType": "S" },
            ],
            "KeySchema": [
                { "AttributeName": "pk", "KeyType": "HASH" },
                { "AttributeName": "sk", "KeyType": "RANGE" },
            ],
        }),
    )
    .await;
    let key = call(&http, &endpoint, "kms", "TrentService.CreateKey", json!({})).await;
    let arn = key["KeyMetadata"]["Arn"].as_str().unwrap().to_owned();

    let home = Home::new()
        .with("AWS_ENDPOINT_URL", &endpoint)
        .with("AWS_REGION", "us-east-1")
        .with("AWS_ACCESS_KEY_ID", "testing")
        .with("AWS_SECRET_ACCESS_KEY", "testing")
        .with("AWS_CONFIG_FILE", "/dev/null")
        .with("AWS_SHARED_CREDENTIALS_FILE", "/dev/null")
        .with("AWS_EC2_METADATA_DISABLED", "true");
    let config = home.dir.path().join("server.toml");
    std::fs::write(
        &config,
        format!(
            "[state]\nbackend = \"dynamodb\"\ndynamodb_table = \"knoggin-state\"\n\
             [keys]\nprovider = \"kms\"\nkms_key_id = \"{arn}\"\n\
             [storage]\nbucket = \"snapshots\"\nregion = \"us-east-1\"\n\
             endpoint_url = \"{endpoint}\"\nforce_path_style = true\n"
        ),
    )
    .unwrap();
    let o = tokio::task::spawn_blocking(move || {
        let o = home.run(&[
            "--json",
            "--config",
            config.to_str().unwrap(),
            "admin",
            "selftest",
        ]);
        drop(home);
        o
    })
    .await
    .unwrap();
    let report = o.json();
    // The bucket check is not this test's: moto's S3 has no bucket here.
    assert_eq!(report["stateDatabase"], "ok", "{}", o.stderr);
    assert_eq!(report["keyProvider"], "ok", "{}", o.stderr);

    // The schema record the self-test wrote is in moto's table.
    let item = call(
        &http,
        &endpoint,
        "dynamodb",
        "DynamoDB_20120810.GetItem",
        json!({
            "TableName": "knoggin-state",
            "Key": { "pk": { "S": "meta" }, "sk": { "S": "schema" } },
            "ConsistentRead": true,
        }),
    )
    .await;
    assert_eq!(item["Item"]["version"]["N"], "1");
    let doc: Value = serde_json::from_str(item["Item"]["doc"]["S"].as_str().unwrap()).unwrap();
    assert_eq!(doc["version"], 1);

    container.rm().await.unwrap();
}

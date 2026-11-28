// SPDX-License-Identifier: AGPL-3.0-only
//! `KmsKeyProvider` against KMS on a moto container: data keys made with the store and the
//! generation as encryption context open for that store and generation only; a damaged blob,
//! another store, another generation, another key and a missing key all fail; after KMS rotates the key's material, old blobs still open
//! and nothing is re-wrapped. No AWS account: dummy credentials and moto's address are set
//! here, on the client, and nowhere else.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stdout,
    clippy::print_stderr,
    dead_code,
    unreachable_pub
)]

use std::time::{Duration, Instant};

use aws_config::Region;
use aws_sdk_kms::config::Credentials;
use knoggin_core::StoreId;
use knoggin_crypto::{KeyError, KeyProvider, KeyScope, WrappedKey};
use knoggin_server::kms::KmsKeyProvider;
use testcontainers::GenericImage;
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use uuid::Uuid;

/// `<repository>@sha256:<digest>` from `tests/images.toml`.
fn pinned_image(name: &str) -> String {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/images.toml");
    let file: toml::Table = std::fs::read_to_string(path).unwrap().parse().unwrap();
    file["images"][name].as_str().unwrap().to_owned()
}

/// A client config for moto alone: its address, a region and dummy credentials.
fn moto_config(endpoint: &str) -> aws_sdk_kms::Config {
    KmsKeyProvider::with_timeouts(
        aws_sdk_kms::config::Builder::new()
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("testing", "testing", None, None, "moto"))
            .endpoint_url(endpoint),
    )
    .build()
}

fn store() -> KeyScope {
    KeyScope {
        store: StoreId::from_uuid(Uuid::now_v7()),
        generation: 7,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "needs docker"]
async fn kms_key_provider_on_moto() {
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
    let raw = aws_sdk_kms::Client::from_conf(moto_config(&endpoint));
    let deadline = Instant::now() + Duration::from_secs(30);
    let arn = loop {
        match raw
            .create_key()
            .description("knoggin master key")
            .send()
            .await
        {
            Ok(made) => break made.key_metadata.unwrap().arn.unwrap(),
            Err(e) if Instant::now() < deadline => {
                eprintln!("waiting for moto: {e}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("moto did not start: {e:?}"),
        }
    };
    let other_arn = raw
        .create_key()
        .send()
        .await
        .unwrap()
        .key_metadata
        .unwrap()
        .arn
        .unwrap();

    let kms = KmsKeyProvider::from_conf(moto_config(&endpoint), &arn);
    let a = store();
    let (key, wrapped) = kms.generate(a).await.unwrap();
    assert_eq!(
        wrapped.kek_id, arn,
        "kek_id is the key ARN that KMS returns"
    );
    assert!(!wrapped.blob.is_empty());
    let (key2, wrapped2) = kms.generate(a).await.unwrap();
    assert_ne!(key.expose(), key2.expose());
    assert_ne!(wrapped.blob, wrapped2.blob);
    assert_eq!(
        kms.unwrap(a, &wrapped).await.unwrap().expose(),
        key.expose()
    );

    // The admin self-test's round trip under the nil UUID, generation 0.
    let (k, w) = kms.generate(KeyScope::STATE).await.unwrap();
    assert_eq!(
        kms.unwrap(KeyScope::STATE, &w).await.unwrap().expose(),
        k.expose()
    );

    // Another store's or another generation's context does not open it, nor does a damaged
    // blob.
    assert!(matches!(
        kms.unwrap(store(), &wrapped).await,
        Err(KeyError::Unwrap)
    ));
    let next = KeyScope {
        generation: a.generation + 1,
        ..a
    };
    assert!(matches!(
        kms.unwrap(next, &wrapped).await,
        Err(KeyError::Unwrap)
    ));
    let mut damaged = wrapped.clone();
    let last = damaged.blob.len() - 1;
    damaged.blob[last] ^= 1;
    assert!(matches!(
        kms.unwrap(a, &damaged).await,
        Err(KeyError::Unwrap)
    ));

    // A provider over another key cannot open it. Real KMS answers IncorrectKeyException
    // (`UnknownMasterKey`, unit-tested); moto answers AccessDeniedException, an outage here.
    let other = KmsKeyProvider::from_conf(moto_config(&endpoint), &other_arn);
    let e = other.unwrap(a, &wrapped).await.unwrap_err();
    assert!(
        matches!(
            e,
            KeyError::UnknownMasterKey { .. } | KeyError::Unavailable(_)
        ),
        "{e:?}"
    );

    // A key that KMS does not know is a missing master key.
    let gone = format!("{}{}", &arn[..arn.rfind('/').unwrap() + 1], Uuid::new_v4());
    let missing = KmsKeyProvider::from_conf(moto_config(&endpoint), &gone);
    assert!(
        matches!(missing.generate(a).await, Err(KeyError::UnknownMasterKey { ref kek_id }) if *kek_id == gone)
    );
    // A locally wrapped key never reaches KMS.
    let local = WrappedKey {
        kek_id: "local:3f9a1c04b2e87d55".into(),
        blob: vec![0; 60],
    };
    assert!(matches!(
        kms.unwrap(a, &local).await,
        Err(KeyError::UnknownMasterKey { .. })
    ));

    // KMS rotates its own material and keeps the old: old blobs keep working, new ones
    // carry the same ARN, and nothing needs wrapping again.
    // Moto answers `null` here, which the SDK does not parse; the call still applies.
    let _ = raw.enable_key_rotation().key_id(&arn).send().await;
    raw.rotate_key_on_demand()
        .key_id(&arn)
        .send()
        .await
        .unwrap();
    assert_eq!(
        kms.unwrap(a, &wrapped).await.unwrap().expose(),
        key.expose()
    );
    assert_eq!(
        kms.unwrap(a, &wrapped2).await.unwrap().expose(),
        key2.expose()
    );
    let (k3, w3) = kms.generate(a).await.unwrap();
    assert_eq!(w3.kek_id, arn);
    assert_eq!(kms.unwrap(a, &w3).await.unwrap().expose(), k3.expose());

    container.rm().await.unwrap();
}

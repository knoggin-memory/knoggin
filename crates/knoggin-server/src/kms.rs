// SPDX-License-Identifier: AGPL-3.0-only
//! `KmsKeyProvider`: data keys wrapped by one KMS key (tab "Snapshots and keys", KMS
//! wrapping). `generate` is `GenerateDataKey` with `KeySpec = AES_256`; `unwrap` is `Decrypt`
//! with `KeyId` set to the configured key. Both carry the encryption context
//! `{ "knoggin:store": "<store id>" }`, so a blob opens only for its own store, and the
//! router's role may use the key only with that context key. KMS rotates its own key
//! material and keeps the old material, so old blobs keep working and nothing is re-wrapped.

use std::time::Duration;

use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_kms::Client;
use aws_sdk_kms::config::timeout::TimeoutConfig;
use aws_sdk_kms::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_kms::primitives::Blob;
use aws_sdk_kms::types::DataKeySpec;
use knoggin_core::StoreId;
use knoggin_crypto::key::KEY_LEN;
use knoggin_crypto::{DataKey, KeyError, KeyProvider, WrappedKey};
use zeroize::Zeroizing;

/// The encryption context's one key; the router's KMS statement requires it.
pub const CONTEXT_KEY: &str = "knoggin:store";
/// One try of a call.
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
/// One call with its retries.
const OPERATION_TIMEOUT: Duration = Duration::from_secs(20);

/// The master key on AWS: one KMS key.
#[derive(Debug, Clone)]
pub struct KmsKeyProvider {
    client: Client,
    key_id: String,
}

impl KmsKeyProvider {
    /// A provider over `key_id` (`keys.kms_key_id`, the key ARN) with the shared config.
    pub fn new(sdk: &SdkConfig, key_id: impl Into<String>) -> Self {
        Self::from_conf(Self::kms_config(sdk), key_id)
    }

    /// The KMS client config: the shared config, the dated behaviour version and the time
    /// limits of every call.
    pub fn kms_config(sdk: &SdkConfig) -> aws_sdk_kms::Config {
        Self::with_timeouts(aws_sdk_kms::config::Builder::from(sdk)).build()
    }

    /// Adds the behaviour version and the time limits to a client config.
    pub fn with_timeouts(conf: aws_sdk_kms::config::Builder) -> aws_sdk_kms::config::Builder {
        conf.behavior_version(crate::aws::behavior_version())
            .timeout_config(
                TimeoutConfig::builder()
                    .connect_timeout(crate::link::CONNECT_TIMEOUT)
                    .read_timeout(ATTEMPT_TIMEOUT)
                    .operation_attempt_timeout(ATTEMPT_TIMEOUT)
                    .operation_timeout(OPERATION_TIMEOUT)
                    .build(),
            )
    }

    /// A provider over a ready client config.
    pub fn from_conf(conf: aws_sdk_kms::Config, key_id: impl Into<String>) -> Self {
        Self {
            client: Client::from_conf(conf),
            key_id: key_id.into(),
        }
    }
}

/// Copies KMS's plaintext into the key type and wipes the SDK's buffer at once.
fn data_key(plaintext: Option<Blob>) -> Result<DataKey, KeyError> {
    let bytes = Zeroizing::new(plaintext.map(Blob::into_inner).unwrap_or_default());
    let key: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| KeyError::Unwrap)?;
    let key = Zeroizing::new(key);
    Ok(DataKey::from_bytes(*key))
}

/// A blob that is damaged, or not this store's, does not unwrap; a key that KMS does not know,
/// or a blob wrapped under another key than this one, is a missing master key (the store
/// locks with `master_key_missing`); anything else is an outage, with KMS's code only.
fn kms_err<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>, kek_id: &str) -> KeyError {
    match e {
        SdkError::ServiceError(s) => match s.err().code() {
            Some("InvalidCiphertextException") => KeyError::Unwrap,
            Some("NotFoundException" | "IncorrectKeyException") => KeyError::UnknownMasterKey {
                kek_id: kek_id.to_owned(),
            },
            code => {
                KeyError::Unavailable(format!("KMS error {}", code.unwrap_or("without a code")))
            }
        },
        SdkError::TimeoutError(_) => KeyError::Unavailable("KMS did not answer in time".into()),
        _ => KeyError::Unavailable("KMS cannot be reached".into()),
    }
}

#[async_trait]
impl KeyProvider for KmsKeyProvider {
    async fn generate(&self, store: StoreId) -> Result<(DataKey, WrappedKey), KeyError> {
        let mut answer = self
            .client
            .generate_data_key()
            .key_id(&self.key_id)
            .key_spec(DataKeySpec::Aes256)
            .encryption_context(CONTEXT_KEY, store.to_string())
            .send()
            .await
            .map_err(|e| kms_err(&e, &self.key_id))?;
        let key = data_key(answer.plaintext.take())?;
        let blob = answer
            .ciphertext_blob
            .take()
            .map(Blob::into_inner)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| KeyError::Unavailable("KMS returned no wrapped key".into()))?;
        let kek_id = answer
            .key_id
            .take()
            .ok_or_else(|| KeyError::Unavailable("KMS returned no key ARN".into()))?;
        Ok((key, WrappedKey { kek_id, blob }))
    }

    async fn unwrap(&self, store: StoreId, wrapped: &WrappedKey) -> Result<DataKey, KeyError> {
        // A key wrapped by a local key set (one host) has no KMS key to open it.
        if wrapped.kek_id.starts_with("local:") {
            return Err(KeyError::UnknownMasterKey {
                kek_id: wrapped.kek_id.clone(),
            });
        }
        let mut answer = self
            .client
            .decrypt()
            .key_id(&self.key_id)
            .ciphertext_blob(Blob::new(wrapped.blob.clone()))
            .encryption_context(CONTEXT_KEY, store.to_string())
            .send()
            .await
            .map_err(|e| kms_err(&e, &wrapped.kek_id))?;
        data_key(answer.plaintext.take())
    }
}

#[cfg(test)]
mod tests {
    use aws_config::Region;
    use aws_sdk_kms::config::Credentials;
    use serde_json::{Value, json};
    use uuid::Uuid;
    use wiremock::matchers::{header, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const ARN: &str = "arn:aws:kms:eu-west-1:111122223333:key/1234abcd-12ab-34cd-56ef-1234567890ab";

    async fn stub() -> (MockServer, KmsKeyProvider) {
        let server = MockServer::start().await;
        let conf = KmsKeyProvider::with_timeouts(
            aws_sdk_kms::config::Builder::new()
                .region(Region::new("eu-west-1"))
                .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "t"))
                .endpoint_url(server.uri())
                .retry_config(
                    aws_sdk_kms::config::retry::RetryConfig::standard()
                        .with_max_attempts(2)
                        .with_initial_backoff(Duration::from_millis(1)),
                ),
        )
        .build();
        (server, KmsKeyProvider::from_conf(conf, ARN))
    }

    fn answer(status: u16, body: Value) -> ResponseTemplate {
        ResponseTemplate::new(status)
            .insert_header("content-type", "application/x-amz-json-1.1")
            .set_body_string(body.to_string())
    }

    fn b64(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    async fn bodies(server: &MockServer) -> Vec<(String, Value)> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r.headers["x-amz-target"].to_str().unwrap().to_owned(),
                    serde_json::from_slice(&r.body).unwrap(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn generate_and_unwrap_send_the_store_as_encryption_context() {
        let (server, kms) = stub().await;
        Mock::given(header("x-amz-target", "TrentService.GenerateDataKey"))
            .respond_with(answer(
                200,
                json!({ "Plaintext": b64(&[7; 32]), "CiphertextBlob": b64(b"wrapped"), "KeyId": ARN }),
            ))
            .mount(&server)
            .await;
        Mock::given(header("x-amz-target", "TrentService.Decrypt"))
            .respond_with(answer(
                200,
                json!({ "Plaintext": b64(&[7; 32]), "KeyId": ARN }),
            ))
            .mount(&server)
            .await;
        let store = StoreId::from_uuid(Uuid::now_v7());
        let (key, wrapped) = kms.generate(store).await.unwrap();
        assert_eq!(key.expose(), &[7; 32]);
        assert_eq!(
            wrapped,
            WrappedKey {
                kek_id: ARN.into(),
                blob: b"wrapped".to_vec()
            }
        );
        let back = kms.unwrap(store, &wrapped).await.unwrap();
        assert_eq!(back.expose(), &[7; 32]);

        let calls = bodies(&server).await;
        assert_eq!(calls.len(), 2);
        let context = json!({ "knoggin:store": store.to_string() });
        assert_eq!(
            calls[0].1,
            json!({ "KeyId": ARN, "KeySpec": "AES_256", "EncryptionContext": context })
        );
        assert_eq!(
            calls[1].1,
            json!({ "KeyId": ARN, "CiphertextBlob": b64(b"wrapped"), "EncryptionContext": context })
        );
    }

    #[tokio::test]
    async fn kms_errors_map_to_key_errors_without_their_text() {
        let (server, kms) = stub().await;
        let store = StoreId::from_uuid(Uuid::now_v7());
        let wrapped = WrappedKey {
            kek_id: ARN.into(),
            blob: vec![1, 2, 3],
        };
        // The error as text, without the message KMS sent.
        let kind = |e: &KeyError| match e {
            KeyError::Unwrap => "unwrap".to_owned(),
            KeyError::UnknownMasterKey { kek_id } => format!("unknown {kek_id}"),
            KeyError::Unavailable(m) => m.clone(),
            other => format!("{other:?}"),
        };
        let cases = [
            ("InvalidCiphertextException", 400, "unwrap".to_owned()),
            ("IncorrectKeyException", 400, format!("unknown {ARN}")),
            ("NotFoundException", 400, format!("unknown {ARN}")),
            (
                "DisabledException",
                400,
                "KMS error DisabledException".to_owned(),
            ),
            (
                "AccessDeniedException",
                400,
                "KMS error AccessDeniedException".to_owned(),
            ),
            (
                "ThrottlingException",
                400,
                "KMS error ThrottlingException".to_owned(),
            ),
            (
                "KMSInternalException",
                500,
                "KMS error KMSInternalException".to_owned(),
            ),
        ];
        for (code, status, expected) in cases {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(answer(
                    status,
                    json!({ "__type": code, "message": "marker@example.com" }),
                ))
                .mount(&server)
                .await;
            let e = kms.unwrap(store, &wrapped).await.unwrap_err();
            assert_eq!(kind(&e), expected, "{code}");
            assert!(!e.to_string().contains("marker"));
            let e = kms.generate(store).await.unwrap_err();
            assert!(!e.to_string().contains("marker"));
        }
        // A plaintext of another length, or none, is not a data key.
        for body in [
            json!({ "Plaintext": b64(&[1; 16]), "KeyId": ARN }),
            json!({ "KeyId": ARN }),
        ] {
            server.reset().await;
            Mock::given(method("POST"))
                .respond_with(answer(200, body))
                .mount(&server)
                .await;
            assert!(matches!(
                kms.unwrap(store, &wrapped).await,
                Err(KeyError::Unwrap)
            ));
        }
        // A locally wrapped key never reaches KMS.
        server.reset().await;
        let local = WrappedKey {
            kek_id: "local:3f9a1c04b2e87d55".into(),
            blob: vec![0; 60],
        };
        assert!(matches!(
            kms.unwrap(store, &local).await,
            Err(KeyError::UnknownMasterKey { kek_id }) if kek_id == local.kek_id
        ));
        assert!(bodies(&server).await.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_kms_cannot_hold_a_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let held = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                open.push(stream);
            }
        });
        let conf = KmsKeyProvider::with_timeouts(
            aws_sdk_kms::config::Builder::new()
                .region(Region::new("eu-west-1"))
                .credentials_provider(Credentials::new("AKIDTEST", "secret", None, None, "t"))
                .endpoint_url(format!("http://{addr}")),
        )
        .build();
        let kms = KmsKeyProvider::from_conf(conf, ARN);
        let started = tokio::time::Instant::now();
        assert!(matches!(
            kms.generate(StoreId::from_uuid(Uuid::nil())).await,
            Err(KeyError::Unavailable(_))
        ));
        assert!(started.elapsed() <= 2 * OPERATION_TIMEOUT);
        held.abort();
    }
}

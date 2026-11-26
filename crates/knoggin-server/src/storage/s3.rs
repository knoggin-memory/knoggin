// SPDX-License-Identifier: AGPL-3.0-only
//! `S3Storage`: any S3-compatible service through `aws-sdk-s3`. Links are presigned by the
//! SDK; listing and deleting are SDK calls. With `storage.endpoint_url` set (a service that
//! is not AWS) checksums are calculated and validated only when required, because such
//! services may not accept the newer ones, and path-style addresses follow
//! `storage.force_path_style`.

use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use aws_config::SdkConfig;
use aws_sdk_s3::Client;
use aws_sdk_s3::config::{RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::presigning::{PresignedRequest, PresigningConfig};
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use knoggin_core::Timestamp;

use super::{ObjectInfo, ObjectStorage, SignedRequest, StorageError};
use crate::settings::StorageSettings;

/// S3's limit on keys per delete call.
const DELETE_BATCH: usize = 1000;

/// The snapshot bucket on S3 or an S3-compatible service.
#[derive(Debug, Clone)]
pub struct S3Storage {
    client: Client,
    bucket: String,
}

impl S3Storage {
    /// Loads the AWS config (region, `storage.credentials_file` or the default chain) and
    /// builds the client.
    pub async fn connect(settings: &StorageSettings) -> Self {
        let sdk = crate::aws::sdk_config(
            settings.region.as_deref(),
            settings.credentials_file.as_deref(),
        )
        .await;
        Self::from_sdk_config(&sdk, settings)
    }

    /// Builds the client from a loaded config and the storage settings.
    pub fn from_sdk_config(sdk: &SdkConfig, settings: &StorageSettings) -> Self {
        Self::from_conf(Self::s3_config(sdk, settings), settings.bucket.clone())
    }

    /// The S3 client config for these settings.
    pub fn s3_config(sdk: &SdkConfig, settings: &StorageSettings) -> aws_sdk_s3::Config {
        let mut conf = aws_sdk_s3::config::Builder::from(sdk)
            .behavior_version(crate::aws::behavior_version())
            .force_path_style(settings.force_path_style);
        if let Some(endpoint) = &settings.endpoint_url {
            conf = conf
                .endpoint_url(endpoint)
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        }
        conf.build()
    }

    /// A storage over a ready client config.
    pub fn from_conf(conf: aws_sdk_s3::Config, bucket: String) -> Self {
        Self {
            client: Client::from_conf(conf),
            bucket,
        }
    }
}

fn presigning(ttl: Duration, now: Timestamp) -> Result<PresigningConfig, StorageError> {
    let millis = u64::try_from(now.millis()).map_err(|_| StorageError::Invalid("time"))?;
    PresigningConfig::builder()
        .start_time(UNIX_EPOCH + Duration::from_millis(millis))
        .expires_in(ttl)
        .build()
        .map_err(|_| StorageError::Invalid("link lifetime"))
}

fn signed(req: &PresignedRequest, ttl: Duration, now: Timestamp) -> SignedRequest {
    let ttl_ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
    SignedRequest {
        method: req.method().to_owned(),
        url: req.uri().to_owned(),
        headers: req
            .headers()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("host"))
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect(),
        expires_ms: now.millis().saturating_add(ttl_ms),
    }
}

/// A 5xx, a throttle or a transport failure is an outage; any other answer is a refusal
/// with the service's code. Messages and addresses are dropped.
fn sdk_error<E: ProvideErrorMetadata, R>(err: &SdkError<E, R>) -> StorageError {
    match err {
        SdkError::ServiceError(service) => {
            let code = service.err().code().unwrap_or("unknown");
            if matches!(code, "SlowDown" | "InternalError" | "ServiceUnavailable") {
                StorageError::Unavailable
            } else {
                StorageError::Refused(code.to_owned())
            }
        }
        _ => StorageError::Unavailable,
    }
}

#[async_trait]
impl ObjectStorage for S3Storage {
    async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>, StorageError> {
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        let mut out = Vec::new();
        while let Some(page) = pages.next().await {
            let page = page.map_err(|e| sdk_error(&e))?;
            for object in page.contents() {
                let Some(key) = object.key() else { continue };
                let last_modified = object
                    .last_modified()
                    .and_then(|t| t.to_millis().ok())
                    .map_or(Timestamp::from_millis(0), Timestamp::from_millis);
                out.push(ObjectInfo {
                    key: key.to_owned(),
                    size: object
                        .size()
                        .and_then(|s| u64::try_from(s).ok())
                        .unwrap_or(0),
                    last_modified,
                });
            }
        }
        Ok(out)
    }

    async fn presign_get(
        &self,
        key: &str,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError> {
        let req = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .presigned(presigning(ttl, now)?)
            .await
            .map_err(|_| StorageError::Invalid("get link"))?;
        Ok(signed(&req, ttl, now))
    }

    async fn presign_put_new(
        &self,
        key: &str,
        len: u64,
        ttl: Duration,
        now: Timestamp,
    ) -> Result<SignedRequest, StorageError> {
        let len = i64::try_from(len).map_err(|_| StorageError::Invalid("length"))?;
        let req = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .content_length(len)
            .if_none_match("*")
            .presigned(presigning(ttl, now)?)
            .await
            .map_err(|_| StorageError::Invalid("put link"))?;
        Ok(signed(&req, ttl, now))
    }

    async fn delete(&self, keys: &[String]) -> Result<(), StorageError> {
        for batch in keys.chunks(DELETE_BATCH) {
            let objects = batch
                .iter()
                .map(|k| ObjectIdentifier::builder().key(k).build())
                .collect::<Result<Vec<_>, _>>()
                .map_err(|_| StorageError::Invalid("key"))?;
            let delete = Delete::builder()
                .set_objects(Some(objects))
                .quiet(true)
                .build()
                .map_err(|_| StorageError::Invalid("delete"))?;
            let out = self
                .client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(delete)
                .send()
                .await
                .map_err(|e| sdk_error(&e))?;
            if let Some(failed) = out.errors().first() {
                return Err(StorageError::Refused(
                    failed.code().unwrap_or("unknown").to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use aws_config::Region;
    use aws_sdk_s3::config::Credentials;

    use super::*;

    fn settings(endpoint: Option<&str>, path_style: bool) -> StorageSettings {
        StorageSettings {
            bucket: "snapshots".into(),
            region: Some("eu-west-1".into()),
            endpoint_url: endpoint.map(str::to_owned),
            force_path_style: path_style,
            credentials_file: None,
            link_ttl_seconds: 300,
        }
    }

    fn storage(settings: &StorageSettings) -> S3Storage {
        let sdk = SdkConfig::builder()
            .behavior_version(crate::aws::behavior_version())
            .region(Region::new("eu-west-1"))
            .credentials_provider(aws_sdk_s3::config::SharedCredentialsProvider::new(
                Credentials::new("AKIDTEST", "secret", None, None, "test"),
            ))
            .build();
        S3Storage::from_sdk_config(&sdk, settings)
    }

    fn query(url: &str, name: &str) -> Option<String> {
        url.split_once('?')?
            .1
            .split('&')
            .find_map(|p| p.strip_prefix(&format!("{name}=")))
            .map(|v| v.replace("%3B", ";").replace("%2F", "/"))
    }

    const NOW: Timestamp = Timestamp::from_millis(1_795_000_000_000);

    #[tokio::test]
    async fn an_upload_link_signs_its_length_and_create_only() {
        for s in [
            settings(None, false),
            settings(Some("http://127.0.0.1:9"), true),
        ] {
            let st = storage(&s);
            let key = "stores/x/snap/0000000001-0000000001.kgs";
            let req = st
                .presign_put_new(key, 1234, Duration::from_secs(300), NOW)
                .await
                .unwrap();
            assert_eq!(req.method, "PUT");
            let signed = query(&req.url, "X-Amz-SignedHeaders").unwrap();
            let names: Vec<&str> = signed.split(';').collect();
            assert!(names.contains(&"content-length"), "{signed}");
            assert!(names.contains(&"if-none-match"), "{signed}");
            assert!(names.contains(&"host"), "{signed}");
            let headers: Vec<(String, String)> = req
                .headers
                .iter()
                .map(|(n, v)| (n.to_ascii_lowercase(), v.clone()))
                .collect();
            assert!(headers.contains(&("content-length".into(), "1234".into())));
            assert!(headers.contains(&("if-none-match".into(), "*".into())));
            assert!(!headers.iter().any(|(n, _)| n == "host"));
            // No checksum computed over a body the signer never saw.
            assert!(
                !headers.iter().any(|(n, _)| n.starts_with("x-amz-checksum")),
                "{headers:?}"
            );
            assert_eq!(query(&req.url, "X-Amz-Expires").as_deref(), Some("300"));
            assert_eq!(
                query(&req.url, "X-Amz-Date").as_deref(),
                Some("20261118T110640Z")
            );
            assert_eq!(req.expires_ms, NOW.millis() + 300_000);
            if s.endpoint_url.is_some() {
                assert!(
                    req.url
                        .starts_with(&format!("http://127.0.0.1:9/snapshots/{key}?")),
                    "path style"
                );
            } else {
                assert!(
                    req.url.starts_with(&format!(
                        "https://snapshots.s3.eu-west-1.amazonaws.com/{key}?"
                    )),
                    "virtual host"
                );
            }
        }
    }

    #[tokio::test]
    async fn a_download_link() {
        let st = storage(&settings(Some("http://127.0.0.1:9"), true));
        let req = st
            .presign_get("selftest/a", Duration::from_secs(1), NOW)
            .await
            .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(query(&req.url, "x-id").as_deref(), Some("GetObject"));
        assert!(query(&req.url, "X-Amz-Signature").is_some());
        assert!(req.headers.is_empty(), "{:?}", req.headers);
        assert!(!format!("{req:?}").contains("Signature"));
    }

    fn list_page(keys: &[&str], next: Option<&str>) -> String {
        let contents: String = keys
            .iter()
            .map(|k| {
                format!(
                    "<Contents><Key>{k}</Key><LastModified>2026-11-18T11:06:40.000Z</LastModified>\
                     <Size>42</Size></Contents>"
                )
            })
            .collect();
        let more = match next {
            Some(t) => format!(
                "<IsTruncated>true</IsTruncated><NextContinuationToken>{t}</NextContinuationToken>"
            ),
            None => "<IsTruncated>false</IsTruncated>".into(),
        };
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
             <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
             <Name>snapshots</Name><Prefix>stores/</Prefix><KeyCount>{}</KeyCount>{more}{contents}\
             </ListBucketResult>",
            keys.len()
        )
    }

    #[tokio::test]
    async fn listing_follows_pages_on_a_path_style_endpoint() {
        use wiremock::matchers::{method, path, query_param, query_param_is_missing};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/snapshots/"))
            .and(query_param("list-type", "2"))
            .and(query_param("prefix", "stores/"))
            .and(query_param_is_missing("continuation-token"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(list_page(&["stores/a", "stores/b"], Some("t2"))),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/snapshots/"))
            .and(query_param("continuation-token", "t2"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(list_page(&["stores/c"], None)),
            )
            .mount(&server)
            .await;
        let st = storage(&settings(Some(&server.uri()), true));
        let listed = st.list("stores/").await.unwrap();
        let keys: Vec<&str> = listed.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, ["stores/a", "stores/b", "stores/c"]);
        assert_eq!(listed[0].size, 42);
        assert_eq!(listed[0].last_modified, NOW);
    }

    #[tokio::test]
    async fn deleting_goes_in_batches_and_errors_keep_no_detail() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/snapshots/"))
            .and(query_param("delete", ""))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><DeleteResult></DeleteResult>",
            ))
            .mount(&server)
            .await;
        let st = storage(&settings(Some(&server.uri()), true));
        let keys: Vec<String> = (0..1500).map(|i| format!("stores/k{i:04}")).collect();
        st.delete(&keys).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2);
        let first = String::from_utf8(requests[0].body.clone()).unwrap();
        assert!(
            first.contains("<Key>stores/k0000</Key>") && first.contains("<Key>stores/k0999</Key>")
        );
        assert!(!first.contains("<Key>stores/k1000</Key>"));
        let second = String::from_utf8(requests[1].body.clone()).unwrap();
        assert!(second.contains("<Key>stores/k1499</Key>"));

        let refusing = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code>\
                 <Message>secret detail</Message></Error>",
            ))
            .mount(&refusing)
            .await;
        let st = storage(&settings(Some(&refusing.uri()), true));
        assert_eq!(
            st.list("stores/").await.unwrap_err(),
            StorageError::Refused("AccessDenied".into())
        );
        let st = storage(&settings(Some("http://127.0.0.1:1"), true));
        assert_eq!(
            st.list("stores/").await.unwrap_err(),
            StorageError::Unavailable
        );
    }

    #[test]
    fn checksums_only_when_required_off_aws() {
        let sdk = SdkConfig::builder()
            .behavior_version(crate::aws::behavior_version())
            .build();
        let off_aws = S3Storage::s3_config(&sdk, &settings(Some("http://127.0.0.1:9"), true));
        assert_eq!(
            off_aws.request_checksum_calculation(),
            Some(&RequestChecksumCalculation::WhenRequired)
        );
        assert_eq!(
            off_aws.response_checksum_validation(),
            Some(&ResponseChecksumValidation::WhenRequired)
        );
        let aws = S3Storage::s3_config(&sdk, &settings(None, false));
        assert_ne!(
            aws.request_checksum_calculation(),
            Some(&RequestChecksumCalculation::WhenRequired)
        );
    }
}

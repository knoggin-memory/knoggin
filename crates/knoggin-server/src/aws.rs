// SPDX-License-Identifier: AGPL-3.0-only
//! The shared AWS SDK configuration: one dated behaviour version, so that an SDK update
//! cannot change retry or checksum defaults unseen, and the credentials of
//! `storage.credentials_file` when it is set. S3, DynamoDB, KMS and ECS use it.

use std::path::Path;

pub use aws_config::SdkConfig;
use aws_config::profile::ProfileFileCredentialsProvider;
#[allow(
    deprecated,
    reason = "aws-config offers these only under their old names; aws-runtime is no dependency"
)]
use aws_config::profile::profile_file::{ProfileFileKind, ProfileFiles};
use aws_config::{BehaviorVersion, Region};

/// The behaviour version every AWS client is built with.
#[allow(deprecated, reason = "pinned on purpose")]
pub fn behavior_version() -> BehaviorVersion {
    BehaviorVersion::v2026_11_17()
}

/// Loads the shared config: `region` if given, the credentials in `credentials_file`
/// (AWS shared-credentials format, default profile) if given, the default chain otherwise.
#[allow(
    deprecated,
    reason = "aws-config offers these only under their old names; aws-runtime is no dependency"
)]
pub async fn sdk_config(region: Option<&str>, credentials_file: Option<&Path>) -> SdkConfig {
    let mut loader = aws_config::defaults(behavior_version());
    if let Some(region) = region {
        loader = loader.region(Region::new(region.to_owned()));
    }
    if let Some(path) = credentials_file {
        let files = ProfileFiles::builder()
            .with_file(ProfileFileKind::Credentials, path)
            .build();
        loader = loader.credentials_provider(
            ProfileFileCredentialsProvider::builder()
                .profile_files(files)
                .build(),
        );
    }
    loader.load().await
}

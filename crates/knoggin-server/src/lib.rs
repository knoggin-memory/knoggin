// SPDX-License-Identifier: AGPL-3.0-only
//! What only the multi-user server needs (tab "Server").
//!
//! The foundation holds the settings, the pinned internal link with its secrets and
//! tokens, object storage, and the launcher interface. The router role (`router`, with
//! `lease`, `pool`, `reconcile` and `rollout`) and the container role and broker are
//! built on top of these; `CONTRACT.md` says where each goes.

pub mod admin;
pub mod aws;
pub mod broker;
pub mod container;
mod hex;
pub mod kms;
pub mod launcher;
pub mod launcher_service;
pub mod lease;
pub mod lifecycle;
pub mod link;
pub mod pool;
pub mod reconcile;
pub mod rollout;
pub mod router;
pub mod settings;
pub mod storage;
pub mod testing;

pub use launcher::{
    BuildInfo, ContainerEnv, ContainerInfo, ContainerSpec, LaunchError, LaunchHandle, Launcher,
};
pub use storage::{ObjectInfo, ObjectStorage, SignedRequest, StorageError};

/// A mutex's guard; a poisoned lock is used as it is (no code here leaves one half done).
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A JSON answer, 200. The serialized bytes are wiped once copied into the body, since a
/// broker answer can carry a data key.
pub(crate) fn json(value: &impl serde::Serialize) -> axum::response::Response {
    use axum::response::IntoResponse;
    // The answers are structs of numbers and strings; they always serialize.
    let bytes = zeroize::Zeroizing::new(serde_json::to_vec(value).unwrap_or_default());
    (
        [(
            axum::http::header::CONTENT_TYPE,
            axum::http::HeaderValue::from_static("application/json"),
        )],
        bytes::Bytes::copy_from_slice(&bytes),
    )
        .into_response()
}

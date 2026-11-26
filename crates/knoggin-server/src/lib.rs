// SPDX-License-Identifier: AGPL-3.0-only
//! What only the multi-user server needs (tab "Server").
//!
//! This milestone's foundation holds the settings, the pinned internal link with its
//! secrets and tokens, object storage, and the launcher interface. The router, the broker,
//! the container role, the lease and pool logic, the reconcile loop, the lifecycle task,
//! the admin commands and the launcher service are built on top of these; `CONTRACT.md`
//! says where each goes.

pub mod aws;
mod hex;
pub mod launcher;
pub mod link;
pub mod settings;
pub mod storage;

pub use launcher::{
    BuildInfo, ContainerEnv, ContainerInfo, ContainerSpec, LaunchError, LaunchHandle, Launcher,
};
pub use storage::{ObjectInfo, ObjectStorage, SignedRequest, StorageError};

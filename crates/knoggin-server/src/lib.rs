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

// SPDX-License-Identifier: AGPL-3.0-only
//! The Knoggin engine: one store of memories in memory, with search, decay and graph.
//!
//! Every operation is a plain, synchronous, deterministic function. The engine reads no
//! clock and no random source: the caller passes the current time and any new IDs.

pub mod decay;
pub mod embed;
pub mod error;
pub mod graph;
pub mod id;
pub mod model;
pub mod ops;
pub mod rif;
pub mod search;
pub mod sensitive;
pub mod store;
pub mod testing;
pub mod time;

pub use error::{EngineError, ErrorCode};
pub use id::{MemoryId, NamespaceId, StoreId, UserId};
pub use sensitive::Sensitive;
pub use store::Store;
pub use time::Timestamp;

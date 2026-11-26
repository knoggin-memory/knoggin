// SPDX-License-Identifier: AGPL-3.0-only
//! The state database: users, stores, API-key hashes, sessions, leases and containers
//! behind one interface, `StateDb`, with typed helpers that are the only code knowing
//! key layouts. It never holds memory content.

pub mod api_keys;
pub mod db;
pub mod erase;
mod keys;
pub mod leases;
pub mod local;
pub mod meta;
pub mod postgres;
pub mod record;
pub mod seal;
pub mod sessions;
pub mod stores;
pub mod testing;
mod typed;
pub mod users;

pub use api_keys::{ApiKeys, NewApiKey};
pub use db::{Clock, Cond, Item, MAX_WRITES, StateDb, StateError, Write, system_clock};
pub use erase::{EraseReport, erase};
pub use leases::{Containers, Leases};
pub use local::LocalFileDb;
pub use meta::{Meta, rewrap_master_key};
pub use postgres::PostgresDb;
pub use record::Versioned;
pub use seal::FieldKeys;
pub use sessions::Sessions;
pub use stores::Stores;
pub use users::{NewStore, NewUser, Users};

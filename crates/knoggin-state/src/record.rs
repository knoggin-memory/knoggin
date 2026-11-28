// SPDX-License-Identifier: AGPL-3.0-only
//! Every record of the state database, field for field, and how a record becomes a sealed
//! document. Field names are the JSON keys, in `snake_case`; a field that is not set is `null`.

use std::fmt;
use std::str::FromStr;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use knoggin_core::time::Timestamp;
use knoggin_core::{Sensitive, StoreId, UserId};
use knoggin_crypto::WrappedKey;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::db::{Cond, Item, StateError};
use crate::seal::FieldKeys;

/// Ten minutes: how long a lease keeps its `claims` and `abnormal_ends`.
pub const LEASE_WINDOW_MS: i64 = 10 * 60 * 1000;

/// A record type: its document is this struct as JSON, with the fields in `SEALED` sealed.
pub trait Record: Serialize + DeserializeOwned {
    /// Fields sealed with the field key. Each is a string or `null`.
    const SEALED: &'static [&'static str] = &[];
}

/// A record as read, with its version. Keeps the stored document so that a write keeps
/// the sealed string of every value that did not change.
#[derive(Clone)]
pub struct Versioned<T> {
    /// The record.
    pub value: T,
    /// The item's version, for `Cond::Version`.
    pub version: u64,
    /// The item's own expiry.
    pub expires_ms: Option<i64>,
    pub(crate) doc: Value,
}

impl<T> Versioned<T> {
    /// The condition that the item is still as read.
    pub fn cond(&self) -> Cond {
        Cond::Version(self.version)
    }
}

impl<T: fmt::Debug> fmt::Debug for Versioned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Versioned")
            .field("value", &self.value)
            .field("version", &self.version)
            .field("expires_ms", &self.expires_ms)
            .finish_non_exhaustive()
    }
}

/// `Absent` for a record that was not there, else `Version(n)`.
pub fn cond_of<T>(read: Option<&Versioned<T>>) -> Cond {
    read.map_or(Cond::Absent, Versioned::cond)
}

/// The document of `value` for the item `pk`/`sk`. A sealed field whose value equals the
/// one in `prev` keeps `prev`'s sealed string.
pub(crate) fn encode<T: Record>(
    keys: Option<&FieldKeys>,
    pk: &str,
    sk: &str,
    value: &T,
    prev: Option<&Value>,
) -> Result<Value, StateError> {
    let mut doc = serde_json::to_value(value)
        .map_err(|_| StateError::Invalid("a record that is not JSON"))?;
    for &field in T::SEALED {
        let Some(Value::String(plain)) = doc.get(field) else {
            continue;
        };
        let keys = keys.ok_or(StateError::Invalid("sealing without the field key"))?;
        let kept = prev
            .and_then(|p| p.get(field))
            .and_then(Value::as_str)
            .filter(|old| {
                keys.open_field(pk, sk, field, old)
                    .is_ok_and(|o| &o == plain)
            })
            .map(str::to_owned);
        let sealed = match kept {
            Some(s) => s,
            None => keys.seal_field(pk, sk, field, plain)?,
        };
        doc[field] = Value::String(sealed);
    }
    Ok(doc)
}

/// The record in `item`, with its sealed fields opened.
pub(crate) fn decode<T: Record>(
    keys: Option<&FieldKeys>,
    item: Item,
) -> Result<Versioned<T>, StateError> {
    let mut plain = item.doc.clone();
    for &field in T::SEALED {
        let Some(Value::String(sealed)) = plain.get(field) else {
            continue;
        };
        let keys = keys.ok_or(StateError::Invalid("opening without the field key"))?;
        let opened = keys.open_field(&item.pk, &item.sk, field, sealed)?;
        plain[field] = Value::String(opened);
    }
    let value = serde_json::from_value(plain).map_err(|_| StateError::Corrupt("a record"))?;
    Ok(Versioned {
        value,
        version: item.version,
        expires_ms: item.expires_ms,
        doc: item.doc,
    })
}

/// A record whose document is not personal and is read without the field key.
pub(crate) fn decode_plain<T: Record>(item: Item) -> Result<Versioned<T>, StateError> {
    decode(None, item)
}

/// `YYYY-MM-DD` in UTC, the form of every `_day` field.
pub fn utc_day(now_ms: i64) -> String {
    Timestamp::from_millis(now_ms)
        .utc_date()
        .unwrap_or_default()
}

/// Who deleted a user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletedBy {
    /// The user deleted their own account and may undo it.
    #[serde(rename = "self")]
    Owner,
    /// An admin removed the user.
    Admin,
}

/// `user#<user id>` / `-`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    /// The user's ID.
    pub id: UserId,
    /// The sign-in issuer; `local` for the one user of a local install or a server without sign-in.
    pub issuer: String,
    /// The sign-in subject. Sealed.
    pub subject: Sensitive<String>,
    /// The hash in the key of the sign-in index item, kept so that the lifecycle task can delete it.
    pub sub_hash: String,
    /// Sealed.
    pub email: Option<Sensitive<String>>,
    /// Whether the provider said the email was verified.
    pub email_verified: bool,
    /// Sealed.
    pub name: Option<Sensitive<String>>,
    /// The user's personal store.
    pub personal_store_id: StoreId,
    /// When the user was created.
    pub created_ms: i64,
    /// `YYYY-MM-DD` of the last request whose credential passed the whole check.
    pub last_active_day: Option<String>,
    /// Set when the account is deleted.
    pub deleted_at_ms: Option<i64>,
    /// Who deleted it.
    pub deleted_by: Option<DeletedBy>,
}

impl Record for User {
    const SEALED: &'static [&'static str] = &["subject", "email", "name"];
}

/// What kind of store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreKind {
    /// The user's personal store; every user has one.
    Personal,
    /// An app.
    App,
}

/// The newest committed snapshot of a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Newest {
    /// Lease generation.
    pub generation: u64,
    /// Sequence within the generation.
    pub sequence: u64,
}

/// What one container serves. Written `user:<uuid>` and `store:<uuid>` in keys, the link
/// header and logs; `{"user": "<uuid>"}` or `{"store": "<uuid>"}` in JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Slot {
    /// A user's own container: the personal store and every app of that user that has no
    /// container of its own.
    User(UserId),
    /// The container of one isolated or shared app.
    Store(StoreId),
}

impl fmt::Display for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::User(u) => write!(f, "user:{u}"),
            Self::Store(s) => write!(f, "store:{s}"),
        }
    }
}

/// A text that is not `user:<uuid>` or `store:<uuid>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("not a slot")]
pub struct NotASlot;

impl FromStr for Slot {
    type Err = NotASlot;

    fn from_str(text: &str) -> Result<Self, NotASlot> {
        let (kind, id) = text.split_once(':').ok_or(NotASlot)?;
        // Only the hyphenated form that `Display` writes.
        if id.len() != 36 {
            return Err(NotASlot);
        }
        let id = Uuid::parse_str(id).map_err(|_| NotASlot)?;
        match kind {
            "user" => Ok(Self::User(UserId::from_uuid(id))),
            "store" => Ok(Self::Store(StoreId::from_uuid(id))),
            _ => Err(NotASlot),
        }
    }
}

/// The lease (slot and generation) whose container last opened a store. Only that lease
/// may save the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// The container's slot.
    pub slot: Slot,
    /// The lease's generation at that open.
    pub generation: u64,
}

/// `user-stores#<user id>` / `<store id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreRecord {
    /// The store's ID.
    pub id: StoreId,
    /// Its owner.
    pub user_id: UserId,
    /// Personal or app.
    pub kind: StoreKind,
    /// Sealed. Unique per user, ignoring case; change it only with `Stores::rename`.
    pub name: Sensitive<String>,
    /// When the store was created.
    pub created_ms: i64,
    /// Set when the store is marked deleted.
    pub deleted_at_ms: Option<i64>,
    /// The data key, wrapped; base64url without padding.
    pub wrapped_key: String,
    /// Names the master key that wrapped it.
    pub kek_id: String,
    /// Memories at the last commit.
    pub memory_count: u64,
    /// Namespaces at the last commit.
    pub namespace_count: u64,
    /// The last write's time.
    pub last_write_ms: Option<i64>,
    /// The client of the last write. Sealed.
    pub last_write_client: Option<Sensitive<String>>,
    /// The newest committed snapshot.
    pub newest: Option<Newest>,
    /// A snapshot name an admin chose to restore.
    pub restore_from: Option<String>,
    /// Served by a container of its own. Never true for the personal store.
    #[serde(default)]
    pub isolated: bool,
    /// Shared with other people; always served by a container of its own.
    #[serde(default)]
    pub shared: bool,
    /// The lease whose container last opened the store; written only by the broker's open.
    #[serde(default)]
    pub holder: Option<Holder>,
}

impl Record for StoreRecord {
    const SEALED: &'static [&'static str] = &["name", "last_write_client"];
}

impl StoreRecord {
    /// A new store with no snapshot yet.
    pub fn new(
        id: StoreId,
        user_id: UserId,
        kind: StoreKind,
        name: String,
        wrapped: &WrappedKey,
        now: i64,
    ) -> Self {
        Self {
            id,
            user_id,
            kind,
            name: Sensitive::new(name),
            created_ms: now,
            deleted_at_ms: None,
            wrapped_key: URL_SAFE_NO_PAD.encode(&wrapped.blob),
            kek_id: wrapped.kek_id.clone(),
            memory_count: 0,
            namespace_count: 0,
            last_write_ms: None,
            last_write_client: None,
            newest: None,
            restore_from: None,
            isolated: false,
            shared: false,
            holder: None,
        }
    }

    /// Where the store is served: a container of its own when it is isolated or shared,
    /// else its owner's container. Every place that needs to know asks this.
    pub fn slot(&self) -> Slot {
        if self.isolated || self.shared {
            Slot::Store(self.id)
        } else {
            Slot::User(self.user_id)
        }
    }

    /// The wrapped data key, for `KeyProvider::unwrap`.
    pub fn wrapped(&self) -> Result<WrappedKey, StateError> {
        Ok(WrappedKey {
            kek_id: self.kek_id.clone(),
            blob: URL_SAFE_NO_PAD
                .decode(&self.wrapped_key)
                .map_err(|_| StateError::Corrupt("a wrapped key"))?,
        })
    }

    /// Replaces the wrapped data key (master-key rotation).
    pub fn set_wrapped(&mut self, wrapped: &WrappedKey) {
        self.wrapped_key = URL_SAFE_NO_PAD.encode(&wrapped.blob);
        self.kek_id = wrapped.kek_id.clone();
    }
}

/// The two permissions of an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Permission {
    /// Read memories.
    #[serde(rename = "memory:read")]
    Read,
    /// Write memories.
    #[serde(rename = "memory:write")]
    Write,
}

/// `key#<hash of the whole key>` / `-`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiKey {
    /// The key's ID.
    pub id: Uuid,
    /// The one store it reaches.
    pub store_id: StoreId,
    /// The store's owner.
    pub user_id: UserId,
    /// Sealed.
    pub name: Sensitive<String>,
    /// The first 8 characters of the random part.
    pub prefix: String,
    /// `memory:read`, or both.
    pub permissions: Vec<Permission>,
    /// When the key was created.
    pub created_ms: i64,
    /// When the key stops working (not the item's own expiry).
    pub key_expires_ms: i64,
    /// `YYYY-MM-DD` of the last use.
    pub last_used_day: Option<String>,
    /// Set when the key is revoked.
    pub revoked_ms: Option<i64>,
}

impl Record for ApiKey {
    const SEALED: &'static [&'static str] = &["name"];
}

/// 30 days: a key's records stay this long after it expired or was revoked.
pub const KEY_RECORD_GRACE_MS: i64 = 30 * 24 * 60 * 60 * 1000;

impl ApiKey {
    /// Whether the key works at `now`: not revoked and not expired.
    pub fn is_active(&self, now: i64) -> bool {
        self.revoked_ms.is_none() && self.key_expires_ms > now
    }

    /// The item's own expiry: 30 days after the key expired, or after it was revoked if sooner.
    pub fn item_expires_ms(&self) -> i64 {
        let end = self
            .revoked_ms
            .map_or(self.key_expires_ms, |r| r.min(self.key_expires_ms));
        end.saturating_add(KEY_RECORD_GRACE_MS)
    }
}

/// `store-keys#<store id>` / `<key id>`: the same document plus `hash`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreKey {
    /// Hex SHA-256 of the whole key, the key of its `key#` copy.
    pub hash: String,
    /// The key.
    #[serde(flatten)]
    pub key: ApiKey,
}

impl Record for StoreKey {
    const SEALED: &'static [&'static str] = &["name"];
}

/// `session#<hash of the cookie value>` / `-`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Whose session.
    pub user_id: UserId,
    /// When it was created.
    pub created_ms: i64,
    /// The last request, at most a minute stale.
    pub last_seen_ms: i64,
    /// On a local install, whoever signed in (verified). Sealed.
    pub email: Option<Sensitive<String>>,
    /// On a local install, whoever signed in. Sealed.
    pub name: Option<Sensitive<String>>,
}

impl Record for Session {
    const SEALED: &'static [&'static str] = &["email", "name"];
}

/// `user-sessions#<user id>` / `<session hash>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionIndex {
    pub(crate) created_ms: i64,
}

impl Record for SessionIndex {}

/// A lease's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseState {
    /// No container.
    Parked,
    /// A router is handing a container the slot.
    Claiming,
    /// The container serves the slot.
    Warm,
}

/// Why a router asks a container to park.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParkReason {
    /// An urgent upgrade.
    Upgrade,
    /// The container's lifetime, or a generation that must rise.
    Lifetime,
    /// Its host drains.
    Drain,
    /// An admin restores a store.
    Restore,
    /// The account was deleted.
    Deleted,
    /// The router stops.
    Shutdown,
    /// A store it holds now belongs to another container.
    Moved,
}

/// How a warm container ended abnormally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndKind {
    /// Killed for memory.
    Memory,
    /// Any other failure.
    Failed,
}

/// One abnormal end of a lease's container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbnormalEnd {
    /// When.
    pub at_ms: i64,
    /// How.
    pub kind: EndKind,
}

/// `lease#<slot>` / `-`. A missing lease is `Lease::default()`: parked, generation 0.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lease {
    /// Parked, claiming or warm.
    pub state: LeaseState,
    /// Rises with every claim; snapshot names carry it.
    pub generation: u64,
    /// The container, copied at claim time with the next four fields.
    pub container_id: Option<Uuid>,
    /// The launcher's handle.
    pub handle: Option<String>,
    /// IPv4 address and port.
    pub address: Option<String>,
    /// The container's certificate pin.
    pub cert_sha256: Option<String>,
    /// The container's build.
    pub build: Option<String>,
    /// When the state last changed.
    pub state_since_ms: i64,
    /// The router that claimed.
    pub claimed_by: Option<String>,
    park_requested: Option<ParkReason>,
    park_requested_ms: Option<i64>,
    claims: Vec<i64>,
    abnormal_ends: Vec<AbnormalEnd>,
}

impl Record for Lease {}

impl Default for Lease {
    fn default() -> Self {
        Self {
            state: LeaseState::Parked,
            generation: 0,
            container_id: None,
            handle: None,
            address: None,
            cert_sha256: None,
            build: None,
            state_since_ms: 0,
            claimed_by: None,
            park_requested: None,
            park_requested_ms: None,
            claims: Vec::new(),
            abnormal_ends: Vec::new(),
        }
    }
}

impl Lease {
    /// The generation of the next claim: the old one plus 1, or the current Unix time in
    /// seconds if that is more, so names keep rising after a restore from a backup.
    pub fn next_generation(&self, now_ms: i64) -> u64 {
        let secs = u64::try_from(now_ms.div_euclid(1000)).unwrap_or(0);
        self.generation.saturating_add(1).max(secs)
    }

    /// Changes the state. Parking also clears the park request: no other write does.
    pub fn set_state(&mut self, state: LeaseState, now: i64) {
        if state == LeaseState::Parked {
            self.park_requested = None;
            self.park_requested_ms = None;
        }
        self.state = state;
        self.state_since_ms = now;
    }

    /// Asks the container to park. A request already there keeps its reason and time;
    /// returns whether this one was recorded.
    pub fn request_park(&mut self, reason: ParkReason, now: i64) -> bool {
        if self.park_requested.is_some() {
            return false;
        }
        self.park_requested = Some(reason);
        self.park_requested_ms = Some(now);
        true
    }

    /// The standing park request.
    pub fn park_requested(&self) -> Option<ParkReason> {
        self.park_requested
    }

    /// When the standing park request was first made.
    pub fn park_requested_ms(&self) -> Option<i64> {
        self.park_requested_ms
    }

    /// Records that this lease became warm; keeps the last 10 minutes.
    pub fn add_claim(&mut self, now: i64) {
        self.claims.retain(|&t| t > now - LEASE_WINDOW_MS);
        self.claims.push(now);
    }

    /// The claims of the last 10 minutes.
    pub fn claims(&self, now: i64) -> usize {
        self.claims
            .iter()
            .filter(|&&t| t > now - LEASE_WINDOW_MS)
            .count()
    }

    /// Records an abnormal end; keeps the last 10 minutes.
    pub fn add_abnormal_end(&mut self, now: i64, kind: EndKind) {
        self.abnormal_ends
            .retain(|e| e.at_ms > now - LEASE_WINDOW_MS);
        self.abnormal_ends.push(AbnormalEnd { at_ms: now, kind });
    }

    /// The abnormal ends of the last 10 minutes, oldest first.
    pub fn abnormal_ends(&self, now: i64) -> Vec<AbnormalEnd> {
        self.abnormal_ends
            .iter()
            .filter(|e| e.at_ms > now - LEASE_WINDOW_MS)
            .copied()
            .collect()
    }
}

/// A container's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContainerState {
    /// Launched, loading the model.
    Starting,
    /// In the pool.
    Ready,
    /// Serves one slot.
    Claimed,
    /// Finished; kept an hour for diagnosis.
    Gone,
}

/// Why a router stopped a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// It did not answer `status`.
    Unresponsive,
    /// A park request stood for 2 hours.
    ParkTimeout,
    /// Past its lifetime.
    Lifetime,
    /// Its host drains.
    Drain,
    /// An older build in the pool.
    Rollout,
    /// More ready containers than the target.
    Surplus,
    /// No record names it.
    Stray,
    /// It refused or missed its claim.
    ClaimFailed,
    /// No valid `hello` in time.
    LaunchFailed,
}

/// `containers` / `<container id>`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Container {
    /// The Knoggin container ID.
    pub id: Uuid,
    /// The launcher's handle, once `launch` returned.
    pub handle: Option<String>,
    /// IPv4 address and port.
    pub address: Option<String>,
    /// Certificate pin.
    pub cert_sha256: Option<String>,
    /// The build its `hello` reported.
    pub build: String,
    /// What the launcher needed to start it.
    pub spec: String,
    /// Starting, ready, claimed or gone.
    pub state: ContainerState,
    /// The slot, once claimed.
    #[serde(default)]
    pub slot: Option<Slot>,
    /// The user whose container limit it counts toward: the owner of what it serves.
    #[serde(default)]
    pub charged_to: Option<UserId>,
    /// When it was claimed.
    #[serde(default)]
    pub claimed_ms: Option<i64>,
    /// When it was launched.
    pub started_ms: i64,
    /// When it is retired.
    pub retire_at_ms: i64,
    /// The host it runs on.
    pub host: Option<String>,
    stop_reason: Option<StopReason>,
    /// When it became gone.
    pub gone_ms: Option<i64>,
}

impl Record for Container {}

impl Container {
    /// A container about to be launched, in state `starting`.
    pub fn starting(id: Uuid, build: String, spec: String, now: i64, retire_at_ms: i64) -> Self {
        Self {
            id,
            handle: None,
            address: None,
            cert_sha256: None,
            build,
            spec,
            state: ContainerState::Starting,
            slot: None,
            charged_to: None,
            claimed_ms: None,
            started_ms: now,
            retire_at_ms,
            host: None,
            stop_reason: None,
            gone_ms: None,
        }
    }

    /// Records why a router stops it. The first reason stays and is never cleared;
    /// returns whether this one was recorded.
    pub fn set_stop_reason(&mut self, reason: StopReason) -> bool {
        if self.stop_reason.is_some() {
            return false;
        }
        self.stop_reason = Some(reason);
        true
    }

    /// Why a router stopped it, if one did.
    pub fn stop_reason(&self) -> Option<StopReason> {
        self.stop_reason
    }
}

/// `leader` / `reconcile`. The item expires at `until_ms`, so an expired lease is absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Leader {
    /// The leading router.
    pub router_id: String,
    /// Its build.
    pub build: String,
    /// Leadership ends unless renewed by then.
    pub until_ms: i64,
}

impl Record for Leader {}

/// A build and what the launcher needs to start it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSpec {
    /// The release version.
    pub build: String,
    /// The ECS task definition revision; empty on one host.
    pub spec: String,
}

/// `meta` / `rollout`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rollout {
    /// The build claims use.
    pub claim: BuildSpec,
    /// The build the pool moves to.
    pub desired: BuildSpec,
    /// Containers started before this are retired.
    pub retire_before_ms: Option<i64>,
}

impl Record for Rollout {}

/// `meta` / `field-key`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FieldKeyRecord {
    pub(crate) wrapped_key: String,
    pub(crate) kek_id: String,
}

impl Record for FieldKeyRecord {}

/// `meta` / `schema`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Schema {
    pub(crate) version: u64,
}

impl Record for Schema {}

/// `{ "user_id" }`: the sign-in index and the store ID index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct UserRef {
    pub(crate) user_id: UserId,
}

impl Record for UserRef {}

/// `{ "store_id" }`: the store name item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoreRef {
    pub(crate) store_id: StoreId,
}

impl Record for StoreRef {}

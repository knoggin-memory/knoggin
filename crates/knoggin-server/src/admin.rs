// SPDX-License-Identifier: AGPL-3.0-only
//! The `knoggin admin` commands that act on a server: restore, master-key rotation,
//! `retire-before`, the snapshot list and the self-test with its five bucket checks. They
//! read and write the state
//! database, the bucket and the key provider directly; where a running router must act,
//! they write a request that the reconcile loop carries out. The binary prints.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use knoggin_core::snapshot::{SnapshotName, SnapshotObject, thin};
use knoggin_core::{StoreId, Timestamp};
use knoggin_crypto::source::{FileRule, read_key_file, replace_key_file};
use knoggin_crypto::{KeyProvider, LocalKeyProvider};
use knoggin_state::record::{LeaseState, ParkReason, Slot, StoreRecord};
use knoggin_state::{Clock, FieldKeys, Leases, Meta, StateDb, StateError, Stores};
use serde_json::{Value, json};

use crate::storage::{ObjectStorage, SignedRequest, StorageError, names};

/// How long `restore` waits for the owner's lease to be parked.
pub const RESTORE_WAIT: Duration = Duration::from_secs(150);

/// Why an admin command failed.
#[derive(Debug, thiserror::Error)]
pub enum AdminError {
    /// The state database failed.
    #[error("the state database: {0}")]
    State(#[from] StateError),
    /// The bucket failed.
    #[error("the bucket: {0}")]
    Storage(#[from] StorageError),
    /// The key file or the key provider failed.
    #[error("the master key: {0}")]
    Keys(String),
    /// What the command was asked cannot be done; the message says why.
    #[error("{0}")]
    Refused(String),
}

/// Parses `<generation>-<sequence>`, padding shorter numbers to 10 digits.
pub fn parse_snapshot(text: &str) -> Result<SnapshotName, AdminError> {
    let bad = || AdminError::Refused(format!("{text:?} is not <generation>-<sequence>"));
    let (g, s) = text.trim().split_once('-').ok_or_else(bad)?;
    let number = |p: &str| {
        if p.is_empty() || p.len() > 10 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        p.parse::<u64>().map_err(|_| bad())
    };
    SnapshotName::new(number(g)?, number(s)?).map_err(|_| bad())
}

/// What `retire-before` sets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetireBefore {
    /// The current time.
    Now,
    /// A time given in RFC 3339.
    At(Timestamp),
    /// Removes the value.
    Clear,
}

impl std::str::FromStr for RetireBefore {
    type Err = AdminError;

    fn from_str(text: &str) -> Result<Self, AdminError> {
        match text {
            "now" => Ok(Self::Now),
            "clear" => Ok(Self::Clear),
            t => Timestamp::parse_rfc3339(t).map(Self::At).map_err(|_| {
                AdminError::Refused(format!(
                    "{t:?} is not `now`, `clear` or an RFC 3339 time such as 2026-11-23T10:00:00Z"
                ))
            }),
        }
    }
}

/// The admin commands' view of a deployment: the state database (with the field key,
/// which the admin commands unwrap at start), the bucket and a clock.
pub struct Admin {
    /// The state database.
    pub db: Arc<dyn StateDb>,
    /// Stores, sealed with the field key.
    pub stores: Stores,
    /// The bucket.
    pub storage: Arc<dyn ObjectStorage>,
    /// The time.
    pub clock: Clock,
}

impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admin").finish_non_exhaustive()
    }
}

/// What `restore` found once it had written the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreOutcome {
    /// The owner's lease is parked: the next open loads the chosen snapshot.
    Parked,
    /// The lease did not park in time: the restore takes effect when the user's container
    /// next parks.
    Waiting,
}

impl Admin {
    /// Over `db` with the field keys, and the bucket.
    pub fn new(
        db: Arc<dyn StateDb>,
        fields: Arc<FieldKeys>,
        storage: Arc<dyn ObjectStorage>,
        clock: Clock,
    ) -> Self {
        Self {
            stores: Stores::new(db.clone(), fields),
            db,
            storage,
            clock,
        }
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    async fn record(&self, store: StoreId) -> Result<StoreRecord, AdminError> {
        Ok(self
            .stores
            .get(store)
            .await?
            .ok_or_else(|| AdminError::Refused(format!("there is no store {store}")))?
            .value)
    }

    /// `admin restore --store <id> --snapshot <name>`, the operator's part of a restore:
    /// checks the object exists, writes `restore_from`, asks the owner's
    /// container to park, and waits up to `wait` for the lease to be parked.
    pub async fn restore(
        &self,
        store: StoreId,
        snapshot: SnapshotName,
        wait: Duration,
    ) -> Result<RestoreOutcome, AdminError> {
        let key = names::snapshot_key(store, snapshot);
        let record = self.record(store).await?;
        if !self
            .storage
            .list(&names::snapshot_prefix(store))
            .await?
            .iter()
            .any(|o| o.key == key)
        {
            return Err(AdminError::Refused(format!(
                "the store {store} has no snapshot {snapshot}"
            )));
        }
        let user = record.user_id;
        // Only the container that serves the store parks, and, in the middle of a slot
        // change, the one that still holds it. The holder's is done once its lease is
        // parked or has moved on to another generation.
        let leases = Leases::new(self.db.clone());
        let mut targets: Vec<(Slot, Option<u64>)> = vec![(record.slot(), None)];
        if let Some(holder) = record.holder
            && holder.slot != record.slot()
            && let Some(lease) = leases.get(holder.slot).await?
            && lease.value.state != LeaseState::Parked
            && lease.value.generation == holder.generation
        {
            targets.push((holder.slot, Some(holder.generation)));
        }
        let name = snapshot.to_string();
        self.stores
            .update(user, store, |r| {
                r.restore_from = Some(name.clone());
                Ok::<_, StateError>(())
            })
            .await?;
        for &(slot, _) in &targets {
            for _ in 0..5 {
                let Some(read) = leases.get(slot).await? else {
                    break;
                };
                if read.value.state == LeaseState::Parked {
                    break;
                }
                let mut next = read.value.clone();
                if !next.request_park(ParkReason::Restore, self.now()) {
                    break;
                }
                match leases.put(slot, &next, read.cond()).await {
                    Ok(()) => break,
                    Err(StateError::Conflict) => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let mut parked = true;
            for &(slot, generation) in &targets {
                let done = leases.get(slot).await?.is_none_or(|l| {
                    l.value.state == LeaseState::Parked
                        || generation.is_some_and(|g| l.value.generation != g)
                });
                parked &= done;
            }
            if parked {
                return Ok(RestoreOutcome::Parked);
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(RestoreOutcome::Waiting);
            }
            tokio::time::sleep(Duration::from_secs(1).min(wait)).await;
        }
    }

    /// `admin restore --store <id> --cancel`: clears `restore_from`.
    pub async fn cancel_restore(&self, store: StoreId) -> Result<(), AdminError> {
        let record = self.record(store).await?;
        self.stores
            .update(record.user_id, store, |r| {
                r.restore_from = None;
                Ok::<_, StateError>(())
            })
            .await?;
        Ok(())
    }

    /// `admin retire-before`: sets or clears the rollout record's `retire_before_ms`
    /// (the leader then parks the older containers in batches) and returns how many
    /// leases are still active.
    pub async fn retire_before(&self, at: RetireBefore) -> Result<usize, AdminError> {
        let meta = Meta::new(self.db.clone());
        let value = match at {
            RetireBefore::Now => Some(self.now()),
            RetireBefore::At(t) => Some(t.millis()),
            RetireBefore::Clear => None,
        };
        let mut written = false;
        for _ in 0..5 {
            let Some(read) = meta.rollout().await? else {
                return Err(AdminError::Refused(
                    "no router has started against this database yet".into(),
                ));
            };
            let mut next = read.value.clone();
            next.retire_before_ms = value;
            match meta.put_rollout(&next, read.cond()).await {
                Ok(()) => {
                    written = true;
                    break;
                }
                Err(StateError::Conflict) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !written {
            return Err(StateError::Unavailable("the rollout record kept changing".into()).into());
        }
        let leases = Leases::new(self.db.clone());
        let mut count = 0;
        let mut after = None;
        loop {
            let page = leases.list_active(100, after).await?;
            count += page.len();
            match page.last() {
                Some(last) if page.len() == 100 => after = Some(*last),
                _ => return Ok(count),
            }
        }
    }

    /// `admin snapshots --store <id>`: the shape of `GET /v1/admin/stores/{id}/snapshots`.
    pub async fn snapshots(
        &self,
        store: StoreId,
        retention_days: u32,
    ) -> Result<Value, AdminError> {
        let record = self.record(store).await?;
        Ok(snapshot_list(
            &*self.storage,
            &record,
            retention_days,
            Timestamp::from_millis(self.now()),
        )
        .await?)
    }
}

/// A store's snapshots, newest first, each with the label `thin` gives it: the body of
/// `GET /v1/admin/stores/{id}/snapshots` and of `knoggin admin snapshots`.
pub async fn snapshot_list(
    storage: &dyn ObjectStorage,
    record: &StoreRecord,
    retention_days: u32,
    now: Timestamp,
) -> Result<Value, StorageError> {
    let prefix = names::snapshot_prefix(record.id);
    let mut objects: Vec<(SnapshotObject, u64)> = storage
        .list(&prefix)
        .await?
        .iter()
        .filter_map(|o| {
            let name = names::name_in(&prefix, &o.key)?;
            Some((
                SnapshotObject {
                    name,
                    last_modified: o.last_modified,
                },
                o.size,
            ))
        })
        .collect();
    objects.sort_by_key(|o| std::cmp::Reverse(o.0.name));
    let restore_from = record
        .restore_from
        .as_deref()
        .and_then(|t| t.parse::<SnapshotName>().ok());
    let newest = record
        .newest
        .and_then(|n| SnapshotName::new(n.generation, n.sequence).ok());
    let plain: Vec<SnapshotObject> = objects.iter().map(|(o, _)| *o).collect();
    let labels = thin(&plain, restore_from, newest, retention_days, now);
    let items: Vec<Value> = objects
        .iter()
        .zip(labels)
        .map(|((o, size), kept)| {
            json!({
                "name": o.name.to_string(),
                "generation": o.name.generation(),
                "sequence": o.name.sequence(),
                "sizeBytes": size,
                "lastModified": o.last_modified.to_rfc3339(),
                "keptBecause": kept,
            })
        })
        .collect();
    Ok(json!({ "items": items }))
}

/// `admin rotate-master-key` on one host, with the router stopped: adds a new master key
/// to the key set file as `current` (written to a temporary file, synced and renamed over
/// the old one, mode 0444 like the other secret files), then wraps the field key and every
/// store's data key again under it. The file comes first: if the second step stops, the
/// set still holds the old key, and running the command again finishes the work. Returns
/// the new key's `kek_id` and the number of stores re-wrapped.
pub async fn rotate_master_key(
    db: Arc<dyn StateDb>,
    key_file: &Path,
) -> Result<(String, usize), AdminError> {
    let keys_err = |e: &dyn std::fmt::Display| AdminError::Keys(e.to_string());
    let path = key_file.to_owned();
    let set = tokio::task::spawn_blocking(move || {
        let mut set = read_key_file(&path, FileRule::NotWritableByOthers)?;
        set.rotate()?;
        replace_key_file(&path, &set)?;
        std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o444))
            .map_err(knoggin_crypto::source::SourceError::Io)?;
        Ok::<_, knoggin_crypto::source::SourceError>(set)
    })
    .await
    .map_err(|_| AdminError::Keys("the key file task stopped".into()))?
    .map_err(|e| keys_err(&e))?;
    let kek_id = set.current_kek_id();
    let provider = LocalKeyProvider::new(set);
    let fields = Arc::new(
        Meta::new(db.clone())
            .field_keys(&provider)
            .await
            .map_err(|e| keys_err(&e))?,
    );
    let count = knoggin_state::rewrap_master_key(db, fields, &provider).await?;
    Ok((kek_id, count))
}

/// One of the five checks of the bucket self-test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BucketStep {
    /// 1. A signed create-only upload succeeds.
    Upload,
    /// 2. The same upload again is refused with 412.
    CreateOnly,
    /// 3. Without `If-None-Match`, with another `Content-Length`, or without its
    ///    signature, the upload is refused.
    Signed,
    /// 4. A link signed 10 seconds ago with a 1-second lifetime is refused.
    Expiry,
    /// 5. A listing shows the object with the right size at once.
    Listing,
}

impl BucketStep {
    /// The check's name, as `admin selftest` prints it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::CreateOnly => "create_only",
            Self::Signed => "signed",
            Self::Expiry => "expiry",
            Self::Listing => "listing",
        }
    }
}

/// A bucket check that failed, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("bucket check {} failed: {reason}", step.as_str())]
pub struct BucketFailure {
    /// Which check.
    pub step: BucketStep,
    /// Why, without any address or credential.
    pub reason: String,
}

const SELFTEST_BODY: &[u8] = b"knoggin bucket self-test\n";

/// The five bucket checks, with objects under `selftest/`. `http` sends the signed
/// requests (no redirects); `now` is the signing time. The router runs this at start and
/// refuses to start if it fails; `admin selftest` runs it too.
pub async fn bucket_selftest(
    storage: &dyn ObjectStorage,
    http: &reqwest::Client,
    now: Timestamp,
) -> Result<(), BucketFailure> {
    let fail = |step: BucketStep, reason: String| BucketFailure { step, reason };
    let ttl = Duration::from_secs(60);
    let len = SELFTEST_BODY.len() as u64;
    let new_key = || format!("{}{}", names::SELFTEST, uuid::Uuid::now_v7());
    let key = new_key();
    let sign = |key: String, ttl: Duration, at: Timestamp| async move {
        storage.presign_put_new(&key, len, ttl, at).await
    };

    // 1.
    let link = sign(key.clone(), ttl, now)
        .await
        .map_err(|e| fail(BucketStep::Upload, e.to_string()))?;
    let status = send(http, &link, SELFTEST_BODY.to_vec(), Probe::AsSigned)
        .await
        .map_err(|e| fail(BucketStep::Upload, e))?;
    if !status.is_success() {
        return Err(fail(BucketStep::Upload, format!("the upload got {status}")));
    }
    // 2.
    let again = send(http, &link, SELFTEST_BODY.to_vec(), Probe::AsSigned)
        .await
        .map_err(|e| fail(BucketStep::CreateOnly, e))?;
    if again != reqwest::StatusCode::PRECONDITION_FAILED {
        return Err(fail(
            BucketStep::CreateOnly,
            format!("a second upload to the same name got {again}, not 412"),
        ));
    }
    // 3. On a fresh name, so that only the signature can refuse them.
    let fresh = sign(new_key(), ttl, now)
        .await
        .map_err(|e| fail(BucketStep::Signed, e.to_string()))?;
    for (probe, what) in [
        (Probe::WithoutIfNoneMatch, "without If-None-Match"),
        (Probe::LongerBody, "with another Content-Length"),
        (Probe::WithoutSignature, "without its signature"),
    ] {
        let status = send(http, &fresh, SELFTEST_BODY.to_vec(), probe)
            .await
            .map_err(|e| fail(BucketStep::Signed, e))?;
        if status.is_success() {
            return Err(fail(
                BucketStep::Signed,
                format!("an upload {what} was accepted"),
            ));
        }
    }
    // 4. Signed as of 10 seconds ago with a 1-second lifetime: no wait needed.
    let stale = sign(
        new_key(),
        Duration::from_secs(1),
        Timestamp::from_millis(now.millis().saturating_sub(10_000)),
    )
    .await
    .map_err(|e| fail(BucketStep::Expiry, e.to_string()))?;
    let status = send(http, &stale, SELFTEST_BODY.to_vec(), Probe::AsSigned)
        .await
        .map_err(|e| fail(BucketStep::Expiry, e))?;
    if status.is_success() {
        return Err(fail(
            BucketStep::Expiry,
            "an upload with an expired link was accepted".into(),
        ));
    }
    // 5.
    let listed = storage
        .list(names::SELFTEST)
        .await
        .map_err(|e| fail(BucketStep::Listing, e.to_string()))?;
    match listed.iter().find(|o| o.key == key) {
        Some(o) if o.size == len => Ok(()),
        Some(o) => Err(fail(
            BucketStep::Listing,
            format!("the listing gives {} bytes, not {len}", o.size),
        )),
        None => Err(fail(
            BucketStep::Listing,
            "the listing does not show the new object".into(),
        )),
    }
}

/// How a self-test upload departs from its signed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    AsSigned,
    WithoutIfNoneMatch,
    LongerBody,
    WithoutSignature,
}

/// Sends `link` with `body`; the status. Errors name no address: a link is a secret.
async fn send(
    http: &reqwest::Client,
    link: &SignedRequest,
    mut body: Vec<u8>,
    probe: Probe,
) -> Result<reqwest::StatusCode, String> {
    let mut url = link.url.clone();
    if probe == Probe::WithoutSignature {
        let (base, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
        let kept: Vec<&str> = query
            .split('&')
            .filter(|p| !p.starts_with("X-Amz-Signature="))
            .collect();
        url = format!("{base}?{}", kept.join("&"));
    }
    let method = reqwest::Method::from_bytes(link.method.as_bytes())
        .map_err(|_| "a bad method".to_owned())?;
    let mut req = http.request(method, url);
    for (name, value) in &link.headers {
        let lower = name.to_ascii_lowercase();
        if lower == "content-length"
            || (probe == Probe::WithoutIfNoneMatch && lower == "if-none-match")
        {
            continue;
        }
        req = req.header(name.as_str(), value.as_str());
    }
    if probe == Probe::LongerBody {
        body.push(b'x');
    }
    let answer = tokio::time::timeout(Duration::from_secs(30), req.body(body).send())
        .await
        .map_err(|_| "the bucket did not answer in time".to_owned())?
        .map_err(|e| format!("the bucket cannot be reached ({})", e.without_url()))?;
    Ok(answer.status())
}

/// What `admin selftest` found: each part `Ok` or why it failed.
#[derive(Debug)]
pub struct SelftestReport {
    /// One read of the state database.
    pub database: Result<(), String>,
    /// A data key made under the nil store ID and unwrapped again.
    pub key_provider: Result<(), String>,
    /// The five bucket checks.
    pub bucket: Result<(), BucketFailure>,
}

impl SelftestReport {
    /// Whether every part passed.
    pub fn passed(&self) -> bool {
        self.database.is_ok() && self.key_provider.is_ok() && self.bucket.is_ok()
    }
}

/// `admin selftest`: the state database (one read), the key provider and the bucket.
pub async fn selftest(
    db: Arc<dyn StateDb>,
    keys: &dyn KeyProvider,
    storage: &dyn ObjectStorage,
    http: &reqwest::Client,
    now: Timestamp,
) -> SelftestReport {
    // The schema read is the health check.
    let database = Meta::new(db)
        .check_schema()
        .await
        .map_err(|e| e.to_string());
    let key_provider = async {
        let (key, wrapped) = keys
            .generate(knoggin_crypto::KeyScope::STATE)
            .await
            .map_err(|e| e.to_string())?;
        let back = keys
            .unwrap(knoggin_crypto::KeyScope::STATE, &wrapped)
            .await
            .map_err(|e| e.to_string())?;
        if knoggin_crypto::token::eq(key.expose(), back.expose()) {
            Ok(())
        } else {
            Err("the key did not unwrap to itself".to_owned())
        }
    }
    .await;
    let bucket = bucket_selftest(storage, http, now).await;
    SelftestReport {
        database,
        key_provider,
        bucket,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_are_padded() {
        assert_eq!(
            parse_snapshot("12-3").unwrap().to_string(),
            "0000000012-0000000003"
        );
        assert_eq!(
            parse_snapshot("0000000012-0000000003").unwrap(),
            SnapshotName::new(12, 3).unwrap()
        );
        for bad in ["12", "-3", "12-", "a-1", "1-2-3", "12345678901-1", "1 -2"] {
            assert!(parse_snapshot(bad).is_err(), "{bad}");
        }
        assert!(parse_snapshot("9999999999-1").is_ok());
    }

    #[test]
    fn retire_before_takes_now_clear_or_a_time() {
        assert_eq!("now".parse::<RetireBefore>().unwrap(), RetireBefore::Now);
        assert_eq!(
            "clear".parse::<RetireBefore>().unwrap(),
            RetireBefore::Clear
        );
        assert!(matches!(
            "2026-11-23T10:00:00Z".parse::<RetireBefore>().unwrap(),
            RetireBefore::At(_)
        ));
        assert!("yesterday".parse::<RetireBefore>().is_err());
    }
}

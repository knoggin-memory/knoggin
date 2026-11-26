// SPDX-License-Identifier: AGPL-3.0-only
//! Object keys in the bucket: the one place that knows `stores/<store id>/snap/`. The
//! snapshot name itself comes from `knoggin_core::snapshot::SnapshotName`.

use knoggin_core::StoreId;
use knoggin_core::snapshot::SnapshotName;
use uuid::Uuid;

/// The prefix of every store's objects.
pub const STORES: &str = "stores/";

/// The prefix of the bucket self-test's objects.
pub const SELFTEST: &str = "selftest/";

const SNAP: &str = "/snap/";

/// `stores/<store id>/`: every object of one store, which the lifecycle task erases.
pub fn store_prefix(store: StoreId) -> String {
    format!("{STORES}{store}/")
}

/// `stores/<store id>/snap/`: the store's snapshot folder.
pub fn snapshot_prefix(store: StoreId) -> String {
    format!("{STORES}{store}{SNAP}")
}

/// `stores/<store id>/snap/<generation:010>-<sequence:010>.kgs`.
pub fn snapshot_key(store: StoreId, name: SnapshotName) -> String {
    format!("{STORES}{store}{SNAP}{}", name.file_name())
}

/// Parses a snapshot key, strictly: `stores/<lower-case hyphenated UUID>/snap/<10
/// digits>-<10 digits>.kgs` and nothing else, so no encoded or odd character gets through.
/// The one-host blob route checks the raw path with it before any decoding.
pub fn parse_snapshot_key(key: &str) -> Option<(StoreId, SnapshotName)> {
    let rest = key.strip_prefix(STORES)?;
    let (id, file) = rest.split_once(SNAP)?;
    let uuid = Uuid::parse_str(id).ok()?;
    if uuid.hyphenated().to_string() != id {
        return None;
    }
    let name = SnapshotName::from_file_name(file).ok()?;
    Some((StoreId::from_uuid(uuid), name))
}

/// The snapshot name of a key listed under `prefix` (from [`snapshot_prefix`]); stray
/// objects are `None`.
pub fn name_in(prefix: &str, key: &str) -> Option<SnapshotName> {
    SnapshotName::from_file_name(key.strip_prefix(prefix)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_round_trip_and_odd_keys_are_refused() {
        let id = Uuid::parse_str("0190a5c4-1111-7222-8333-444455556666").unwrap();
        let store = StoreId::from_uuid(id);
        let name = SnapshotName::new(1_795_000_000, 3).unwrap();
        let key = snapshot_key(store, name);
        assert_eq!(
            key,
            "stores/0190a5c4-1111-7222-8333-444455556666/snap/1795000000-0000000003.kgs"
        );
        assert_eq!(parse_snapshot_key(&key), Some((store, name)));
        assert!(key.starts_with(&snapshot_prefix(store)));
        assert!(key.starts_with(&store_prefix(store)));
        assert_eq!(name_in(&snapshot_prefix(store), &key), Some(name));
        for bad in [
            "stores/0190A5C4-1111-7222-8333-444455556666/snap/1795000000-0000000003.kgs",
            "stores/0190a5c411117222833344445555666/snap/1795000000-0000000003.kgs",
            "stores/{0190a5c4-1111-7222-8333-444455556666}/snap/1795000000-0000000003.kgs",
            "stores/0190a5c4-1111-7222-8333-444455556666/snap/1795000000-3.kgs",
            "stores/0190a5c4-1111-7222-8333-444455556666/snap/1795000000-0000000003.kgs.tmp",
            "stores/0190a5c4-1111-7222-8333-444455556666/snap/../x/1795000000-0000000003.kgs",
            "stores/0190a5c4-1111-7222-8333-444455556666/snap/1795000000-0000000003%2ekgs",
            "stores/0190a5c4-1111-7222-8333-444455556666%2Fsnap/1795000000-0000000003.kgs",
            "/stores/0190a5c4-1111-7222-8333-444455556666/snap/1795000000-0000000003.kgs",
            "selftest/x",
        ] {
            assert_eq!(parse_snapshot_key(bad), None, "{bad}");
        }
    }
}

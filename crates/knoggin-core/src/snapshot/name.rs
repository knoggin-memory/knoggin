// SPDX-License-Identifier: AGPL-3.0-only
//! Snapshot names (`<generation:010>-<sequence:010>`) and `thin`, the server's rule for
//! which snapshots of a store are kept.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::Serialize;

use crate::time::{MILLIS_PER_DAY, Timestamp};

/// The largest generation or sequence a name can hold: 10 decimal digits.
const MAX_NUMBER: u64 = 9_999_999_999;
const DIGITS: usize = 10;
/// The extension of a snapshot file or object.
const EXTENSION: &str = ".kgs";
/// The extension of a snapshot file that is still being written.
const TEMP_EXTENSION: &str = ".kgs.tmp";

/// Why a text or a number is not a snapshot name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    /// Not two groups of 10 decimal digits joined by `-` (with `.kgs` for a file name).
    #[error("not a snapshot name")]
    Malformed,
    /// A generation or a sequence of 10,000,000,000 or more.
    #[error("generation and sequence must be below 10,000,000,000")]
    TooLarge,
}

/// `<generation>-<sequence>`, each zero-padded to 10 digits so that names sort as text
/// in the same order as `(generation, sequence)`. The newest snapshot is the highest name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SnapshotName {
    generation: u64,
    sequence: u64,
}

impl SnapshotName {
    /// A name; refuses numbers of 10,000,000,000 or more, as every writer must.
    pub const fn new(generation: u64, sequence: u64) -> Result<Self, NameError> {
        if generation > MAX_NUMBER || sequence > MAX_NUMBER {
            return Err(NameError::TooLarge);
        }
        Ok(Self {
            generation,
            sequence,
        })
    }

    /// Lease generation on the server; open counter locally.
    pub const fn generation(self) -> u64 {
        self.generation
    }

    /// Counts up within a generation.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    /// `<name>.kgs`: the local file name, and the last part of the object key
    /// `stores/<store id>/snap/<name>.kgs`.
    pub fn file_name(self) -> String {
        format!("{self}{EXTENSION}")
    }

    /// `<name>.kgs.tmp`: the local file a snapshot is written to before it is linked.
    pub fn temp_file_name(self) -> String {
        format!("{self}{TEMP_EXTENSION}")
    }

    /// Parses `<name>.kgs`; anything else (a `.tmp` file, a stray file) is `Malformed`.
    pub fn from_file_name(file_name: &str) -> Result<Self, NameError> {
        file_name
            .strip_suffix(EXTENSION)
            .ok_or(NameError::Malformed)?
            .parse()
    }
}

impl fmt::Display for SnapshotName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:010}-{:010}", self.generation, self.sequence)
    }
}

impl FromStr for SnapshotName {
    type Err = NameError;

    /// Exactly `dddddddddd-dddddddddd`; a shorter, unpadded form is refused, because it
    /// would not sort correctly as text.
    fn from_str(text: &str) -> Result<Self, NameError> {
        let (g, s) = text.split_once('-').ok_or(NameError::Malformed)?;
        let number = |part: &str| {
            if part.len() == DIGITS && part.bytes().all(|b| b.is_ascii_digit()) {
                part.parse::<u64>().map_err(|_| NameError::Malformed)
            } else {
                Err(NameError::Malformed)
            }
        };
        Self::new(number(g)?, number(s)?)
    }
}

/// One stored snapshot as a listing shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotObject {
    /// Its name.
    pub name: SnapshotName,
    /// When the bucket or the file system last wrote it. Only places it in a window.
    pub last_modified: Timestamp,
}

/// Why `thin` keeps a snapshot. When several apply, the first in this order is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeptBecause {
    /// The highest name, or the one the store's record names as newest.
    Newest,
    /// The snapshot an admin chose to restore.
    Restore,
    /// The highest name of a 10-minute window within the last hour.
    LastHour,
    /// The highest name of an hour within the last 24 hours.
    Hourly,
    /// The highest name of a UTC day within the last `retention.days` days.
    Daily,
}

impl KeptBecause {
    /// The label of the admin snapshot list (`keptBecause`).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Newest => "newest",
            Self::Restore => "restore",
            Self::LastHour => "last_hour",
            Self::Hourly => "hourly",
            Self::Daily => "daily",
        }
    }
}

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;

/// The server's thinning rule (tab "Server", the lifecycle task): one label per object,
/// in the order given, or `None` for an object the next run deletes.
///
/// Kept: the highest name and the recorded `newest` (even below unrecorded higher
/// names); `restore_from`; of the last hour (`now - 60 min < t <= now`) the highest
/// name of each 10-minute window; of the last 24 hours the highest of each hour; and
/// the highest of each UTC day of today and the `retention_days - 1` days before.
/// Windows follow the UTC clock. `last_modified` only places an object in a window;
/// within a window the highest name wins, so a late upload never pushes out a higher
/// name. A `last_modified` after `now` (a clock ahead) counts as `now`.
pub fn thin(
    objects: &[SnapshotObject],
    restore_from: Option<SnapshotName>,
    newest: Option<SnapshotName>,
    retention_days: u32,
    now: Timestamp,
) -> Vec<Option<KeptBecause>> {
    let now = now.millis();
    let today = now.div_euclid(MILLIS_PER_DAY);
    let windows = |t: i64| {
        let t = t.min(now);
        let age = now.saturating_sub(t);
        let day = t.div_euclid(MILLIS_PER_DAY);
        [
            (age < HOUR_MS).then(|| (KeptBecause::LastHour, t.div_euclid(10 * MINUTE_MS))),
            (age < 24 * HOUR_MS).then(|| (KeptBecause::Hourly, t.div_euclid(HOUR_MS))),
            (today.saturating_sub(day) < i64::from(retention_days))
                .then_some((KeptBecause::Daily, day)),
        ]
    };
    let mut best: BTreeMap<(KeptBecause, i64), SnapshotName> = BTreeMap::new();
    for o in objects {
        for key in windows(o.last_modified.millis()).into_iter().flatten() {
            let top = best.entry(key).or_insert(o.name);
            *top = (*top).max(o.name);
        }
    }
    let highest = objects.iter().map(|o| o.name).max();
    objects
        .iter()
        .map(|o| {
            if Some(o.name) == highest || Some(o.name) == newest {
                return Some(KeptBecause::Newest);
            }
            if Some(o.name) == restore_from {
                return Some(KeptBecause::Restore);
            }
            windows(o.last_modified.millis())
                .into_iter()
                .flatten()
                .find(|key| best.get(key) == Some(&o.name))
                .map(|(why, _)| why)
        })
        .collect()
}

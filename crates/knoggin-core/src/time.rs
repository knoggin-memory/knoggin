// SPDX-License-Identifier: AGPL-3.0-only
//! Timestamps: milliseconds since the Unix epoch, written in JSON as RFC 3339 UTC strings.
//!
//! The engine reads no clock. The caller passes `now` to every operation that needs it.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Milliseconds in one day.
pub const MILLIS_PER_DAY: i64 = 86_400_000;

/// A point in time, in milliseconds since 1970-01-01T00:00:00Z.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(i64);

/// A text or a number that is not a time the API accepts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TimeError {
    /// Not an RFC 3339 string.
    #[error("not an RFC 3339 time")]
    Format,
    /// Before 1970-01-01 or after 9999-12-31.
    #[error("time is outside 1970-01-01 to 9999-12-31")]
    OutOfRange,
}

impl Timestamp {
    /// 1970-01-01T00:00:00.000Z, the earliest time the API accepts.
    pub const MIN_API: Timestamp = Timestamp(0);
    /// 9999-12-31T23:59:59.999Z, the latest time the API accepts.
    pub const MAX_API: Timestamp = Timestamp(253_402_300_799_999);

    /// Wraps a count of milliseconds.
    pub const fn from_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// The count of milliseconds.
    pub const fn millis(self) -> i64 {
        self.0
    }

    /// Wraps milliseconds that a client sent, refusing values outside 1970 to 9999.
    pub fn from_api_millis(millis: i64) -> Result<Self, TimeError> {
        let t = Self(millis);
        if (Self::MIN_API..=Self::MAX_API).contains(&t) {
            Ok(t)
        } else {
            Err(TimeError::OutOfRange)
        }
    }

    /// Parses an RFC 3339 string with any offset, refusing times outside 1970 to 9999.
    pub fn parse_rfc3339(text: &str) -> Result<Self, TimeError> {
        let parsed =
            time::OffsetDateTime::parse(text, &time::format_description::well_known::Rfc3339)
                .map_err(|_| TimeError::Format)?;
        let millis = parsed.unix_timestamp_nanos().div_euclid(1_000_000);
        let millis = i64::try_from(millis).map_err(|_| TimeError::OutOfRange)?;
        Self::from_api_millis(millis)
    }

    /// The RFC 3339 UTC form with milliseconds, for example `2026-11-23T10:15:30.123Z`.
    ///
    /// `None` for a time outside the years 0 to 9999, which RFC 3339 cannot write.
    pub fn to_rfc3339(self) -> Option<String> {
        let (year, month, day, ms_of_day) = self.civil()?;
        let secs = ms_of_day / 1000;
        Some(format!(
            "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
            secs / 3600,
            secs % 3600 / 60,
            secs % 60,
            ms_of_day % 1000
        ))
    }

    /// The UTC calendar date as `YYYY-MM-DD`, or `None` outside the years 0 to 9999.
    pub fn utc_date(self) -> Option<String> {
        let (year, month, day, _) = self.civil()?;
        Some(format!("{year:04}-{month:02}-{day:02}"))
    }

    /// The UTC date with its weekday, for example `Fri 2023-01-20`, as a compact recall shows it.
    pub fn utc_weekday_date(self) -> Option<String> {
        const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
        let date = self.utc_date()?;
        // 1970-01-01 was a Thursday.
        let weekday = self
            .0
            .div_euclid(MILLIS_PER_DAY)
            .checked_add(4)?
            .rem_euclid(7);
        let name = WEEKDAYS.get(usize::try_from(weekday).ok()?)?;
        Some(format!("{name} {date}"))
    }

    /// Year, month, day and millisecond of the day, for years 0 to 9999 only.
    fn civil(self) -> Option<(i64, i64, i64, i64)> {
        let days = self.0.div_euclid(MILLIS_PER_DAY);
        let ms_of_day = self.0.rem_euclid(MILLIS_PER_DAY);
        let (year, month, day) = civil_from_days(days);
        (0..=9999)
            .contains(&year)
            .then_some((year, month, day, ms_of_day))
    }
}

/// Days since 1970-01-01 to a proleptic Gregorian date (H. Hinnant's algorithm).
///
/// Only called for timestamps, whose day count is far from the `i64` limits.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe.saturating_add(era.saturating_mul(400)) + i64::from(month <= 2);
    (year, month, day)
}

/// Days from `anchor` to `now` as `f32`. A time in the future counts as zero.
///
/// Ported from recalld `time::elapsed_days`: saturate before converting, so the
/// `i64` extremes give a finite number.
pub fn elapsed_days(now: Timestamp, anchor: Timestamp) -> f32 {
    let millis = now.0.saturating_sub(anchor.0).max(0) as f64;
    (millis / 86_400_000.0) as f32
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.to_rfc3339() {
            Some(text) => f.write_str(&text),
            None => write!(f, "{}ms", self.0),
        }
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.to_rfc3339() {
            Some(text) => serializer.serialize_str(&text),
            None => Err(serde::ser::Error::custom(
                "time is outside the years 0 to 9999",
            )),
        }
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        Self::parse_rfc3339(&text).map_err(serde::de::Error::custom)
    }
}

impl utoipa::PartialSchema for Timestamp {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat, Type};
        ObjectBuilder::new()
            .schema_type(Type::String)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::DateTime)))
            .into()
    }
}

impl utoipa::ToSchema for Timestamp {}

/// In TypeScript a timestamp is the RFC 3339 string the API sends.
impl ts_rs::TS for Timestamp {
    type WithoutGenerics = Self;
    type OptionInnerType = Self;

    fn name(_: &ts_rs::Config) -> String {
        "string".to_owned()
    }

    fn inline(_: &ts_rs::Config) -> String {
        "string".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_round_trips_with_milliseconds() {
        let t = Timestamp::parse_rfc3339("2026-11-23T10:15:30.123Z").unwrap();
        assert_eq!(t.to_rfc3339().unwrap(), "2026-11-23T10:15:30.123Z");
        assert_eq!(
            serde_json::to_string(&t).unwrap(),
            "\"2026-11-23T10:15:30.123Z\""
        );
        let back: Timestamp = serde_json::from_str("\"2026-11-23T10:15:30.123Z\"").unwrap();
        assert_eq!(back, t);
    }

    // Ported from recalld `time::tests::test_parse_rfc3339*`.
    #[test]
    fn parse_rfc3339_with_and_without_offset() {
        assert_eq!(
            Timestamp::parse_rfc3339("2024-06-24T10:00:00Z").unwrap(),
            Timestamp::from_millis(1_719_223_200_000)
        );
        assert_eq!(
            Timestamp::parse_rfc3339("2024-06-24T06:00:00-04:00").unwrap(),
            Timestamp::from_millis(1_719_223_200_000)
        );
        assert_eq!(
            Timestamp::parse_rfc3339("not a date"),
            Err(TimeError::Format)
        );
    }

    #[test]
    fn api_bounds_are_1970_to_9999() {
        assert_eq!(
            Timestamp::parse_rfc3339("1969-12-31T23:59:59.999Z"),
            Err(TimeError::OutOfRange)
        );
        assert_eq!(
            Timestamp::parse_rfc3339("1970-01-01T00:00:00Z").unwrap(),
            Timestamp::MIN_API
        );
        assert_eq!(
            Timestamp::parse_rfc3339("9999-12-31T23:59:59.999Z").unwrap(),
            Timestamp::MAX_API
        );
        assert_eq!(
            Timestamp::MAX_API.to_rfc3339().unwrap(),
            "9999-12-31T23:59:59.999Z"
        );
        assert_eq!(Timestamp::from_api_millis(-1), Err(TimeError::OutOfRange));
        assert!(Timestamp::from_millis(i64::MAX).to_rfc3339().is_none());
    }

    #[test]
    fn dates_and_weekdays() {
        let t = Timestamp::parse_rfc3339("2023-01-20T23:59:59.999Z").unwrap();
        assert_eq!(t.utc_date().unwrap(), "2023-01-20");
        assert_eq!(t.utc_weekday_date().unwrap(), "Fri 2023-01-20");
        assert_eq!(
            Timestamp::from_millis(0).utc_weekday_date().unwrap(),
            "Thu 1970-01-01"
        );
        let leap = Timestamp::parse_rfc3339("2024-02-29T12:00:00Z").unwrap();
        assert_eq!(leap.utc_weekday_date().unwrap(), "Thu 2024-02-29");
    }

    // Ported from recalld `manual_clock_and_elapsed_time_are_monotone_and_saturating`,
    // without the clock, which Knoggin does not have.
    #[test]
    fn elapsed_time_is_saturating() {
        let t = Timestamp::from_millis;
        assert_eq!(elapsed_days(t(0), t(1)), 0.0);
        assert_eq!(elapsed_days(t(86_400_000), t(0)), 1.0);
        assert!(elapsed_days(t(i64::MAX), t(i64::MIN)).is_finite());
    }
}

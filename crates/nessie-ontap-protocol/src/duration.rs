//! ISO-8601 duration formatting for the snapshot `delta.time_elapsed` field.

use std::fmt::Write as _;

/// Format a non-negative number of seconds as an ISO-8601 duration
/// (`PT…H…M…S`), the shape ONTAP reports for `snapshot.delta.time_elapsed`.
///
/// Zero (or negative, clamped) renders as `PT0S`. Zero-valued leading components
/// are omitted: 45 → `PT45S`, 3·3600+27·60+45 → `PT3H27M45S`.
#[must_use]
pub fn iso8601_duration(secs: i64) -> String {
    if secs <= 0 {
        return "PT0S".to_string();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let mut out = String::from("PT");
    if h > 0 {
        let _ = write!(out, "{h}H");
    }
    if m > 0 {
        let _ = write!(out, "{m}M");
    }
    if s > 0 {
        let _ = write!(out, "{s}S");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_and_negative_are_pt0s() {
        assert_eq!(iso8601_duration(0), "PT0S");
        assert_eq!(iso8601_duration(-5), "PT0S");
    }

    #[test]
    fn seconds_only() {
        assert_eq!(iso8601_duration(45), "PT45S");
    }

    #[test]
    fn full_components() {
        assert_eq!(iso8601_duration(3 * 3600 + 27 * 60 + 45), "PT3H27M45S");
    }

    #[test]
    fn whole_hours_omit_minutes_and_seconds() {
        assert_eq!(iso8601_duration(7200), "PT2H");
    }
}

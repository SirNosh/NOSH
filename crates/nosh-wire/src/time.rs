//! UTC timestamps in the shape of JavaScript's `Date.prototype.toISOString`.

use time::OffsetDateTime;

/// The current time as `YYYY-MM-DDTHH:MM:SS.mmmZ`, like `nowUtc` in `packages/core/src/time.ts`.
pub fn now_utc() -> String {
    format_utc(OffsetDateTime::now_utc())
}

fn format_utc(t: OffsetDateTime) -> String {
    let t = t.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.millisecond()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_to_iso_string() {
        // 2026-10-14T19:05:09.123456789Z: JavaScript Dates hold milliseconds, so the rest is truncated.
        let t = OffsetDateTime::from_unix_timestamp_nanos(1_792_004_709_123_456_789).unwrap();
        assert_eq!(format_utc(t), "2026-10-14T19:05:09.123Z");
        let east = t.to_offset(time::UtcOffset::from_hms(5, 30, 0).unwrap());
        assert_eq!(format_utc(east), "2026-10-14T19:05:09.123Z");
        assert_eq!(
            format_utc(OffsetDateTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn now_is_a_valid_wire_timestamp() {
        let event = serde_json::json!({ "timestamp": now_utc() });
        assert!(crate::registry::is_wire_timestamp(
            event["timestamp"].as_str().unwrap()
        ));
    }
}

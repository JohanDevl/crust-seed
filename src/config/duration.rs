//! Parser for Vercel's [`ms`](https://github.com/vercel/ms) duration strings.
//!
//! The config file expresses every timeout and cadence in this format
//! (`"30 seconds"`, `"2 weeks"`, `"1d"`), and the original ran the `ms` npm
//! package over them. Values are milliseconds, matching the storage format used
//! throughout the app.

const SECOND: f64 = 1000.0;
const MINUTE: f64 = SECOND * 60.0;
const HOUR: f64 = MINUTE * 60.0;
const DAY: f64 = HOUR * 24.0;
const WEEK: f64 = DAY * 7.0;
const YEAR: f64 = DAY * 365.25;

/// Parses `"1.5 h"`, `"2 weeks"`, `"100"` (bare number = milliseconds).
///
/// Returns `None` for anything `ms()` would return `NaN`/`undefined` for, which
/// the config loader reports as a validation error.
pub fn parse_ms(input: &str) -> Option<f64> {
    let s = input.trim();
    if s.is_empty() || s.len() > 100 {
        return None;
    }

    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let value: f64 = number.parse().ok()?;
    let unit = unit.trim().to_ascii_lowercase();

    let multiplier = match unit.as_str() {
        "" | "ms" | "msec" | "msecs" | "millisecond" | "milliseconds" => 1.0,
        "s" | "sec" | "secs" | "second" | "seconds" => SECOND,
        "m" | "min" | "mins" | "minute" | "minutes" => MINUTE,
        "h" | "hr" | "hrs" | "hour" | "hours" => HOUR,
        "d" | "day" | "days" => DAY,
        "w" | "week" | "weeks" => WEEK,
        "y" | "yr" | "yrs" | "year" | "years" => YEAR,
        _ => return None,
    };

    Some(value * multiplier)
}

/// [`parse_ms`] rounded to whole milliseconds — every stored duration is an
/// integer.
pub fn parse_ms_i64(input: &str) -> Option<i64> {
    parse_ms(input).map(|v| v.round() as i64)
}

/// Renders a millisecond count the way `ms(n, { long: true })` does — used for
/// the job-status payload the web UI displays verbatim.
pub fn format_ms_long(ms: i64) -> String {
    fn plural(ms: f64, n: f64, name: &str) -> String {
        let count = (ms / n).round();
        if count == 1.0 {
            format!("1 {name}")
        } else {
            format!("{count} {name}s")
        }
    }

    let abs = (ms as f64).abs();
    if abs >= DAY {
        plural(ms as f64, DAY, "day")
    } else if abs >= HOUR {
        plural(ms as f64, HOUR, "hour")
    } else if abs >= MINUTE {
        plural(ms as f64, MINUTE, "minute")
    } else if abs >= SECOND {
        plural(ms as f64, SECOND, "second")
    } else {
        format!("{ms} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_formats_the_config_uses() {
        assert_eq!(parse_ms_i64("30 seconds"), Some(30_000));
        assert_eq!(parse_ms_i64("2 weeks"), Some(1_209_600_000));
        assert_eq!(parse_ms_i64("3 days"), Some(259_200_000));
        assert_eq!(parse_ms_i64("30 minutes"), Some(1_800_000));
        assert_eq!(parse_ms_i64("1 day"), Some(86_400_000));
        assert_eq!(parse_ms_i64("2 minutes"), Some(120_000));
        assert_eq!(parse_ms_i64("1.5h"), Some(5_400_000));
        assert_eq!(parse_ms_i64("100"), Some(100));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_ms("soon"), None);
        assert_eq!(parse_ms(""), None);
        assert_eq!(parse_ms("5 fortnights"), None);
    }

    #[test]
    fn long_format_matches_ms_long() {
        assert_eq!(format_ms_long(86_400_000), "1 day");
        assert_eq!(format_ms_long(172_800_000), "2 days");
        assert_eq!(format_ms_long(1_800_000), "30 minutes");
        assert_eq!(format_ms_long(30_000), "30 seconds");
    }
}

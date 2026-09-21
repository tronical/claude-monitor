//! Turning an Anthropic API response into a usage reading.
//!
//! There is no usage endpoint for subscriptions. The numbers ride on the
//! `anthropic-ratelimit-unified-*` response headers of an ordinary
//! `/v1/messages` request, so the device sends the cheapest request it can and
//! only ever looks at the response head.
//!
//! This file depends on nothing but `core`, so it can be tested on the host:
//!
//! ```sh
//! rustc +stable --edition 2024 --test src/usage.rs -o target/usage-tests && target/usage-tests
//! ```

#![cfg_attr(test, allow(dead_code))]

pub const API_HOST: &str = "api.anthropic.com";

/// The request body: the cheapest model, asked for a single token. The
/// completion is thrown away; the headers are the payload.
pub const PROBE_BODY: &str =
    r#"{"model":"claude-haiku-4-5","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#;

/// One reading of the subscription limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    /// Five-hour session utilisation, 0..=100.
    pub session_pct: u8,
    /// Seven-day utilisation, 0..=100.
    pub weekly_pct: u8,
    /// Seconds until the session window resets, as of the response.
    pub session_reset_in: Option<u32>,
    /// Seconds until the weekly window resets, as of the response.
    pub weekly_reset_in: Option<u32>,
    /// A limit has been hit and requests are being rejected.
    pub limited: bool,
}

/// What a response head told us.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Reading(Reading),
    /// The request worked but carried no unified limit headers: the token
    /// belongs to an account without Pro/Max style windows.
    NoLimits,
    /// 401/403: the token is wrong, expired or revoked.
    Unauthorized,
    /// Any other failure without usable headers.
    Http(u16),
    /// Not an HTTP response at all.
    Malformed,
}

/// Parse a response head (status line and headers, up to the blank line).
pub fn parse_response_head(head: &str) -> Outcome {
    let mut lines = head.split("\r\n");
    let Some(status) = lines.next().and_then(parse_status_line) else {
        return Outcome::Malformed;
    };

    let mut session_pct = None;
    let mut weekly_pct = None;
    let mut session_reset_at = None;
    let mut weekly_reset_at = None;
    let mut session_status = None;
    let mut server_time = None;

    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        let is = |expected: &str| name.eq_ignore_ascii_case(expected);

        if is("anthropic-ratelimit-unified-5h-utilization") {
            session_pct = parse_fraction_as_percent(value);
        } else if is("anthropic-ratelimit-unified-7d-utilization") {
            weekly_pct = parse_fraction_as_percent(value);
        } else if is("anthropic-ratelimit-unified-5h-reset") {
            session_reset_at = parse_epoch(value);
        } else if is("anthropic-ratelimit-unified-7d-reset") {
            weekly_reset_at = parse_epoch(value);
        } else if is("anthropic-ratelimit-unified-5h-status") {
            session_status = Some(value);
        } else if is("date") {
            server_time = parse_http_date(value);
        }
    }

    // The limit headers also come back on a 429, which is exactly when they
    // matter most, so look for them before judging the status code.
    let Some(session_pct) = session_pct else {
        return match status {
            401 | 403 => Outcome::Unauthorized,
            200..=299 => Outcome::NoLimits,
            other => Outcome::Http(other),
        };
    };

    // The device has no wall clock. The server's own `Date` header is the
    // reference for turning absolute reset stamps into countdowns.
    let reset_in = |reset_at: Option<i64>| {
        let remaining = reset_at?.checked_sub(server_time?)?;
        Some(remaining.clamp(0, u32::MAX as i64) as u32)
    };

    Outcome::Reading(Reading {
        session_pct,
        weekly_pct: weekly_pct.unwrap_or(0),
        session_reset_in: reset_in(session_reset_at),
        weekly_reset_in: reset_in(weekly_reset_at),
        limited: status == 429 || session_status.is_some_and(|s| !s.starts_with("allowed")),
    })
}

fn parse_status_line(line: &str) -> Option<u16> {
    let mut parts = line.split(' ');
    if !parts.next()?.starts_with("HTTP/1.") {
        return None;
    }
    parts.next()?.parse().ok()
}

/// Utilisation arrives as a 0..1 fraction; the display wants whole percent.
fn parse_fraction_as_percent(value: &str) -> Option<u8> {
    let fraction: f64 = value.parse().ok()?;
    if !fraction.is_finite() {
        return None;
    }
    // `f64::round` is not available in `core`; the value is non-negative after
    // the clamp, so adding a half and truncating rounds to nearest.
    Some((fraction.clamp(0.0, 1.0) * 100.0 + 0.5) as u8)
}

/// Reset stamps are UTC epoch seconds, sometimes with a fractional part.
fn parse_epoch(value: &str) -> Option<i64> {
    let seconds: f64 = value.parse().ok()?;
    (seconds.is_finite() && seconds > 0.0).then_some((seconds + 0.5) as i64)
}

/// Parse an IMF-fixdate (`Sun, 06 Nov 1994 08:49:37 GMT`) into epoch seconds.
fn parse_http_date(value: &str) -> Option<i64> {
    let (_weekday, rest) = value.split_once(", ")?;
    let mut parts = rest.split(' ');
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let mut clock = parts.next()?.split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next()?.parse().ok()?;
    if parts.next()? != "GMT" || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }

    Some(days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK_HEAD: &str = "HTTP/1.1 200 OK\r\n\
        Date: Mon, 21 Sep 2026 09:10:00 GMT\r\n\
        Content-Type: application/json\r\n\
        anthropic-ratelimit-unified-5h-status: allowed\r\n\
        anthropic-ratelimit-unified-5h-utilization: 0.4249\r\n\
        anthropic-ratelimit-unified-5h-reset: 1789989000\r\n\
        Anthropic-Ratelimit-Unified-7d-Utilization: 0.78\r\n\
        anthropic-ratelimit-unified-7d-reset: 1790244000.4\r\n";

    #[test]
    fn http_date_matches_known_epochs() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_http_date("Thu, 29 Feb 2024 12:00:00 GMT"), Some(1_709_208_000));
        assert_eq!(parse_http_date("Mon, 21 Sep 2026 09:10:00 GMT"), Some(1_789_981_800));
        assert_eq!(parse_http_date("yesterday"), None);
        assert_eq!(parse_http_date("Mon, 21 Sep 2026 09:10:00 CET"), None);
    }

    #[test]
    fn reads_usage_from_headers() {
        let Outcome::Reading(reading) = parse_response_head(OK_HEAD) else {
            panic!("expected a reading");
        };
        assert_eq!(reading.session_pct, 42);
        assert_eq!(reading.weekly_pct, 78);
        // 1789989000 - 1789981800
        assert_eq!(reading.session_reset_in, Some(7_200));
        assert_eq!(reading.weekly_reset_in, Some(262_200));
        assert!(!reading.limited);
    }

    #[test]
    fn rate_limited_response_still_yields_a_reading() {
        let head = "HTTP/1.1 429 Too Many Requests\r\n\
            date: Mon, 21 Sep 2026 09:10:00 GMT\r\n\
            anthropic-ratelimit-unified-5h-status: rejected\r\n\
            anthropic-ratelimit-unified-5h-utilization: 1.0\r\n\
            anthropic-ratelimit-unified-5h-reset: 1789981860\r\n";
        let Outcome::Reading(reading) = parse_response_head(head) else {
            panic!("expected a reading");
        };
        assert_eq!(reading.session_pct, 100);
        assert_eq!(reading.session_reset_in, Some(60));
        assert!(reading.limited);
    }

    #[test]
    fn missing_date_leaves_countdowns_unknown() {
        let head = "HTTP/1.1 200 OK\r\n\
            anthropic-ratelimit-unified-5h-utilization: 0.1\r\n\
            anthropic-ratelimit-unified-5h-reset: 1789989000\r\n";
        let Outcome::Reading(reading) = parse_response_head(head) else {
            panic!("expected a reading");
        };
        assert_eq!(reading.session_pct, 10);
        assert_eq!(reading.session_reset_in, None);
    }

    #[test]
    fn a_reset_already_in_the_past_clamps_to_zero() {
        let head = "HTTP/1.1 200 OK\r\n\
            date: Mon, 21 Sep 2026 09:10:00 GMT\r\n\
            anthropic-ratelimit-unified-5h-utilization: 0.1\r\n\
            anthropic-ratelimit-unified-5h-reset: 1789900000\r\n";
        let Outcome::Reading(reading) = parse_response_head(head) else {
            panic!("expected a reading");
        };
        assert_eq!(reading.session_reset_in, Some(0));
    }

    #[test]
    fn classifies_failures() {
        assert_eq!(
            parse_response_head("HTTP/1.1 401 Unauthorized\r\ndate: x\r\n"),
            Outcome::Unauthorized
        );
        assert_eq!(parse_response_head("HTTP/1.1 200 OK\r\n"), Outcome::NoLimits);
        assert_eq!(parse_response_head("HTTP/1.1 529 Overloaded\r\n"), Outcome::Http(529));
        assert_eq!(parse_response_head("SSH-2.0-OpenSSH\r\n"), Outcome::Malformed);
        assert_eq!(parse_response_head(""), Outcome::Malformed);
    }

    #[test]
    fn garbage_utilisation_is_not_a_reading() {
        let head = "HTTP/1.1 200 OK\r\nanthropic-ratelimit-unified-5h-utilization: NaN\r\n";
        assert_eq!(parse_response_head(head), Outcome::NoLimits);
    }
}

//! `Retry-After` parsing (RFC 9110 §10.2.3): delta-seconds or IMF-fixdate.
//! Invalid values are treated as absent.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Parse a Retry-After header value relative to `now`.
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let v = value.trim();
    if v.is_empty() {
        return None;
    }
    if v.bytes().all(|b| b.is_ascii_digit()) {
        // Cap absurd values instead of overflowing; anything > 60s means manual resume anyway.
        let secs: u64 = v.parse().unwrap_or(u64::MAX).min(365 * 24 * 3600);
        return Some(Duration::from_secs(secs));
    }
    let at = parse_imf_fixdate(v)?;
    let now_s = now.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
    Some(Duration::from_secs((at - now_s).max(0) as u64))
}

/// "Sun, 06 Nov 1994 08:49:37 GMT" -> unix seconds.
fn parse_imf_fixdate(s: &str) -> Option<i64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() != 6 || !parts[0].ends_with(',') || parts[5] != "GMT" {
        return None;
    }
    let day: i64 = parts[1].parse().ok()?;
    let month = match parts[2] {
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
    let year: i64 = parts[3].parse().ok()?;
    let hms: Vec<&str> = parts[4].split(':').collect();
    if hms.len() != 3 {
        return None;
    }
    let (h, m, sec): (i64, i64, i64) = (hms[0].parse().ok()?, hms[1].parse().ok()?, hms[2].parse().ok()?);
    if !(1..=31).contains(&day) || h > 23 || m > 59 || sec > 60 || year < 1970 {
        return None;
    }
    Some(days_from_civil(year, month, day) * 86400 + h * 3600 + m * 60 + sec)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_and_date() {
        let now = UNIX_EPOCH + Duration::from_secs(784111777); // 1994-11-06 08:49:37
        assert_eq!(parse_retry_after("5", now), Some(Duration::from_secs(5)));
        assert_eq!(parse_retry_after(" 120 ", now), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:50:07 GMT", now), Some(Duration::from_secs(30)));
        assert_eq!(parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("soon", now), None);
        assert_eq!(parse_retry_after("-1", now), None);
        assert_eq!(parse_retry_after("1.5", now), None);
        assert_eq!(parse_retry_after("", now), None);
    }
}

//! Small shared helpers: clock, hashing, ids, human durations.

use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0x0f) as u32, 16).unwrap());
    }
    out
}

/// Short content hash. 16 hex chars is 64 bits — collision risk across a local
/// cache of a few million blobs is negligible, and it keeps cache keys readable.
pub fn content_hash(bytes: &[u8]) -> String {
    sha256_hex(bytes)[..16].to_string()
}

/// Run ids are `r_` plus 6 hex chars derived from the wall clock in nanoseconds.
/// Monotonic enough to sort roughly by time, short enough to type.
pub fn new_run_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("r_{}", &sha256_hex(&nanos.to_le_bytes())[..6])
}

/// Relative ages, not timestamps. An agent asking "is this data stale?" wants `2h`,
/// not an ISO string it has to subtract from the current date.
pub fn age(secs: i64) -> String {
    match secs {
        s if s < 0 => "0s".into(),
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s if s < 2_592_000 => format!("{}d", s / 86_400),
        s => format!("{}mo", s / 2_592_000),
    }
}

/// Parses the `ttl = "1h"` form used throughout the pack files.
pub fn parse_duration(s: &str) -> anyhow::Result<i64> {
    let s = s.trim();
    let (num, unit) = s
        .split_at(s.find(|c: char| c.is_alphabetic()).ok_or_else(|| {
            anyhow::anyhow!("duration '{s}' has no unit (expected e.g. 30m, 7d)")
        })?);
    let n: i64 = num
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("duration '{s}' has a non-numeric amount"))?;
    let mult = match unit.trim() {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        "mo" => 2_592_000,
        u => anyhow::bail!("unknown duration unit '{u}' (expected s, m, h, d, w, mo)"),
    };
    Ok(n * mult)
}

// ---------------------------------------------------------------------------
// Dates
//
// Packs need two things: `since=<date>` windows in URLs, and "how old is this
// timestamp". Both are a few lines of civil-calendar arithmetic (Howard Hinnant's
// days_from_civil / civil_from_days), which is cheaper than carrying a date crate
// for two functions.
// ---------------------------------------------------------------------------

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DD` for a unix timestamp — the form GitHub and friends accept in
/// `since=` query parameters.
pub fn iso_date(epoch: i64) -> String {
    let (y, m, d) = civil_from_days(epoch.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// Full `YYYY-MM-DDTHH:MM:SSZ`, used in reports where an absolute time matters.
pub fn iso_datetime(epoch: i64) -> String {
    let secs = epoch.rem_euclid(86_400);
    format!(
        "{}T{:02}:{:02}:{:02}Z",
        iso_date(epoch),
        secs / 3_600,
        (secs % 3_600) / 60,
        secs % 60
    )
}

/// Parses the timestamp shapes that actually appear in API responses: RFC 3339 with
/// or without fractional seconds, bare dates, and numeric unix epochs (seconds or
/// milliseconds). Returns None rather than guessing on anything else.
pub fn parse_timestamp(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    // Numeric epoch. Values past ~2001 in milliseconds are far larger than any
    // plausible second-count, which is what the threshold distinguishes.
    if let Ok(n) = s.parse::<i64>() {
        return Some(if n.abs() > 100_000_000_000 {
            n / 1000
        } else {
            n
        });
    }
    if let Ok(f) = s.parse::<f64>() {
        let n = f as i64;
        return Some(if n.abs() > 100_000_000_000 {
            n / 1000
        } else {
            n
        });
    }

    let bytes = s.as_bytes();
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let mo: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    let mut epoch = days_from_civil(y, mo, d) * 86_400;

    if bytes.len() >= 19 && (bytes[10] == b'T' || bytes[10] == b' ') {
        let h: i64 = s[11..13].parse().ok()?;
        let mi: i64 = s[14..16].parse().ok()?;
        let se: i64 = s[17..19].parse().ok()?;
        epoch += h * 3_600 + mi * 60 + se;

        // Offsets like +07:00 are normalised to UTC; Z and absent both mean UTC.
        let tail = &s[19..];
        let tail = tail.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
        if let Some(sign) = tail.chars().next()
            && (sign == '+' || sign == '-')
        {
            let off = &tail[1..];
            if off.len() >= 5 {
                let oh: i64 = off[0..2].parse().ok()?;
                let om: i64 = off[3..5].parse().ok()?;
                let total = oh * 3_600 + om * 60;
                epoch += if sign == '+' { -total } else { total };
            }
        }
    }
    Some(epoch)
}

/// Whole days between a timestamp and now. Negative ages clamp to zero — a source
/// with a clock ahead of ours is not "-3 days old".
pub fn days_since(epoch: i64) -> f64 {
    ((now() - epoch).max(0) as f64) / 86_400.0
}

/// Date variables every pack can use in URL templates without computing them.
///
/// Each window comes in two spellings because APIs disagree: `2026-08-18` for anything
/// RFC-3339-shaped, and `20260818` for the ones that want a bare stamp (Wikimedia's
/// pageviews endpoint, among others).
pub fn date_vars(now_epoch: i64) -> std::collections::HashMap<String, String> {
    let mut v = std::collections::HashMap::new();
    for (name, offset_days) in [
        ("today", 0i64),
        ("since_7d", 7),
        ("since_30d", 30),
        ("since_90d", 90),
        ("since_1y", 365),
        ("since_2y", 730),
    ] {
        let iso = iso_date(now_epoch - offset_days * 86_400);
        v.insert(format!("{name}_compact"), iso.replace('-', ""));
        v.insert(name.to_string(), iso);
    }
    v
}

/// Percent-encodes a value for use in a URL query or path segment.
///
/// Deliberately conservative: everything outside the unreserved set is escaped, so the
/// same helper is safe in a path and in a query string.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Substitutes `{var}` placeholders. Leaves unknown placeholders in place so that a
/// misspelled variable surfaces as a broken URL rather than a silently empty one.
///
/// `{env.NAME}` additionally reads the environment, which is how account-specific
/// values — a probe's avatar id, a tenant slug — reach a pack without anyone having to
/// edit the pack file. An unset variable is left visible for the same reason.
pub fn template(s: &str, vars: &std::collections::HashMap<String, String>) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }

    // Resolve `{env.NAME}` after named vars, so a pack can still shadow one explicitly.
    while let Some(start) = out.find("{env.") {
        let Some(end) = out[start..].find('}').map(|i| start + i) else {
            break;
        };
        let name = &out[start + 5..end];
        match std::env::var(name) {
            Ok(v) => out.replace_range(start..=end, &v),
            // Leave it in place, but stop scanning — otherwise this loops forever on
            // the placeholder it just refused to replace.
            Err(_) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn ages_are_relative_and_compact() {
        assert_eq!(age(30), "30s");
        assert_eq!(age(90), "1m");
        assert_eq!(age(7_200), "2h");
        assert_eq!(age(172_800), "2d");
        assert_eq!(age(5_184_000), "2mo");
        assert_eq!(age(-5), "0s");
    }

    #[test]
    fn durations_cover_every_unit_used_by_packs() {
        assert_eq!(parse_duration("30s").unwrap(), 30);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("1h").unwrap(), 3_600);
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert_eq!(parse_duration("2w").unwrap(), 1_209_600);
        assert_eq!(parse_duration("1mo").unwrap(), 2_592_000);
    }

    #[test]
    fn durations_reject_garbage_loudly() {
        assert!(parse_duration("7").is_err());
        assert!(parse_duration("xd").is_err());
        assert!(parse_duration("7y").is_err());
    }

    #[test]
    fn unknown_placeholders_survive_so_they_are_visible() {
        let mut v = HashMap::new();
        v.insert("owner".to_string(), "rust-lang".to_string());
        assert_eq!(
            template("https://x/{owner}/{repo}", &v),
            "https://x/rust-lang/{repo}"
        );
    }

    #[test]
    fn an_unset_env_placeholder_stays_visible_and_does_not_loop() {
        let out = template(
            "a={env.PILOTAGE_DEFINITELY_UNSET} b={env.ALSO_UNSET}",
            &HashMap::new(),
        );
        assert!(out.contains("{env.PILOTAGE_DEFINITELY_UNSET}"), "{out}");
    }

    #[test]
    fn named_vars_are_substituted_before_env_lookups() {
        let mut v = HashMap::new();
        v.insert("script".to_string(), "hello".to_string());
        assert_eq!(template("say {script}", &v), "say hello");
    }

    #[test]
    fn percent_encoding_makes_free_text_safe_in_a_url() {
        assert_eq!(percent_encode("machine learning"), "machine%20learning");
        assert_eq!(
            percent_encode("Go (programming language)"),
            "Go%20%28programming%20language%29"
        );
        assert_eq!(percent_encode("rust-lang_1.0~x"), "rust-lang_1.0~x");
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn compact_dates_are_offered_alongside_iso_ones() {
        let v = date_vars(1_700_000_000);
        assert_eq!(v["today"], "2023-11-14");
        assert_eq!(v["today_compact"], "20231114");
        assert!(v["since_2y"] < v["since_1y"]);
    }

    #[test]
    fn content_hash_is_stable_and_short() {
        assert_eq!(content_hash(b"abc"), content_hash(b"abc"));
        assert_ne!(content_hash(b"abc"), content_hash(b"abd"));
        assert_eq!(content_hash(b"abc").len(), 16);
    }

    #[test]
    fn iso_dates_round_trip_through_the_epoch() {
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_datetime(0), "1970-01-01T00:00:00Z");
        for epoch in [0, 951_782_400, 1_700_000_000, 2_000_000_000] {
            let back = parse_timestamp(&iso_datetime(epoch)).unwrap();
            assert_eq!(back, epoch, "failed for {epoch}");
        }
    }

    #[test]
    fn leap_day_is_handled() {
        // 2000-02-29 exists (400-year rule); 1900-02-29 does not.
        assert_eq!(iso_date(951_782_400), "2000-02-29");
        assert_eq!(parse_timestamp("2000-02-29").unwrap(), 951_782_400);
    }

    #[test]
    fn timestamps_cover_the_shapes_apis_actually_return() {
        assert_eq!(parse_timestamp("2000-02-29T00:00:00Z"), Some(951_782_400));
        assert_eq!(parse_timestamp("2000-02-29"), Some(951_782_400));
        assert_eq!(
            parse_timestamp("2000-02-29T00:00:00.123Z"),
            Some(951_782_400)
        );
        // Unix seconds and milliseconds both appear in the wild.
        assert_eq!(parse_timestamp("951782400"), Some(951_782_400));
        assert_eq!(parse_timestamp("951782400000"), Some(951_782_400));
    }

    #[test]
    fn timezone_offsets_normalise_to_utc() {
        let utc = parse_timestamp("2024-01-01T12:00:00Z").unwrap();
        assert_eq!(parse_timestamp("2024-01-01T19:00:00+07:00").unwrap(), utc);
        assert_eq!(parse_timestamp("2024-01-01T07:00:00-05:00").unwrap(), utc);
    }

    #[test]
    fn unparseable_timestamps_return_none_rather_than_a_wrong_date() {
        for s in ["", "never", "2024-13-01", "2024-01-99", "not-a-date"] {
            assert_eq!(parse_timestamp(s), None, "{s} should not parse");
        }
    }

    #[test]
    fn ages_never_go_negative_on_clock_skew() {
        assert_eq!(days_since(now() + 86_400), 0.0);
    }

    #[test]
    fn date_vars_are_ordered_backwards_in_time() {
        let v = date_vars(1_700_000_000);
        assert_eq!(v["today"], "2023-11-14");
        assert_eq!(v["since_30d"], "2023-10-15");
        assert!(v["since_1y"] < v["since_90d"]);
        assert!(v["since_90d"] < v["since_30d"]);
    }

    #[test]
    fn run_ids_are_unique_across_rapid_calls() {
        let ids: std::collections::HashSet<_> = (0..50).map(|_| new_run_id()).collect();
        assert_eq!(ids.len(), 50);
    }
}

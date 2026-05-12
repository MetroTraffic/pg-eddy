/// Cypher executor — interprets a logical plan by calling the pg_eddy
/// storage layer directly (node_store, edge_store, catalog).
///
/// v0.6.0 scope: executes LabelScan + Expand + Filter + Project +
/// CrossProduct plans and returns SETOF JSONB rows.
/// v0.10.0 scope: VarLengthExpand (BFS), NamedPath, Path value,
/// nodes()/relationships() path functions.
/// v0.14.0 scope: temporal types (date, time, localtime, localdatetime, datetime, duration).
use crate::cypher::ast::*;
use crate::cypher::planner::LogicalPlan;
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike};
use chrono_tz::Tz;
use pgrx::prelude::*;
use std::collections::HashMap;

/// A row of bindings: variable name → Value.
pub type Row = HashMap<String, Value>;

/// Runtime value for a binding.
#[derive(Debug, Clone)]
pub enum Value {
    /// A node: its id plus the full JSONB document.
    Node {
        node_id: i64,
        labels: Vec<String>,
        properties: serde_json::Map<String, serde_json::Value>,
    },
    /// An edge: its id plus the full JSONB document.
    Edge {
        edge_id: i64,
        rel_type: String,
        source: i64,
        target: i64,
        properties: serde_json::Map<String, serde_json::Value>,
    },
    /// A path: alternating nodes and relationships.
    Path {
        nodes: Vec<Value>,
        rels: Vec<Value>,
    },
    Int(i64),
    Float(f64),
    Str(String),
    Bool(bool),
    Null,
    Json(serde_json::Value),
    /// A temporal value (date, time, localtime, localdatetime, datetime).
    /// Stored as an ISO 8601 string plus a discriminant for property access.
    Temporal(TemporalValue),
    /// An ISO 8601 duration.
    Duration(CypherDuration),
}

// ---------------------------------------------------------------------------
// Temporal types
// ---------------------------------------------------------------------------

/// openCypher temporal value discriminants.
#[derive(Debug, Clone, PartialEq)]
pub enum TemporalKind {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    DateTime,
}

/// A temporal instant.  We keep the canonical ISO 8601 string for output
/// and the parsed components for property access / arithmetic.
#[derive(Debug, Clone)]
pub struct TemporalValue {
    pub kind: TemporalKind,
    /// Canonical ISO 8601 string (used for RETURN / storage).
    pub iso: String,
    /// Date part (None for pure time values).
    pub date: Option<NaiveDate>,
    /// Time part (None for pure date values).
    pub time: Option<NaiveTime>,
    /// UTC offset in seconds (None for local types).
    pub offset_secs: Option<i32>,
    /// IANA timezone name (for DateTime with zone only).
    pub tz_name: Option<String>,
}

/// An openCypher ISO 8601 duration decomposed into its component fields.
/// All fields are signed integers; the ISO string is the canonical form.
#[derive(Debug, Clone)]
pub struct CypherDuration {
    pub years: i64,
    pub months: i64,
    pub weeks: i64,
    pub days: i64,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
    pub nanoseconds: i64,
    /// Canonical ISO 8601 duration string.
    pub iso: String,
}

impl CypherDuration {
    /// Total months (years*12 + months).
    #[allow(dead_code)]
    pub fn total_months(&self) -> i64 {
        self.years * 12 + self.months
    }

    /// Total days (weeks*7 + days).
    pub fn total_days(&self) -> i64 {
        self.weeks * 7 + self.days
    }

    /// Total seconds (hours*3600 + minutes*60 + seconds).
    pub fn total_seconds(&self) -> i64 {
        self.hours * 3600 + self.minutes * 60 + self.seconds
    }

    /// Sub-second nanoseconds (nanoseconds % 1_000_000_000).
    pub fn nanoseconds_of_second(&self) -> i64 {
        self.nanoseconds % 1_000_000_000
    }

    /// Build an ISO 8601 duration string from components.
    #[allow(clippy::too_many_arguments)]
    pub fn build_iso(
        years: i64, months: i64, weeks: i64, days: i64,
        hours: i64, minutes: i64, seconds: i64, nanos: i64,
    ) -> String {
        // Represent fractional seconds as decimal
        let sec_str = if nanos != 0 {
            let frac = nanos.unsigned_abs();
            // trim trailing zeros
            let s = format!("{frac:09}");
            let s = s.trim_end_matches('0');
            format!("{seconds}.{s}S")
        } else if seconds != 0 || (hours == 0 && minutes == 0 && days == 0
                                   && weeks == 0 && months == 0 && years == 0) {
            format!("{seconds}S")
        } else {
            String::new()
        };

        let mut out = String::from("P");
        if years != 0   { out.push_str(&format!("{years}Y")); }
        if months != 0  { out.push_str(&format!("{months}M")); }
        if weeks != 0   { out.push_str(&format!("{weeks}W")); }
        if days != 0    { out.push_str(&format!("{days}D")); }
        if hours != 0 || minutes != 0 || !sec_str.is_empty() {
            out.push('T');
            if hours != 0   { out.push_str(&format!("{hours}H")); }
            if minutes != 0 { out.push_str(&format!("{minutes}M")); }
            out.push_str(&sec_str);
        }
        if out == "P" { out.push_str("T0S"); }
        out
    }

    /// Parse an ISO 8601 duration string into a CypherDuration.
    pub fn parse(s: &str) -> Option<Self> {
        // Format: P[nY][nM][nW][nD][T[nH][nM][n[.f]S]]
        let s = s.trim();
        if !s.starts_with('P') { return None; }
        let rest = &s[1..];

        let (date_part, time_part) = if let Some(t_pos) = rest.find('T') {
            (&rest[..t_pos], &rest[t_pos+1..])
        } else {
            (rest, "")
        };

        fn parse_component(s: &str, unit: char) -> Option<(i64, &str)> {
            if let Some(pos) = s.find(unit) {
                // find start: could be negative, could have decimal
                let num_str = &s[..pos];
                let val = num_str.parse::<f64>().ok()?;
                Some((val as i64, &s[pos+1..]))
            } else {
                None
            }
        }

        fn parse_f_component(s: &str, unit: char) -> Option<(i64, i64, &str)> {
            if let Some(pos) = s.find(unit) {
                let num_str = &s[..pos];
                if let Some(dot) = num_str.find('.') {
                    let whole: i64 = num_str[..dot].parse().ok()?;
                    let frac_str = &num_str[dot+1..];
                    // pad or truncate to 9 digits for nanoseconds
                    let padded = format!("{:0<9}", frac_str);
                    let nanos: i64 = padded[..9].parse().ok()?;
                    let sign = if whole < 0 { -1i64 } else { 1i64 };
                    Some((whole, sign * nanos, &s[pos+1..]))
                } else {
                    let whole: i64 = num_str.parse().ok()?;
                    Some((whole, 0, &s[pos+1..]))
                }
            } else {
                None
            }
        }

        let mut years = 0i64;
        let mut months = 0i64;
        let mut weeks = 0i64;
        let mut days = 0i64;
        let mut hours = 0i64;
        let mut minutes = 0i64;
        let mut seconds = 0i64;
        let mut nanos = 0i64;

        let mut cur = date_part;
        if let Some((v, rest)) = parse_component(cur, 'Y') { years = v; cur = rest; }
        if let Some((v, rest)) = parse_component(cur, 'M') { months = v; cur = rest; }
        if let Some((v, rest)) = parse_component(cur, 'W') { weeks = v; cur = rest; }
        if let Some((v, rest)) = parse_component(cur, 'D') { days = v; cur = rest; }
        let _ = cur;

        let mut cur = time_part;
        if let Some((v, rest)) = parse_component(cur, 'H') { hours = v; cur = rest; }
        if let Some((v, rest)) = parse_component(cur, 'M') { minutes = v; cur = rest; }
        if let Some((v, n, rest)) = parse_f_component(cur, 'S') { seconds = v; nanos = n; cur = rest; }
        let _ = cur;

        let iso = Self::build_iso(years, months, weeks, days, hours, minutes, seconds, nanos);
        Some(CypherDuration { years, months, weeks, days, hours, minutes, seconds, nanoseconds: nanos, iso })
    }
}

// ---------------------------------------------------------------------------
// Temporal parsing helpers
// ---------------------------------------------------------------------------

/// Parse a map-literal argument like {year: 2015, month: 7, day: 21}.
/// Returns a serde_json::Map of the keys.
#[allow(dead_code)]
fn temporal_map_arg(v: &Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    match v {
        Value::Json(serde_json::Value::Object(m)) => Some(m.clone()),
        _ => None,
    }
}

fn map_i(m: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i32> {
    m.get(key).and_then(|v| v.as_i64()).map(|v| v as i32)
}

fn map_i64(m: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<i64> {
    m.get(key).and_then(|v| v.as_i64())
}

/// Parse a date from an ISO 8601 string. Supports extended (2015-07-21),
/// basic (20150721), week-based (2015-W30-2 / 2015W302), ordinal (2015-202),
/// year-only (2015), year-month (2015-07 / 201507).
fn parse_date_str(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    // Extended: YYYY-MM-DD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") { return Some(d); }
    // Basic: YYYYMMDD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%m%d") { return Some(d); }
    // Ordinal extended: YYYY-DDD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%j") { return Some(d); }
    // Ordinal basic: YYYYDDD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%j") { return Some(d); }
    // Year-month extended: YYYY-MM → 1st of month
    if s.len() == 7 && s.as_bytes()[4] == b'-'
        && let Ok(d) = NaiveDate::parse_from_str(&format!("{s}-01"), "%Y-%m-%d") { return Some(d); }
    // Year-month basic: YYYYMM → 1st of month
    if s.len() == 6 && s.chars().all(|c| c.is_ascii_digit())
        && let Ok(d) = NaiveDate::parse_from_str(&format!("{s}01"), "%Y%m%d") { return Some(d); }
    // Year-only: YYYY → Jan 1
    if s.len() == 4 && s.chars().all(|c| c.is_ascii_digit())
        && let Ok(y) = s.parse::<i32>() { return NaiveDate::from_ymd_opt(y, 1, 1); }
    // Week-based extended: YYYY-Www-D
    // Format: YYYY-W{ww}-{d}
    if s.len() >= 8 && &s[4..6] == "-W" {
        let y: i32 = s[..4].parse().ok()?;
        if s.len() == 8 {
            // YYYY-Www → Monday of that week
            let w: u32 = s[6..8].parse().ok()?;
            return NaiveDate::from_isoywd_opt(y, w, chrono::Weekday::Mon);
        } else if s.len() == 10 && s.as_bytes()[8] == b'-' {
            let w: u32 = s[6..8].parse().ok()?;
            let d: u8 = s[9..10].parse().ok()?;
            let wd = iso_weekday(d)?;
            return NaiveDate::from_isoywd_opt(y, w, wd);
        }
    }
    // Week-based basic: YYYYWwwD
    if s.len() == 8 && s.as_bytes()[4] == b'W' {
        let y: i32 = s[..4].parse().ok()?;
        let w: u32 = s[5..7].parse().ok()?;
        let d: u8 = s[7..8].parse().ok()?;
        let wd = iso_weekday(d)?;
        return NaiveDate::from_isoywd_opt(y, w, wd);
    }
    // Week-based basic: YYYYWww (no day → Monday)
    if s.len() == 7 && s.as_bytes()[4] == b'W' {
        let y: i32 = s[..4].parse().ok()?;
        let w: u32 = s[5..7].parse().ok()?;
        return NaiveDate::from_isoywd_opt(y, w, chrono::Weekday::Mon);
    }
    None
}

fn iso_weekday(d: u8) -> Option<chrono::Weekday> {
    match d {
        1 => Some(chrono::Weekday::Mon),
        2 => Some(chrono::Weekday::Tue),
        3 => Some(chrono::Weekday::Wed),
        4 => Some(chrono::Weekday::Thu),
        5 => Some(chrono::Weekday::Fri),
        6 => Some(chrono::Weekday::Sat),
        7 => Some(chrono::Weekday::Sun),
        _ => None,
    }
}

/// Parse a local time string.  Supports HH:MM, HH:MM:SS, HH:MM:SS.f.
fn parse_localtime_str(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    // Strip any timezone suffix for local parsing
    let s = if let Some(pos) = s.find(['+', 'Z']) { &s[..pos] } else { s };
    let s = if let Some(pos) = s.rfind('-') {
        // only strip if it's a timezone offset like -05:00, not part of time
        if pos >= 3 { &s[..pos] } else { s }
    } else { s };
    // Extended formats (with colons): HH:MM:SS.f, HH:MM:SS, HH:MM
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M:%S%.f") { return Some(t); }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M:%S") { return Some(t); }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H:%M") { return Some(t); }
    // Basic (compact) formats: HHMMSS.f, HHMMSS, HHMM, HH
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M%S%.f") { return Some(t); }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M%S") { return Some(t); }
    if let Ok(t) = NaiveTime::parse_from_str(s, "%H%M") { return Some(t); }
    // Hour-only: "21" → 21:00:00
    if s.len() == 2 && s.chars().all(|c| c.is_ascii_digit())
        && let Ok(h) = s.parse::<u32>()
        && h < 24
    {
        return NaiveTime::from_hms_opt(h, 0, 0);
    }
    None
}

/// Parse an offset like +01:00, -05:00, Z, +0100, -05.
/// Returns seconds east of UTC.
fn parse_offset(s: &str) -> Option<i32> {
    if s == "Z" || s == "z" { return Some(0); }
    let (sign, rest) = if let Some(r) = s.strip_prefix('+') {
        (1i32, r)
    } else if let Some(r) = s.strip_prefix('-') {
        (-1i32, r)
    } else {
        return None;
    };
    // HH:MM or HHMM or HH
    let (h, m) = if rest.len() == 5 && rest.as_bytes()[2] == b':' {
        let h: i32 = rest[..2].parse().ok()?;
        let m: i32 = rest[3..].parse().ok()?;
        (h, m)
    } else if rest.len() == 4 {
        let h: i32 = rest[..2].parse().ok()?;
        let m: i32 = rest[2..].parse().ok()?;
        (h, m)
    } else if rest.len() == 2 {
        let h: i32 = rest.parse().ok()?;
        (h, 0)
    } else {
        return None;
    };
    Some(sign * (h * 3600 + m * 60))
}

/// Build a TemporalValue for `date()`.
fn temporal_date(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "date(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            let d = parse_date_str(s).ok_or_else(err)?;
            let iso = d.format("%Y-%m-%d").to_string();
            Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(d), time: None, offset_secs: None, tz_name: None })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let y = map_i(m, "year").ok_or_else(err)?;
            let mo = map_i(m, "month").unwrap_or(1);
            let d = map_i(m, "day").unwrap_or(1);
            let date = NaiveDate::from_ymd_opt(y, mo as u32, d as u32).ok_or_else(err)?;
            let iso = date.format("%Y-%m-%d").to_string();
            Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(date), time: None, offset_secs: None, tz_name: None })
        }
        _ => Err(err()),
    }
}

/// Build a TemporalValue for `localtime()`.
fn temporal_localtime(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "localtime(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            let t = parse_localtime_str(s).ok_or_else(err)?;
            let iso = format_localtime(&t);
            Ok(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(t), offset_secs: None, tz_name: None })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let h = map_i(m, "hour").unwrap_or(0);
            let mi = map_i(m, "minute").unwrap_or(0);
            let s = map_i(m, "second").unwrap_or(0);
            let ns = map_i64(m, "nanosecond").unwrap_or(0) as u32;
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = ns + ms + us;
            let t = NaiveTime::from_hms_nano_opt(h as u32, mi as u32, s as u32, nanos).ok_or_else(err)?;
            let iso = format_localtime(&t);
            Ok(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(t), offset_secs: None, tz_name: None })
        }
        _ => Err(err()),
    }
}

/// Build a TemporalValue for `time()`.
fn temporal_time(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "time(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            let s = s.trim();
            // Find timezone suffix
            let (time_str, offset_secs, _tz_name) = extract_time_tz(s);
            let t = parse_localtime_str(time_str).ok_or_else(err)?;
            let off = offset_secs.unwrap_or(0);
            let iso = format_time_with_offset(&t, off);
            Ok(TemporalValue { kind: TemporalKind::Time, iso, date: None, time: Some(t), offset_secs: Some(off), tz_name: None })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let h = map_i(m, "hour").unwrap_or(0);
            let mi = map_i(m, "minute").unwrap_or(0);
            let s = map_i(m, "second").unwrap_or(0);
            let ns = map_i64(m, "nanosecond").unwrap_or(0) as u32;
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = ns + ms + us;
            let t = NaiveTime::from_hms_nano_opt(h as u32, mi as u32, s as u32, nanos).ok_or_else(err)?;
            let tz_str = m.get("timezone").and_then(|v| v.as_str()).unwrap_or("+00:00");
            let off = parse_offset(tz_str).unwrap_or(0);
            let iso = format_time_with_offset(&t, off);
            Ok(TemporalValue { kind: TemporalKind::Time, iso, date: None, time: Some(t), offset_secs: Some(off), tz_name: None })
        }
        _ => Err(err()),
    }
}

/// Build a TemporalValue for `localdatetime()`.
fn temporal_localdatetime(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "localdatetime(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            let s = s.trim();
            // Split at T
            let (date_str, time_str) = split_datetime(s)?;
            let date = parse_date_str(date_str).ok_or_else(err)?;
            // Strip any offset from the time part
            let (time_str_clean, _, _) = extract_time_tz(time_str);
            let time = parse_localtime_str(time_str_clean).ok_or_else(err)?;
            let iso = format_localdatetime(&date, &time);
            Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(date), time: Some(time), offset_secs: None, tz_name: None })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let y = map_i(m, "year").ok_or_else(err)?;
            let mo = map_i(m, "month").unwrap_or(1) as u32;
            let d = map_i(m, "day").unwrap_or(1) as u32;
            let h = map_i(m, "hour").unwrap_or(0) as u32;
            let mi = map_i(m, "minute").unwrap_or(0) as u32;
            let s = map_i(m, "second").unwrap_or(0) as u32;
            let ns = map_i64(m, "nanosecond").unwrap_or(0) as u32;
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = ns + ms + us;
            let date = NaiveDate::from_ymd_opt(y, mo, d).ok_or_else(err)?;
            let time = NaiveTime::from_hms_nano_opt(h, mi, s, nanos).ok_or_else(err)?;
            let iso = format_localdatetime(&date, &time);
            Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(date), time: Some(time), offset_secs: None, tz_name: None })
        }
        _ => Err(err()),
    }
}

/// Build a TemporalValue for `datetime()`.
fn temporal_datetime(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "datetime(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            let s = s.trim();
            // Handle timezone name in brackets: ...+01:00[Europe/Stockholm]
            let (s_no_zone, tz_name) = strip_tz_bracket(s);
            let (date_str, time_str) = split_datetime(s_no_zone)?;
            let date = parse_date_str(date_str).ok_or_else(err)?;
            let (time_str_clean, offset_secs, _) = extract_time_tz(time_str);
            let time = parse_localtime_str(time_str_clean).ok_or_else(err)?;
            let off = offset_secs.unwrap_or(0);
            let iso = format_datetime(&date, &time, off, tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time), offset_secs: Some(off), tz_name })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let y = map_i(m, "year").ok_or_else(err)?;
            let mo = map_i(m, "month").unwrap_or(1) as u32;
            let d = map_i(m, "day").unwrap_or(1) as u32;
            let h = map_i(m, "hour").unwrap_or(0) as u32;
            let mi = map_i(m, "minute").unwrap_or(0) as u32;
            let s = map_i(m, "second").unwrap_or(0) as u32;
            let ns = map_i64(m, "nanosecond").unwrap_or(0) as u32;
            let ms_val = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = ns + ms_val + us;
            let date = NaiveDate::from_ymd_opt(y, mo, d).ok_or_else(err)?;
            let time = NaiveTime::from_hms_nano_opt(h, mi, s, nanos).ok_or_else(err)?;
            let tz_str = m.get("timezone").and_then(|v| v.as_str()).unwrap_or("UTC");
            let (off, tz_name) = if let Some(o) = parse_offset(tz_str) {
                (o, None)
            } else {
                let tz: Tz = tz_str.parse().unwrap_or(chrono_tz::UTC);
                let ndt = NaiveDateTime::new(date, time);
                let dt = tz.from_local_datetime(&ndt)
                    .earliest()
                    .unwrap_or_else(|| chrono_tz::UTC.from_local_datetime(&ndt).unwrap());
                // offset = local wall-clock timestamp minus UTC timestamp
                let utc_ts = dt.timestamp();
                let local_ts = ndt.and_utc().timestamp();
                let off = (local_ts - utc_ts) as i32;
                (off, Some(tz_str.to_string()))
            };
            let iso = format_datetime(&date, &time, off, tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time), offset_secs: Some(off), tz_name })
        }
        _ => Err(err()),
    }
}

/// Build a CypherDuration for `duration()`.
fn temporal_duration(arg: &Value) -> Result<CypherDuration, ExecError> {
    let err = || ExecError { message: "duration(): invalid argument".into() };
    match arg {
        Value::Str(s) => CypherDuration::parse(s).ok_or_else(err),
        Value::Json(serde_json::Value::Object(m)) => {
            let years   = map_i64(m, "years").or_else(|| map_i64(m, "year")).unwrap_or(0);
            let months  = map_i64(m, "months").or_else(|| map_i64(m, "month")).unwrap_or(0);
            let weeks   = map_i64(m, "weeks").or_else(|| map_i64(m, "week")).unwrap_or(0);
            let days    = map_i64(m, "days").or_else(|| map_i64(m, "day")).unwrap_or(0);
            let hours   = map_i64(m, "hours").or_else(|| map_i64(m, "hour")).unwrap_or(0);
            let minutes = map_i64(m, "minutes").or_else(|| map_i64(m, "minute")).unwrap_or(0);
            let seconds = map_i64(m, "seconds").or_else(|| map_i64(m, "second")).unwrap_or(0);
            let ms = map_i64(m, "milliseconds").or_else(|| map_i64(m, "millisecond")).unwrap_or(0) * 1_000_000;
            let us = map_i64(m, "microseconds").or_else(|| map_i64(m, "microsecond")).unwrap_or(0) * 1_000;
            let nanos = map_i64(m, "nanoseconds").or_else(|| map_i64(m, "nanosecond")).unwrap_or(0) + ms + us;
            let iso = CypherDuration::build_iso(years, months, weeks, days, hours, minutes, seconds, nanos);
            Ok(CypherDuration { years, months, weeks, days, hours, minutes, seconds, nanoseconds: nanos, iso })
        }
        _ => Err(err()),
    }
}

// ---------------------------------------------------------------------------
// Temporal-to-epoch helpers
// ---------------------------------------------------------------------------

/// Convert a TemporalValue to epoch seconds (UTC).
/// For date-only: midnight UTC of that date.
/// For time-only: seconds since midnight adjusted for offset.
/// For datetime: epoch of that instant.
fn temporal_epoch_seconds(tv: &TemporalValue) -> Option<i64> {
    match tv.kind {
        TemporalKind::Date => {
            let d = tv.date?;
            Some(NaiveDateTime::new(d, NaiveTime::from_hms_opt(0, 0, 0)?).and_utc().timestamp())
        }
        TemporalKind::LocalTime => {
            let t = tv.time?;
            Some(t.num_seconds_from_midnight() as i64)
        }
        TemporalKind::Time => {
            let t = tv.time?;
            let off = tv.offset_secs.unwrap_or(0) as i64;
            Some(t.num_seconds_from_midnight() as i64 - off)
        }
        TemporalKind::LocalDateTime => {
            let d = tv.date?;
            let t = tv.time?;
            Some(NaiveDateTime::new(d, t).and_utc().timestamp())
        }
        TemporalKind::DateTime => {
            let d = tv.date?;
            let t = tv.time?;
            let off = tv.offset_secs.unwrap_or(0) as i64;
            Some(NaiveDateTime::new(d, t).and_utc().timestamp() - off)
        }
    }
}

/// Compute duration.between(lhs, rhs): exact difference preserving sign.
/// The result has a months component (years*12+months) and a seconds component;
/// the spec says it is as precise as the inputs allow.
fn duration_between(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    // Compute total nanoseconds difference for the sub-month part
    // and total months difference for the month part.
    let lhs_epoch_ns = temporal_epoch_ns(lhs);
    let rhs_epoch_ns = temporal_epoch_ns(rhs);
    let ns_diff = rhs_epoch_ns - lhs_epoch_ns;
    let secs = ns_diff / 1_000_000_000;
    let nanos = ns_diff % 1_000_000_000;
    let iso = CypherDuration::build_iso(0, 0, 0, 0,
        secs / 3600, (secs % 3600) / 60, secs % 60, nanos);
    CypherDuration { years: 0, months: 0, weeks: 0, days: 0,
        hours: secs / 3600, minutes: (secs % 3600) / 60,
        seconds: secs % 60, nanoseconds: nanos, iso }
}

fn temporal_epoch_ns(tv: &TemporalValue) -> i64 {
    match tv.kind {
        TemporalKind::Date => {
            let d = tv.date.unwrap_or_else(|| NaiveDate::from_ymd_opt(1970,1,1).unwrap());
            NaiveDateTime::new(d, NaiveTime::from_hms_opt(0,0,0).unwrap()).and_utc().timestamp() * 1_000_000_000
        }
        TemporalKind::LocalTime => {
            let t = tv.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
            (t.num_seconds_from_midnight() as i64) * 1_000_000_000 + t.nanosecond() as i64
        }
        TemporalKind::Time => {
            let t = tv.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
            let off = tv.offset_secs.unwrap_or(0) as i64;
            (t.num_seconds_from_midnight() as i64 - off) * 1_000_000_000 + t.nanosecond() as i64
        }
        TemporalKind::LocalDateTime => {
            let d = tv.date.unwrap_or_else(|| NaiveDate::from_ymd_opt(1970,1,1).unwrap());
            let t = tv.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
            NaiveDateTime::new(d, t).and_utc().timestamp() * 1_000_000_000 + t.nanosecond() as i64
        }
        TemporalKind::DateTime => {
            let d = tv.date.unwrap_or_else(|| NaiveDate::from_ymd_opt(1970,1,1).unwrap());
            let t = tv.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
            let off = tv.offset_secs.unwrap_or(0) as i64;
            (NaiveDateTime::new(d, t).and_utc().timestamp() - off) * 1_000_000_000 + t.nanosecond() as i64
        }
    }
}

/// duration.inMonths: full calendar months between two temporals (truncates sub-month).
fn duration_in_months(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    // Only meaningful when both have a date part; otherwise return PT0S.
    let (ld, rd) = match (lhs.date, rhs.date) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            return CypherDuration {
                years: 0, months: 0, weeks: 0, days: 0,
                hours: 0, minutes: 0, seconds: 0, nanoseconds: 0,
                iso: "PT0S".into(),
            };
        }
    };
    let total_months = (rd.year() as i64 * 12 + rd.month() as i64)
        - (ld.year() as i64 * 12 + ld.month() as i64);
    let years = total_months / 12;
    let months = total_months % 12;
    let iso = CypherDuration::build_iso(years, months, 0, 0, 0, 0, 0, 0);
    CypherDuration { years, months, weeks: 0, days: 0, hours: 0, minutes: 0, seconds: 0, nanoseconds: 0, iso }
}

/// duration.inDays: full calendar days between two temporals (truncates sub-day).
fn duration_in_days(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    let (ld, rd) = match (lhs.date, rhs.date) {
        (Some(l), Some(r)) => (l, r),
        _ => {
            return CypherDuration {
                years: 0, months: 0, weeks: 0, days: 0,
                hours: 0, minutes: 0, seconds: 0, nanoseconds: 0,
                iso: "PT0S".into(),
            };
        }
    };
    let days = (rd - ld).num_days();
    let iso = CypherDuration::build_iso(0, 0, 0, days, 0, 0, 0, 0);
    CypherDuration { years: 0, months: 0, weeks: 0, days, hours: 0, minutes: 0, seconds: 0, nanoseconds: 0, iso }
}

/// duration.inSeconds: full seconds between two temporals (truncates sub-second).
fn duration_in_seconds(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    let ns_diff = temporal_epoch_ns(rhs) - temporal_epoch_ns(lhs);
    let secs = ns_diff / 1_000_000_000;
    let nanos = ns_diff % 1_000_000_000;
    let iso = CypherDuration::build_iso(0, 0, 0, 0, secs / 3600, (secs % 3600) / 60, secs % 60, nanos);
    CypherDuration { years: 0, months: 0, weeks: 0, days: 0,
        hours: secs / 3600, minutes: (secs % 3600) / 60,
        seconds: secs % 60, nanoseconds: nanos, iso }
}

// ---------------------------------------------------------------------------
// Temporal formatting helpers
// ---------------------------------------------------------------------------

fn format_localtime(t: &NaiveTime) -> String {
    let ns = t.nanosecond();
    let sec = t.second();
    if ns == 0 && sec == 0 {
        // Truncated form: HH:MM (omit zero seconds)
        t.format("%H:%M").to_string()
    } else if ns == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        // trim trailing zeros
        let frac = format!("{ns:09}");
        let frac = frac.trim_end_matches('0');
        format!("{}.{frac}", t.format("%H:%M:%S"))
    }
}

fn format_time_with_offset(t: &NaiveTime, offset_secs: i32) -> String {
    let local_str = format_localtime(t);
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    format!("{local_str}{sign}{h:02}:{m:02}")
}

fn format_localdatetime(d: &NaiveDate, t: &NaiveTime) -> String {
    format!("{}T{}", d.format("%Y-%m-%d"), format_localtime(t))
}

fn format_datetime(d: &NaiveDate, t: &NaiveTime, offset_secs: i32, tz_name: Option<&str>) -> String {
    let base = format!("{}T{}", d.format("%Y-%m-%d"), format_localtime(t));
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    let off_str = format!("{sign}{h:02}:{m:02}");
    if let Some(tz) = tz_name {
        format!("{base}{off_str}[{tz}]")
    } else {
        format!("{base}{off_str}")
    }
}

/// Split "2015-07-21T21:40:32.142" or "2015-07-21T21:40:32.142+0100" at T.
fn split_datetime(s: &str) -> Result<(&str, &str), ExecError> {
    s.find('T').map(|p| (&s[..p], &s[p+1..]))
        .ok_or_else(|| ExecError { message: format!("datetime: missing T separator in '{s}'") })
}

/// Strip a trailing [Timezone/Name] from a datetime string.
/// Returns (string_without_bracket, Some(tz_name)) or (original, None).
fn strip_tz_bracket(s: &str) -> (&str, Option<String>) {
    if let (Some(lb), Some(rb)) = (s.rfind('['), s.rfind(']'))
        && rb == s.len() - 1
    {
        return (&s[..lb], Some(s[lb+1..rb].to_string()));
    }
    (s, None)
}

/// Extract time string, offset_secs, and optional tz name from a time string
/// that may have a trailing +HH:MM, -HH:MM, or Z.
fn extract_time_tz(s: &str) -> (&str, Option<i32>, Option<String>) {
    if s.ends_with('Z') || s.ends_with('z') {
        return (&s[..s.len()-1], Some(0), None);
    }
    // Look for + or - after the first character (HH might start with -)
    // scan from right for a + or -, but avoid the first char
    for i in (1..s.len()).rev() {
        let b = s.as_bytes()[i];
        if b == b'+' || (b == b'-' && i >= 2) {
            // check that the suffix looks like an offset
            let suffix = &s[i..];
            if parse_offset(suffix).is_some() {
                return (&s[..i], parse_offset(suffix), None);
            }
        }
    }
    (s, None, None)
}

/// Try to parse an ISO-formatted temporal string (date, localtime, time, localdatetime, datetime).
/// Returns None if the string doesn't look like any known temporal format.
fn try_parse_temporal_str(s: &str) -> Option<TemporalValue> {
    let s = s.trim();
    // Try date: YYYY-MM-DD
    if s.len() == 10 && s.as_bytes()[4] == b'-' && s.as_bytes()[7] == b'-'
        && let Some(d) = parse_date_str(s)
    {
        return Some(TemporalValue { kind: TemporalKind::Date, iso: s.to_string(), date: Some(d), time: None, offset_secs: None, tz_name: None });
    }
    // Try localdatetime: contains 'T' but no timezone offset
    if let Some(t_pos) = s.find('T') {
        let date_part = &s[..t_pos];
        let time_part = &s[t_pos+1..];
        if let (Some(d), Some(t)) = (parse_date_str(date_part), parse_localtime_str(time_part)) {
            let iso = s.to_string();
            let has_tz = time_part.contains('+') || time_part.contains('Z')
                || (time_part.len() > 6 && time_part[time_part.len()-6..].contains('-'));
            if !has_tz {
                return Some(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(d), time: Some(t), offset_secs: None, tz_name: None });
            }
            // Datetime with offset
            let (time_str, off, tz) = extract_time_tz(time_part);
            if let Some(t2) = parse_localtime_str(time_str) {
                return Some(TemporalValue { kind: TemporalKind::DateTime, iso: s.to_string(), date: Some(d), time: Some(t2), offset_secs: off, tz_name: tz });
            }
        }
    }
    // Try localtime: HH:MM[:SS[.f]]
    if s.len() >= 4 && s.len() <= 20 && !s.contains('T') && !s.contains('-')
        && let Some(t) = parse_localtime_str(s)
    {
        let iso = format_localtime(&t);
        return Some(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(t), offset_secs: None, tz_name: None });
    }
    None
}

/// Get a named property from a TemporalValue.
fn temporal_get_property(tv: &TemporalValue, key: &str) -> Value {
    match key {
        "year" => tv.date.map(|d| Value::Int(d.year() as i64)).unwrap_or(Value::Null),
        "month" => tv.date.map(|d| Value::Int(d.month() as i64)).unwrap_or(Value::Null),
        "day" => tv.date.map(|d| Value::Int(d.day() as i64)).unwrap_or(Value::Null),
        "hour" => tv.time.map(|t| Value::Int(t.hour() as i64)).unwrap_or(Value::Null),
        "minute" => tv.time.map(|t| Value::Int(t.minute() as i64)).unwrap_or(Value::Null),
        "second" => tv.time.map(|t| Value::Int(t.second() as i64)).unwrap_or(Value::Null),
        "millisecond" => tv.time.map(|t| Value::Int((t.nanosecond() / 1_000_000) as i64)).unwrap_or(Value::Null),
        "microsecond" => tv.time.map(|t| Value::Int((t.nanosecond() / 1_000) as i64 % 1000)).unwrap_or(Value::Null),
        "nanosecond" => tv.time.map(|t| Value::Int(t.nanosecond() as i64 % 1000)).unwrap_or(Value::Null),
        "nanoseconds" | "nanosecondsOfSecond" => tv.time.map(|t| Value::Int(t.nanosecond() as i64)).unwrap_or(Value::Null),
        "epochSeconds" => temporal_epoch_seconds(tv).map(Value::Int).unwrap_or(Value::Null),
        "epochMillis" => temporal_epoch_seconds(tv).map(|s| Value::Int(s * 1000)).unwrap_or(Value::Null),
        "timezone" => tv.tz_name.as_deref().or_else(|| {
            tv.offset_secs.map(|_| "")
        }).map(|_| {
            if let Some(name) = &tv.tz_name { Value::Str(name.clone()) }
            else if let Some(off) = tv.offset_secs {
                let sign = if off < 0 { '-' } else { '+' };
                let abs = off.unsigned_abs();
                Value::Str(format!("{sign}{:02}:{:02}", abs/3600, (abs%3600)/60))
            } else { Value::Null }
        }).unwrap_or(Value::Null),
        "offset" => tv.offset_secs.map(|off| {
            let sign = if off < 0 { '-' } else { '+' };
            let abs = off.unsigned_abs();
            Value::Str(format!("{sign}{:02}:{:02}", abs/3600, (abs%3600)/60))
        }).unwrap_or(Value::Null),
        "offsetSeconds" => tv.offset_secs.map(|o| Value::Int(o as i64)).unwrap_or(Value::Null),
        "quarter" => tv.date.map(|d| Value::Int(((d.month() - 1) / 3 + 1) as i64)).unwrap_or(Value::Null),
        "dayOfWeek" => tv.date.map(|d| Value::Int(d.weekday().number_from_monday() as i64)).unwrap_or(Value::Null),
        "dayOfQuarter" => tv.date.map(|d| {
            let q_start_month = ((d.month() - 1) / 3) * 3 + 1;
            let q_start = NaiveDate::from_ymd_opt(d.year(), q_start_month, 1).unwrap();
            Value::Int((d - q_start).num_days() + 1)
        }).unwrap_or(Value::Null),
        "week" => tv.date.map(|d| Value::Int(d.iso_week().week() as i64)).unwrap_or(Value::Null),
        "weekYear" => tv.date.map(|d| Value::Int(d.iso_week().year() as i64)).unwrap_or(Value::Null),
        "ordinalDay" => tv.date.map(|d| Value::Int(d.ordinal() as i64)).unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

/// Get a named property from a CypherDuration.
fn duration_get_property(dur: &CypherDuration, key: &str) -> Value {
    match key {
        "years" => Value::Int(dur.years),
        "months" => Value::Int(dur.months),
        "weeks" => Value::Int(dur.weeks),
        "days" => Value::Int(dur.total_days()),
        "hours" => Value::Int(dur.hours),
        "minutes" => Value::Int(dur.minutes),
        "seconds" => Value::Int(dur.seconds),
        "milliseconds" => Value::Int(dur.nanoseconds / 1_000_000),
        "microseconds" => Value::Int(dur.nanoseconds / 1_000),
        "nanoseconds" => Value::Int(dur.nanoseconds),
        "nanosecondsOfSecond" => Value::Int(dur.nanoseconds_of_second()),
        "monthsOfYear" => Value::Int(dur.months),
        "daysOfWeek" => Value::Int(dur.days % 7),
        "minutesOfHour" => Value::Int(dur.minutes % 60),
        "secondsOfMinute" => Value::Int(dur.seconds % 60),
        "millisecondsOfSecond" => Value::Int((dur.nanoseconds / 1_000_000) % 1000),
        "microsecondsOfSecond" => Value::Int((dur.nanoseconds / 1_000) % 1_000_000),
        _ => Value::Null,
    }
}

impl Value {
    /// Get a property from a Node or Edge value.
    pub fn get_property(&self, key: &str) -> Value {
        match self {
            Value::Node { properties, .. } => match properties.get(key) {
                Some(v) => json_to_value(v),
                None => Value::Null,
            },
            Value::Edge { properties, .. } => match properties.get(key) {
                Some(v) => json_to_value(v),
                None => Value::Null,
            },
            Value::Json(serde_json::Value::Object(m)) => match m.get(key) {
                Some(v) => json_to_value(v),
                None => Value::Null,
            },
            Value::Temporal(tv) => temporal_get_property(tv, key),
            Value::Duration(dur) => duration_get_property(dur, key),
            _ => Value::Null,
        }
    }

    /// Convert to serde_json::Value for output.
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Node { node_id, labels, properties } => {
                let mut m = serde_json::Map::new();
                m.insert("node_id".into(), (*node_id).into());
                m.insert("labels".into(), serde_json::Value::Array(
                    labels.iter().map(|l| serde_json::Value::String(l.clone())).collect()
                ));
                m.insert("properties".into(), serde_json::Value::Object(properties.clone()));
                serde_json::Value::Object(m)
            }
            Value::Edge { edge_id, rel_type, source, target, properties } => {
                let mut m = serde_json::Map::new();
                m.insert("rel_id".into(), (*edge_id).into());
                m.insert("rel_type".into(), rel_type.clone().into());
                m.insert("source_node_id".into(), (*source).into());
                m.insert("target_node_id".into(), (*target).into());
                m.insert("properties".into(), serde_json::Value::Object(properties.clone()));
                serde_json::Value::Object(m)
            }
            Value::Int(v) => (*v).into(),
            Value::Float(v) => {
                if v.is_nan() {
                    serde_json::Value::String("NaN".into())
                } else {
                    serde_json::Number::from_f64(*v)
                        .map(serde_json::Value::Number)
                        .unwrap_or(serde_json::Value::Null)
                }
            }
            Value::Str(s) => s.clone().into(),
            Value::Bool(b) => (*b).into(),
            Value::Null => serde_json::Value::Null,
            Value::Json(v) => v.clone(),
            Value::Temporal(tv) => tv.iso.clone().into(),
            Value::Duration(dur) => dur.iso.clone().into(),
            Value::Path { nodes, rels } => {
                // Serialize as array: [node0, rel0, node1, rel1, ...]
                let mut arr: Vec<serde_json::Value> = Vec::new();
                for (i, node) in nodes.iter().enumerate() {
                    arr.push(node.to_json());
                    if i < rels.len() {
                        arr.push(rels[i].to_json());
                    }
                }
                serde_json::Value::Array(arr)
            }
        }
    }

    /// Get the node_id (for id() function and isomorphism checks).
    pub fn node_id(&self) -> Option<i64> {
        match self {
            Value::Node { node_id, .. } => Some(*node_id),
            _ => None,
        }
    }

    /// Get the edge_id.
    #[allow(dead_code)]
    pub fn edge_id(&self) -> Option<i64> {
        match self {
            Value::Edge { edge_id, .. } => Some(*edge_id),
            _ => None,
        }
    }
}

fn json_to_value(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => {
            // The special string "NaN" represents IEEE 754 NaN (produced by to_json() for
            // Value::Float(NaN)). Treat it as float NaN so it round-trips correctly through
            // lists and UNWIND, and sorts at the correct position in value_ordering.
            if s == "NaN" {
                Value::Float(f64::NAN)
            } else {
                Value::Str(s.clone())
            }
        }
        serde_json::Value::Object(m) => {
            // Detect serialized Node: {"node_id": i64, "labels": [...], "properties": {...}}
            if let (Some(serde_json::Value::Number(nid)), Some(serde_json::Value::Array(lbls)), Some(serde_json::Value::Object(props)))
                = (m.get("node_id"), m.get("labels"), m.get("properties"))
                && let Some(node_id) = nid.as_i64()
            {
                let labels = lbls.iter().filter_map(|l| l.as_str().map(|s| s.to_string())).collect();
                return Value::Node { node_id, labels, properties: props.clone() };
            }
            // Detect serialized Edge: {"rel_id": i64, "rel_type": str, "source_node_id": i64, "target_node_id": i64, "properties": {...}}
            if let (Some(serde_json::Value::Number(eid)), Some(serde_json::Value::String(rtype)),
                    Some(serde_json::Value::Number(src)), Some(serde_json::Value::Number(tgt)),
                    Some(serde_json::Value::Object(props)))
                = (m.get("rel_id"), m.get("rel_type"), m.get("source_node_id"), m.get("target_node_id"), m.get("properties"))
                && let (Some(edge_id), Some(source), Some(target)) = (eid.as_i64(), src.as_i64(), tgt.as_i64())
            {
                return Value::Edge { edge_id, rel_type: rtype.clone(), source, target, properties: props.clone() };
            }
            Value::Json(v.clone())
        }
        serde_json::Value::Array(arr) => {
            // Detect serialized Path: alternating node/edge JSON objects
            // A path serialized by Value::Path.to_json() is an array like [node_json, edge_json, node_json, ...]
            // where node_json has node_id key and edge_json has rel_id key.
            if !arr.is_empty() && is_path_array(arr) {
                let mut nodes = Vec::new();
                let mut rels = Vec::new();
                for (i, item) in arr.iter().enumerate() {
                    let val = json_to_value(item);
                    if i % 2 == 0 {
                        nodes.push(val);
                    } else {
                        rels.push(val);
                    }
                }
                return Value::Path { nodes, rels };
            }
            Value::Json(v.clone())
        }
    }
}

/// Returns true if a JSON array looks like a serialized path (alternating node/edge objects).
fn is_path_array(arr: &[serde_json::Value]) -> bool {
    if arr.is_empty() { return false; }
    for (i, item) in arr.iter().enumerate() {
        if let serde_json::Value::Object(m) = item {
            if i % 2 == 0 {
                // Even index → should be a node
                if !m.contains_key("node_id") { return false; }
            } else {
                // Odd index → should be an edge
                if !m.contains_key("rel_id") { return false; }
            }
        } else {
            return false;
        }
    }
    true
}

/// Execute error.
#[derive(Debug)]
pub struct ExecError {
    pub message: String,
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "exec error: {}", self.message)
    }
}

/// Execute a logical plan and return result rows.
///
/// `params` — query parameters ($name → value).
pub fn execute(
    plan: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    match plan {
        LogicalPlan::SingleRow => {
            Ok(vec![HashMap::new()])
        }
        LogicalPlan::LabelScan { variable, label, inline_props, optional } => {
            exec_label_scan(variable, label.as_deref(), inline_props, *optional, params)
        }
        LogicalPlan::Expand {
            input, src_var, rel_var, dst_var,
            rel_types, direction, rel_props,
            dst_labels, dst_props, optional,
        } => {
            exec_expand(
                input, src_var, rel_var.as_deref(), dst_var,
                rel_types, *direction, rel_props, dst_labels, dst_props,
                *optional, params,
            )
        }
        LogicalPlan::CrossProduct { left, right } => {
            exec_cross_product(left, right, params)
        }
        LogicalPlan::Filter { input, predicate } => {
            exec_filter(input, predicate, params)
        }
        LogicalPlan::Project { input, items, distinct, order_by, skip, limit } => {
            exec_project(input, items, *distinct, order_by, skip, limit, params)
        }
        LogicalPlan::Unwind { input, expr, alias } => {
            exec_unwind(input, expr, alias, params)
        }
        LogicalPlan::VarLengthExpand {
            input, src_var, rel_var, dst_var,
            rel_types, direction, min_hops, max_hops, optional, path_carry_var,
            excluded_rel_vars,
        } => {
            exec_var_length_expand(
                input, src_var, rel_var.as_deref(), dst_var,
                rel_types, *direction, *min_hops, *max_hops,
                *optional, path_carry_var.as_deref(), excluded_rel_vars, params,
            )
        }
        LogicalPlan::BoundRelListExpand {
            input, src_var, list_var, dst_var, direction,
        } => {
            exec_bound_rel_list_expand(input, src_var, list_var, dst_var, *direction, params)
        }
        LogicalPlan::NamedPath { input, path_var, element_vars } => {
            exec_named_path(input, path_var, element_vars, params)
        }
        LogicalPlan::Apply { outer, inner } => {
            exec_apply(outer, inner, params)
        }
        LogicalPlan::Empty => {
            Ok(Vec::new())
        }
        // v0.12.0 write plan nodes
        LogicalPlan::CreatePattern { input, patterns } => {
            exec_create_pattern(input, patterns, params)
        }
        LogicalPlan::SetProp { input, items } => {
            exec_set_prop(input, items, params)
        }
        LogicalPlan::RemoveProp { input, items } => {
            exec_remove_prop(input, items, params)
        }
        LogicalPlan::DeleteNodes { input, exprs, detach } => {
            exec_delete_nodes(input, exprs, *detach, params)
        }
        LogicalPlan::MergePattern { input, pattern, on_create, on_match } => {
            exec_merge_pattern(input, pattern, on_create, on_match, params)
        }
        LogicalPlan::Foreach { input, variable, list_expr, body } => {
            exec_foreach(input, variable, list_expr, body, params)
        }
        LogicalPlan::Union { left, right, all } => {
            let mut left_rows = execute(left, params)?;
            let right_rows = execute(right, params)?;
            left_rows.extend(right_rows);
            if !all {
                // Deduplicate: keep first occurrence of each fingerprint.
                let mut seen = std::collections::HashSet::new();
                left_rows.retain(|row| {
                    let fp = row_fingerprint(row);
                    seen.insert(fp)
                });
            }
            Ok(left_rows)
        }
        LogicalPlan::LeftJoin { outer, inner, null_vars } => {
            exec_left_join(outer, inner, null_vars, params)
        }
    }
}

/// Scan all nodes with an optional label filter.
fn exec_label_scan(
    variable: &str,
    label: Option<&str>,
    inline_props: &[(String, Expr)],
    optional: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, label_id_by_name, prop_key_name};
    use crate::storage::prop_store;

    // Get candidate node IDs.
    let node_ids: Vec<i64> = if let Some(lname) = label {
        let lid = label_id_by_name(lname);
        match lid {
            None => {
                // Label doesn't exist at all.
                if optional {
                    let mut null_row = Row::new();
                    null_row.insert(variable.to_string(), Value::Null);
                    return Ok(vec![null_row]);
                }
                return Ok(Vec::new());
            }
            Some(lid) => {
                Spi::connect(|client| {
                    client
                        .select(
                            "SELECT node_id FROM _pg_eddy.label_index WHERE label_id = $1",
                            None,
                            &[pgrx::datum::DatumWithOid::from(lid)],
                        )
                        .unwrap_or_else(|e| pgrx::error!("cypher label scan SPI: {e}"))
                        .filter_map(|row| row.get::<i64>(1).ok().flatten())
                        .collect()
                })
            }
        }
    } else {
        // Full scan
        unsafe {
            let rel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let mut state = crate::storage::node_store::NodeScanState::begin(rel, snapshot);
            let mut ids = Vec::new();
            while let Some(r) = state.next() {
                ids.push(r.node_id);
            }
            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            ids
        }
    };

    let mut rows = Vec::new();

    for nid in node_ids {
        let record = unsafe {
            let rel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let r = crate::storage::node_store::find_node_by_id(rel, nid, snapshot);
            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            r
        };

        if let Some(mut r) = record {
            // Resolve overflow properties.
            if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                r.prop_bytes = unsafe {
                    let rel = crate::open_nodes_relation();
                    let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    bytes
                };
            }

            let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
            let properties = prop_store::decode(&r.prop_bytes, prop_key_name);

            let val = Value::Node {
                node_id: nid,
                labels,
                properties,
            };

            // Check inline property filters.
            if !inline_props.is_empty() {
                let mut matches = true;
                for (key, expr) in inline_props {
                    let prop_val = val.get_property(key);
                    // Use params as the row context so upstream variables
                    // (e.g., from UNWIND) are visible in inline property filters.
                    let mut scan_row = Row::new();
                    for (k, v) in params {
                        scan_row.insert(k.clone(), json_to_value(v));
                    }
                    let expected = eval_expr(expr, &scan_row, params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            let mut row = Row::new();
            row.insert(variable.to_string(), val);
            rows.push(row);
        }
    }

    // If optional and no rows matched, return one null-filled row.
    if optional && rows.is_empty() {
        let mut null_row = Row::new();
        null_row.insert(variable.to_string(), Value::Null);
        return Ok(vec![null_row]);
    }

    Ok(rows)
}
#[allow(clippy::too_many_arguments)]
fn exec_expand(
    input: &LogicalPlan,
    src_var: &str,
    rel_var: Option<&str>,
    dst_var: &str,
    rel_types: &[String],
    direction: RelDirection,
    rel_props: &[(String, Expr)],
    dst_labels: &[String],
    dst_props: &[(String, Expr)],
    optional: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{ensure_rel_type, label_name, prop_key_name, rel_type_name, label_id_by_name};
    use crate::storage::edge_store::{Direction, adjacency_follow};
    use crate::storage::prop_store;

    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    let dir = match direction {
        RelDirection::Out => Direction::Out,
        RelDirection::In => Direction::In,
        RelDirection::Both => Direction::Both,
    };

    let type_filter: Option<i32> = if rel_types.len() == 1 {
        Some(ensure_rel_type(&rel_types[0]))
    } else {
        None
    };

    // Pre-resolve dst label IDs for filtering.
    let dst_label_ids: Vec<i32> = dst_labels.iter().filter_map(|l| label_id_by_name(l)).collect();
    // If any requested dst label does not exist in the catalog, no node can
    // satisfy the predicate. Short-circuit to an empty (or optional null-row)
    // result without scanning the graph.
    let dst_label_unsatisfiable = !dst_labels.is_empty() && dst_label_ids.len() != dst_labels.len();
    if dst_label_unsatisfiable {
        if optional {
            let mut out = Vec::new();
            for input_row in &input_rows {
                let mut nr = input_row.clone();
                if let Some(rv) = rel_var { nr.insert(rv.to_string(), Value::Null); }
                nr.insert(dst_var.to_string(), Value::Null);
                out.push(nr);
            }
            return Ok(out);
        }
        return Ok(Vec::new());
    }

    for input_row in &input_rows {
        // Look up the source variable in the row, or fall back to params (for correlated subqueries
        // where outer row bindings are passed through params by exec_pattern_inline).
        let src_val = if let Some(v) = input_row.get(src_var) {
            v.clone()
        } else if let Some(pv) = params.get(src_var) {
            json_to_value(pv)
        } else {
            return Err(ExecError { message: format!("unbound variable: {src_var}") });
        };

        // If src is NULL (propagated from upstream optional expand), emit a null row.
        if matches!(src_val, Value::Null) {
            if optional {
                let mut row = input_row.clone();
                row.insert(dst_var.to_string(), Value::Null);
                if let Some(rv) = rel_var { row.insert(rv.to_string(), Value::Null); }
                result.push(row);
            }
            continue;
        }

        let src_node_id = match src_val.node_id() {
            Some(id) => id,
            None => continue, // non-node value: skip this row (0 matches, like null)
        };

        // Follow adjacency chains.
        let edges = unsafe {
            let node_rel = crate::open_nodes_relation();
            let edge_rel = crate::open_edges_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let result = adjacency_follow(
                node_rel, edge_rel, src_node_id, dir, type_filter, snapshot,
            );
            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            result
        };

        // Multi-type filter (when more than 1 type specified).
        let type_ids: Vec<i32> = if rel_types.len() > 1 {
            rel_types.iter().map(|t| ensure_rel_type(t)).collect()
        } else {
            Vec::new()
        };

        let mut matched_any = false;

        for edge in &edges {
            // Type filter for multi-type patterns.
            if !type_ids.is_empty() && !type_ids.contains(&edge.rel_type_id) {
                continue;
            }

            let other_id = match direction {
                RelDirection::Out => edge.target_node_id,
                RelDirection::In => edge.source_node_id,
                RelDirection::Both => {
                    if edge.source_node_id == src_node_id {
                        edge.target_node_id
                    } else {
                        edge.source_node_id
                    }
                }
            };

            // Load the destination node.
            let dst_record = unsafe {
                let rel = crate::open_nodes_relation();
                let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                let r = crate::storage::node_store::find_node_by_id(rel, other_id, snapshot);
                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                r
            };

            let dst_record = match dst_record {
                Some(r) => r,
                None => continue, // invisible or deleted
            };

            // Resolve overflow.
            let mut dst_r = dst_record;
            if dst_r.overflow_blkno != 0 && dst_r.prop_bytes.is_empty() {
                dst_r.prop_bytes = unsafe {
                    let rel = crate::open_nodes_relation();
                    let bytes = crate::storage::node_store::read_overflow_block(rel, dst_r.overflow_blkno);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    bytes
                };
            }

            // Label filter on destination.
            if !dst_label_ids.is_empty() {
                let has_all = dst_label_ids.iter().all(|lid| dst_r.label_ids.contains(lid));
                if !has_all {
                    continue;
                }
            }

            let dst_labels_resolved: Vec<String> = dst_r.label_ids.iter().map(|id| label_name(*id)).collect();
            let dst_properties = prop_store::decode(&dst_r.prop_bytes, prop_key_name);

            let dst_val = Value::Node {
                node_id: other_id,
                labels: dst_labels_resolved,
                properties: dst_properties,
            };

            // Destination inline property filter.
            if !dst_props.is_empty() {
                let mut matches = true;
                for (key, expr) in dst_props {
                    let prop_val = dst_val.get_property(key);
                    let expected = eval_expr(expr, input_row, params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            // Build edge value.
            let edge_props = prop_store::decode(&edge.prop_bytes, prop_key_name);
            let edge_type_name = rel_type_name(edge.rel_type_id);

            let edge_val = Value::Edge {
                edge_id: edge.edge_id,
                rel_type: edge_type_name,
                source: edge.source_node_id,
                target: edge.target_node_id,
                properties: edge_props,
            };

            // Relationship inline property filter.
            if !rel_props.is_empty() {
                let mut matches = true;
                for (key, expr) in rel_props {
                    let prop_val = edge_val.get_property(key);
                    let expected = eval_expr(expr, input_row, params)?;
                    if !values_equal(&prop_val, &expected) {
                        matches = false;
                        break;
                    }
                }
                if !matches {
                    continue;
                }
            }

            // If rel_var is already bound in the input row (forwarded from a previous WITH),
            // only keep edges whose edge_id matches the existing binding.
            if let Some(rv) = rel_var
                && let Some(existing_rel) = input_row.get(rv)
                && !matches!(existing_rel, Value::Null)
            {
                match (existing_rel.edge_id(), edge_val.edge_id()) {
                    (Some(e1), Some(e2)) if e1 == e2 => { /* edge matches */ }
                    _ => continue, // edge doesn't match the bound rel_var
                }
            }

            // Self-loop / variable reuse check: if dst_var is already bound in the input row
            // OR in params (correlated subquery / LeftJoin outer binding), only keep rows
            // where the found destination matches the existing binding.
            // Only check params for user-named variables (not anonymous `_anon_*` vars) to
            // avoid collisions when pattern comprehensions inject outer row bindings as params.
            let existing_dst_val = input_row.get(dst_var).cloned()
                .or_else(|| if !dst_var.starts_with("_anon_") { params.get(dst_var).map(json_to_value) } else { None });
            if let Some(existing_dst) = existing_dst_val
                && !matches!(existing_dst, Value::Null)
                && existing_dst.node_id() != dst_val.node_id()
            {
                continue;
            }

            let mut row = input_row.clone();
            // Ensure src_var is in the row (may have come from params fallback for correlated patterns).
            row.entry(src_var.to_string()).or_insert_with(|| src_val.clone());
            row.insert(dst_var.to_string(), dst_val);
            if let Some(rv) = rel_var {
                row.insert(rv.to_string(), edge_val);
            }
            result.push(row);
            matched_any = true;
        }

        // OPTIONAL: if no edges matched, emit a null row.
        if optional && !matched_any {
            let mut row = input_row.clone();
            // Only null out dst_var if it was not already bound (new variable).
            // If it was already bound (e.g. from a prior MATCH or WITH), preserve the binding.
            if !input_row.contains_key(dst_var) {
                row.insert(dst_var.to_string(), Value::Null);
            }
            if let Some(rv) = rel_var {
                // Only null out rel_var if it was not already bound in the input row.
                // If it was already bound (e.g. from a prior WITH), preserve the binding.
                if !input_row.contains_key(rv) {
                    row.insert(rv.to_string(), Value::Null);
                }
            }
            result.push(row);
        }
    }

    // OPTIONAL MATCH with no source rows at all → one null row
    if optional && result.is_empty() {
        let mut null_row = Row::new();
        null_row.insert(src_var.to_string(), Value::Null);
        null_row.insert(dst_var.to_string(), Value::Null);
        if let Some(rv) = rel_var { null_row.insert(rv.to_string(), Value::Null); }
        result.push(null_row);
    }

    Ok(result)
}

fn exec_unwind(
    input: &LogicalPlan,
    expr: &Expr,
    alias: &str,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    for input_row in &input_rows {
        let val = eval_expr(expr, input_row, params)?;
        let items = match val {
            Value::Json(serde_json::Value::Array(arr)) => arr,
            Value::Null => continue, // UNWIND null → no rows
            other => {
                // Single scalar: wrap in a one-element array.
                vec![other.to_json()]
            }
        };
        for item in items {
            let mut row = input_row.clone();
            row.insert(alias.to_string(), json_to_value(&item));
            result.push(row);
        }
    }

    Ok(result)
}

fn exec_cross_product(
    left: &LogicalPlan,
    right: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let left_rows = execute(left, params)?;
    let mut result = Vec::new();

    for lr in &left_rows {
        // Merge left-row variables into params so the right side can reference
        // them in inline property filters (e.g., UNWIND ... MATCH (n {id: r.x})).
        let mut merged = params.clone();
        for (k, v) in lr {
            merged.insert(k.clone(), v.to_json());
        }
        let right_rows = execute(right, &merged)?;
        for rr in &right_rows {
            let mut row = lr.clone();
            row.extend(rr.clone());
            result.push(row);
        }
    }

    Ok(result)
}

/// Build a Value::Path from element variable names already bound in the row.
/// `elem_vars` alternates [node_var, rel_var, node_var, rel_var, ..., node_var].
fn build_path_from_elem_vars(elem_vars: &[String], row: &Row) -> Value {
    let mut nodes: Vec<Value> = Vec::new();
    let mut rels: Vec<Value> = Vec::new();
    for (i, var) in elem_vars.iter().enumerate() {
        let val = row.get(var).cloned().unwrap_or(Value::Null);
        if i % 2 == 0 {
            nodes.push(val);
        } else {
            rels.push(val);
        }
    }
    Value::Path { nodes, rels }
}

/// Build a Value::Path from lists of node IDs and edge IDs (for var-length named paths).
/// Build a list of relationship values from edge IDs (for var-length rel variables).
fn build_rel_list_from_ids(edge_ids: &[i64]) -> Value {
    use crate::catalog::labels::{prop_key_name, rel_type_name};
    use crate::storage::{edge_store, prop_store};
    let mut rels: Vec<serde_json::Value> = Vec::new();
    for &eid in edge_ids {
        let edge_val = unsafe {
            let edge_rel = crate::open_edges_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let r = edge_store::find_edge_by_id(edge_rel, eid, snapshot);
            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            r
        };
        if let Some(er) = edge_val {
            let edge_props = prop_store::decode(&er.prop_bytes, prop_key_name);
            let edge_type = rel_type_name(er.rel_type_id);
            let ev = Value::Edge {
                edge_id: eid,
                rel_type: edge_type,
                source: er.source_node_id,
                target: er.target_node_id,
                properties: edge_props,
            };
            rels.push(ev.to_json());
        }
    }
    Value::Json(serde_json::Value::Array(rels))
}

fn build_path_from_ids(
    node_ids: &[i64],
    edge_ids: &[i64],
    direction: RelDirection,
) -> Value {
    use crate::catalog::labels::{label_name, prop_key_name, rel_type_name};
    use crate::storage::{node_store, edge_store, prop_store};

    let mut nodes: Vec<Value> = Vec::new();
    let mut rels: Vec<Value> = Vec::new();

    for &nid in node_ids {
        let node_val = unsafe {
            let rel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let r = node_store::find_node_by_id(rel, nid, snapshot);
            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            r
        };
        if let Some(mut nr) = node_val {
            if nr.overflow_blkno != 0 && nr.prop_bytes.is_empty() {
                nr.prop_bytes = unsafe {
                    let rel = crate::open_nodes_relation();
                    let bytes = node_store::read_overflow_block(rel, nr.overflow_blkno);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    bytes
                };
            }
            let labels: Vec<String> = nr.label_ids.iter().map(|id| label_name(*id)).collect();
            let properties = prop_store::decode(&nr.prop_bytes, prop_key_name);
            nodes.push(Value::Node { node_id: nid, labels, properties });
        } else {
            nodes.push(Value::Null);
        }
    }

    for (i, &eid) in edge_ids.iter().enumerate() {
        let edge_val = unsafe {
            let edge_rel = crate::open_edges_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let r = edge_store::find_edge_by_id(edge_rel, eid, snapshot);
            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            r
        };
        if let Some(er) = edge_val {
            let edge_props = prop_store::decode(&er.prop_bytes, prop_key_name);
            let edge_type = rel_type_name(er.rel_type_id);
            // Determine direction for this hop based on node_ids.
            let (src_id, tgt_id) = if i + 1 < node_ids.len() {
                (node_ids[i], node_ids[i + 1])
            } else {
                (er.source_node_id, er.target_node_id)
            };
            let (actual_src, actual_tgt) = match direction {
                RelDirection::Out => (src_id, tgt_id),
                RelDirection::In => (tgt_id, src_id),
                RelDirection::Both => {
                    if er.source_node_id == src_id {
                        (er.source_node_id, er.target_node_id)
                    } else {
                        (er.target_node_id, er.source_node_id)
                    }
                }
            };
            rels.push(Value::Edge {
                edge_id: eid,
                rel_type: edge_type,
                source: actual_src,
                target: actual_tgt,
                properties: edge_props,
            });
        } else {
            rels.push(Value::Null);
        }
    }

    Value::Path { nodes, rels }
}

/// BFS-based variable-length expansion: -[*min..max]-.
#[allow(clippy::too_many_arguments)]
fn exec_var_length_expand(
    input: &LogicalPlan,
    src_var: &str,
    rel_var: Option<&str>,
    dst_var: &str,
    rel_types: &[String],
    direction: RelDirection,
    min_hops: u32,
    max_hops: Option<u32>,
    optional: bool,
    path_carry_var: Option<&str>,
    excluded_rel_vars: &[String],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, prop_key_name};
    use crate::storage::edge_store::{Direction, adjacency_follow};
    use crate::storage::prop_store;

    let input_rows = execute(input, params)?;
    let mut result: Vec<Row> = Vec::new();

    // Pre-resolve type filter IDs.
    let type_filter: Option<i32> = if rel_types.len() == 1 {
        Some(crate::catalog::labels::ensure_rel_type(&rel_types[0]))
    } else {
        None
    };
    let dir = match direction {
        RelDirection::Out => Direction::Out,
        RelDirection::In => Direction::In,
        RelDirection::Both => Direction::Both,
    };

    for input_row in &input_rows {
        let src_val = if let Some(v) = input_row.get(src_var) {
            v.clone()
        } else if let Some(pv) = params.get(src_var) {
            // Fallback to params for correlated subqueries (exec_pattern_inline injects outer row).
            json_to_value(pv)
        } else {
            if optional {
                let mut r = input_row.clone();
                r.insert(dst_var.to_string(), Value::Null);
                if let Some(rv) = rel_var { r.insert(rv.to_string(), Value::Null); }
                result.push(r);
            }
            continue;
        };

        let start_id = match &src_val {
            Value::Node { node_id, .. } => *node_id,
            Value::Null => {
                if optional {
                    let mut r = input_row.clone();
                    r.insert(dst_var.to_string(), Value::Null);
                    if let Some(rv) = rel_var { r.insert(rv.to_string(), Value::Null); }
                    result.push(r);
                }
                continue;
            }
            _ => continue,
        };

        // BFS: (node_id, depth, visited_edge_ids, path_node_ids, path_edge_ids)
        // path_node_ids/path_edge_ids are only populated when path_carry_var is set.
        let max_depth = max_hops.unwrap_or(u32::MAX).min(256); // safety cap

        // Collect edge IDs from excluded_rel_vars (cross-hop uniqueness).
        let excluded_edge_ids: Vec<i64> = excluded_rel_vars.iter().filter_map(|rv| {
            let v = input_row.get(rv).cloned().or_else(|| params.get(rv).map(json_to_value));
            match v {
                Some(Value::Edge { edge_id, .. }) => Some(edge_id),
                _ => None,
            }
        }).collect();

        // If dst_var is already bound in the input row or in params (correlated pattern),
        // filter BFS results to only paths ending at that node.
        // Only check params for user-named variables (not anonymous `_anon_*` vars).
        let expected_dst_id: Option<i64> = {
            let bound_dst = input_row.get(dst_var)
                .cloned()
                .or_else(|| if !dst_var.starts_with("_anon_") { params.get(dst_var).map(json_to_value) } else { None });
            if let Some(Value::Node { node_id, .. }) = bound_dst { Some(node_id) } else { None }
        };

        struct BfsEntry {
            node_id: i64,
            depth: u32,
            visited_edge_ids: Vec<i64>,
            path_node_ids: Vec<i64>,  // node IDs in order (for path tracking)
            path_edge_ids: Vec<i64>,  // edge IDs in order (for path tracking)
        }

        let mut queue: std::collections::VecDeque<BfsEntry> = std::collections::VecDeque::new();
        queue.push_back(BfsEntry {
            node_id: start_id,
            depth: 0,
            visited_edge_ids: Vec::new(),
            path_node_ids: vec![start_id],
            path_edge_ids: Vec::new(),
        });
        let mut found_any = false;

        while let Some(entry) = queue.pop_front() {
            if entry.depth >= min_hops {
                // Emit this as a result row if it's not the start node (or if 0-hop is allowed).
                if entry.depth > 0 || min_hops == 0 {
                    // Load destination node
                    let dst_record = unsafe {
                        let rel = crate::open_nodes_relation();
                        let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                        let r = crate::storage::node_store::find_node_by_id(rel, entry.node_id, snapshot);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        r
                    };
                    if let Some(mut dst_r) = dst_record {
                        if dst_r.overflow_blkno != 0 && dst_r.prop_bytes.is_empty() {
                            dst_r.prop_bytes = unsafe {
                                let rel = crate::open_nodes_relation();
                                let bytes = crate::storage::node_store::read_overflow_block(rel, dst_r.overflow_blkno);
                                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                bytes
                            };
                        }
                        let dst_labels: Vec<String> = dst_r.label_ids.iter().map(|id| label_name(*id)).collect();
                        let dst_props = prop_store::decode(&dst_r.prop_bytes, prop_key_name);
                        let dst_val = Value::Node {
                            node_id: entry.node_id,
                            labels: dst_labels,
                            properties: dst_props,
                        };
                        // If dst_var is bound in the outer scope, only emit rows that match.
                        if let Some(eid) = expected_dst_id {
                            if entry.node_id != eid {
                                // Continue BFS but don't emit this path.
                                // Don't set found_any here — just keep exploring.
                                // (We still enqueue children below.)
                                // We DO need to push to the BFS queue, so use a flag approach.
                                // Actually: just skip emitting and keep going (the enqueue is below).
                                // For found_any tracking: skip.
                            } else {
                                let mut row = input_row.clone();
                                // Store src node so NamedPath can find it.
                                row.entry(src_var.to_string()).or_insert_with(|| src_val.clone());
                                row.insert(dst_var.to_string(), dst_val);
                                if let Some(rv) = rel_var {
                                    row.insert(rv.to_string(), build_rel_list_from_ids(&entry.path_edge_ids));
                                }
                                if let Some(pcv) = path_carry_var {
                                    let path_val = build_path_from_ids(
                                        &entry.path_node_ids,
                                        &entry.path_edge_ids,
                                        direction,
                                    );
                                    row.insert(pcv.to_string(), path_val);
                                }
                                result.push(row);
                                found_any = true;
                            }
                        } else {
                            let mut row = input_row.clone();
                            // Store src node so NamedPath can find it.
                            row.entry(src_var.to_string()).or_insert_with(|| src_val.clone());
                            row.insert(dst_var.to_string(), dst_val);
                            if let Some(rv) = rel_var {
                                row.insert(rv.to_string(), build_rel_list_from_ids(&entry.path_edge_ids));
                            }
                            // Build and store the full path if path_carry_var is set.
                            if let Some(pcv) = path_carry_var {
                                let path_val = build_path_from_ids(
                                    &entry.path_node_ids,
                                    &entry.path_edge_ids,
                                    direction,
                                );
                                row.insert(pcv.to_string(), path_val);
                            }
                            result.push(row);
                            found_any = true;
                        }
                    }
                }
            }

            if entry.depth < max_depth {
                // Expand to neighbours
                let edges = unsafe {
                    let node_rel = crate::open_nodes_relation();
                    let edge_rel = crate::open_edges_relation();
                    let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                    let r = adjacency_follow(
                        node_rel, edge_rel, entry.node_id, dir, type_filter, snapshot,
                    );
                    pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    r
                };

                // Multi-type filter
                let type_ids: Vec<i32> = if rel_types.len() > 1 {
                    rel_types.iter().map(|t| crate::catalog::labels::ensure_rel_type(t)).collect()
                } else {
                    Vec::new()
                };

                for edge in &edges {
                    if !type_ids.is_empty() && !type_ids.contains(&edge.rel_type_id) {
                        continue;
                    }
                    // Skip already-visited edges (no repeated edges)
                    if entry.visited_edge_ids.contains(&edge.edge_id) {
                        continue;
                    }
                    // Skip edges excluded by cross-hop uniqueness.
                    if excluded_edge_ids.contains(&edge.edge_id) {
                        continue;
                    }
                    let next_node_id = match direction {
                        RelDirection::Out => edge.target_node_id,
                        RelDirection::In => edge.source_node_id,
                        RelDirection::Both => {
                            if edge.source_node_id == entry.node_id { edge.target_node_id }
                            else { edge.source_node_id }
                        }
                    };
                    let mut new_visited = entry.visited_edge_ids.clone();
                    new_visited.push(edge.edge_id);
                    let mut new_path_nodes = entry.path_node_ids.clone();
                    new_path_nodes.push(next_node_id);
                    let mut new_path_edges = entry.path_edge_ids.clone();
                    new_path_edges.push(edge.edge_id);
                    queue.push_back(BfsEntry {
                        node_id: next_node_id,
                        depth: entry.depth + 1,
                        visited_edge_ids: new_visited,
                        path_node_ids: new_path_nodes,
                        path_edge_ids: new_path_edges,
                    });
                }
            }
        }

        if !found_any && optional {
            let mut r = input_row.clone();
            // Only null-fill the destination if it was NOT already bound in the
            // input row.  A pre-bound destination that simply didn't match should
            // keep its original value; only the relationship and path variables
            // are new and should be set to null.
            if expected_dst_id.is_none() {
                r.insert(dst_var.to_string(), Value::Null);
            }
            if let Some(rv) = rel_var { r.insert(rv.to_string(), Value::Null); }
            if let Some(pcv) = path_carry_var { r.insert(pcv.to_string(), Value::Null); }
            result.push(r);
        }
    }

    Ok(result)
}

/// Pre-bound relationship list expand (deprecated Cypher feature).
/// `rs` is already bound as a list of Edge values. Walk them in order,
/// verifying they form a connected path from `src_var` to the resulting
/// `dst_var` endpoint. If `src_var` is pre-bound as a specific node, only
/// emit if the chain starts there. Direction is considered when checking
/// connectivity.
fn exec_bound_rel_list_expand(
    input: &LogicalPlan,
    src_var: &str,
    list_var: &str,
    dst_var: &str,
    direction: RelDirection,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, prop_key_name};
    use crate::storage::prop_store;

    let input_rows = execute(input, params)?;
    let mut result: Vec<Row> = Vec::new();

    for input_row in &input_rows {
        // Get the source node.
        let src_val = input_row.get(src_var)
            .cloned()
            .or_else(|| params.get(src_var).map(json_to_value));
        let src_id = match &src_val {
            Some(Value::Node { node_id, .. }) => *node_id,
            _ => continue,
        };

        // Get the pre-bound relationship list from the row or params.
        let list_val = input_row.get(list_var)
            .cloned()
            .or_else(|| params.get(list_var).map(json_to_value));
        // The list is Value::Json(Array([edge_json, ...])) — convert items to Value.
        let edges: Vec<Value> = match &list_val {
            Some(Value::Json(serde_json::Value::Array(arr))) => {
                arr.iter().map(json_to_value).collect()
            }
            _ => continue,
        };

        // Walk the edge list and verify connectivity.
        let mut current_node = src_id;
        let mut connected = true;
        for edge in &edges {
            if let Value::Edge { source, target, .. } = edge {
                match direction {
                    RelDirection::Out => {
                        if *source != current_node {
                            connected = false;
                            break;
                        }
                        current_node = *target;
                    }
                    RelDirection::In => {
                        if *target != current_node {
                            connected = false;
                            break;
                        }
                        current_node = *source;
                    }
                    RelDirection::Both => {
                        if *source == current_node {
                            current_node = *target;
                        } else if *target == current_node {
                            current_node = *source;
                        } else {
                            connected = false;
                            break;
                        }
                    }
                }
            } else {
                connected = false;
                break;
            }
        }

        if !connected {
            continue;
        }

        // Check if dst_var is pre-bound; if so, verify the endpoint matches.
        let expected_dst_id: Option<i64> = {
            let bound_dst = input_row.get(dst_var)
                .cloned()
                .or_else(|| if !dst_var.starts_with("_anon_") { params.get(dst_var).map(json_to_value) } else { None });
            if let Some(Value::Node { node_id, .. }) = bound_dst { Some(node_id) } else { None }
        };
        if let Some(eid) = expected_dst_id {
            if current_node != eid {
                continue;
            }
        }

        // Build the destination node value.
        let dst_val = unsafe {
            let nrel = crate::open_nodes_relation();
            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
            let record = crate::storage::node_store::find_node_by_id(nrel, current_node, snapshot);
            pgrx::pg_sys::table_close(nrel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
            if let Some(mut r) = record {
                if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                    let nrel2 = crate::open_nodes_relation();
                    r.prop_bytes = crate::storage::node_store::read_overflow_block(nrel2, r.overflow_blkno);
                    pgrx::pg_sys::table_close(nrel2, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                }
                let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
                let properties = prop_store::decode(&r.prop_bytes, prop_key_name);
                Value::Node { node_id: current_node, labels, properties }
            } else {
                continue;
            }
        };

        let mut row = input_row.clone();
        row.insert(dst_var.to_string(), dst_val);
        result.push(row);
    }

    Ok(result)
}

/// Named path: packages the matched nodes+rels into a Path value using element_vars.
/// element_vars alternates: [node_var, rel_var, node_var, rel_var, ..., node_var].
/// For var-length segments, the rel_var slot holds a path_carry_var that the
/// VarLengthExpand executor has already stored as a Value::Path.
fn exec_named_path(
    input: &LogicalPlan,
    path_var: &str,
    element_vars: &[String],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;
    let mut result = Vec::new();

    for mut row in rows {
        // Single-node path (element_vars = [node_var]).
        if element_vars.len() == 1 {
            let node = row.get(&element_vars[0]).cloned().unwrap_or(Value::Null);
            let path = Value::Path { nodes: vec![node], rels: vec![] };
            row.insert(path_var.to_string(), path);
            result.push(row);
            continue;
        }

        // If any relationship slot (odd index in element_vars) is null, the optional
        // match found no result → the path variable should be null, not a partial path.
        let has_null_rel = (1..element_vars.len()).step_by(2).any(|i| {
            matches!(row.get(&element_vars[i]), Some(Value::Null) | None)
        });
        if has_null_rel {
            row.insert(path_var.to_string(), Value::Null);
            result.push(row);
            continue;
        }

        // Multi-element: alternating node/rel.
        // element_vars: [n0, r0, n1, r1, ..., nN]
        // Check for var-length carry: if a rel slot holds a Value::Path (from VarLengthExpand),
        // merge that path instead of looking up individual elements.
        let mut nodes: Vec<Value> = Vec::new();
        let mut rels: Vec<Value> = Vec::new();
        let mut i = 0;
        while i < element_vars.len() {
            if i % 2 == 0 {
                // Node slot.
                let v = row.get(&element_vars[i]).cloned().unwrap_or(Value::Null);
                nodes.push(v);
                i += 1;
            } else {
                // Rel slot — could be a path_carry_var (Value::Path) from VarLengthExpand.
                let v = row.get(&element_vars[i]).cloned().unwrap_or(Value::Null);
                if let Value::Path { nodes: pnodes, rels: prels } = v {
                    // Inline the carried path: merge into current path.
                    // The first pnodes[0] is the same as the last pushed node — skip it.
                    // Then alternately add rels and nodes.
                    for (ri, rel_val) in prels.into_iter().enumerate() {
                        rels.push(rel_val);
                        if ri + 1 < pnodes.len() {
                            nodes.push(pnodes[ri + 1].clone());
                        }
                    }
                    // Skip the next node slot (already consumed from path).
                    i += 2;
                } else {
                    rels.push(v);
                    i += 1;
                }
            }
        }

        // Relationship isomorphism: within a named path, each relationship must be
        // distinct (same physical edge cannot appear twice in the same path traversal).
        {
            let mut seen_edge_ids = std::collections::HashSet::new();
            let has_dup_rel = rels.iter().any(|r| {
                if let Some(eid) = r.edge_id() { !seen_edge_ids.insert(eid) } else { false }
            });
            if has_dup_rel {
                continue;
            }
        }

        let path = Value::Path { nodes, rels };
        row.insert(path_var.to_string(), path);
        result.push(row);
    }

    Ok(result)
}

fn exec_filter(
    input: &LogicalPlan,
    predicate: &Expr,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;
    let mut result = Vec::new();

    for row in &rows {
        let val = eval_expr(predicate, row, params)?;
        if val.is_truthy() {
            result.push(row.clone());
        }
    }

    Ok(result)
}

fn exec_project(
    input: &LogicalPlan,
    items: &[ReturnItem],
    distinct: bool,
    order_by: &[crate::cypher::ast::OrderItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let rows = execute(input, params)?;

    // If any return item contains an aggregate function, use grouping.
    if items.iter().any(|i| expr_has_aggregate(&i.expr)) {
        return exec_project_aggregate(rows, items, distinct, order_by, skip, limit, params);
    }

    // Build (projected_row, input_row) pairs; keep input rows for ORDER BY alias resolution.
    let mut projected: Vec<(Row, Row)> = Vec::with_capacity(rows.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for row in rows {
        let mut out_row = Row::new();
        for (idx, item) in items.iter().enumerate() {
            if matches!(item.expr, Expr::Star) {
                // RETURN * — expand all bound variables from the row.
                for (k, v) in &row {
                    out_row.entry(k.clone()).or_insert_with(|| v.clone());
                }
            } else {
                let val = eval_expr(&item.expr, &row, params)?;
                let key = item.alias.clone().unwrap_or_else(|| {
                    expr_default_name(&item.expr, idx)
                });
                out_row.insert(key, val);
            }
        }
        projected.push((out_row, row));
    }

    // ORDER BY — sort before DISTINCT/SKIP/LIMIT
    // Pre-validate: if ORDER BY references an unbound variable on the first
    // row, it will fail on every row — raise immediately.
    if !order_by.is_empty() && !projected.is_empty() {
        let (ref first_proj, ref first_in) = projected[0];
        for item in order_by {
            let res = eval_expr(&item.expr, first_proj, params)
                .or_else(|_| eval_expr(&item.expr, first_in, params));
            if let Err(e) = res
                && e.message.starts_with("unbound variable")
            {
                return Err(ExecError {
                    message: format!("SyntaxError: {}", e.message),
                });
            }
        }
        projected.sort_by(|(proj_a, in_a), (proj_b, in_b)| {
            for item in order_by {
                let av = eval_expr(&item.expr, proj_a, params)
                    .or_else(|_| eval_expr(&item.expr, in_a, params))
                    .unwrap_or(Value::Null);
                let bv = eval_expr(&item.expr, proj_b, params)
                    .or_else(|_| eval_expr(&item.expr, in_b, params))
                    .unwrap_or(Value::Null);
                let cmp = value_ordering(&av, &bv);
                if cmp != std::cmp::Ordering::Equal {
                    return if item.ascending { cmp } else { cmp.reverse() };
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // DISTINCT
    if distinct {
        projected.retain(|(out_row, _)| {
            let fp = row_fingerprint(out_row);
            seen.insert(fp)
        });
    }

    // SKIP
    if let Some(skip_expr) = skip {
        let n = eval_const_usize(skip_expr, params)?;
        if n >= projected.len() {
            projected.clear();
        } else {
            projected.drain(0..n);
        }
    }

    // LIMIT
    if let Some(limit_expr) = limit {
        let n = eval_const_usize(limit_expr, params)?;
        projected.truncate(n);
    }

    Ok(projected.into_iter().map(|(out_row, _)| out_row).collect())
}

/// Aggregating version of exec_project: groups rows by non-aggregate key items,
/// computes aggregates per group, then applies ORDER BY / DISTINCT / SKIP / LIMIT.
fn exec_project_aggregate(
    rows: Vec<Row>,
    items: &[ReturnItem],
    distinct: bool,
    order_by: &[crate::cypher::ast::OrderItem],
    skip: &Option<Expr>,
    limit: &Option<Expr>,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    // Semantic check: AmbiguousAggregationExpression.
    //
    // Collect grouping key expressions (non-aggregate RETURN/WITH items).
    // Also include aliases as Variable("alias") so ORDER BY can reference them.
    let mut key_exprs: Vec<Expr> = Vec::new();
    for item in items {
        if !expr_has_aggregate(&item.expr) {
            key_exprs.push(item.expr.clone());
            if let Some(alias) = &item.alias {
                key_exprs.push(Expr::Variable(alias.clone()));
            }
        }
    }

    // For each aggregate-containing expression (RETURN items and ORDER BY items),
    // every "free" variable/property reference (not nested inside an aggregate call)
    // must appear as an exact key expression.
    for item in items {
        if expr_has_aggregate(&item.expr) {
            let mut free_refs: Vec<Expr> = Vec::new();
            collect_free_var_refs(&item.expr, &mut free_refs);
            for fref in &free_refs {
                if !key_exprs.iter().any(|k| expr_structural_eq(k, fref)) {
                    return Err(ExecError {
                        message: "SyntaxError: AmbiguousAggregationExpression: expression \
                                   mixes aggregate function calls and non-aggregated \
                                   variable references"
                            .into(),
                    });
                }
            }
        }
    }
    for ob in order_by {
        if expr_has_aggregate(&ob.expr) {
            let mut free_refs: Vec<Expr> = Vec::new();
            collect_free_var_refs(&ob.expr, &mut free_refs);
            for fref in &free_refs {
                if !key_exprs.iter().any(|k| expr_structural_eq(k, fref)) {
                    return Err(ExecError {
                        message: "SyntaxError: AmbiguousAggregationExpression in ORDER BY: \
                                   expression mixes aggregate and non-aggregated \
                                   variable references"
                            .into(),
                    });
                }
            }
        }
    }

    // Separate key items (no aggregate) from aggregate items.
    let has_key = items.iter().any(|i| !expr_has_aggregate(&i.expr));

    // Groups: ordered by first seen fingerprint.
    // Each entry: (fingerprint, key_row, group_rows, first_input_row)
    let mut group_order: Vec<String> = Vec::new();
    let mut groups: std::collections::HashMap<String, (Row, Vec<Row>, Row)> =
        std::collections::HashMap::new();

    for row in &rows {
        // Build key row from non-aggregate items.
        let mut key_row = Row::new();
        let mut key_parts: Vec<String> = Vec::new();
        for (idx, item) in items.iter().enumerate() {
            if !expr_has_aggregate(&item.expr) {
                let val = eval_expr(&item.expr, row, params).unwrap_or(Value::Null);
                let col = item.alias.clone()
                    .unwrap_or_else(|| expr_default_name(&item.expr, idx));
                key_parts.push(format!("{}={}", col,
                    serde_json::to_string(&val.to_json()).unwrap_or_default()));
                key_row.insert(col, val);
            }
        }
        let fp = key_parts.join("\x00");
        if !groups.contains_key(&fp) {
            group_order.push(fp.clone());
            groups.insert(fp.clone(), (key_row, Vec::new(), row.clone()));
        }
        groups.get_mut(&fp).unwrap().1.push(row.clone());
    }

    // If no rows and no grouping keys (e.g. RETURN count(*) on empty graph),
    // produce one empty group so COUNT returns 0.
    if groups.is_empty() && !has_key {
        group_order.push(String::new());
        groups.insert(String::new(), (Row::new(), Vec::new(), Row::new()));
    }

    let mut projected: Vec<(Row, Row)> = Vec::new();
    for fp in &group_order {
        let (key_row, group_rows, first_row) = groups.get(fp).unwrap();
        let mut out_row = Row::new();
        for (idx, item) in items.iter().enumerate() {
            let val = eval_with_agg(&item.expr, group_rows, key_row, params)?;
            let col = item.alias.clone()
                .unwrap_or_else(|| expr_default_name(&item.expr, idx));
            out_row.insert(col, val);
        }
        // Merge key_row and first_row into the "input" row for ORDER BY fallback.
        let mut in_row = first_row.clone();
        for (k, v) in key_row { in_row.entry(k.clone()).or_insert_with(|| v.clone()); }
        projected.push((out_row, in_row));
    }

    // ORDER BY
    // Pre-validate: if ORDER BY references an unbound variable on the first
    // row, it will fail on every row — raise immediately.
    if !order_by.is_empty() && !projected.is_empty() {
        let (ref first_proj, ref first_in) = projected[0];
        for item in order_by {
            let res = eval_expr(&item.expr, first_proj, params)
                .or_else(|_| eval_expr(&item.expr, first_in, params));
            if let Err(e) = res
                && e.message.starts_with("unbound variable")
            {
                return Err(ExecError {
                    message: format!("SyntaxError: {}", e.message),
                });
            }
        }
        projected.sort_by(|(proj_a, in_a), (proj_b, in_b)| {
            for item in order_by {
                // Evaluate ORDER BY expression with fallbacks:
                // 1. eval on projected row (fast path for aliases / simple exprs)
                // 2. column lookup by default name (handles aggregate exprs like count(*))
                // 3. eval on original input row (handles node.prop when node not projected)
                let eval_ob = |proj: &Row, fallback: &Row| -> Value {
                    if let Ok(v) = eval_expr(&item.expr, proj, params) {
                        return v;
                    }
                    let col = expr_default_name(&item.expr, 0);
                    if let Some(v) = proj.get(&col) {
                        return v.clone();
                    }
                    eval_expr(&item.expr, fallback, params).unwrap_or(Value::Null)
                };
                let av = eval_ob(proj_a, in_a);
                let bv = eval_ob(proj_b, in_b);
                let cmp = value_ordering(&av, &bv);
                if cmp != std::cmp::Ordering::Equal {
                    return if item.ascending { cmp } else { cmp.reverse() };
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // DISTINCT
    if distinct {
        let mut seen = std::collections::HashSet::new();
        projected.retain(|(out_row, _)| seen.insert(row_fingerprint(out_row)));
    }

    // SKIP
    if let Some(skip_expr) = skip {
        let n = eval_const_usize(skip_expr, params)?;
        if n >= projected.len() { projected.clear(); } else { projected.drain(0..n); }
    }

    // LIMIT
    if let Some(limit_expr) = limit {
        projected.truncate(eval_const_usize(limit_expr, params)?);
    }

    Ok(projected.into_iter().map(|(out_row, _)| out_row).collect())
}

/// Returns true if the expression contains any aggregate function call.
fn expr_has_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::FunctionCall(name, args) => {
            is_aggregate_name(name) || args.iter().any(expr_has_aggregate)
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r)
        | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) | Expr::InList(l, r) => {
            expr_has_aggregate(l) || expr_has_aggregate(r)
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::Property(e, _) | Expr::HasLabel(e, _) => expr_has_aggregate(e),
        Expr::Subscript(l, r) => expr_has_aggregate(l) || expr_has_aggregate(r),
        Expr::ListSlice { list_expr, from, to, .. } => {
            expr_has_aggregate(list_expr)
                || from.as_deref().is_some_and(expr_has_aggregate)
                || to.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::List(exprs) => exprs.iter().any(expr_has_aggregate),
        Expr::MapLiteral(pairs) => pairs.iter().any(|(_, v)| expr_has_aggregate(v)),
        Expr::CaseSearched { branches, else_ } => {
            branches.iter().any(|(c, t)| expr_has_aggregate(c) || expr_has_aggregate(t))
                || else_.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::CaseSimple { test, branches, else_ } => {
            expr_has_aggregate(test)
                || branches.iter().any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::ListComprehension { list_expr, predicate, projection, .. } => {
            expr_has_aggregate(list_expr)
                || predicate.as_deref().is_some_and(expr_has_aggregate)
                || projection.as_deref().is_some_and(expr_has_aggregate)
        }
        Expr::ListPredicate { list_expr, predicate, .. } => {
            expr_has_aggregate(list_expr) || expr_has_aggregate(predicate)
        }
        _ => false,
    }
}

/// Returns true if the function name is an aggregate.
fn is_aggregate_name(name: &str) -> bool {
    let lc = name.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "count" | "count_distinct"
            | "sum" | "sum_distinct"
            | "avg" | "avg_distinct"
            | "min" | "max"
            | "collect" | "collect_distinct"
            | "stdev" | "stdevp"
            | "percentilecont" | "percentiledisc"
    )
}

/// Evaluate an expression that may contain aggregates over a group of rows.
/// Non-aggregate sub-expressions are evaluated on `fallback_row`.
fn eval_with_agg(
    expr: &Expr,
    group_rows: &[Row],
    fallback_row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    if !expr_has_aggregate(expr) {
        // Use first group row if available (they all have the same key values).
        let row = group_rows.first().unwrap_or(fallback_row);
        return eval_expr(expr, row, params);
    }
    match expr {
        Expr::FunctionCall(name, args) if is_aggregate_name(name) => {
            eval_aggregate_call(name, args, group_rows, params)
        }
        Expr::FunctionCall(name, args) => {
            // Non-aggregate function: evaluate each arg (which may contain aggregates) then call function
            let vals: Vec<Value> = args.iter()
                .map(|a| eval_with_agg(a, group_rows, fallback_row, params))
                .collect::<Result<_, _>>()?;
            // Build a synthetic row that maps positional argument slots to values
            // and call eval_function directly.
            eval_function_with_vals(name, &vals)
        }
        Expr::Arith(l, op, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            eval_arith(&lv, op, &rv)
        }
        Expr::Compare(l, op, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            match compare_values(&lv, op, &rv) {
                None => Ok(Value::Null),
                Some(b) => Ok(Value::Bool(b)),
            }
        }
        Expr::And(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            match require_bool(&lv)? {
                Some(false) => Ok(Value::Bool(false)),
                Some(true) => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match require_bool(&rv)? { Some(b) => Value::Bool(b), None => Value::Null })
                }
                None => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match require_bool(&rv)? { Some(false) => Value::Bool(false), _ => Value::Null })
                }
            }
        }
        Expr::Or(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            match require_bool(&lv)? {
                Some(true) => Ok(Value::Bool(true)),
                Some(false) => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match require_bool(&rv)? { Some(b) => Value::Bool(b), None => Value::Null })
                }
                None => {
                    let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
                    Ok(match require_bool(&rv)? { Some(true) => Value::Bool(true), _ => Value::Null })
                }
            }
        }
        Expr::Xor(l, r) => {
            let lv = eval_with_agg(l, group_rows, fallback_row, params)?;
            let rv = eval_with_agg(r, group_rows, fallback_row, params)?;
            Ok(match (require_bool(&lv)?, require_bool(&rv)?) {
                (Some(a), Some(b)) => Value::Bool(a ^ b),
                _ => Value::Null,
            })
        }
        Expr::Not(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(match require_bool(&v)? { Some(b) => Value::Bool(!b), None => Value::Null })
        }
        Expr::IsNull(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        Expr::IsNotNull(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            Ok(Value::Bool(!matches!(v, Value::Null)))
        }
        Expr::Neg(e) => {
            let v = eval_with_agg(e, group_rows, fallback_row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Ok(Value::Null),
            }
        }
        Expr::CaseSearched { branches, else_ } => {
            for (cond, then) in branches {
                let cv = eval_with_agg(cond, group_rows, fallback_row, params)?;
                if matches!(truthy3(&cv), Some(true)) {
                    return eval_with_agg(then, group_rows, fallback_row, params);
                }
            }
            match else_ {
                Some(e) => eval_with_agg(e, group_rows, fallback_row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::MapLiteral(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                let val = eval_with_agg(v, group_rows, fallback_row, params)?;
                map.insert(k.clone(), val.to_json());
            }
            Ok(Value::Json(serde_json::Value::Object(map)))
        }
        Expr::List(items) => {
            let mut arr = Vec::new();
            for item in items {
                arr.push(eval_with_agg(item, group_rows, fallback_row, params)?.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(arr)))
        }
        Expr::ListComprehension { variable, list_expr, predicate, projection } => {
            let list_val = eval_with_agg(list_expr, group_rows, fallback_row, params)?;
            let items = match &list_val {
                Value::Json(serde_json::Value::Array(arr)) => arr.clone(),
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Null),
            };
            let base_row = group_rows.first().unwrap_or(fallback_row);
            let mut result = Vec::new();
            for item in &items {
                let item_val = json_to_value(item);
                let mut iter_row = base_row.clone();
                iter_row.insert(variable.clone(), item_val);
                if let Some(pred) = predicate {
                    let pv = eval_expr(pred, &iter_row, params).unwrap_or(Value::Null);
                    if !matches!(truthy3(&pv), Some(true)) {
                        continue;
                    }
                }
                let proj_val = if let Some(proj) = projection {
                    eval_expr(proj, &iter_row, params)?
                } else {
                    iter_row.get(variable.as_str()).cloned().unwrap_or(Value::Null)
                };
                result.push(proj_val.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        Expr::ListPredicate { kind, variable, list_expr, predicate } => {
            use crate::cypher::ast::ListPredicateKind;
            // Evaluate the list expression with aggregation (e.g., collect() inside ALL()).
            let list_val = eval_with_agg(list_expr, group_rows, fallback_row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Null),
                _ => return Ok(Value::Bool(false)),
            };
            let base_row = group_rows.first().unwrap_or(fallback_row);
            let mut true_count = 0usize;
            let mut null_count = 0usize;
            for item in &arr {
                let mut inner_row = base_row.clone();
                inner_row.insert(variable.clone(), json_to_value(item));
                let pv = eval_expr(predicate, &inner_row, params)?;
                match truthy3(&pv) {
                    Some(true) => true_count += 1,
                    None => null_count += 1,
                    Some(false) => {}
                }
            }
            let total = arr.len();
            let false_count = total - true_count - null_count;
            let has_null = null_count > 0;
            match kind {
                ListPredicateKind::Any => {
                    if true_count > 0 { Ok(Value::Bool(true)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(false)) }
                }
                ListPredicateKind::All => {
                    if false_count > 0 { Ok(Value::Bool(false)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(true)) }
                }
                ListPredicateKind::None_ => {
                    if true_count > 0 { Ok(Value::Bool(false)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(true)) }
                }
                ListPredicateKind::Single => {
                    if true_count > 1 { Ok(Value::Bool(false)) }
                    else if true_count == 1 && !has_null { Ok(Value::Bool(true)) }
                    else if true_count == 0 && !has_null { Ok(Value::Bool(false)) }
                    else { Ok(Value::Null) }
                }
            }
        }
        // For other compound expressions fall back to evaluating on first row
        other => {
            let row = group_rows.first().unwrap_or(fallback_row);
            eval_expr(other, row, params)
        }
    }
}

/// Call a non-aggregate function given already-evaluated argument values.
fn eval_function_with_vals(name: &str, vals: &[Value]) -> Result<Value, ExecError> {
    // Build a synthetic row mapping placeholder keys to the pre-evaluated values,
    // then call eval_function with Variable args pointing to those keys.
    let mut synthetic_row = Row::new();
    let mut synthetic_args: Vec<Expr> = Vec::with_capacity(vals.len());
    for (i, val) in vals.iter().enumerate() {
        let key = format!("__fn_arg_{i}");
        synthetic_row.insert(key.clone(), val.clone());
        synthetic_args.push(Expr::Variable(key));
    }
    let empty_params = HashMap::new();
    eval_function(name, &synthetic_args, &synthetic_row, &empty_params)
}

/// Evaluate an aggregate function call over a group of rows.
fn eval_aggregate_call(
    name: &str,
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    let lc = name.to_ascii_lowercase();
    let distinct = lc.ends_with("_distinct");
    let base = if distinct { &lc[..lc.len() - 9] } else { lc.as_str() };

    match base {
        "count" => {
            if args.len() == 1 && matches!(args[0], Expr::Star) {
                return Ok(Value::Int(group_rows.len() as i64));
            }
            if args.is_empty() {
                return Ok(Value::Int(group_rows.len() as i64));
            }
            let vals: Vec<Value> = group_rows.iter()
                .filter_map(|row| eval_expr(&args[0], row, params).ok())
                .filter(|v| !matches!(v, Value::Null))
                .collect();
            if distinct {
                let mut seen = std::collections::HashSet::new();
                let count = vals.iter()
                    .filter(|v| seen.insert(value_fingerprint(v)))
                    .count();
                Ok(Value::Int(count as i64))
            } else {
                Ok(Value::Int(vals.len() as i64))
            }
        }
        "sum" => {
            if args.is_empty() {
                return Err(ExecError { message: "sum() requires an argument".into() });
            }
            let mut sum_i = 0i64;
            let mut sum_f = 0.0f64;
            let mut is_float = false;
            for row in group_rows {
                match eval_expr(&args[0], row, params).unwrap_or(Value::Null) {
                    Value::Int(i) => sum_i += i,
                    Value::Float(f) => { sum_f += f; is_float = true; }
                    Value::Null => {}
                    _ => {}
                }
            }
            if is_float {
                Ok(Value::Float(sum_f + sum_i as f64))
            } else {
                Ok(Value::Int(sum_i))
            }
        }
        "avg" => {
            if args.is_empty() {
                return Err(ExecError { message: "avg() requires an argument".into() });
            }
            let mut sum = 0.0f64;
            let mut count = 0usize;
            for row in group_rows {
                match eval_expr(&args[0], row, params).unwrap_or(Value::Null) {
                    Value::Int(i) => { sum += i as f64; count += 1; }
                    Value::Float(f) => { sum += f; count += 1; }
                    Value::Null => {}
                    _ => {}
                }
            }
            if count == 0 {
                Ok(Value::Null)
            } else {
                Ok(Value::Float(sum / count as f64))
            }
        }
        "min" => {
            if args.is_empty() {
                return Err(ExecError { message: "min() requires an argument".into() });
            }
            let mut result: Option<Value> = None;
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if matches!(v, Value::Null) { continue; }
                result = Some(match result {
                    None => v,
                    Some(cur) => if value_ordering(&v, &cur) == std::cmp::Ordering::Less { v } else { cur },
                });
            }
            Ok(result.unwrap_or(Value::Null))
        }
        "max" => {
            if args.is_empty() {
                return Err(ExecError { message: "max() requires an argument".into() });
            }
            let mut result: Option<Value> = None;
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if matches!(v, Value::Null) { continue; }
                result = Some(match result {
                    None => v,
                    Some(cur) => if value_ordering(&v, &cur) == std::cmp::Ordering::Greater { v } else { cur },
                });
            }
            Ok(result.unwrap_or(Value::Null))
        }
        "collect" => {
            if args.is_empty() {
                return Err(ExecError { message: "collect() requires an argument".into() });
            }
            let mut vals: Vec<serde_json::Value> = Vec::new();
            for row in group_rows {
                let v = eval_expr(&args[0], row, params).unwrap_or(Value::Null);
                if !matches!(v, Value::Null) {
                    if distinct {
                        let j = v.to_json();
                        let fp = serde_json::to_string(&j).unwrap_or_default();
                        if !vals.iter().any(|x| serde_json::to_string(x).unwrap_or_default() == fp) {
                            vals.push(j);
                        }
                    } else {
                        vals.push(v.to_json());
                    }
                }
            }
            Ok(Value::Json(serde_json::Value::Array(vals)))
        }
        "stdev" => {
            // Sample standard deviation (Bessel's correction)
            eval_stdev(args, group_rows, params, true)
        }
        "stdevp" => {
            // Population standard deviation
            eval_stdev(args, group_rows, params, false)
        }
        "percentilecont" => {
            eval_percentile(args, group_rows, params, true)
        }
        "percentiledisc" => {
            eval_percentile(args, group_rows, params, false)
        }
        _ => Err(ExecError { message: format!("unknown aggregate: {name}") }),
    }
}

fn eval_stdev(
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
    sample: bool,
) -> Result<Value, ExecError> {
    if args.is_empty() {
        return Err(ExecError { message: "stdev() requires an argument".into() });
    }
    let vals: Vec<f64> = group_rows.iter()
        .filter_map(|row| eval_expr(&args[0], row, params).ok())
        .filter_map(|v| match v { Value::Int(i) => Some(i as f64), Value::Float(f) => Some(f), _ => None })
        .collect();
    let n = vals.len();
    if n == 0 || (sample && n == 1) { return Ok(Value::Float(0.0)); }
    let mean = vals.iter().sum::<f64>() / n as f64;
    let variance = vals.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
        / if sample { (n - 1) as f64 } else { n as f64 };
    Ok(Value::Float(variance.sqrt()))
}

fn eval_percentile(
    args: &[Expr],
    group_rows: &[Row],
    params: &HashMap<String, serde_json::Value>,
    interpolate: bool,
) -> Result<Value, ExecError> {
    if args.len() < 2 {
        return Err(ExecError { message: "percentile functions require 2 arguments".into() });
    }
    let pct_val = {
        // Percentile arg should be a constant or scalar expression — evaluate against
        // the first group row so scalar variables (group keys) are resolved.
        let ref_row = group_rows.first().map(|r| r as &Row);
        let empty = Row::new();
        eval_expr(&args[1], ref_row.unwrap_or(&empty), params).unwrap_or(Value::Null)
    };
    let pct = match pct_val {
        Value::Float(f) => f,
        Value::Int(i) => i as f64,
        _ => return Ok(Value::Null),
    };
    if !(0.0..=1.0).contains(&pct) {
        return Err(ExecError { message: "ArgumentError: NumberOutOfRange — percentile must be between 0.0 and 1.0".into() });
    }
    let mut vals: Vec<f64> = group_rows.iter()
        .filter_map(|row| eval_expr(&args[0], row, params).ok())
        .filter_map(|v| match v { Value::Int(i) => Some(i as f64), Value::Float(f) => Some(f), _ => None })
        .collect();
    if vals.is_empty() { return Ok(Value::Null); }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = vals.len();
    if interpolate {
        let idx = pct * (n - 1) as f64;
        let lo = idx.floor() as usize;
        let hi = (lo + 1).min(n - 1);
        let frac = idx - lo as f64;
        Ok(Value::Float(vals[lo] * (1.0 - frac) + vals[hi] * frac))
    } else {
        let idx = (pct * n as f64).ceil() as usize;
        let idx = idx.saturating_sub(1).min(n - 1);
        Ok(Value::Float(vals[idx]))
    }
}

/// Produce a stable string fingerprint for a Value (used in DISTINCT deduplication).
fn value_fingerprint(v: &Value) -> String {
    serde_json::to_string(&v.to_json()).unwrap_or_default()
}



/// Collect all Variable/Property leaf expressions that are NOT nested inside an
/// aggregate function call.  These are the "free variable references" that must
/// be covered by a grouping-key expression.
fn collect_free_var_refs(expr: &Expr, acc: &mut Vec<Expr>) {
    match expr {
        Expr::Variable(_) => acc.push(expr.clone()),
        Expr::Property(base, _) => {
            // Treat the whole `node.prop` as a single unit when the base is a Variable.
            if matches!(**base, Expr::Variable(_)) {
                acc.push(expr.clone());
            } else {
                collect_free_var_refs(base, acc);
            }
        }
        Expr::FunctionCall(name, args) => {
            if is_aggregate_name(name) {
                // Do NOT recurse into aggregate call arguments.
            } else {
                for a in args {
                    collect_free_var_refs(a, acc);
                }
            }
        }
        Expr::Arith(l, _, r) | Expr::Compare(l, _, r) | Expr::And(l, r) | Expr::Or(l, r)
        | Expr::Xor(l, r)
        | Expr::StartsWith(l, r) | Expr::EndsWith(l, r) | Expr::Contains(l, r)
        | Expr::Regex(l, r) | Expr::InList(l, r) => {
            collect_free_var_refs(l, acc);
            collect_free_var_refs(r, acc);
        }
        Expr::Not(e) | Expr::Neg(e) | Expr::IsNull(e) | Expr::IsNotNull(e)
        | Expr::HasLabel(e, _) => {
            collect_free_var_refs(e, acc);
        }
        Expr::Subscript(l, r) => { collect_free_var_refs(l, acc); collect_free_var_refs(r, acc); }
        Expr::List(es) => {
            for e in es { collect_free_var_refs(e, acc); }
        }
        Expr::MapLiteral(pairs) => {
            for (_, v) in pairs { collect_free_var_refs(v, acc); }
        }
        Expr::CaseSearched { branches, else_ } => {
            for (c, t) in branches { collect_free_var_refs(c, acc); collect_free_var_refs(t, acc); }
            if let Some(e) = else_ { collect_free_var_refs(e, acc); }
        }
        Expr::CaseSimple { test, branches, else_ } => {
            collect_free_var_refs(test, acc);
            for (w, t) in branches { collect_free_var_refs(w, acc); collect_free_var_refs(t, acc); }
            if let Some(e) = else_ { collect_free_var_refs(e, acc); }
        }
        // Literals, Param, Star, list comprehensions/predicates — no plain var refs
        _ => {}
    }
}

/// Structural equality for Variable and Property expressions (case-insensitive names).
/// Used to check whether a free variable reference is covered by a grouping key.
fn expr_structural_eq(a: &Expr, b: &Expr) -> bool {
    match (a, b) {
        (Expr::Variable(x), Expr::Variable(y)) => x.to_lowercase() == y.to_lowercase(),
        (Expr::Property(ba, fa), Expr::Property(bb, fb)) => {
            fa.to_lowercase() == fb.to_lowercase() && expr_structural_eq(ba, bb)
        }
        _ => false,
    }
}

/// Evaluate an expression against a row of bindings.
pub fn eval_expr(
    expr: &Expr,
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    match expr {
        Expr::Variable(name) => {
            if let Some(v) = row.get(name) {
                Ok(v.clone())
            } else if let Some(pv) = params.get(name) {
                // Fall back to params for correlated contexts (e.g. MERGE with outer bindings).
                Ok(json_to_value(pv))
            } else {
                Err(ExecError { message: format!("unbound variable: {name}") })
            }
        }
        Expr::Property(base_expr, key) => {
            let base = eval_expr(base_expr, row, params)?;
            match &base {
                Value::Node { .. } | Value::Edge { .. } | Value::Null
                | Value::Temporal(_) | Value::Duration(_)
                | Value::Json(serde_json::Value::Object(_)) => Ok(base.get_property(key)),
                // Strings that look like temporal ISO values (stored as strings in props):
                // try to parse as a temporal and access the component.
                Value::Str(s) => {
                    if let Some(tv) = try_parse_temporal_str(s) {
                        Ok(temporal_get_property(&tv, key))
                    } else {
                        Err(ExecError {
                            message: format!(
                                "TypeError: {} has no property `{}`",
                                value_type_name(&base), key
                            ),
                        })
                    }
                }
                _ => Err(ExecError {
                    message: format!(
                        "TypeError: {} has no property `{}`",
                        value_type_name(&base), key
                    ),
                }),
            }
        }
        Expr::IntLit(v) => Ok(Value::Int(*v)),
        Expr::FloatLit(v) => Ok(Value::Float(*v)),
        Expr::StringLit(s) => Ok(Value::Str(s.clone())),
        Expr::BoolLit(b) => Ok(Value::Bool(*b)),
        Expr::NullLit => Ok(Value::Null),
        Expr::Parameter(name) => {
            match params.get(name) {
                Some(v) => Ok(json_to_value(v)),
                None => Err(ExecError {
                    message: format!("missing parameter: ${name}"),
                }),
            }
        }
        Expr::Compare(left, op, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match compare_values(&l, op, &r) {
                None => Ok(Value::Null),
                Some(b) => Ok(Value::Bool(b)),
            }
        }
        Expr::And(left, right) => {
            // openCypher 3-valued logic with strict boolean type checking.
            // Both operands must be boolean-compatible (bool or null).
            // We evaluate BOTH sides to ensure type errors are always reported.
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            let lb = require_bool(&l)?;
            let rb = require_bool(&r)?;
            match (lb, rb) {
                (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
                (Some(true), Some(true)) => Ok(Value::Bool(true)),
                _ => Ok(Value::Null),
            }
        }
        Expr::Or(left, right) => {
            // openCypher 3-valued logic with strict boolean type checking.
            // Both operands must be boolean-compatible (bool or null).
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            let lb = require_bool(&l)?;
            let rb = require_bool(&r)?;
            match (lb, rb) {
                (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
                (Some(false), Some(false)) => Ok(Value::Bool(false)),
                _ => Ok(Value::Null),
            }
        }
        Expr::Not(inner) => {
            let v = eval_expr(inner, row, params)?;
            match require_bool(&v)? {
                Some(b) => Ok(Value::Bool(!b)),
                None => Ok(Value::Null),
            }
        }
        Expr::IsNull(inner) => {
            let v = eval_expr(inner, row, params)?;
            Ok(Value::Bool(matches!(v, Value::Null)))
        }
        Expr::IsNotNull(inner) => {
            let v = eval_expr(inner, row, params)?;
            Ok(Value::Bool(!matches!(v, Value::Null)))
        }
        Expr::HasLabel(inner, labels) => {
            let v = eval_expr(inner, row, params)?;
            match v {
                Value::Null => Ok(Value::Null),
                Value::Node { labels: node_labels, .. } => {
                    let has_all = labels.iter().all(|l| node_labels.iter().any(|nl| nl == l));
                    Ok(Value::Bool(has_all))
                }
                Value::Edge { rel_type, .. } => {
                    let has_all = labels.iter().all(|l| l == &rel_type);
                    Ok(Value::Bool(has_all))
                }
                _ => Ok(Value::Bool(false)),
            }
        }
        Expr::Arith(left, op, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            eval_arith(&l, op, &r)
        }
        Expr::Neg(inner) => {
            let v = eval_expr(inner, row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Int(-i)),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Ok(Value::Null),
            }
        }
        Expr::FunctionCall(name, args) => {
            eval_function(name, args, row, params)
        }
        Expr::Star => {
            // Star in projection: return all bound variables as a JSON object.
            let mut m = serde_json::Map::new();
            for (k, v) in row {
                m.insert(k.clone(), v.to_json());
            }
            Ok(Value::Json(serde_json::Value::Object(m)))
        }
        Expr::List(exprs) => {
            let vals: Vec<serde_json::Value> = exprs
                .iter()
                .map(|e| eval_expr(e, row, params).map(|v| v.to_json()))
                .collect::<Result<_, _>>()?;
            Ok(Value::Json(serde_json::Value::Array(vals)))
        }
        Expr::InList(left, right_list) => {
            let val = eval_expr(left, row, params)?;
            let list = eval_expr(right_list, row, params)?;
            let arr = match &list {
                Value::Json(serde_json::Value::Array(a)) => a.clone(),
                Value::Null => return Ok(Value::Null),
                // Per openCypher spec, IN on a non-list type raises SyntaxError.
                _ => return Err(ExecError {
                    message: "SyntaxError: InvalidArgumentType: \
                              IN operator requires a list on the right-hand side".into(),
                }),
            };
            // null IN [] => false; null IN [non-empty] => null
            if matches!(val, Value::Null) {
                return Ok(if arr.is_empty() { Value::Bool(false) } else { Value::Null });
            }
            let mut has_null = false;
            for item in &arr {
                let item_val = json_to_value(item);
                // Use ternary comparison: None means the comparison is uncertain (null-related).
                match compare_values(&val, &CmpOp::Eq, &item_val) {
                    Some(true) => return Ok(Value::Bool(true)),
                    None => has_null = true,
                    Some(false) => {}
                }
            }
            if has_null { Ok(Value::Null) } else { Ok(Value::Bool(false)) }
        }
        Expr::StartsWith(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(prefix)) => Ok(Value::Bool(s.starts_with(prefix.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::EndsWith(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(suffix)) => Ok(Value::Bool(s.ends_with(suffix.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::Contains(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(sub)) => Ok(Value::Bool(s.contains(sub.as_str()))),
                _ => Ok(Value::Null),
            }
        }
        Expr::Regex(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            match (&l, &r) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(pattern)) => {
                    // Use PostgreSQL's regexp_like via SPI for full POSIX regex support.
                    let result = Spi::get_one_with_args::<bool>(
                        "SELECT $1 ~ $2",
                        &[
                            pgrx::datum::DatumWithOid::from(s.as_str()),
                            pgrx::datum::DatumWithOid::from(pattern.as_str()),
                        ],
                    ).unwrap_or(Some(false)).unwrap_or(false);
                    Ok(Value::Bool(result))
                }
                _ => Ok(Value::Null),
            }
        }
        Expr::CaseSearched { branches, else_ } => {
            for (cond, then) in branches {
                let cv = eval_expr(cond, row, params)?;
                if matches!(truthy3(&cv), Some(true)) {
                    return eval_expr(then, row, params);
                }
            }
            match else_ {
                Some(e) => eval_expr(e, row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::CaseSimple { test, branches, else_ } => {
            let test_val = eval_expr(test, row, params)?;
            for (when, then) in branches {
                let wv = eval_expr(when, row, params)?;
                if values_equal(&test_val, &wv) {
                    return eval_expr(then, row, params);
                }
            }
            match else_ {
                Some(e) => eval_expr(e, row, params),
                None => Ok(Value::Null),
            }
        }
        Expr::ListComprehension { variable, list_expr, predicate, projection } => {
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
            };
            let mut result = Vec::new();
            for item in arr {
                let mut inner_row = row.clone();
                inner_row.insert(variable.clone(), json_to_value(&item));
                // Apply WHERE predicate if present.
                if let Some(pred) = predicate {
                    let pv = eval_expr(pred, &inner_row, params)?;
                    if !matches!(truthy3(&pv), Some(true)) {
                        continue;
                    }
                }
                // Apply projection if present, else return element.
                let out = if let Some(proj) = projection {
                    eval_expr(proj, &inner_row, params)?
                } else {
                    json_to_value(&item)
                };
                result.push(out.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        Expr::ListPredicate { kind, variable, list_expr, predicate } => {
            use crate::cypher::ast::ListPredicateKind;
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Null),
                _ => return Ok(Value::Bool(false)),
            };
            let mut true_count = 0usize;
            let mut null_count = 0usize;
            for item in &arr {
                let mut inner_row = row.clone();
                inner_row.insert(variable.clone(), json_to_value(item));
                let pv = eval_expr(predicate, &inner_row, params)?;
                match truthy3(&pv) {
                    Some(true) => true_count += 1,
                    None => null_count += 1,
                    Some(false) => {}
                }
            }
            let total = arr.len();
            let false_count = total - true_count - null_count;
            let has_null = null_count > 0;
            match kind {
                ListPredicateKind::Any => {
                    if true_count > 0 { Ok(Value::Bool(true)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(false)) }
                }
                ListPredicateKind::All => {
                    if false_count > 0 { Ok(Value::Bool(false)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(true)) }
                }
                ListPredicateKind::None_ => {
                    if true_count > 0 { Ok(Value::Bool(false)) }
                    else if has_null { Ok(Value::Null) }
                    else { Ok(Value::Bool(true)) }
                }
                ListPredicateKind::Single => {
                    if true_count > 1 { Ok(Value::Bool(false)) }
                    else if true_count == 1 && !has_null { Ok(Value::Bool(true)) }
                    else if true_count == 0 && !has_null { Ok(Value::Bool(false)) }
                    else { Ok(Value::Null) }
                }
            }
        }
        Expr::Xor(left, right) => {
            let l = eval_expr(left, row, params)?;
            let r = eval_expr(right, row, params)?;
            Ok(match (require_bool(&l)?, require_bool(&r)?) {
                (Some(a), Some(b)) => Value::Bool(a ^ b),
                _ => Value::Null,
            })
        }
        Expr::Subscript(list_expr, index_expr) => {
            let list_val = eval_expr(list_expr, row, params)?;
            let idx_val = eval_expr(index_expr, row, params)?;
            // Map subscript: map[stringKey]
            if let Value::Json(serde_json::Value::Object(m)) = &list_val {
                return match &idx_val {
                    Value::Null => Ok(Value::Null),
                    Value::Str(s) => Ok(m.get(s.as_str()).map(json_to_value).unwrap_or(Value::Null)),
                    _ => Err(ExecError {
                        message: "TypeError: map element access requires a string key".into(),
                    }),
                };
            }
            // Dynamic property access on nodes/edges: n['propname']
            if let Value::Str(key) = &idx_val {
                match &list_val {
                    Value::Null => return Ok(Value::Null),
                    Value::Node { properties, .. } => {
                        return Ok(properties.get(key.as_str()).map(json_to_value).unwrap_or(Value::Null));
                    }
                    Value::Edge { properties, .. } => {
                        return Ok(properties.get(key.as_str()).map(json_to_value).unwrap_or(Value::Null));
                    }
                    Value::Json(serde_json::Value::Array(_)) => {
                        // String index on a list → TypeError
                        return Err(ExecError {
                            message: "TypeError: InvalidArgumentType: \
                                      list element access requires an integer index".into(),
                        });
                    }
                    _ => {} // fall through to list handling
                }
            }
            let arr = match &list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Null),
                // Non-list, non-map: raise TypeError for non-null index.
                _ => {
                    if matches!(idx_val, Value::Null) {
                        return Ok(Value::Null);
                    }
                    return Err(ExecError {
                        message: "TypeError: InvalidArgumentType: \
                                  element access requires a list".into(),
                    });
                }
            };
            let idx = match idx_val {
                Value::Int(i) => i,
                Value::Float(f) => {
                    // Non-integer float → TypeError
                    if f.fract() != 0.0 {
                        return Err(ExecError {
                            message: "TypeError: InvalidArgumentType: \
                                      list index must be an integer, not a float".into(),
                        });
                    }
                    f as i64
                }
                Value::Null => return Ok(Value::Null),
                _ => return Err(ExecError {
                    message: "TypeError: InvalidArgumentType: \
                              list index must be an integer".into(),
                }),
            };
            let len = arr.len() as i64;
            let actual = if idx < 0 { len + idx } else { idx };
            if actual < 0 || actual >= len {
                Ok(Value::Null)
            } else {
                Ok(json_to_value(&arr[actual as usize]))
            }
        }
        Expr::ListSlice { list_expr, from, to } => {
            let list_val = eval_expr(list_expr, row, params)?;
            let arr = match list_val {
                Value::Json(serde_json::Value::Array(a)) => a,
                Value::Null => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
                _ => return Ok(Value::Json(serde_json::Value::Array(vec![]))),
            };
            let len = arr.len() as i64;
            let start = if let Some(f) = from {
                match eval_expr(f, row, params)? {
                    Value::Null => return Ok(Value::Null),
                    Value::Int(i) => if i < 0 { (len + i).max(0) } else { i.min(len) },
                    Value::Float(f) => { let i = f as i64; if i < 0 { (len + i).max(0) } else { i.min(len) } }
                    _ => 0,
                }
            } else { 0 };
            let end = if let Some(t) = to {
                match eval_expr(t, row, params)? {
                    Value::Null => return Ok(Value::Null),
                    Value::Int(i) => if i < 0 { (len + i).max(0) } else { i.min(len) },
                    Value::Float(f) => { let i = f as i64; if i < 0 { (len + i).max(0) } else { i.min(len) } }
                    _ => len,
                }
            } else { len };
            let sliced: Vec<serde_json::Value> = arr
                .into_iter()
                .skip(start as usize)
                .take((end - start).max(0) as usize)
                .collect();
            Ok(Value::Json(serde_json::Value::Array(sliced)))
        }
        Expr::ShortestPath { all, pattern } => {
            // Execute the pattern as a full plan and return path(s).
            use crate::cypher::planner;
            // Build a mini-query: MATCH (pattern) RETURN *
            // We can call plan_pattern_onto directly with an empty bound set.
            match planner::plan_pattern_for_shortest_path(pattern, *all, row, params) {
                Ok(path_or_null) => Ok(path_or_null),
                Err(e) => Err(ExecError { message: e.message }),
            }
        }
        Expr::PatternComprehension { path_variable, pattern, predicate, projection } => {
            // Execute the pattern as an inline plan, then project each row.
            use crate::cypher::planner;
            let rows_result = planner::exec_pattern_inline(pattern, row, params);
            let (inner_rows, element_vars) = match rows_result {
                Ok(r) => r,
                Err(e) => return Err(ExecError { message: e.message }),
            };
            let mut result_arr = Vec::new();
            for mut inner_row in inner_rows {
                // Merge outer row bindings (inner vars take precedence).
                for (k, v) in row {
                    inner_row.entry(k.clone()).or_insert_with(|| v.clone());
                }
                // If there is a path variable, build the path from the pattern's element vars
                // (using the element_vars list returned by exec_pattern_inline which matches
                // plan_pattern_onto naming conventions exactly).
                // For var-length patterns, if NamedPath already built the path correctly,
                // use that. Otherwise build from element_vars (fixed-hop case).
                if let Some(pv_name) = path_variable {
                    if !inner_row.contains_key(pv_name.as_str()) || element_vars.is_empty() {
                        let path_val = build_path_from_elem_vars(&element_vars, &inner_row);
                        inner_row.insert(pv_name.clone(), path_val);
                    }
                    // If already set (NamedPath did it), verify it's a valid path (not corrupted by
                    // var-length relay). The NamedPath path is preferred when element_vars contains
                    // a var-length sub-path carry var (Value::Path in a rel slot).
                    // Check: if any rel-slot element_var holds a Value::Path, the NamedPath result
                    // is correct; if not (all are Value::Edge), rebuild for accuracy.
                    let has_subpath = element_vars.iter().enumerate().any(|(i, v)| {
                        i % 2 == 1 && matches!(inner_row.get(v), Some(Value::Path { .. }))
                    });
                    if !has_subpath && !element_vars.is_empty() {
                        let path_val = build_path_from_elem_vars(&element_vars, &inner_row);
                        inner_row.insert(pv_name.clone(), path_val);
                    }
                }
                // Apply WHERE predicate if present.
                if let Some(pred) = predicate {
                    let pv = eval_expr(pred, &inner_row, params)?;
                    if !matches!(truthy3(&pv), Some(true)) {
                        continue;
                    }
                }
                let out = eval_expr(projection, &inner_row, params)?;
                result_arr.push(out.to_json());
            }
            Ok(Value::Json(serde_json::Value::Array(result_arr)))
        }
        Expr::Exists { subquery, .. } => {
            // Plan the subquery with outer-scope variables known as bound.
            // This ensures `MATCH (n)-->()` (where n is outer) uses n as a
            // bound variable rather than scanning all nodes.
            use crate::cypher::planner;
            // Include both row keys and params keys: params may contain outer row
            // bindings propagated from enclosing EXISTS subqueries (e.g. nested EXISTS
            // where the middle row doesn't carry outermost variables in `row`).
            let mut outer_vars: std::collections::HashSet<String> = row.keys().cloned().collect();
            outer_vars.extend(params.keys().cloned());
            let plan = planner::plan_with_outer(subquery, &outer_vars)
                .map_err(|e| ExecError { message: e.message })?;
            // Inject outer row vars into params so the inner plan can look them up.
            let mut inner_params = params.clone();
            for (k, v) in row {
                inner_params.insert(k.clone(), v.to_json());
            }
            let inner_rows = execute(&plan, &inner_params)?;
            // EXISTS is true if any inner row is compatible with outer bindings.
            // A row is compatible if all outer-scope variables present in the inner
            // row match the outer values.
            let found = inner_rows.into_iter().any(|inner_row| {
                row.iter().all(|(k, outer_val)| {
                    if let Some(inner_val) = inner_row.get(k) {
                        values_equal(outer_val, inner_val)
                    } else {
                        true // outer var not in inner scope; ignore
                    }
                })
            });
            Ok(Value::Bool(found))
        }
        Expr::MapLiteral(pairs) => {
            let mut m = serde_json::Map::new();
            for (k, v_expr) in pairs {
                let v = eval_expr(v_expr, row, params)?;
                m.insert(k.clone(), v.to_json());
            }
            Ok(Value::Json(serde_json::Value::Object(m)))
        }
    }
}

impl Value {
    fn is_truthy(&self) -> bool {
        match self {
            Value::Bool(b) => *b,
            Value::Null => false,
            Value::Int(0) => false,
            Value::Str(s) => !s.is_empty(),
            _ => true,
        }
    }
}

/// Three-valued logic for openCypher: returns None for NULL.
fn truthy3(v: &Value) -> Option<bool> {
    match v {
        Value::Null => None,
        other => Some(other.is_truthy()),
    }
}

/// Strict three-valued logic for openCypher boolean operators (AND/OR/NOT/XOR).
/// Returns Err(TypeError) if the value is not a Bool or Null.
fn require_bool(v: &Value) -> Result<Option<bool>, ExecError> {
    match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => Err(ExecError {
            message: format!("TypeError: expected Boolean but got {}", value_type_name(other)),
        }),
    }
}

/// Returns the openCypher type name of a value for error messages.
fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Bool(_) => "Boolean",
        Value::Int(_) => "Integer",
        Value::Float(_) => "Float",
        Value::Str(_) => "String",
        Value::Null => "Null",
        Value::Node { .. } => "Node",
        Value::Edge { .. } => "Relationship",
        Value::Path { .. } => "Path",
        Value::Json(serde_json::Value::Array(_)) => "List",
        Value::Json(serde_json::Value::Object(_)) => "Map",
        Value::Json(_) => "Any",
        Value::Temporal(_) => "DateTime",
        Value::Duration(_) => "Duration",
    }
}

fn compare_values(left: &Value, op: &CmpOp, right: &Value) -> Option<bool> {
    // Null comparisons always return null.
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return None;
    }
    // NaN comparisons: NaN = anything → false, NaN <> anything → true (openCypher spec).
    let left_is_nan = matches!(left, Value::Float(f) if f.is_nan());
    let right_is_nan = matches!(right, Value::Float(f) if f.is_nan());
    if left_is_nan || right_is_nan {
        return Some(matches!(op, CmpOp::Neq));
    }
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Some(int_cmp(*a, op, *b)),
        (Value::Float(a), Value::Float(b)) => Some(float_cmp(*a, op, *b)),
        (Value::Int(a), Value::Float(b)) => Some(float_cmp(*a as f64, op, *b)),
        (Value::Float(a), Value::Int(b)) => Some(float_cmp(*a, op, *b as f64)),
        (Value::Str(a), Value::Str(b)) => Some(str_cmp(a, op, b)),
        // Boolean ordering: false < true (treated as 0/1)
        (Value::Bool(a), Value::Bool(b)) => {
            let av = if *a { 1i32 } else { 0 };
            let bv = if *b { 1i32 } else { 0 };
            Some(match op {
                CmpOp::Eq => av == bv,
                CmpOp::Neq => av != bv,
                CmpOp::Lt => av < bv,
                CmpOp::Gt => av > bv,
                CmpOp::Le => av <= bv,
                CmpOp::Ge => av >= bv,
            })
        }
        (Value::Node { node_id: a, .. }, Value::Node { node_id: b, .. }) => Some(int_cmp(*a, op, *b)),
        // Edge (relationship) equality: two edges are equal iff they have the same edge_id.
        (Value::Edge { edge_id: a, .. }, Value::Edge { edge_id: b, .. }) => Some(int_cmp(*a, op, *b)),
        // Path equality: two paths are equal iff they have the same node sequence and edge sequence.
        (Value::Path { nodes: an, rels: ar }, Value::Path { nodes: bn, rels: br }) => {
            match op {
                CmpOp::Eq | CmpOp::Neq => {
                    if an.len() != bn.len() || ar.len() != br.len() {
                        return Some(matches!(op, CmpOp::Neq));
                    }
                    for (a, b) in an.iter().zip(bn.iter()) {
                        if !values_equal(a, b) { return Some(matches!(op, CmpOp::Neq)); }
                    }
                    for (a, b) in ar.iter().zip(br.iter()) {
                        if !values_equal(a, b) { return Some(matches!(op, CmpOp::Neq)); }
                    }
                    Some(matches!(op, CmpOp::Eq))
                }
                _ => None,
            }
        }
        // Temporal comparison: compare by canonical ISO string (lexicographic ≈ chronological for same-kind)
        (Value::Temporal(a), Value::Temporal(b)) => Some(str_cmp(&a.iso, op, &b.iso)),
        (Value::Temporal(a), Value::Str(b)) => Some(str_cmp(&a.iso, op, b)),
        (Value::Str(a), Value::Temporal(b)) => Some(str_cmp(a, op, &b.iso)),
        (Value::Duration(a), Value::Duration(b)) => {
            // Compare total seconds
            let sa = a.total_seconds();
            let sb = b.total_seconds();
            Some(int_cmp(sa, op, sb))
        }
        (Value::Duration(a), Value::Str(b)) => Some(str_cmp(&a.iso, op, b)),
        (Value::Str(a), Value::Duration(b)) => Some(str_cmp(a, op, &b.iso)),
        // List comparison (lexicographic)
        (Value::Json(serde_json::Value::Array(a)), Value::Json(serde_json::Value::Array(b))) => {
            compare_lists(a, b, op)
        }
        // Map equality: two maps are equal iff they have the same keys and equal values.
        (Value::Json(serde_json::Value::Object(a)), Value::Json(serde_json::Value::Object(b))) => {
            match op {
                CmpOp::Eq | CmpOp::Neq => {
                    if a.len() != b.len() {
                        return Some(matches!(op, CmpOp::Neq));
                    }
                    let mut has_null = false;
                    for (k, av) in a.iter() {
                        match b.get(k) {
                            None => return Some(matches!(op, CmpOp::Neq)),
                            Some(bv) => {
                                match compare_values(&json_to_value(av), &CmpOp::Eq, &json_to_value(bv)) {
                                    None => { has_null = true; }
                                    Some(false) => return Some(matches!(op, CmpOp::Neq)),
                                    Some(true) => {}
                                }
                            }
                        }
                    }
                    if has_null { None } else { Some(matches!(op, CmpOp::Eq)) }
                }
                _ => None, // maps have no ordering
            }
        }
        // Type mismatch: = and <> are defined (different types are not equal),
        // ordering operators return null.
        _ => match op {
            CmpOp::Eq => Some(false),
            CmpOp::Neq => Some(true),
            _ => None,
        },
    }
}

/// Lexicographic list comparison returning Option<bool> (None = null).
fn compare_lists(a: &[serde_json::Value], b: &[serde_json::Value], op: &CmpOp) -> Option<bool> {
    match op {
        CmpOp::Eq | CmpOp::Neq => {
            // Different lengths → definitively not equal
            if a.len() != b.len() {
                return Some(matches!(op, CmpOp::Neq));
            }
            // Same length: compare element by element recursively
            let mut has_null = false;
            for (ai, bi) in a.iter().zip(b.iter()) {
                let av = json_to_value(ai);
                let bv = json_to_value(bi);
                match compare_values(&av, &CmpOp::Eq, &bv) {
                    None => { has_null = true; }
                    Some(false) => return Some(matches!(op, CmpOp::Neq)), // definitely ≠
                    Some(true) => {}  // equal, continue
                }
            }
            if has_null { None } else { Some(matches!(op, CmpOp::Eq)) }
        }
        _ => {
            // Ordering comparison: lexicographic using elem_ordering
            let min_len = a.len().min(b.len());
            let mut has_null = false;
            for i in 0..min_len {
                let av = json_to_value(&a[i]);
                let bv = json_to_value(&b[i]);
                match elem_ordering(&av, &bv) {
                    None => { has_null = true; }
                    Some(std::cmp::Ordering::Equal) => {}
                    Some(std::cmp::Ordering::Less) => {
                        return Some(matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Neq));
                    }
                    Some(std::cmp::Ordering::Greater) => {
                        return Some(matches!(op, CmpOp::Gt | CmpOp::Ge | CmpOp::Neq));
                    }
                }
            }
            // All compared elements equal (or null-indeterminate)
            match a.len().cmp(&b.len()) {
                std::cmp::Ordering::Greater => Some(matches!(op, CmpOp::Gt | CmpOp::Ge | CmpOp::Neq)),
                std::cmp::Ordering::Less    => Some(matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Neq)),
                std::cmp::Ordering::Equal   => if has_null { None } else {
                    Some(matches!(op, CmpOp::Eq | CmpOp::Le | CmpOp::Ge))
                },
            }
        }
    }
}

/// Element-level ordering for list comparison. None = null/incomparable.
fn elem_ordering(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).partial_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.partial_cmp(&(*y as f64)),
        (Value::Str(x), Value::Str(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => {
            Some((if *x { 1i32 } else { 0 }).cmp(&(if *y { 1i32 } else { 0 })))
        }
        _ => None,
    }
}

fn int_cmp(a: i64, op: &CmpOp, b: i64) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn float_cmp(a: f64, op: &CmpOp, b: f64) -> bool {
    // NaN comparisons: NaN == anything → false, NaN != anything → true, NaN ord anything → false
    if a.is_nan() || b.is_nan() {
        return matches!(op, CmpOp::Neq);
    }
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn str_cmp(a: &str, op: &CmpOp, b: &str) -> bool {
    match op {
        CmpOp::Eq => a == b,
        CmpOp::Neq => a != b,
        CmpOp::Lt => a < b,
        CmpOp::Gt => a > b,
        CmpOp::Le => a <= b,
        CmpOp::Ge => a >= b,
    }
}

fn eval_arith(left: &Value, op: &ArithOp, right: &Value) -> Result<Value, ExecError> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => match op {
            ArithOp::Add => Ok(Value::Int(a + b)),
            ArithOp::Sub => Ok(Value::Int(a - b)),
            ArithOp::Mul => Ok(Value::Int(a * b)),
            ArithOp::Div => {
                if *b == 0 { Ok(Value::Null) } else { Ok(Value::Int(a / b)) }
            }
            ArithOp::Mod => {
                if *b == 0 { Ok(Value::Null) } else { Ok(Value::Int(a % b)) }
            }
            ArithOp::Pow => Ok(Value::Float((*a as f64).powf(*b as f64))),
        },
        (Value::Float(a), Value::Float(b)) => float_arith(*a, op, *b),
        (Value::Int(a), Value::Float(b)) => float_arith(*a as f64, op, *b),
        (Value::Float(a), Value::Int(b)) => float_arith(*a, op, *b as f64),
        (Value::Str(a), Value::Str(b)) if matches!(op, ArithOp::Add) => {
            Ok(Value::Str(format!("{a}{b}")))
        }
        // List concatenation: list + list
        (Value::Json(serde_json::Value::Array(a)), Value::Json(serde_json::Value::Array(b)))
            if matches!(op, ArithOp::Add) => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // List append: list + scalar
        (Value::Json(serde_json::Value::Array(a)), _)
            if matches!(op, ArithOp::Add) => {
            let mut result = a.clone();
            result.push(right.to_json());
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // List prepend: scalar + list
        (_, Value::Json(serde_json::Value::Array(b)))
            if matches!(op, ArithOp::Add) => {
            let mut result = vec![left.to_json()];
            result.extend(b.iter().cloned());
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        _ => Ok(Value::Null),
    }
}

fn float_arith(a: f64, op: &ArithOp, b: f64) -> Result<Value, ExecError> {
    match op {
        ArithOp::Add => Ok(Value::Float(a + b)),
        ArithOp::Sub => Ok(Value::Float(a - b)),
        ArithOp::Mul => Ok(Value::Float(a * b)),
        ArithOp::Div => {
            if b == 0.0 {
                if a == 0.0 { Ok(Value::Float(f64::NAN)) } // 0.0/0.0 → NaN
                else { Ok(Value::Null) }                    // x/0.0 → null
            } else {
                Ok(Value::Float(a / b))
            }
        }
        ArithOp::Mod => {
            if b == 0.0 { Ok(Value::Null) } else { Ok(Value::Float(a % b)) }
        }
        ArithOp::Pow => Ok(Value::Float(a.powf(b))),
    }
}

fn eval_function(
    name: &str,
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Value, ExecError> {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        "id" => {
            if args.len() != 1 {
                return Err(ExecError { message: "id() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { node_id, .. } => Ok(Value::Int(node_id)),
                Value::Edge { edge_id, .. } => Ok(Value::Int(edge_id)),
                _ => Ok(Value::Null),
            }
        }
        "labels" => {
            if args.len() != 1 {
                return Err(ExecError { message: "labels() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { labels, .. } => {
                    let arr: Vec<serde_json::Value> = labels.into_iter().map(|l| l.into()).collect();
                    Ok(Value::Json(serde_json::Value::Array(arr)))
                }
                _ => Ok(Value::Null),
            }
        }
        "type" => {
            if args.len() != 1 {
                return Err(ExecError { message: "type() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Edge { rel_type, .. } => Ok(Value::Str(rel_type)),
                _ => Ok(Value::Null),
            }
        }
        "properties" => {
            if args.len() != 1 {
                return Err(ExecError { message: "properties() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { properties, .. } | Value::Edge { properties, .. } => {
                    Ok(Value::Json(serde_json::Value::Object(properties)))
                }
                Value::Json(serde_json::Value::Object(m)) => {
                    Ok(Value::Json(serde_json::Value::Object(m)))
                }
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError {
                    message: "TypeError: properties() requires a node, relationship, or map".into(),
                }),
            }
        }
        "keys" => {
            if args.len() != 1 {
                return Err(ExecError { message: "keys() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Node { properties, .. } | Value::Edge { properties, .. } => {
                    // Exclude null-valued properties per openCypher spec.
                    let keys: Vec<serde_json::Value> = properties.iter()
                        .filter(|(_, v)| !v.is_null())
                        .map(|(k, _)| k.clone().into())
                        .collect();
                    Ok(Value::Json(serde_json::Value::Array(keys)))
                }
                Value::Json(serde_json::Value::Object(obj)) => {
                    let keys: Vec<serde_json::Value> = obj.keys().map(|k| k.clone().into()).collect();
                    Ok(Value::Json(serde_json::Value::Array(keys)))
                }
                _ => Ok(Value::Null),
            }
        }
        "tostring" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toString() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Str(i.to_string())),
                Value::Float(f) => Ok(Value::Str(f.to_string())),
                Value::Str(s) => Ok(Value::Str(s)),
                Value::Bool(b) => Ok(Value::Str(b.to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tointeger" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toInteger() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Int(i)),
                Value::Float(f) => Ok(Value::Int(f as i64)),
                Value::Str(s) => {
                    // Try integer parse first, then float-truncation.
                    if let Ok(i) = s.trim().parse::<i64>() {
                        Ok(Value::Int(i))
                    } else if let Ok(f) = s.trim().parse::<f64>() {
                        Ok(Value::Int(f as i64))
                    } else {
                        Ok(Value::Null)
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tofloat" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toFloat() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Int(i) => Ok(Value::Float(i as f64)),
                Value::Float(f) => Ok(Value::Float(f)),
                Value::Str(s) => match s.parse::<f64>() {
                    Ok(f) => Ok(Value::Float(f)),
                    Err(_) => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "coalesce" => {
            for arg in args {
                let val = eval_expr(arg, row, params)?;
                if !matches!(val, Value::Null) {
                    return Ok(val);
                }
            }
            Ok(Value::Null)
        }
        // --- Type conversion ---
        "toboolean" => {
            if args.len() != 1 {
                return Err(ExecError { message: "toBoolean() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Bool(b) => Ok(Value::Bool(b)),
                Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        // --- Size / length ---
        "size" => {
            if args.len() != 1 {
                return Err(ExecError { message: "size() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Json(serde_json::Value::Array(a)) => Ok(Value::Int(a.len() as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "length" => {
            if args.len() != 1 {
                return Err(ExecError { message: "length() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Int(s.chars().count() as i64)),
                Value::Json(serde_json::Value::Array(a)) => Ok(Value::Int(a.len() as i64)),
                Value::Path { rels, .. } => Ok(Value::Int(rels.len() as i64)),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "nodes" => {
            if args.len() != 1 {
                return Err(ExecError { message: "nodes() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Path { nodes, .. } => {
                    let arr: Vec<serde_json::Value> = nodes.iter().map(Value::to_json).collect();
                    Ok(Value::Json(serde_json::Value::Array(arr)))
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "relationships" | "rels" => {
            if args.len() != 1 {
                return Err(ExecError { message: "relationships() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Path { rels, .. } => {
                    let arr: Vec<serde_json::Value> = rels.iter().map(Value::to_json).collect();
                    Ok(Value::Json(serde_json::Value::Array(arr)))
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        // --- List functions ---
        "head" => {
            if args.len() != 1 {
                return Err(ExecError { message: "head() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(mut a)) => {
                    if a.is_empty() { Ok(Value::Null) } else { Ok(json_to_value(&a.remove(0))) }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "tail" => {
            if args.len() != 1 {
                return Err(ExecError { message: "tail() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(a)) => {
                    if a.is_empty() {
                        Ok(Value::Json(serde_json::Value::Array(vec![])))
                    } else {
                        Ok(Value::Json(serde_json::Value::Array(a[1..].to_vec())))
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "last" => {
            if args.len() != 1 {
                return Err(ExecError { message: "last() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Json(serde_json::Value::Array(a)) => {
                    match a.last() {
                        Some(v) => Ok(json_to_value(v)),
                        None => Ok(Value::Null),
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "reverse" => {
            if args.len() != 1 {
                return Err(ExecError { message: "reverse() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
                Value::Json(serde_json::Value::Array(mut a)) => {
                    a.reverse();
                    Ok(Value::Json(serde_json::Value::Array(a)))
                }
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "range" => {
            let (start, end, step) = match args.len() {
                2 => {
                    let s = eval_expr(&args[0], row, params)?;
                    let e = eval_expr(&args[1], row, params)?;
                    (s, e, Value::Int(1))
                }
                3 => {
                    let s = eval_expr(&args[0], row, params)?;
                    let e = eval_expr(&args[1], row, params)?;
                    let st = eval_expr(&args[2], row, params)?;
                    (s, e, st)
                }
                _ => return Err(ExecError { message: "range() takes 2 or 3 arguments".into() }),
            };
            let (s, e, st) = match (&start, &end, &step) {
                (Value::Int(s), Value::Int(e), Value::Int(st)) => (*s, *e, *st),
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => return Ok(Value::Null),
                _ => return Err(ExecError {
                    message: format!(
                        "TypeError: expected Integer arguments for range(), got ({}, {}, {})",
                        value_type_name(&start), value_type_name(&end), value_type_name(&step)
                    ),
                }),
            };
            if st == 0 {
                return Err(ExecError { message: "range() step cannot be 0".into() });
            }
            let mut result = Vec::new();
            let mut cur = s;
            while (st > 0 && cur <= e) || (st < 0 && cur >= e) {
                result.push(serde_json::Value::Number(cur.into()));
                cur += st;
            }
            Ok(Value::Json(serde_json::Value::Array(result)))
        }
        // --- String functions ---
        "trim" => {
            if args.len() != 1 { return Err(ExecError { message: "trim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "ltrim" => {
            if args.len() != 1 { return Err(ExecError { message: "ltrim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim_start().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "rtrim" => {
            if args.len() != 1 { return Err(ExecError { message: "rtrim() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.trim_end().to_string())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "upper" | "toupper" => {
            if args.len() != 1 { return Err(ExecError { message: "upper() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_uppercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "lower" | "tolower" => {
            if args.len() != 1 { return Err(ExecError { message: "lower() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_lowercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "substring" => {
            if args.len() < 2 { return Err(ExecError { message: "substring() takes 2 or 3 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let start = eval_expr(&args[1], row, params)?;
            let len_val = if args.len() >= 3 { Some(eval_expr(&args[2], row, params)?) } else { None };
            match (s, start) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(start)) => {
                    let chars: Vec<char> = s.chars().collect();
                    let start = start.max(0) as usize;
                    let start = start.min(chars.len());
                    let result: String = match len_val {
                        Some(Value::Int(len)) => chars[start..].iter().take(len.max(0) as usize).collect(),
                        Some(Value::Null) => return Ok(Value::Null),
                        None => chars[start..].iter().collect(),
                        _ => chars[start..].iter().collect(),
                    };
                    Ok(Value::Str(result))
                }
                _ => Ok(Value::Null),
            }
        }
        "replace" => {
            if args.len() != 3 { return Err(ExecError { message: "replace() takes exactly 3 arguments".into() }); }
            let original = eval_expr(&args[0], row, params)?;
            let search = eval_expr(&args[1], row, params)?;
            let replacement = eval_expr(&args[2], row, params)?;
            match (original, search, replacement) {
                (Value::Null, _, _) | (_, Value::Null, _) | (_, _, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(from), Value::Str(to)) => {
                    Ok(Value::Str(s.replace(&from as &str, &to as &str)))
                }
                _ => Ok(Value::Null),
            }
        }
        "split" => {
            if args.len() != 2 { return Err(ExecError { message: "split() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let delim = eval_expr(&args[1], row, params)?;
            match (s, delim) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Str(d)) => {
                    let parts: Vec<serde_json::Value> = s.split(&d as &str)
                        .map(|p| serde_json::Value::String(p.to_string()))
                        .collect();
                    Ok(Value::Json(serde_json::Value::Array(parts)))
                }
                _ => Ok(Value::Null),
            }
        }
        // --- Math functions ---
        "abs" => math1(args, row, params, |x| x.abs(), |x: f64| x.abs()),
        "ceil" | "ceiling" => math1(args, row, params, |x| x, |x: f64| x.ceil()),
        "floor" => math1(args, row, params, |x| x, |x: f64| x.floor()),
        "round" => math1(args, row, params, |x| x, |x: f64| x.round()),
        "sign" => math1(args, row, params, |x: i64| x.signum(), |x: f64| x.signum()),
        "sqrt" => {
            if args.len() != 1 { return Err(ExecError { message: "sqrt() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Int(i) => Ok(Value::Float((i as f64).sqrt())),
                Value::Float(f) => Ok(Value::Float(f.sqrt())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "log" => math1f(args, row, params, |x: f64| x.ln()),
        "log10" => math1f(args, row, params, |x: f64| x.log10()),
        "exp" => math1f(args, row, params, |x: f64| x.exp()),
        "sin" => math1f(args, row, params, |x: f64| x.sin()),
        "cos" => math1f(args, row, params, |x: f64| x.cos()),
        "tan" => math1f(args, row, params, |x: f64| x.tan()),
        "asin" => math1f(args, row, params, |x: f64| x.asin()),
        "acos" => math1f(args, row, params, |x: f64| x.acos()),
        "atan" => math1f(args, row, params, |x: f64| x.atan()),
        "atan2" => {
            if args.len() != 2 { return Err(ExecError { message: "atan2() takes exactly 2 arguments".into() }); }
            let y = to_f64(&eval_expr(&args[0], row, params)?);
            let x = to_f64(&eval_expr(&args[1], row, params)?);
            match (y, x) {
                (Some(y), Some(x)) => Ok(Value::Float(y.atan2(x))),
                _ => Ok(Value::Null),
            }
        }
        "pi" => {
            if !args.is_empty() { return Err(ExecError { message: "pi() takes no arguments".into() }); }
            Ok(Value::Float(std::f64::consts::PI))
        }
        "e" => {
            if !args.is_empty() { return Err(ExecError { message: "e() takes no arguments".into() }); }
            Ok(Value::Float(std::f64::consts::E))
        }
        // --- Remaining string functions ---
        "left" => {
            if args.len() != 2 { return Err(ExecError { message: "left() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let n = eval_expr(&args[1], row, params)?;
            match (s, n) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(n)) => {
                    let n = n.max(0) as usize;
                    Ok(Value::Str(s.chars().take(n).collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        "right" => {
            if args.len() != 2 { return Err(ExecError { message: "right() takes exactly 2 arguments".into() }); }
            let s = eval_expr(&args[0], row, params)?;
            let n = eval_expr(&args[1], row, params)?;
            match (s, n) {
                (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
                (Value::Str(s), Value::Int(n)) => {
                    let n = n.max(0) as usize;
                    let chars: Vec<char> = s.chars().collect();
                    let start = chars.len().saturating_sub(n);
                    Ok(Value::Str(chars[start..].iter().collect()))
                }
                _ => Ok(Value::Null),
            }
        }
        // --- Remaining math functions ---
        "toradians" => math1f(args, row, params, |x| x.to_radians()),
        "todegrees" => math1f(args, row, params, |x| x.to_degrees()),
        "rand" => {
            if !args.is_empty() { return Err(ExecError { message: "rand() takes no arguments".into() }); }
            let f = Spi::get_one::<f64>("SELECT random()").unwrap_or(Some(0.0)).unwrap_or(0.0);
            Ok(Value::Float(f))
        }
        "randomuuid" => {
            if !args.is_empty() { return Err(ExecError { message: "randomUUID() takes no arguments".into() }); }
            let s = Spi::get_one::<String>("SELECT gen_random_uuid()::text")
                .unwrap_or(Some(String::new())).unwrap_or_default();
            Ok(Value::Str(s))
        }
        // --- Temporal constructors ---
        "date" => {
            if args.is_empty() {
                // date() with no args → today
                let s = Spi::get_one::<String>("SELECT current_date::text")
                    .unwrap_or(Some("1970-01-01".into())).unwrap_or_default();
                let tv = temporal_date(&Value::Str(s))?;
                return Ok(Value::Temporal(tv));
            }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Temporal(temporal_date(&arg)?))
        }
        "localtime" => {
            if args.is_empty() {
                let s = Spi::get_one::<String>("SELECT localtime::text")
                    .unwrap_or(Some("00:00:00".into())).unwrap_or_default();
                let tv = temporal_localtime(&Value::Str(s))?;
                return Ok(Value::Temporal(tv));
            }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Temporal(temporal_localtime(&arg)?))
        }
        "time" => {
            if args.is_empty() {
                let s = Spi::get_one::<String>("SELECT current_time::text")
                    .unwrap_or(Some("00:00:00+00:00".into())).unwrap_or_default();
                let tv = temporal_time(&Value::Str(s))?;
                return Ok(Value::Temporal(tv));
            }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Temporal(temporal_time(&arg)?))
        }
        "localdatetime" => {
            if args.is_empty() {
                let s = Spi::get_one::<String>("SELECT localtimestamp::text")
                    .unwrap_or(Some("1970-01-01T00:00:00".into())).unwrap_or_default();
                // PG uses space as separator
                let s = s.replace(' ', "T");
                let tv = temporal_localdatetime(&Value::Str(s))?;
                return Ok(Value::Temporal(tv));
            }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Temporal(temporal_localdatetime(&arg)?))
        }
        "datetime" => {
            if args.is_empty() {
                let s = Spi::get_one::<String>("SELECT now()::text")
                    .unwrap_or(Some("1970-01-01T00:00:00+00:00".into())).unwrap_or_default();
                let s = s.replace(' ', "T");
                let tv = temporal_datetime(&Value::Str(s))?;
                return Ok(Value::Temporal(tv));
            }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Temporal(temporal_datetime(&arg)?))
        }
        "duration" => {
            if args.len() != 1 { return Err(ExecError { message: "duration() takes exactly 1 argument".into() }); }
            let arg = eval_expr(&args[0], row, params)?;
            Ok(Value::Duration(temporal_duration(&arg)?))
        }
        // --- duration.between / inMonths / inDays / inSeconds ---
        // These arrive as FunctionCall("between"|"inMonths"|…, [duration_expr, lhs, rhs])
        // because the parser sees `duration.between(a,b)` as:
        //   parse_property_chain(Variable("duration")) → method call → FunctionCall("between", [Variable("duration"), a, b])
        "between" if args.len() == 3 => {
            // args[0] is the "duration" variable reference (ignored — it's just the namespace)
            let lhs_val = eval_expr(&args[1], row, params)?;
            let rhs_val = eval_expr(&args[2], row, params)?;
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_between(lhs, rhs)))
        }
        "inmonths" if args.len() == 3 => {
            let lhs_val = eval_expr(&args[1], row, params)?;
            let rhs_val = eval_expr(&args[2], row, params)?;
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_months(lhs, rhs)))
        }
        "indays" if args.len() == 3 => {
            let lhs_val = eval_expr(&args[1], row, params)?;
            let rhs_val = eval_expr(&args[2], row, params)?;
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_days(lhs, rhs)))
        }
        "inseconds" if args.len() == 3 => {
            let lhs_val = eval_expr(&args[1], row, params)?;
            let rhs_val = eval_expr(&args[2], row, params)?;
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_seconds(lhs, rhs)))
        }
        // transaction() / statement() / realtime() — clock-access subtypes of datetime()
        "transaction" | "statement" | "realtime" if args.len() <= 1 => {
            let s = Spi::get_one::<String>("SELECT now()::text")
                .unwrap_or(Some("1970-01-01T00:00:00+00:00".into())).unwrap_or_default();
            let s = s.replace(' ', "T");
            Ok(Value::Temporal(temporal_datetime(&Value::Str(s))?))
        }
        // truncate() on temporals — not full spec but stub to avoid unknown-function
        "truncate" if args.len() == 2 => {
            // temporal.truncate(unit, value) — return the value as-is for now
            let val = eval_expr(&args[1], row, params)?;
            Ok(val)
        }
        // startNode(r) / endNode(r) — return source/target node of a relationship
        "startnode" | "startNode" if args.len() == 1 => {
            let rel_val = eval_expr(&args[0], row, params)?;
            match rel_val {
                Value::Edge { source, .. } => {
                    // Look up the source node.
                    use crate::catalog::labels::{label_name, prop_key_name};
                    use crate::storage::prop_store;
                    let record = unsafe {
                        let rel = crate::open_nodes_relation();
                        let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                        let r = crate::storage::node_store::find_node_by_id(rel, source, snapshot);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        r
                    };
                    match record {
                        Some(r) => {
                            let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
                            let properties = prop_store::decode(&r.prop_bytes, prop_key_name);
                            Ok(Value::Node { node_id: source, labels, properties })
                        }
                        None => Ok(Value::Null),
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError { message: "startNode() requires a relationship value".into() }),
            }
        }
        "endnode" | "endNode" if args.len() == 1 => {
            let rel_val = eval_expr(&args[0], row, params)?;
            match rel_val {
                Value::Edge { target, .. } => {
                    use crate::catalog::labels::{label_name, prop_key_name};
                    use crate::storage::prop_store;
                    let record = unsafe {
                        let rel = crate::open_nodes_relation();
                        let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                        let r = crate::storage::node_store::find_node_by_id(rel, target, snapshot);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        r
                    };
                    match record {
                        Some(r) => {
                            let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
                            let properties = prop_store::decode(&r.prop_bytes, prop_key_name);
                            Ok(Value::Node { node_id: target, labels, properties })
                        }
                        None => Ok(Value::Null),
                    }
                }
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError { message: "endNode() requires a relationship value".into() }),
            }
        }
        _ => Err(ExecError {
            message: format!("unknown function: {name}()"),
        }),
    }
}

fn to_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn as_temporal(v: &Value) -> Result<&TemporalValue, ExecError> {
    match v {
        Value::Temporal(tv) => Ok(tv),
        _ => Err(ExecError { message: format!("expected a temporal value, got {:?}", v) }),
    }
}

/// Helper for math functions that accept int or float and return int for int input.
fn math1(
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
    int_fn: impl Fn(i64) -> i64,
    float_fn: impl Fn(f64) -> f64,
) -> Result<Value, ExecError> {
    if args.len() != 1 {
        return Err(ExecError { message: "math function takes exactly 1 argument".into() });
    }
    let v = eval_expr(&args[0], row, params)?;
    match v {
        Value::Int(i) => Ok(Value::Int(int_fn(i))),
        Value::Float(f) => Ok(Value::Float(float_fn(f))),
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

/// Helper for math functions that always return float.
fn math1f(
    args: &[Expr],
    row: &Row,
    params: &HashMap<String, serde_json::Value>,
    float_fn: impl Fn(f64) -> f64,
) -> Result<Value, ExecError> {
    if args.len() != 1 {
        return Err(ExecError { message: "math function takes exactly 1 argument".into() });
    }
    let v = eval_expr(&args[0], row, params)?;
    match v {
        Value::Int(i) => Ok(Value::Float(float_fn(i as f64))),
        Value::Float(f) => Ok(Value::Float(float_fn(f))),
        Value::Null => Ok(Value::Null),
        _ => Ok(Value::Null),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    // NaN is not equal to anything (including itself)
    if matches!(a, Value::Float(f) if f.is_nan()) || matches!(b, Value::Float(f) if f.is_nan()) {
        return false;
    }
    match (a, b) {
        // JSON array equality: element-wise
        (Value::Json(serde_json::Value::Array(aa)), Value::Json(serde_json::Value::Array(bb))) => {
            if aa.len() != bb.len() { return false; }
            aa.iter().zip(bb.iter()).all(|(x, y)| {
                let xv = json_to_value(x);
                let yv = json_to_value(y);
                values_equal(&xv, &yv)
            })
        }
        _ => matches!(compare_values(a, &CmpOp::Eq, b), Some(true)),
    }
}

fn cmp_op_str(op: &CmpOp) -> &'static str {
    match op {
        CmpOp::Eq  => "=",
        CmpOp::Neq => "<>",
        CmpOp::Lt  => "<",
        CmpOp::Gt  => ">",
        CmpOp::Le  => "<=",
        CmpOp::Ge  => ">=",
    }
}

fn arith_op_str(op: &ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Div => "/",
        ArithOp::Mod => "%",
        ArithOp::Pow => "^",
    }
}

fn expr_default_name(expr: &Expr, idx: usize) -> String {
    match expr {
        Expr::Variable(name) => name.clone(),
        Expr::Property(base, prop) => {
            format!("{}.{prop}", expr_default_name(base, idx))
        }
        Expr::FunctionCall(name, args) => {
            // count_distinct(x) → "count(DISTINCT x)"
            if let Some(base) = name.strip_suffix("_distinct") {
                let arg_names: Vec<String> = args.iter()
                    .enumerate()
                    .map(|(i, a)| expr_default_name(a, i))
                    .collect();
                return format!("{base}(DISTINCT {})", arg_names.join(", "));
            }
            let arg_names: Vec<String> = args.iter()
                .enumerate()
                .map(|(i, a)| expr_default_name(a, i))
                .collect();
            format!("{name}({})", arg_names.join(", "))
        }
        Expr::Star => "*".into(),
        Expr::NullLit => "null".into(),
        Expr::IntLit(v) => v.to_string(),
        Expr::FloatLit(v) => format!("{v}"),
        Expr::BoolLit(b) => b.to_string(),
        Expr::StringLit(s) => format!("'{s}'"),
        Expr::Parameter(name) => format!("${name}"),
        Expr::IsNull(e) => format!("{} IS NULL", expr_default_name(e, idx)),
        Expr::IsNotNull(e) => format!("{} IS NOT NULL", expr_default_name(e, idx)),
        Expr::Not(e) => format!("NOT ({})", expr_default_name(e, idx)),
        Expr::Neg(e) => format!("-({})", expr_default_name(e, idx)),
        Expr::Compare(l, op, r) => format!(
            "{} {} {}",
            expr_default_name(l, idx),
            cmp_op_str(op),
            expr_default_name(r, idx),
        ),
        Expr::Arith(l, op, r) => format!(
            "{} {} {}",
            expr_default_name(l, idx),
            arith_op_str(op),
            expr_default_name(r, idx),
        ),
        Expr::And(l, r) => format!(
            "{} AND {}",
            expr_default_name(l, idx),
            expr_default_name(r, idx),
        ),
        Expr::Or(l, r) => format!(
            "{} OR {}",
            expr_default_name(l, idx),
            expr_default_name(r, idx),
        ),
        Expr::Subscript(base, idx_expr) => {
            let base_str = expr_default_name(base, idx);
            let idx_str = expr_default_name(idx_expr, idx);
            format!("{base_str}[{idx_str}]")
        }
        Expr::List(items) => {
            let parts: Vec<String> = items.iter().enumerate()
                .map(|(i, e)| expr_default_name(e, i))
                .collect();
            format!("[{}]", parts.join(", "))
        }
        Expr::HasLabel(inner, labels) => {
            // (n:Foo) → "n:Foo" — matches TCK column header `(n:Foo)` after paren stripping.
            format!("{}:{}", expr_default_name(inner, idx), labels.join(":"))
        }
        Expr::MapLiteral(pairs) => {
            let parts: Vec<String> = pairs.iter()
                .map(|(k, v)| format!("{k}: {}", expr_default_name(v, idx)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
        _ => format!("_col{idx}"),
    }
}

fn row_fingerprint(row: &Row) -> String {
    let mut parts: Vec<String> = row.iter().map(|(k, v)| {
        format!("{k}={}", serde_json::to_string(&v.to_json()).unwrap_or_default())
    }).collect();
    parts.sort();
    parts.join("|")
}

/// Compare two Values for ordering (used by ORDER BY).
/// NULL sorts last (greater than any non-null).
/// Cross-type ordering (openCypher): null > list > number > string > boolean
fn value_ordering(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::{Equal, Greater, Less};

    // openCypher ascending type ordering (smallest to largest):
    // Map < Node < Rel < List < Path < String < Boolean < Number < NaN < Null
    fn type_rank(v: &Value) -> i32 {
        match v {
            Value::Json(serde_json::Value::Object(_)) => 0,  // Map
            Value::Node { .. } => 1,
            Value::Edge { .. } => 2,
            Value::Json(serde_json::Value::Array(_)) => 3,  // List
            Value::Path { .. } => 4,
            Value::Str(_) => 5,
            Value::Bool(_) => 6,
            Value::Float(f) if f.is_nan() => 8,  // NaN > numbers but < null
            Value::Int(_) | Value::Float(_) => 7,
            Value::Null => 9,
            _ => 5,
        }
    }

    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Greater,
        (_, Value::Null) => Less,
        // Same-type comparisons
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => {
            if x.is_nan() && y.is_nan() { return Equal; }
            if x.is_nan() { return Greater; }  // NaN > regular floats
            if y.is_nan() { return Less; }
            x.partial_cmp(y).unwrap_or(Equal)
        }
        (Value::Int(x), Value::Float(y)) => {
            if y.is_nan() { return Less; }
            (*x as f64).partial_cmp(y).unwrap_or(Equal)
        }
        (Value::Float(x), Value::Int(y)) => {
            if x.is_nan() { return Greater; }
            x.partial_cmp(&(*y as f64)).unwrap_or(Equal)
        }
        (Value::Str(x), Value::Str(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Json(serde_json::Value::Array(xa)), Value::Json(serde_json::Value::Array(ya))) => {
            list_ordering(xa, ya)
        }
        // Cross-type: use the openCypher type rank
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// Lexicographic ordering of two JSON arrays (for ORDER BY).
/// Nulls within a list sort as Greater than any value (i.e., highest).
fn list_ordering(a: &[serde_json::Value], b: &[serde_json::Value]) -> std::cmp::Ordering {
    let min_len = a.len().min(b.len());
    for i in 0..min_len {
        let av = json_to_value(&a[i]);
        let bv = json_to_value(&b[i]);
        let cmp = value_ordering(&av, &bv);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
    }
    a.len().cmp(&b.len())
}

/// Evaluate a SKIP/LIMIT expression to a usize (params available, no row context needed).
fn eval_const_usize(expr: &Expr, params: &HashMap<String, serde_json::Value>) -> Result<usize, ExecError> {
    let dummy = Row::new();
    match eval_expr(expr, &dummy, params).unwrap_or(Value::Null) {
        Value::Int(n) if n < 0 =>
            Err(ExecError { message: "SyntaxError::NegativeIntegerArgument: SKIP/LIMIT must be a non-negative integer".into() }),
        Value::Int(n) => Ok(n as usize),
        Value::Float(_) =>
            Err(ExecError { message: "SyntaxError::InvalidArgumentType: SKIP/LIMIT must be an integer, not a float".into() }),
        _ => Ok(0),
    }
}

/// Apply: for each outer row, execute the inner plan and produce all (outer, inner) merged rows.
/// Used for CALL { subquery } and CALL proc() YIELD.
fn exec_apply(
    outer: &LogicalPlan,
    inner: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let outer_rows = execute(outer, params)?;
    let mut result = Vec::new();
    for outer_row in outer_rows {
        // Merge outer bindings into params for inner execution.
        let mut inner_params = params.clone();
        for (k, v) in &outer_row {
            inner_params.insert(k.clone(), v.to_json());
        }
        let inner_rows = execute(inner, &inner_params)?;
        if inner_rows.is_empty() {
            // No inner results: the outer row is dropped (inner join semantics for CALL).
            continue;
        }
        for inner_row in inner_rows {
            let mut merged = outer_row.clone();
            // Inner vars override outer (inner subquery can shadow outer names).
            for (k, v) in inner_row {
                merged.insert(k, v);
            }
            result.push(merged);
        }
    }
    Ok(result)
}

/// Convert result rows to JSONB output format.
/// LeftJoin: for each outer row, execute inner with outer vars in params.
/// If inner produces rows: emit merged (inner + outer) rows.
/// If inner produces no rows: emit outer row with null_vars set to Null.
/// This implements OPTIONAL MATCH semantics when new variables are introduced.
fn exec_left_join(
    outer: &LogicalPlan,
    inner: &LogicalPlan,
    null_vars: &[String],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let outer_rows = execute(outer, params)?;
    let mut result = Vec::new();
    for outer_row in &outer_rows {
        // Inject outer bindings into params so inner plan can look them up.
        let mut inner_params = params.clone();
        for (k, v) in outer_row {
            inner_params.insert(k.clone(), v.to_json());
        }
        let inner_rows = execute(inner, &inner_params)?;

        if inner_rows.is_empty() {
            // No match: emit outer row with null_vars = Null.
            let mut row = outer_row.clone();
            for v in null_vars {
                row.insert(v.clone(), Value::Null);
            }
            result.push(row);
        } else {
            // Matches found: emit each inner row merged with outer row.
            for inner_row in inner_rows {
                let mut row = outer_row.clone();
                // Inner vars take precedence (may refine or extend outer bindings).
                for (k, v) in inner_row {
                    row.insert(k, v);
                }
                result.push(row);
            }
        }
    }
    Ok(result)
}

pub fn rows_to_jsonb(rows: Vec<Row>) -> Vec<pgrx::JsonB> {
    rows.into_iter().map(|row| {
        let mut m = serde_json::Map::new();
        for (k, v) in &row {
            m.insert(k.clone(), v.to_json());
        }
        pgrx::JsonB(serde_json::Value::Object(m))
    }).collect()
}

// ===========================================================================
// v0.12.0: Write clause executors
// ===========================================================================

/// Accumulates catalog index writes so they can be flushed as bulk INSERTs
/// instead of one SPI round-trip per row.  All values are plain integers so
/// the format! path is safe from SQL injection.
#[derive(Default)]
struct CatalogWriteBuffer {
    label_index:   Vec<(i32, i64)>,        // (label_id, node_id)
    edge_type_src: Vec<(i32, i64, i64)>,   // (type_id, src_node_id, edge_id)
    edge_type_dst: Vec<(i32, i64, i64)>,   // (type_id, dst_node_id, edge_id)
}

impl CatalogWriteBuffer {
    fn flush(self) {
        if !self.label_index.is_empty() {
            let vals: String = self.label_index.iter()
                .map(|(l, n)| format!("({},{})", l, n))
                .collect::<Vec<_>>()
                .join(",");
            pgrx::Spi::run(&format!(
                "INSERT INTO _pg_eddy.label_index(label_id, node_id) VALUES {}",
                vals
            )).unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index bulk insert: {e}"));
        }
        if !self.edge_type_src.is_empty() {
            let vals: String = self.edge_type_src.iter()
                .map(|(t, s, e)| format!("({},{},{})", t, s, e))
                .collect::<Vec<_>>()
                .join(",");
            pgrx::Spi::run(&format!(
                "INSERT INTO _pg_eddy.edge_type_src(type_id, src_node_id, edge_id) VALUES {}",
                vals
            )).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src bulk insert: {e}"));
        }
        if !self.edge_type_dst.is_empty() {
            let vals: String = self.edge_type_dst.iter()
                .map(|(t, d, e)| format!("({},{},{})", t, d, e))
                .collect::<Vec<_>>()
                .join(",");
            pgrx::Spi::run(&format!(
                "INSERT INTO _pg_eddy.edge_type_dst(type_id, dst_node_id, edge_id) VALUES {}",
                vals
            )).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst bulk insert: {e}"));
        }
    }
}

/// Execute CREATE patterns, creating nodes and relationships.
/// For each input row, create all pattern nodes/rels and pass through the row
/// (augmented with any new variables introduced by CREATE).
fn exec_create_pattern(
    input: &LogicalPlan,
    patterns: &[Pattern],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let input_rows = execute(input, params)?;
    let mut result = Vec::new();
    let mut buf = CatalogWriteBuffer::default();
    for row in input_rows {
        let mut new_row = row.clone();
        for pattern in patterns {
            create_pattern_in_row(pattern, &mut new_row, params, &mut buf)?;
        }
        result.push(new_row);
    }
    // If there were no input rows (empty pipeline), create once.
    if result.is_empty() && matches!(input, LogicalPlan::SingleRow) {
        let mut new_row = Row::new();
        for pattern in patterns {
            create_pattern_in_row(pattern, &mut new_row, params, &mut buf)?;
        }
        result.push(new_row);
    }
    buf.flush();
    Ok(result)
}

/// Create a single pattern (sequence of nodes/rels) updating `row` with new vars.
/// Catalog index writes are appended to `buf` for bulk flushing by the caller.
fn create_pattern_in_row(
    pattern: &Pattern,
    row: &mut Row,
    params: &HashMap<String, serde_json::Value>,
    buf: &mut CatalogWriteBuffer,
) -> Result<(), ExecError> {
    use crate::catalog::labels::{ensure_label, ensure_prop_key, ensure_rel_type, next_node_id, next_edge_id};
    use crate::storage::{prop_store, node_store, edge_store};

    let mut last_node_id: Option<i64> = None;
    // Track whether the Relationship arm pre-created the destination node (anonymous or named).
    // When true, the next Node arm should skip creating a new node and use last_node_id.
    let mut node_was_precreated = false;

    for (i, elem) in pattern.elements.iter().enumerate() {
        match elem {
            PatternElement::Node(n) => {
                let var = n.variable.as_deref();

                // If this node was pre-created by the preceding Relationship arm, skip it.
                if node_was_precreated && i > 0 {
                    node_was_precreated = false;
                    // last_node_id is already set to the pre-created node's ID.
                    continue;
                }
                node_was_precreated = false;

                // If the node has a variable and it's already bound, reuse it.
                if let Some(v) = var
                    && let Some(existing) = row.get(v)
                        && let Value::Node { node_id, .. } = existing {
                            // Using a bound variable as a relationship endpoint is OK.
                            // But CREATE (n) — a standalone already-bound variable — is a SyntaxError:
                            // you cannot CREATE a node that is already bound.
                            if pattern.elements.len() == 1 {
                                return Err(ExecError {
                                    message: format!(
                                        "SyntaxError: variable `{v}` is already bound; \
                                         cannot CREATE it as a standalone node"
                                    ),
                                });
                            }
                            last_node_id = Some(*node_id);
                            continue;
                        }

                // Create a new node.
                let labels: Vec<String> = n.labels.clone();
                let label_ids: Vec<i32> = labels.iter().map(|l| ensure_label(l)).collect();

                // Evaluate properties.
                let mut prop_map = serde_json::Map::new();
                for (k, expr) in &n.properties {
                    let v = eval_expr(expr, row, params)?;
                    prop_map.insert(k.clone(), v.to_json());
                }
                let prop_bytes = prop_store::encode(&prop_map, |name| -> Result<i32, std::convert::Infallible> {
                    Ok(ensure_prop_key(name))
                }).unwrap_or_default();

                let node_id = next_node_id();
                unsafe {
                    let rel = crate::open_nodes_relation();
                    node_store::insert_node(rel, node_id, &label_ids, &prop_bytes);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                }
                // Batch label_index writes.
                for lid in &label_ids {
                    buf.label_index.push((*lid, node_id));
                }

                let node_val = Value::Node { node_id, labels: labels.clone(), properties: prop_map };
                if let Some(v) = var {
                    row.insert(v.to_string(), node_val);
                }
                last_node_id = Some(node_id);
            }
            PatternElement::Relationship(r) => {
                if i == 0 {
                    return Err(ExecError { message: "pattern cannot start with a relationship".into() });
                }

                let src_id = last_node_id.ok_or_else(|| ExecError {
                    message: "no source node for relationship".into(),
                })?;

                // Peek at the next element to get the destination node id.
                let next_elem = pattern.elements.get(i + 1);
                let dst_id = match next_elem {
                    Some(PatternElement::Node(n2)) => {
                        // If destination is already bound, use its id; otherwise will be created by next iteration.
                        // We create destination nodes eagerly here to handle forward references.
                        let var = n2.variable.as_deref();
                        if let Some(v) = var {
                            if let Some(existing) = row.get(v) {
                                if let Value::Node { node_id, .. } = existing {
                                    *node_id
                                } else {
                                    return Err(ExecError { message: format!("variable {} is not a node", v) });
                                }
                            } else {
                                // Pre-create the destination node so we can form the edge.
                                let labels: Vec<String> = n2.labels.clone();
                                let label_ids: Vec<i32> = labels.iter().map(|l| ensure_label(l)).collect();
                                let mut prop_map = serde_json::Map::new();
                                for (k, expr) in &n2.properties {
                                    let v2 = eval_expr(expr, row, params)?;
                                    prop_map.insert(k.clone(), v2.to_json());
                                }
                                let prop_bytes = prop_store::encode(&prop_map, |name| -> Result<i32, std::convert::Infallible> {
                                    Ok(ensure_prop_key(name))
                                }).unwrap_or_default();
                                let nid = next_node_id();
                                unsafe {
                                    let rel = crate::open_nodes_relation();
                                    node_store::insert_node(rel, nid, &label_ids, &prop_bytes);
                                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                }
                                for lid in &label_ids {
                                    buf.label_index.push((*lid, nid));
                                }
                                let node_val = Value::Node { node_id: nid, labels, properties: prop_map };
                                if let Some(v) = var {
                                    row.insert(v.to_string(), node_val);
                                }
                                // Mark next node element as pre-created so it doesn't create again.
                                last_node_id = Some(nid);
                                node_was_precreated = true;
                                nid
                            }
                        } else {
                            // Anonymous destination node — create it.
                            let labels: Vec<String> = n2.labels.clone();
                            let label_ids: Vec<i32> = labels.iter().map(|l| ensure_label(l)).collect();
                            let mut prop_map = serde_json::Map::new();
                            for (k, expr) in &n2.properties {
                                let vv = eval_expr(expr, row, params)?;
                                prop_map.insert(k.clone(), vv.to_json());
                            }
                            let prop_bytes = prop_store::encode(&prop_map, |name| -> Result<i32, std::convert::Infallible> {
                                Ok(ensure_prop_key(name))
                            }).unwrap_or_default();
                            let nid = next_node_id();
                            unsafe {
                                let rel = crate::open_nodes_relation();
                                node_store::insert_node(rel, nid, &label_ids, &prop_bytes);
                                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            }
                            for lid in &label_ids {
                                buf.label_index.push((*lid, nid));
                            }
                            // Mark next node element as pre-created so it doesn't create again.
                            last_node_id = Some(nid);
                            node_was_precreated = true;
                            nid
                        }
                    }
                    _ => return Err(ExecError { message: "relationship must be followed by a node".into() }),
                };

                // Create the relationship.
                let rel_type = r.rel_types.first().cloned().unwrap_or_else(|| "RELATED_TO".to_string());
                let type_id = ensure_rel_type(&rel_type);
                let mut prop_map = serde_json::Map::new();
                for (k, expr) in &r.properties {
                    let v = eval_expr(expr, row, params)?;
                    prop_map.insert(k.clone(), v.to_json());
                }
                let prop_bytes = prop_store::encode(&prop_map, |name| -> Result<i32, std::convert::Infallible> {
                    Ok(ensure_prop_key(name))
                }).unwrap_or_default();

                let (actual_src, actual_dst) = match r.direction {
                    RelDirection::Both => {
                        return Err(ExecError {
                            message: "SyntaxError: only directed relationships are supported in CREATE".into(),
                        });
                    }
                    RelDirection::In => (dst_id, src_id),
                    RelDirection::Out => (src_id, dst_id),
                };

                let edge_id = next_edge_id();
                unsafe {
                    let node_rel = crate::open_nodes_relation();
                    let edge_rel = crate::open_edges_relation();
                    edge_store::insert_edge(node_rel, edge_rel, edge_id, type_id, actual_src, actual_dst, &prop_bytes);
                    pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                }
                buf.edge_type_src.push((type_id, actual_src, edge_id));
                buf.edge_type_dst.push((type_id, actual_dst, edge_id));

                if let Some(rv) = &r.variable {
                    let edge_val = Value::Edge { edge_id, rel_type, source: actual_src, target: actual_dst, properties: prop_map };
                    row.insert(rv.clone(), edge_val);
                }
            }
        }
    }
    // Build path value if pattern has a path variable.
    if let Some(ref pvar) = pattern.variable {
        let elem_vars: Vec<String> = pattern.elements.iter().map(|e| match e {
            PatternElement::Node(n) => n.variable.clone().unwrap_or_else(|| "_anon_n0".to_string()),
            PatternElement::Relationship(r) => r.variable.clone().unwrap_or_else(|| "_anon_r".to_string()),
        }).collect();
        let path_val = build_path_from_elem_vars(&elem_vars, row);
        row.insert(pvar.clone(), path_val);
    }
    Ok(())
}

/// Execute SET clause: modify properties or labels on bound nodes/rels.
fn exec_set_prop(
    input: &LogicalPlan,
    items: &[SetItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{ensure_label, ensure_prop_key, label_name, prop_key_name, label_id_by_name};
    use crate::storage::{prop_store, node_store};

    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    for mut row in input_rows {
        // Inject params into row for variable lookup (e.g. MERGE ON MATCH/ON CREATE).
        for (k, v) in params {
            row.entry(k.clone()).or_insert_with(|| json_to_value(v));
        }
        for item in items {
            match item {
                SetItem::Property(prop_expr, val_expr) => {
                    // prop_expr is Expr::Property(Expr::Variable(v), key)
                    let (var, key) = match prop_expr {
                        Expr::Property(inner, k) => {
                            match inner.as_ref() {
                                Expr::Variable(v) => (v.clone(), k.clone()),
                                _ => return Err(ExecError { message: "SET property must reference a variable".into() }),
                            }
                        }
                        _ => return Err(ExecError { message: "SET property must reference a property access".into() }),
                    };
                    let val = eval_expr(val_expr, &row, params)?;
                    // Validate property type: maps and lists-of-maps are not valid property values.
                    match &val {
                        Value::Json(serde_json::Value::Object(_)) => {
                            return Err(ExecError {
                                message: "TypeError: InvalidPropertyType — \
                                          map is not a valid property value".into(),
                            });
                        }
                        Value::Json(serde_json::Value::Array(arr))
                            if arr.iter().any(|e| matches!(e, serde_json::Value::Object(_))) =>
                        {
                            return Err(ExecError {
                                message: "TypeError: InvalidPropertyType — \
                                          list of maps is not a valid property value".into(),
                            });
                        }
                        _ => {}
                    }
                    // Skip if target variable is null (SET on null is a no-op).
                    if matches!(row.get(&var), Some(Value::Null)) {
                        continue;
                    }
                    // Handle edge property SET.
                    if matches!(row.get(&var), Some(Value::Edge { .. })) {
                        let edge_id = match row.get(&var) {
                            Some(Value::Edge { edge_id, .. }) => *edge_id,
                            _ => unreachable!(),
                        };
                        // Load current edge props and update.
                        let mut props2 = match row.get(&var) {
                            Some(Value::Edge { properties, .. }) => properties.clone(),
                            _ => serde_json::Map::new(),
                        };
                        if val == Value::Null {
                            props2.remove(&key);
                        } else {
                            props2.insert(key.clone(), val.to_json());
                        }
                        let new_bytes = prop_store::encode(&props2, |name| -> Result<i32, std::convert::Infallible> {
                            Ok(ensure_prop_key(name))
                        }).unwrap_or_default();
                        unsafe {
                            let edge_rel = crate::open_edges_relation();
                            crate::storage::edge_store::update_edge_props(edge_rel, edge_id, &new_bytes);
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        // Update in-memory row value.
                        if let Some(Value::Edge { properties, .. }) = row.get_mut(&var) {
                            if val == Value::Null {
                                properties.remove(&key);
                            } else {
                                properties.insert(key.clone(), val.to_json());
                            }
                        }
                        continue;
                    }
                    // Load current node state.
                    let node_id = node_id_from_row(&row, &var)?;
                    let mut rec = load_node(node_id)?;
                    // Resolve overflow if needed.
                    if rec.overflow_blkno != 0 && rec.prop_bytes.is_empty() {
                        rec.prop_bytes = unsafe {
                            let rel = crate::open_nodes_relation();
                            let bytes = node_store::read_overflow_block(rel, rec.overflow_blkno);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            bytes
                        };
                    }
                    let mut props = prop_store::decode(&rec.prop_bytes, prop_key_name);
                    if val == Value::Null {
                        props.remove(&key);
                    } else {
                        props.insert(key.clone(), val.to_json());
                    }
                    let new_bytes = prop_store::encode(&props, |name| -> Result<i32, std::convert::Infallible> {
                        Ok(ensure_prop_key(name))
                    }).unwrap_or_default();
                    unsafe {
                        let rel = crate::open_nodes_relation();
                        node_store::update_node(rel, node_id, &rec.label_ids, &new_bytes);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    // Update in-memory row value.
                    if let Some(Value::Node { properties, .. }) = row.get_mut(&var) {
                        if val == Value::Null {
                            properties.remove(&key);
                        } else {
                            properties.insert(key.clone(), val.to_json());
                        }
                    }
                }
                SetItem::Variable(var, val_expr) => {
                    // Skip if target variable is null (SET on null is a no-op).
                    if matches!(row.get(var), Some(Value::Null)) {
                        continue;
                    }
                    // n = {map|node|edge} — replace all properties with the source properties.
                    let val = eval_expr(val_expr, &row, params)?;
                    let new_props = match val {
                        // Node: copy node's properties
                        Value::Node { properties, .. } => properties,
                        // Edge: copy edge's properties
                        Value::Edge { properties, .. } => properties,
                        // Map: use directly
                        Value::Json(serde_json::Value::Object(m)) => m,
                        // Null: no-op (skip)
                        Value::Null => continue,
                        _ => return Err(ExecError { message: format!("SET {var} = must be a map, node, or relationship") }),
                    };
                    let new_bytes = prop_store::encode(&new_props, |name| -> Result<i32, std::convert::Infallible> {
                        Ok(ensure_prop_key(name))
                    }).unwrap_or_default();
                    // Handle both Node and Edge.
                    if let Some(Value::Edge { edge_id, .. }) = row.get(var) {
                        let edge_id = *edge_id;
                        unsafe {
                            let edge_rel = crate::open_edges_relation();
                            crate::storage::edge_store::update_edge_props(edge_rel, edge_id, &new_bytes);
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if let Some(Value::Edge { properties, .. }) = row.get_mut(var) {
                            *properties = new_props;
                        }
                    } else {
                        let node_id = node_id_from_row(&row, var)?;
                        let rec = load_node(node_id)?;
                        unsafe {
                            let rel = crate::open_nodes_relation();
                            node_store::update_node(rel, node_id, &rec.label_ids, &new_bytes);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if let Some(Value::Node { properties, .. }) = row.get_mut(var) {
                            *properties = new_props;
                        }
                    }
                }
                SetItem::MergeMap(var, val_expr) => {
                    // Skip if target variable is null (SET on null is a no-op).
                    if matches!(row.get(var), Some(Value::Null)) {
                        continue;
                    }
                    // n += {map|node|edge} — merge properties from source.
                    let val = eval_expr(val_expr, &row, params)?;
                    let extra = match val {
                        Value::Node { properties, .. } => properties,
                        Value::Edge { properties, .. } => properties,
                        Value::Json(serde_json::Value::Object(m)) => m,
                        Value::Null => continue,
                        _ => return Err(ExecError { message: format!("SET {var} += must be a map, node, or relationship") }),
                    };
                    // Handle both Node and Edge.
                    if let Some(Value::Edge { edge_id, properties: edge_props, .. }) = row.get(var) {
                        let edge_id = *edge_id;
                        // Merge extra into current edge properties.
                        let mut props = edge_props.clone();
                        for (k, v) in extra.iter() {
                            if v.is_null() {
                                props.remove(k);
                            } else {
                                props.insert(k.clone(), v.clone());
                            }
                        }
                        let new_bytes = prop_store::encode(&props, |name| -> Result<i32, std::convert::Infallible> {
                            Ok(ensure_prop_key(name))
                        }).unwrap_or_default();
                        unsafe {
                            let edge_rel = crate::open_edges_relation();
                            crate::storage::edge_store::update_edge_props(edge_rel, edge_id, &new_bytes);
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if let Some(Value::Edge { properties, .. }) = row.get_mut(var) {
                            for (k, v) in &extra {
                                if v.is_null() {
                                    properties.remove(k);
                                } else {
                                    properties.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    } else {
                        let node_id = node_id_from_row(&row, var)?;
                        let mut rec = load_node(node_id)?;
                        if rec.overflow_blkno != 0 && rec.prop_bytes.is_empty() {
                            rec.prop_bytes = unsafe {
                                let rel = crate::open_nodes_relation();
                                let bytes = node_store::read_overflow_block(rel, rec.overflow_blkno);
                                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                bytes
                            };
                        }
                        let mut props = prop_store::decode(&rec.prop_bytes, prop_key_name);
                        for (k, v) in extra.iter() {
                            if v.is_null() {
                                props.remove(k);
                            } else {
                                props.insert(k.clone(), v.clone());
                            }
                        }
                        let new_bytes = prop_store::encode(&props, |name| -> Result<i32, std::convert::Infallible> {
                            Ok(ensure_prop_key(name))
                        }).unwrap_or_default();
                        unsafe {
                            let rel = crate::open_nodes_relation();
                            node_store::update_node(rel, node_id, &rec.label_ids, &new_bytes);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if let Some(Value::Node { properties, .. }) = row.get_mut(var) {
                            for (k, v) in &extra {
                                if v.is_null() {
                                    properties.remove(k);
                                } else {
                                    properties.insert(k.clone(), v.clone());
                                }
                            }
                        }
                    }
                }
                SetItem::Label(var, new_labels) => {
                    // n:Label — add labels to node.
                    // Skip if variable is null (null is ignored in SET label).
                    if matches!(row.get(var), Some(Value::Null)) {
                        continue;
                    }
                    let node_id = node_id_from_row(&row, var)?;
                    let mut rec = load_node(node_id)?;
                    if rec.overflow_blkno != 0 && rec.prop_bytes.is_empty() {
                        rec.prop_bytes = unsafe {
                            let rel = crate::open_nodes_relation();
                            let bytes = node_store::read_overflow_block(rel, rec.overflow_blkno);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            bytes
                        };
                    }
                    let mut label_ids = rec.label_ids.clone();
                    for lname in new_labels {
                        let lid = ensure_label(lname);
                        if !label_ids.contains(&lid) {
                            label_ids.push(lid);
                            pgrx::Spi::run_with_args(
                                "INSERT INTO _pg_eddy.label_index(label_id, node_id) VALUES ($1, $2)",
                                &[pgrx::datum::DatumWithOid::from(lid), pgrx::datum::DatumWithOid::from(node_id)],
                            ).unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index insert: {e}"));
                        }
                    }
                    unsafe {
                        let rel = crate::open_nodes_relation();
                        node_store::update_node(rel, node_id, &label_ids, &rec.prop_bytes);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    // Update in-memory node labels.
                    if let Some(Value::Node { labels, .. }) = row.get_mut(var) {
                        for lname in new_labels {
                            if !labels.contains(lname) {
                                labels.push(lname.clone());
                            }
                        }
                    }
                    let _ = label_id_by_name; // suppress unused warning
                    let _ = label_name;
                }
            }
        }
        result.push(row);
    }
    Ok(result)
}

/// Execute REMOVE clause: remove properties or labels from bound nodes.
fn exec_remove_prop(
    input: &LogicalPlan,
    items: &[RemoveItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_id_by_name, ensure_prop_key, prop_key_name};
    use crate::storage::{prop_store, node_store};

    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    for mut row in input_rows {
        // Inject params into row for variable lookup.
        for (k, v) in params {
            row.entry(k.clone()).or_insert_with(|| json_to_value(v));
        }
        for item in items {
            match item {
                RemoveItem::Property(var_expr, key) => {
                    let var = if let Expr::Variable(v) = var_expr { v } else {
                        return Err(ExecError { message: "REMOVE property must reference a variable".into() });
                    };
                    // Skip null (ignore null in REMOVE).
                    if matches!(row.get(var), Some(Value::Null)) {
                        continue;
                    }
                    // Handle edge property removal.
                    if matches!(row.get(var), Some(Value::Edge { .. })) {
                        let (edge_id, mut cur_props) = match row.get(var) {
                            Some(Value::Edge { edge_id, properties, .. }) => (*edge_id, properties.clone()),
                            _ => unreachable!(),
                        };
                        cur_props.remove(key);
                        let new_bytes = prop_store::encode(&cur_props, |name| -> Result<i32, std::convert::Infallible> {
                            Ok(ensure_prop_key(name))
                        }).unwrap_or_default();
                        unsafe {
                            let edge_rel = crate::open_edges_relation();
                            crate::storage::edge_store::update_edge_props(edge_rel, edge_id, &new_bytes);
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if let Some(Value::Edge { properties, .. }) = row.get_mut(var) {
                            properties.remove(key);
                        }
                        continue;
                    }
                    let node_id = node_id_from_row(&row, var)?;
                    let mut rec = load_node(node_id)?;
                    if rec.overflow_blkno != 0 && rec.prop_bytes.is_empty() {
                        rec.prop_bytes = unsafe {
                            let rel = crate::open_nodes_relation();
                            let bytes = node_store::read_overflow_block(rel, rec.overflow_blkno);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            bytes
                        };
                    }
                    let mut props = prop_store::decode(&rec.prop_bytes, prop_key_name);
                    props.remove(key);
                    let new_bytes = prop_store::encode(&props, |name| -> Result<i32, std::convert::Infallible> {
                        Ok(ensure_prop_key(name))
                    }).unwrap_or_default();
                    unsafe {
                        let rel = crate::open_nodes_relation();
                        node_store::update_node(rel, node_id, &rec.label_ids, &new_bytes);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    if let Some(Value::Node { properties, .. }) = row.get_mut(var) {
                        properties.remove(key);
                    }
                }
                RemoveItem::Label(var, rm_labels) => {
                    // Skip null (ignore null in REMOVE label).
                    if matches!(row.get(var), Some(Value::Null)) {
                        continue;
                    }
                    let node_id = node_id_from_row(&row, var)?;
                    let mut rec = load_node(node_id)?;
                    if rec.overflow_blkno != 0 && rec.prop_bytes.is_empty() {
                        rec.prop_bytes = unsafe {
                            let rel = crate::open_nodes_relation();
                            let bytes = node_store::read_overflow_block(rel, rec.overflow_blkno);
                            pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            bytes
                        };
                    }
                    let mut label_ids = rec.label_ids.clone();
                    for lname in rm_labels {
                        if let Some(lid) = label_id_by_name(lname) {
                            label_ids.retain(|&l| l != lid);
                            pgrx::Spi::run_with_args(
                                "DELETE FROM _pg_eddy.label_index WHERE label_id = $1 AND node_id = $2",
                                &[pgrx::datum::DatumWithOid::from(lid), pgrx::datum::DatumWithOid::from(node_id)],
                            ).unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete: {e}"));
                        }
                    }
                    unsafe {
                        let rel = crate::open_nodes_relation();
                        node_store::update_node(rel, node_id, &label_ids, &rec.prop_bytes);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    if let Some(Value::Node { labels, .. }) = row.get_mut(var) {
                        labels.retain(|l| !rm_labels.contains(l));
                    }
                }
            }
        }
        result.push(row);
    }
    Ok(result)
}

/// Execute DELETE / DETACH DELETE.
fn exec_delete_nodes(
    input: &LogicalPlan,
    exprs: &[Expr],
    detach: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::next_edge_id;
    use crate::storage::edge_store::{Direction, adjacency_follow, delete_edge};
    use crate::storage::node_store::delete_node_by_id;
    use std::collections::HashSet;

    let input_rows = execute(input, params)?;

    // Track already-deleted nodes/edges to avoid double-delete (e.g., undirected expand).
    let mut deleted_nodes: HashSet<i64> = HashSet::new();
    let mut deleted_edges: HashSet<i64> = HashSet::new();

    for row in &input_rows {
        for expr in exprs {
            let val = eval_expr(expr, row, params)?;
            match val {
                Value::Node { node_id, .. } => {
                    if deleted_nodes.contains(&node_id) { continue; }
                    if detach {
                        // Collect and delete all edges first.
                        let all_edge_ids: Vec<i64> = unsafe {
                            let node_rel = crate::open_nodes_relation();
                            let edge_rel = crate::open_edges_relation();
                            let snapshot = pgrx::pg_sys::GetActiveSnapshot();
                            let out_edges = adjacency_follow(node_rel, edge_rel, node_id, Direction::Out, None, snapshot);
                            let in_edges = adjacency_follow(node_rel, edge_rel, node_id, Direction::In, None, snapshot);
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                            let mut seen: HashSet<i64> = HashSet::new();
                            for e in out_edges.iter().chain(in_edges.iter()) { seen.insert(e.edge_id); }
                            seen.into_iter().collect()
                        };
                        let new_edge_ids: Vec<i64> = all_edge_ids.into_iter()
                            .filter(|eid| !deleted_edges.contains(eid))
                            .collect();
                        unsafe {
                            let edge_rel = crate::open_edges_relation();
                            for eid in &new_edge_ids { delete_edge(edge_rel, *eid); }
                            pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                        }
                        if !new_edge_ids.is_empty() {
                            pgrx::Spi::run_with_args(
                                "DELETE FROM _pg_eddy.edge_type_src WHERE edge_id = ANY($1)",
                                &[pgrx::datum::DatumWithOid::from(new_edge_ids.as_slice())],
                            ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src delete: {e}"));
                            pgrx::Spi::run_with_args(
                                "DELETE FROM _pg_eddy.edge_type_dst WHERE edge_id = ANY($1)",
                                &[pgrx::datum::DatumWithOid::from(new_edge_ids.as_slice())],
                            ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst delete: {e}"));
                            for eid in new_edge_ids { deleted_edges.insert(eid); }
                        }
                    }
                    // Delete the node.
                    unsafe {
                        let rel = crate::open_nodes_relation();
                        delete_node_by_id(rel, node_id);
                        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    pgrx::Spi::run_with_args(
                        "DELETE FROM _pg_eddy.label_index WHERE node_id = $1",
                        &[pgrx::datum::DatumWithOid::from(node_id)],
                    ).unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete: {e}"));
                    deleted_nodes.insert(node_id);
                }
                Value::Null => { /* no-op for null nodes */ }
                Value::Edge { edge_id, .. } => {
                    if deleted_edges.contains(&edge_id) { continue; }
                    // DELETE a relationship directly.
                    unsafe {
                        let edge_rel = crate::open_edges_relation();
                        delete_edge(edge_rel, edge_id);
                        pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    }
                    pgrx::Spi::run_with_args(
                        "DELETE FROM _pg_eddy.edge_type_src WHERE edge_id = $1",
                        &[pgrx::datum::DatumWithOid::from(edge_id)],
                    ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src delete: {e}"));
                    pgrx::Spi::run_with_args(
                        "DELETE FROM _pg_eddy.edge_type_dst WHERE edge_id = $1",
                        &[pgrx::datum::DatumWithOid::from(edge_id)],
                    ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst delete: {e}"));
                    deleted_edges.insert(edge_id);
                }
                other => {
                    // Handle Path: delete all nodes and edges in the path.
                    // Per spec, DELETE on a path deletes all elements (both
                    // nodes and relationships) without requiring DETACH.
                    if let Value::Path { nodes, rels } = other {
                        for rel in &rels {
                            if let Value::Edge { edge_id, .. } = rel {
                                if deleted_edges.contains(edge_id) { continue; }
                                unsafe {
                                    let edge_rel = crate::open_edges_relation();
                                    delete_edge(edge_rel, *edge_id);
                                    pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                }
                                pgrx::Spi::run_with_args(
                                    "DELETE FROM _pg_eddy.edge_type_src WHERE edge_id = $1",
                                    &[pgrx::datum::DatumWithOid::from(*edge_id)],
                                ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src delete: {e}"));
                                pgrx::Spi::run_with_args(
                                    "DELETE FROM _pg_eddy.edge_type_dst WHERE edge_id = $1",
                                    &[pgrx::datum::DatumWithOid::from(*edge_id)],
                                ).unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst delete: {e}"));
                                deleted_edges.insert(*edge_id);
                            }
                        }
                        for node_val in &nodes {
                            if let Value::Node { node_id, .. } = node_val {
                                if deleted_nodes.contains(node_id) { continue; }
                                unsafe {
                                    let rel = crate::open_nodes_relation();
                                    delete_node_by_id(rel, *node_id);
                                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                }
                                pgrx::Spi::run_with_args(
                                    "DELETE FROM _pg_eddy.label_index WHERE node_id = $1",
                                    &[pgrx::datum::DatumWithOid::from(*node_id)],
                                ).unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete: {e}"));
                                deleted_nodes.insert(*node_id);
                            }
                        }
                    } else {
                        return Err(ExecError {
                            message: format!("DELETE: expected a node, got {:?}", other),
                        });
                    }
                }
            }
        }
    }
    let _ = next_edge_id; // suppress unused import warning
    // DELETE passes through the input rows so downstream clauses (RETURN, SKIP, LIMIT, etc.)
    // can still use them. The deleted nodes remain in memory as stale values, which is fine
    // since the property values were captured at the time of MATCH.
    Ok(input_rows)
}

/// Execute MERGE pattern.
fn exec_foreach(
    input: &LogicalPlan,
    variable: &str,
    list_expr: &Expr,
    body: &LogicalPlan,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let input_rows = execute(input, params)?;
    // FOREACH does not produce rows — it executes side-effects and passes through its input.
    for input_row in &input_rows {
        // Evaluate the list in the context of the outer row.
        let list_val = eval_expr(list_expr, input_row, params)?;
        let items: Vec<Value> = match list_val {
            Value::Json(serde_json::Value::Array(arr)) => arr.iter().map(json_to_value).collect(),
            Value::Null => continue,
            other => vec![other],
        };
        for item in items {
            // Build a row with the loop variable bound.
            let mut iter_row = input_row.clone();
            iter_row.insert(variable.to_string(), item.clone());
            // Inject the loop variable into params as a JSON value so that the
            // body sub-plan (which starts from SingleRow) can find it.
            let mut iter_params = params.clone();
            iter_params.insert(variable.to_string(), item.to_json());
            // Execute the body plan (ignoring output rows — only side-effects matter).
            // We need to pass the outer row's variables into the body via a SingleRow
            // seed that has the variable bound. Since SingleRow always produces one
            // empty row and the body plan builds on top of that, we thread the
            // loop-variable binding through params.
            let _ = execute(body, &iter_params);
        }
    }
    // Pass through the input rows unchanged (spec: FOREACH produces no new rows).
    Ok(input_rows)
}

fn exec_merge_pattern(
    input: &LogicalPlan,
    pattern: &Pattern,
    on_create: &[SetItem],
    on_match: &[SetItem],
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::cypher::planner::plan;

    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    // Build a read-only match plan from the pattern.
    let match_clause = crate::cypher::ast::QueryClause::Match {
        optional: false,
        patterns: vec![pattern.clone()],
        where_clause: None,
    };
    let match_query = crate::cypher::ast::Query { clauses: vec![match_clause], union: None };
    let match_plan = plan(&match_query).map_err(|e| ExecError { message: e.message })?;

    for outer_row in input_rows {
        // Inject outer row bindings into params.
        let mut merged_params = params.clone();
        for (k, v) in &outer_row {
            merged_params.insert(k.clone(), v.to_json());
        }

        // Check for null properties in the MERGE pattern (SemanticError: MergeReadOwnWrites).
        for elem in &pattern.elements {
            let props = match elem {
                PatternElement::Node(n) => &n.properties,
                PatternElement::Relationship(r) => &r.properties,
            };
            for (key, expr) in props {
                let val = eval_expr(expr, &outer_row, &merged_params)?;
                if matches!(val, Value::Null) {
                    return Err(ExecError {
                        message: format!(
                            "SemanticError: MergeReadOwnWrites — \
                             MERGE pattern property '{key}' cannot be null"
                        ),
                    });
                }
            }
        }

        // Try to match the pattern.
        let matches = execute(&match_plan, &merged_params)?;

        if matches.is_empty() {
            // Pattern not found — CREATE it.
            // Normalize undirected relationships to OUT (openCypher MERGE semantics).
            let mut create_pattern = pattern.clone();
            for elem in &mut create_pattern.elements {
                if let PatternElement::Relationship(r) = elem
                    && r.direction == crate::cypher::ast::RelDirection::Both {
                        r.direction = crate::cypher::ast::RelDirection::Out;
                    }
            }
            // For path binding, ensure every element has a variable so the path
            // can be constructed from the row after create.
            if pattern.variable.is_some() {
                for (i, elem) in create_pattern.elements.iter_mut().enumerate() {
                    match elem {
                        PatternElement::Node(n) if n.variable.is_none() => {
                            n.variable = Some(format!("_anon_merge_n{i}"));
                        }
                        PatternElement::Relationship(r) if r.variable.is_none() => {
                            r.variable = Some(format!("_anon_merge_r{i}"));
                        }
                        _ => {}
                    }
                }
            }
            let mut new_row = outer_row.clone();
            let mut buf = CatalogWriteBuffer::default();
            create_pattern_in_row(&create_pattern, &mut new_row, &merged_params, &mut buf)?;
            buf.flush();
            // Apply ON CREATE SET.
            if !on_create.is_empty() {
                // Run set with the new_row as context params.
                let mut ctx = merged_params.clone();
                for (k, v) in &new_row {
                    ctx.insert(k.clone(), v.to_json());
                }
                let set_rows = exec_set_prop(
                    &crate::cypher::planner::LogicalPlan::SingleRow,
                    on_create,
                    &ctx,
                )?;
                if let Some(set_row) = set_rows.into_iter().next() {
                    for (k, v) in set_row {
                        new_row.insert(k, v);
                    }
                }
            }
            // Build path value if pattern has a path variable.
            if let Some(ref pvar) = pattern.variable {
                let elem_vars: Vec<String> = create_pattern.elements.iter().map(|e| match e {
                    PatternElement::Node(n) => n.variable.clone().unwrap_or_else(|| "_anon_n0".to_string()),
                    PatternElement::Relationship(r) => r.variable.clone().unwrap_or_else(|| "_anon_r".to_string()),
                }).collect();
                let path_val = build_path_from_elem_vars(&elem_vars, &new_row);
                new_row.insert(pvar.clone(), path_val);
            }
            result.push(new_row);
        } else {
            // Pattern found — apply ON MATCH SET to each match.
            for mut match_row in matches {
                // Merge outer row.
                for (k, v) in &outer_row {
                    match_row.entry(k.clone()).or_insert_with(|| v.clone());
                }
                if !on_match.is_empty() {
                    let mut ctx = merged_params.clone();
                    for (k, v) in &match_row {
                        ctx.insert(k.clone(), v.to_json());
                    }
                    // Re-execute set on the actual matched row.
                    let set_rows = exec_set_prop(
                        &crate::cypher::planner::LogicalPlan::SingleRow,
                        on_match,
                        &ctx,
                    )?;
                    if let Some(set_row) = set_rows.into_iter().next() {
                        for (k, v) in set_row {
                            match_row.insert(k, v);
                        }
                    }
                }
                // Build path value if pattern has a path variable.
                if let Some(ref pvar) = pattern.variable {
                    let elem_vars: Vec<String> = pattern.elements.iter().map(|e| match e {
                        PatternElement::Node(n) => n.variable.clone().unwrap_or_else(|| "_anon_n0".to_string()),
                        PatternElement::Relationship(r) => r.variable.clone().unwrap_or_else(|| "_anon_r".to_string()),
                    }).collect();
                    let path_val = build_path_from_elem_vars(&elem_vars, &match_row);
                    match_row.insert(pvar.clone(), path_val);
                }
                result.push(match_row);
            }
        }
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Write-clause helpers
// ---------------------------------------------------------------------------

use crate::storage::node_store::NodeRecord;

fn load_node(node_id: i64) -> Result<NodeRecord, ExecError> {
    use crate::storage::node_store;
    let rec = unsafe {
        let rel = crate::open_nodes_relation();
        let snapshot = pgrx::pg_sys::GetActiveSnapshot();
        let r = node_store::find_node_by_id(rel, node_id, snapshot);
        pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
        r
    };
    rec.ok_or_else(|| ExecError { message: format!("node {} not found", node_id) })
}

fn node_id_from_row(row: &Row, var: &str) -> Result<i64, ExecError> {
    node_id_from_row_or_params(row, var, &HashMap::new())
}

fn node_id_from_row_or_params(row: &Row, var: &str, params: &HashMap<String, serde_json::Value>) -> Result<i64, ExecError> {
    let val = row.get(var)
        .cloned()
        .or_else(|| params.get(var).map(json_to_value));
    match val {
        Some(Value::Node { node_id, .. }) => Ok(node_id),
        Some(Value::Int(id)) => Ok(id),
        Some(other) => Err(ExecError { message: format!("variable {var} is not a node (got {:?})", other) }),
        None => Err(ExecError { message: format!("variable {var} not in scope") }),
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (Value::Str(a), Value::Str(b)) => a == b,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            _ => false,
        }
    }
}

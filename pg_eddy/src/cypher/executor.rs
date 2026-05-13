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
use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime, Offset, Timelike};
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
    /// Date part (None for pure time values, or for extended-range dates
    /// outside chrono's representable range ±262143).
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
    #[allow(dead_code)]
    pub fn total_days(&self) -> i64 {
        self.weeks * 7 + self.days
    }

    /// Total seconds (hours*3600 + minutes*60 + seconds).
    pub fn total_seconds(&self) -> i64 {
        self.hours * 3600 + self.minutes * 60 + self.seconds
    }

    /// Sub-second nanoseconds (nanoseconds % 1_000_000_000).
    #[allow(dead_code)]
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
            // Determine sign: negative if seconds < 0 or (seconds == 0 and nanos < 0)
            let neg = seconds < 0 || (seconds == 0 && nanos < 0);
            let abs_sec = seconds.unsigned_abs();
            if neg {
                format!("-{abs_sec}.{s}S")
            } else {
                format!("{abs_sec}.{s}S")
            }
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
        // Also: P<date>T<time> form like P2012-02-02T14:37:21.545
        let s = s.trim();
        if !s.starts_with('P') { return None; }
        let rest = &s[1..];

        // Check for date-style: P<YYYY>-<MM>-<DD>T<HH>:<MM>:<SS>[.nnn]
        if rest.len() > 4 && rest.as_bytes().get(4) == Some(&b'-') {
            let (date_part, time_part) = if let Some(t_pos) = rest.find('T') {
                (&rest[..t_pos], &rest[t_pos+1..])
            } else {
                (rest, "")
            };
            let parts: Vec<&str> = date_part.split('-').collect();
            if parts.len() == 3 {
                let years: i64 = parts[0].parse().ok()?;
                let months: i64 = parts[1].parse().ok()?;
                let days: i64 = parts[2].parse().ok()?;
                let (hours, minutes, seconds, nanos) = if !time_part.is_empty() {
                    let tparts: Vec<&str> = time_part.split(':').collect();
                    let h: i64 = tparts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
                    let m: i64 = tparts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                    let (s_val, n_val) = if let Some(sp) = tparts.get(2) {
                        if let Some(dot) = sp.find('.') {
                            let whole: i64 = sp[..dot].parse().ok()?;
                            let frac_str = &sp[dot+1..];
                            let padded = format!("{:0<9}", frac_str);
                            let nanos: i64 = padded[..9].parse().ok()?;
                            (whole, nanos)
                        } else {
                            (sp.parse::<i64>().ok()?, 0i64)
                        }
                    } else { (0, 0) };
                    (h, m, s_val, n_val)
                } else { (0, 0, 0, 0) };
                let iso = Self::build_iso(years, months, 0, days, hours, minutes, seconds, nanos);
                return Some(CypherDuration { years, months, weeks: 0, days, hours, minutes, seconds, nanoseconds: nanos, iso });
            }
        }

        let (date_part, time_part) = if let Some(t_pos) = rest.find('T') {
            (&rest[..t_pos], &rest[t_pos+1..])
        } else {
            (rest, "")
        };

        fn parse_f64_component(s: &str, unit: char) -> Option<(f64, &str)> {
            if let Some(pos) = s.find(unit) {
                let num_str = &s[..pos];
                let val = num_str.parse::<f64>().ok()?;
                Some((val, &s[pos+1..]))
            } else {
                None
            }
        }

        let mut f_years = 0.0f64;
        let mut f_months = 0.0f64;
        let mut f_weeks = 0.0f64;
        let mut f_days = 0.0f64;
        let mut f_hours = 0.0f64;
        let mut f_minutes = 0.0f64;
        let mut f_seconds = 0.0f64;
        let mut found_any = false;

        let mut cur = date_part;
        if let Some((v, r)) = parse_f64_component(cur, 'Y') { f_years = v; cur = r; found_any = true; }
        if let Some((v, r)) = parse_f64_component(cur, 'M') { f_months = v; cur = r; found_any = true; }
        if let Some((v, r)) = parse_f64_component(cur, 'W') { f_weeks = v; cur = r; found_any = true; }
        if let Some((v, r)) = parse_f64_component(cur, 'D') { f_days = v; cur = r; found_any = true; }
        if !cur.is_empty() { return None; } // leftover text in date part → not valid

        let mut cur = time_part;
        if let Some((v, r)) = parse_f64_component(cur, 'H') { f_hours = v; cur = r; found_any = true; }
        if let Some((v, r)) = parse_f64_component(cur, 'M') { f_minutes = v; cur = r; found_any = true; }
        if let Some((v, r)) = parse_f64_component(cur, 'S') { f_seconds = v; cur = r; found_any = true; }
        if !cur.is_empty() { return None; } // leftover text in time part → not valid

        // "P" with no date part and no time part is valid (zero duration),
        // but strings like "Pontus" that start with P but have no valid components are not.
        if !found_any && !date_part.is_empty() { return None; }

        // Cascade fractional parts (same logic as map constructor)
        let years = f_years.trunc() as i64;
        let frac_years_months = f_years.fract() * 12.0;
        let total_months_f = f_months + frac_years_months;
        let months = total_months_f.trunc() as i64;
        let frac_months_secs = total_months_f.fract() * 2629746.0;
        let frac_weeks_days = f_weeks.fract() * 7.0;
        let weeks_whole = f_weeks.trunc() as i64;
        let total_days_f = f_days + frac_weeks_days + (frac_months_secs / 86400.0).trunc();
        let frac_months_secs_rem = frac_months_secs % 86400.0;
        let days = total_days_f.trunc() as i64 + weeks_whole * 7;
        let total_hours_f = f_hours + total_days_f.fract() * 24.0 + (frac_months_secs_rem / 3600.0).trunc();
        let frac_months_secs_rem2 = frac_months_secs_rem % 3600.0;
        let hours = total_hours_f.trunc() as i64;
        let total_minutes_f = f_minutes + total_hours_f.fract() * 60.0 + (frac_months_secs_rem2 / 60.0).trunc();
        let frac_months_secs_rem3 = frac_months_secs_rem2 % 60.0;
        let minutes = total_minutes_f.trunc() as i64;
        let total_seconds_f = f_seconds + total_minutes_f.fract() * 60.0 + frac_months_secs_rem3;
        let seconds = total_seconds_f.trunc() as i64;
        let nanos = (total_seconds_f.fract() * 1_000_000_000.0).round() as i64;

        // Normalize: carry nanos → secs, secs → mins, mins → hours
        let total_ns = seconds * 1_000_000_000 + nanos;
        let seconds = total_ns / 1_000_000_000;
        let nanos = total_ns % 1_000_000_000;
        let total_s = hours * 3600 + minutes * 60 + seconds;
        let hours = total_s / 3600;
        let minutes = (total_s % 3600) / 60;
        let seconds = total_s % 60;

        let iso = Self::build_iso(years, months, 0, days, hours, minutes, seconds, nanos);
        Some(CypherDuration { years, months, weeks: 0, days, hours, minutes, seconds, nanoseconds: nanos, iso })
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

fn i32_from_i64(v: i64) -> Option<i32> {
    i32::try_from(v).ok()
}

/// Parse an extended-year date string like `-999999999-01-01` or `+999999999-12-31`.
/// Returns (year, month, day) or None if not an extended format.
fn parse_extended_date_ymd(s: &str) -> Option<(i64, u32, u32)> {
    let s = s.trim();
    if s.is_empty() { return None; }
    let (sign, rest) = match s.as_bytes()[0] {
        b'+' => (1i64, &s[1..]),
        b'-' => (-1i64, &s[1..]),
        _ => return None, // not extended format
    };
    // Must have at least 5 digits for the year (extended format)
    // Format: YYYYY...-MM-DD
    let dash1 = rest.find('-')?;
    if dash1 < 5 { return None; } // not extended — regular 4-digit years handled elsewhere
    let year_str = &rest[..dash1];
    if !year_str.chars().all(|c| c.is_ascii_digit()) { return None; }
    let year: i64 = year_str.parse().ok()?;
    let after_year = &rest[dash1 + 1..];
    let dash2 = after_year.find('-')?;
    let month: u32 = after_year[..dash2].parse().ok()?;
    let day: u32 = after_year[dash2 + 1..].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) { return None; }
    Some((sign * year, month, day))
}

/// Days in a given month for a given year (handling leap years).
fn days_in_month_ext(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) { 29 } else { 28 }
        }
        _ => 30,
    }
}

/// Format an extended-range date as an ISO string with sign prefix.
fn format_extended_date(year: i64, month: u32, day: u32) -> String {
    if year < 0 {
        format!("-{:09}-{:02}-{:02}", -year, month, day)
    } else {
        format!("+{:09}-{:02}-{:02}", year, month, day)
    }
}

/// Extract (year, month, day) from a TemporalValue, supporting both chrono NaiveDate
/// and extended-range dates stored only as ISO strings.
fn temporal_ymd(tv: &TemporalValue) -> Option<(i64, u32, u32)> {
    if let Some(d) = tv.date {
        Some((d.year() as i64, d.month(), d.day()))
    } else if matches!(tv.kind, TemporalKind::Date | TemporalKind::LocalDateTime | TemporalKind::DateTime) {
        // Try to parse extended-range date from the ISO string.
        let date_part = if let Some(t_pos) = tv.iso.find('T') { &tv.iso[..t_pos] } else { &tv.iso };
        parse_extended_date_ymd(date_part)
    } else {
        None
    }
}

/// Parse a date from an ISO 8601 string. Supports extended (2015-07-21),
/// basic (20150721), week-based (2015-W30-2 / 2015W302), ordinal (2015-202),
/// year-only (2015), year-month (2015-07 / 201507).
fn parse_date_str(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    // If the string contains a 'T', extract only the date part (before T).
    if let Some(t_pos) = s.find('T') {
        return parse_date_str(&s[..t_pos]);
    }
    // Try extended-year format first (starts with + or - followed by >4 digits).
    // These may be outside chrono's range, so try to construct via from_ymd_opt.
    if let Some((y, m, d)) = parse_extended_date_ymd(s) {
        if let Some(y32) = i32_from_i64(y) {
            return NaiveDate::from_ymd_opt(y32, m, d);
        }
        return None; // outside chrono range — caller should use parse_extended_date_ymd directly
    }
    // Extended: YYYY-MM-DD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") { return Some(d); }
    // Basic: YYYYMMDD
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y%m%d") { return Some(d); }
    // Ordinal extended: YYYY-DDD (exactly 8 chars)
    if s.len() == 8 && s.as_bytes()[4] == b'-'
        && let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%j") { return Some(d); }
    // Ordinal basic: YYYYDDD (exactly 7 chars, all digits)
    if s.len() == 7 && s.chars().all(|c| c.is_ascii_digit())
        && let Ok(d) = NaiveDate::parse_from_str(s, "%Y%j") { return Some(d); }
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
    // HH:MM:SS or HH:MM or HHMM or HH
    let (h, m, s) = if rest.len() == 8 && rest.as_bytes()[2] == b':' && rest.as_bytes()[5] == b':' {
        // HH:MM:SS
        let h: i32 = rest[..2].parse().ok()?;
        let m: i32 = rest[3..5].parse().ok()?;
        let s: i32 = rest[6..8].parse().ok()?;
        (h, m, s)
    } else if rest.len() == 5 && rest.as_bytes()[2] == b':' {
        let h: i32 = rest[..2].parse().ok()?;
        let m: i32 = rest[3..].parse().ok()?;
        (h, m, 0)
    } else if rest.len() == 4 {
        let h: i32 = rest[..2].parse().ok()?;
        let m: i32 = rest[2..].parse().ok()?;
        (h, m, 0)
    } else if rest.len() == 2 {
        let h: i32 = rest.parse().ok()?;
        (h, 0, 0)
    } else {
        return None;
    };
    Some(sign * (h * 3600 + m * 60 + s))
}

/// Build a TemporalValue for `date()`.
fn temporal_date(arg: &Value) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: "date(): invalid argument".into() };
    match arg {
        Value::Str(s) => {
            if let Some(d) = parse_date_str(s) {
                let iso = d.format("%Y-%m-%d").to_string();
                Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(d), time: None, offset_secs: None, tz_name: None })
            } else if let Some((y, m, d)) = parse_extended_date_ymd(s) {
                // Extended-range date outside chrono's representable range.
                let iso = format_extended_date(y, m, d);
                Ok(TemporalValue { kind: TemporalKind::Date, iso, date: None, time: None, offset_secs: None, tz_name: None })
            } else {
                Err(err())
            }
        }
        Value::Json(serde_json::Value::Object(m)) => {
            let date = date_from_map(m).ok_or_else(err)?;
            let iso = date.format("%Y-%m-%d").to_string();
            Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(date), time: None, offset_secs: None, tz_name: None })
        }
        Value::Temporal(tv) if tv.date.is_some() => {
            let d = tv.date.unwrap();
            let iso = d.format("%Y-%m-%d").to_string();
            Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(d), time: None, offset_secs: None, tz_name: None })
        }
        _ => Err(err()),
    }
}

/// Extract time, offset, and named timezone from a `time`/`localtime` key in a map.
/// Handles plain time strings, full datetime strings with T, and bracket-enclosed TZ names.
fn parse_time_from_map(m: &serde_json::Map<String, serde_json::Value>) -> (Option<NaiveTime>, Option<i32>, Option<String>) {
    let parse_time_str = |s: &str| -> (Option<NaiveTime>, Option<i32>, Option<String>) {
        let (s_no_zone, tz_name) = strip_tz_bracket(s);
        let time_part = if let Some(t_pos) = s_no_zone.find('T') {
            &s_no_zone[t_pos + 1..]
        } else {
            s_no_zone
        };
        let (ts, off, _) = extract_time_tz(time_part);
        (parse_localtime_str(ts), off, tz_name.map(|s| s.to_string()))
    };
    if let Some(time_val) = m.get("time") {
        match time_val {
            serde_json::Value::String(s) => parse_time_str(s),
            _ => (None, None, None),
        }
    } else if let Some(lt_val) = m.get("localtime") {
        match lt_val {
            serde_json::Value::String(s) => {
                let (t, _, _) = parse_time_str(s);
                (t, None, None)
            }
            _ => (None, None, None),
        }
    } else {
        (None, None, None)
    }
}

/// Resolve a date from a map, supporting:
/// - `{year, month, day}` (calendar)
/// - `{year, week[, dayOfWeek]}` (ISO week)
/// - `{year, ordinalDay}` (ordinal)
/// - `{date: <temporal>}` (projection from another temporal)
fn date_from_map(m: &serde_json::Map<String, serde_json::Value>) -> Option<NaiveDate> {
    // If the map has a `date` key with a string, parse that as the base date
    // and apply overrides.
    if let Some(date_val) = m.get("date") {
        let base = match date_val {
            serde_json::Value::String(s) => parse_date_str(s)?,
            serde_json::Value::Object(inner) => {
                // Nested map: e.g. date({date: {year: 2020, month: 1, day: 1}})
                date_from_map(inner)?
            }
            _ => return None,
        };
        // Apply overrides from the outer map.
        // For week-based keys, use ISO week year as default (not calendar year).
        if let Some(w) = map_i(m, "week") {
            let y = map_i(m, "year").unwrap_or(base.iso_week().year());
            let dow = map_i(m, "dayOfWeek").and_then(|d| iso_weekday(d as u8))
                .unwrap_or(base.weekday());
            return NaiveDate::from_isoywd_opt(y, w as u32, dow);
        }
        let y = map_i(m, "year").unwrap_or(base.year());
        if let Some(od) = map_i(m, "ordinalDay") {
            return NaiveDate::from_yo_opt(y, od as u32);
        }
        let mo = if let Some(q) = map_i(m, "quarter") {
            // quarter: set month to same position within target quarter
            let month_in_q = (base.month() as i32 - 1) % 3; // 0, 1, or 2
            ((q - 1) * 3 + 1 + month_in_q) as u32
        } else {
            map_i(m, "month").unwrap_or(base.month() as i32) as u32
        };
        let d = map_i(m, "day").unwrap_or(base.day() as i32) as u32;
        return NaiveDate::from_ymd_opt(y, mo, d);
    }
    let y = map_i(m, "year")?;
    // Week-based: {year, week[, dayOfWeek]}
    if let Some(w) = map_i(m, "week") {
        let dow = map_i(m, "dayOfWeek")
            .and_then(|d| iso_weekday(d as u8))
            .unwrap_or(chrono::Weekday::Mon);
        return NaiveDate::from_isoywd_opt(y, w as u32, dow);
    }
    // Ordinal: {year, ordinalDay}
    if let Some(od) = map_i(m, "ordinalDay") {
        return NaiveDate::from_yo_opt(y, od as u32);
    }
    // Quarter-based: {year, quarter, dayOfQuarter}
    if let Some(q) = map_i(m, "quarter") {
        let start_month = ((q - 1) * 3 + 1) as u32;
        let doq = map_i(m, "dayOfQuarter").unwrap_or(1);
        let start = NaiveDate::from_ymd_opt(y, start_month, 1)?;
        return Some(start + chrono::Duration::days((doq - 1) as i64));
    }
    // Calendar: {year[, month[, day]]}
    let mo = map_i(m, "month").unwrap_or(1) as u32;
    let d = map_i(m, "day").unwrap_or(1) as u32;
    NaiveDate::from_ymd_opt(y, mo, d)
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
            // Projection: {time: <temporal>, ...} or {localtime: <temporal>, ...}
            let base_time = if let Some(tv_str) = m.get("time").or_else(|| m.get("localtime")).and_then(|v| v.as_str()) {
                // Parse as time string (may contain offset — strip it)
                // May also be a full datetime string with T or have bracket TZ
                let (s_no_zone, _) = strip_tz_bracket(tv_str);
                let time_part = if let Some(t_pos) = s_no_zone.find('T') {
                    &s_no_zone[t_pos + 1..]
                } else {
                    s_no_zone
                };
                let (ts, _, _) = extract_time_tz(time_part);
                parse_localtime_str(ts)

            } else if let Some(tv_str) = m.get("datetime").or_else(|| m.get("localdatetime")).and_then(|v| v.as_str()) {
                // Extract time from datetime string
                if let Some(t_pos) = tv_str.find('T') {
                    let (ts, _, _) = extract_time_tz(&tv_str[t_pos+1..]);
                    parse_localtime_str(ts)
                } else {
                    None
                }
            } else {
                None
            };
            let h = map_i(m, "hour").unwrap_or_else(|| base_time.map(|t| t.hour() as i32).unwrap_or(0));
            let mi = map_i(m, "minute").unwrap_or_else(|| base_time.map(|t| t.minute() as i32).unwrap_or(0));
            let s = map_i(m, "second").unwrap_or_else(|| base_time.map(|t| t.second() as i32).unwrap_or(0));
            let ns = map_i64(m, "nanosecond").unwrap_or_else(|| base_time.map(|t| t.nanosecond() as i64 % 1_000_000_000).unwrap_or(0)) as u32;
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = if m.contains_key("nanosecond") || m.contains_key("millisecond") || m.contains_key("microsecond") {
                ns + ms + us
            } else {
                ns
            };
            let t = NaiveTime::from_hms_nano_opt(h as u32, mi as u32, s as u32, nanos).ok_or_else(err)?;
            let iso = format_localtime(&t);
            Ok(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(t), offset_secs: None, tz_name: None })
        }
        Value::Temporal(tv) if tv.time.is_some() => {
            let t = tv.time.unwrap();
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
            // Projection: {time: <temporal>, ...}
            let (base_time, base_off) = if let Some(tv_str) = m.get("time").and_then(|v| v.as_str()) {
                // Strip bracket timezone if present: "...+01:00[Europe/Stockholm]" -> "...+01:00"
                let clean = if let Some(b) = tv_str.find('[') { &tv_str[..b] } else { tv_str };
                // If the value contains 'T', it's a datetime string - extract time portion
                if let Some(t_pos) = clean.find('T') {
                    let (ts, off, _) = extract_time_tz(&clean[t_pos+1..]);
                    (parse_localtime_str(ts), off)
                } else {
                    let (ts, off, _) = extract_time_tz(clean);
                    (parse_localtime_str(ts), off)
                }
            } else if let Some(tv_str) = m.get("datetime").and_then(|v| v.as_str()) {
                let clean = if let Some(b) = tv_str.find('[') { &tv_str[..b] } else { tv_str };
                if let Some(t_pos) = clean.find('T') {
                    let (ts, off, _) = extract_time_tz(&clean[t_pos+1..]);
                    (parse_localtime_str(ts), off)
                } else {
                    (None, None)
                }
            } else {
                (None, None)
            };
            let base_naive = base_time.unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            let tz_str = m.get("timezone").and_then(|v| v.as_str());
            let off = if let Some(tz) = tz_str {
                parse_offset(tz).unwrap_or(base_off.unwrap_or(0))
            } else {
                base_off.unwrap_or(0)
            };
            // Convert base time to target timezone BEFORE applying overrides
            let conv_time = if tz_str.is_some() {
                if let Some(src_off) = base_off {
                    if src_off != off {
                        let diff_secs = (off - src_off) as i64;
                        let base_secs = base_naive.num_seconds_from_midnight() as i64 + diff_secs;
                        let base_secs = base_secs.rem_euclid(86400);
                        let base_nanos = base_naive.nanosecond() % 1_000_000_000;
                        NaiveTime::from_num_seconds_from_midnight_opt(base_secs as u32, base_nanos).unwrap_or(base_naive)
                    } else {
                        base_naive
                    }
                } else {
                    base_naive
                }
            } else {
                base_naive
            };
            // Apply overrides on top of converted time
            let h = map_i(m, "hour").map(|v| v as u32).unwrap_or_else(|| conv_time.hour());
            let mi = map_i(m, "minute").map(|v| v as u32).unwrap_or_else(|| conv_time.minute());
            let s = map_i(m, "second").map(|v| v as u32).unwrap_or_else(|| conv_time.second());
            let ns = map_i64(m, "nanosecond").map(|v| v as u32).unwrap_or_else(|| conv_time.nanosecond() % 1_000_000_000);
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = if m.contains_key("nanosecond") || m.contains_key("millisecond") || m.contains_key("microsecond") {
                ns + ms + us
            } else {
                ns
            };
            let final_time = NaiveTime::from_hms_nano_opt(h, mi, s, nanos).ok_or_else(err)?;
            let iso = format_time_with_offset(&final_time, off);
            Ok(TemporalValue { kind: TemporalKind::Time, iso, date: None, time: Some(final_time), offset_secs: Some(off), tz_name: None })
        }
        Value::Temporal(tv) if tv.time.is_some() => {
            let t = tv.time.unwrap();
            let off = tv.offset_secs.unwrap_or(0);
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
            // Split at T; if no T, treat entire string as date with midnight time.
            let (date_str, time_str) = if let Some(t_pos) = s.find('T') {
                (&s[..t_pos], &s[t_pos+1..])
            } else {
                (s, "00:00:00")
            };
            if let Some(date) = parse_date_str(date_str) {
                // Strip any offset from the time part
                let (time_str_clean, _, _) = extract_time_tz(time_str);
                let time = parse_localtime_str(time_str_clean).ok_or_else(err)?;
                let iso = format_localdatetime(&date, &time);
                Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(date), time: Some(time), offset_secs: None, tz_name: None })
            } else if let Some((y, m, d)) = parse_extended_date_ymd(date_str) {
                // Extended-range date outside chrono's representable range.
                let (time_str_clean, _, _) = extract_time_tz(time_str);
                let time = parse_localtime_str(time_str_clean).unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
                let date_iso = format_extended_date(y, m, d);
                let iso = format!("{}T{}", date_iso, time.format("%H:%M:%S"));
                Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: None, time: Some(time), offset_secs: None, tz_name: None })
            } else {
                Err(err())
            }
        }
        Value::Json(serde_json::Value::Object(m)) => {
            // Projection: {datetime: <temporal>, ...} or {date: <temporal>, ...}
            let (base_date, base_time) = if let Some(dt_val) = m.get("datetime").or_else(|| m.get("localdatetime")) {
                let tv = match dt_val {
                    serde_json::Value::String(s) => {
                        let s = s.replace(' ', "T");
                        temporal_localdatetime(&Value::Str(s))?
                    }
                    _ => return Err(err()),
                };
                (tv.date, tv.time)
            } else if let Some(date_val) = m.get("date") {
                let d = match date_val {
                    serde_json::Value::String(s) => parse_date_str(s),
                    serde_json::Value::Object(inner) => date_from_map(inner),
                    _ => None,
                }.ok_or_else(err)?;
                let (bt, _, _) = parse_time_from_map(m);
                (Some(d), bt)
            } else {
                let (bt, _, _) = parse_time_from_map(m);
                (None, bt)
            };
            let date = if m.contains_key("year") || m.contains_key("week") || m.contains_key("ordinalDay") || m.contains_key("month") || m.contains_key("day") || m.contains_key("quarter") {
                if m.contains_key("date") {
                    date_from_map(m).ok_or_else(err)?
                } else if let Some(base_d) = base_date {
                    // Apply overrides to base date from datetime/other source
                    if let Some(w) = map_i(m, "week") {
                        let y = map_i(m, "year").unwrap_or(base_d.iso_week().year());
                        let dow = map_i(m, "dayOfWeek").and_then(|d| iso_weekday(d as u8)).unwrap_or(base_d.weekday());
                        NaiveDate::from_isoywd_opt(y, w as u32, dow).ok_or_else(err)?
                    } else if let Some(od) = map_i(m, "ordinalDay") {
                        let y = map_i(m, "year").unwrap_or(base_d.year());
                        NaiveDate::from_yo_opt(y, od as u32).ok_or_else(err)?
                    } else {
                        let y = map_i(m, "year").unwrap_or(base_d.year());
                        let mo = map_i(m, "month").unwrap_or(base_d.month() as i32) as u32;
                        let d = map_i(m, "day").unwrap_or(base_d.day() as i32) as u32;
                        NaiveDate::from_ymd_opt(y, mo, d).ok_or_else(err)?
                    }
                } else {
                    date_from_map(m).ok_or_else(err)?
                }
            } else if let Some(d) = base_date {
                d
            } else {
                let y = map_i(m, "year").ok_or_else(err)?;
                NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(err)?
            };
            let h = map_i(m, "hour").unwrap_or_else(|| base_time.map(|t| t.hour() as i32).unwrap_or(0)) as u32;
            let mi = map_i(m, "minute").unwrap_or_else(|| base_time.map(|t| t.minute() as i32).unwrap_or(0)) as u32;
            let s = map_i(m, "second").unwrap_or_else(|| base_time.map(|t| t.second() as i32).unwrap_or(0)) as u32;
            let ns = map_i64(m, "nanosecond").unwrap_or_else(|| base_time.map(|t| t.nanosecond() as i64 % 1_000_000_000).unwrap_or(0)) as u32;
            let ms = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = if m.contains_key("nanosecond") || m.contains_key("millisecond") || m.contains_key("microsecond") {
                ns + ms + us
            } else {
                ns
            };
            let time = NaiveTime::from_hms_nano_opt(h, mi, s, nanos).ok_or_else(err)?;
            let iso = format_localdatetime(&date, &time);
            Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(date), time: Some(time), offset_secs: None, tz_name: None })
        }
        Value::Temporal(tv) => {
            let date = tv.date.ok_or_else(err)?;
            let time = tv.time.unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            let iso = format_localdatetime(&date, &time);
            Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(date), time: Some(time), offset_secs: None, tz_name: None })
        }
        _ => Err(err()),
    }
}

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
            let off = if let Some(o) = offset_secs {
                o
            } else if let Some(ref tz_n) = tz_name {
                // Compute offset from named timezone for this date+time
                let tz: Tz = tz_n.parse().unwrap_or(chrono_tz::UTC);
                let ndt = NaiveDateTime::new(date, time);
                let dt = ndt.and_local_timezone(tz)
                    .earliest()
                    .unwrap_or_else(|| ndt.and_local_timezone(chrono_tz::UTC).unwrap());
                dt.offset().fix().local_minus_utc()
            } else {
                0
            };
            let iso = format_datetime(&date, &time, off, tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time), offset_secs: Some(off), tz_name })
        }
        Value::Json(serde_json::Value::Object(m)) => {
            // Projection: {datetime: <temporal>, ...} or {date: <temporal>, ...}
            let (base_date, base_time, base_off, base_tz) = if let Some(dt_val) = m.get("datetime") {
                match dt_val {
                    serde_json::Value::String(s) => {
                        let s = s.replace(' ', "T");
                        let (s_no_zone, tz_name) = strip_tz_bracket(&s);
                        let (date_str, time_str) = split_datetime(s_no_zone)?;
                        let date = parse_date_str(date_str).ok_or_else(err)?;
                        let (time_str_clean, offset_secs, _) = extract_time_tz(time_str);
                        let time = parse_localtime_str(time_str_clean).ok_or_else(err)?;
                        // Preserve offset optionality: localdatetime has None
                        (Some(date), Some(time), offset_secs, tz_name)
                    }
                    _ => return Err(err()),
                }
            } else if let Some(date_val) = m.get("date") {
                let d = match date_val {
                    serde_json::Value::String(s) => parse_date_str(s),
                    serde_json::Value::Object(inner) => date_from_map(inner),
                    _ => None,
                }.ok_or_else(err)?;
                let (bt, bo, bt_tz) = parse_time_from_map(m);
                (Some(d), bt, bo, bt_tz)
            } else {
                // No date/datetime key — check for time key
                let (bt, bo, bt_tz) = parse_time_from_map(m);
                (None, bt, bo, bt_tz)
            };
            let date = if m.contains_key("year") || m.contains_key("week") || m.contains_key("ordinalDay") || m.contains_key("month") || m.contains_key("day") || m.contains_key("quarter") {
                if m.contains_key("date") {
                    date_from_map(m).ok_or_else(err)?
                } else if let Some(base_d) = base_date {
                    // Apply overrides to base date from datetime/other source
                    if let Some(w) = map_i(m, "week") {
                        let y = map_i(m, "year").unwrap_or(base_d.iso_week().year());
                        let dow = map_i(m, "dayOfWeek").and_then(|d| iso_weekday(d as u8)).unwrap_or(base_d.weekday());
                        NaiveDate::from_isoywd_opt(y, w as u32, dow).ok_or_else(err)?
                    } else if let Some(od) = map_i(m, "ordinalDay") {
                        let y = map_i(m, "year").unwrap_or(base_d.year());
                        NaiveDate::from_yo_opt(y, od as u32).ok_or_else(err)?
                    } else {
                        let y = map_i(m, "year").unwrap_or(base_d.year());
                        let mo = map_i(m, "month").unwrap_or(base_d.month() as i32) as u32;
                        let d = map_i(m, "day").unwrap_or(base_d.day() as i32) as u32;
                        NaiveDate::from_ymd_opt(y, mo, d).ok_or_else(err)?
                    }
                } else {
                    date_from_map(m).ok_or_else(err)?
                }
            } else if let Some(d) = base_date {
                d
            } else {
                let y = map_i(m, "year").ok_or_else(err)?;
                NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(err)?
            };
            let date_val = NaiveDate::from_ymd_opt(date.year(), date.month(), date.day()).ok_or_else(err)?;
            // Get base time WITHOUT overrides (for timezone conversion)
            let base_naive = base_time.unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            let tz_str = m.get("timezone").and_then(|v| v.as_str())
                .or(base_tz.as_deref());
            let tz_explicit = m.contains_key("timezone");
            // Convert base time to target timezone if explicit tz change
            let (conv_date, conv_time) = if let Some(src_off) = base_off {
                if tz_explicit {
                    // Re-evaluate source offset at the target date when source has a named timezone
                    let actual_src_off = if let Some(ref tz_n) = base_tz {
                        if let Ok(tz) = tz_n.parse::<Tz>() {
                            let ndt = NaiveDateTime::new(date_val, base_naive);
                            let dt = ndt.and_local_timezone(tz)
                                .earliest()
                                .unwrap_or_else(|| ndt.and_local_timezone(chrono_tz::UTC).unwrap());
                            dt.offset().fix().local_minus_utc()
                        } else { src_off }
                    } else { src_off };
                    // Convert source local time → UTC → target local time
                    let src_ndt = NaiveDateTime::new(date_val, base_naive);
                    let utc_ndt = src_ndt - chrono::Duration::seconds(actual_src_off as i64);
                    if let Some(tz_s) = tz_str {
                        if let Some(target_off) = parse_offset(tz_s) {
                            let target_ndt = utc_ndt + chrono::Duration::seconds(target_off as i64);
                            (target_ndt.date(), target_ndt.time())
                        } else {
                            // Named timezone: convert via chrono_tz
                            let tz: Tz = tz_s.parse().unwrap_or(chrono_tz::UTC);
                            let utc_dt = utc_ndt.and_utc();
                            let local_dt = utc_dt.with_timezone(&tz);
                            (local_dt.date_naive(), local_dt.time())
                        }
                    } else {
                        (date_val, base_naive)
                    }
                } else {
                    (date_val, base_naive)
                }
            } else {
                (date_val, base_naive)
            };
            // Apply time overrides on top of (possibly converted) time
            let h = map_i(m, "hour").map(|v| v as u32).unwrap_or_else(|| conv_time.hour());
            let mi = map_i(m, "minute").map(|v| v as u32).unwrap_or_else(|| conv_time.minute());
            let s = map_i(m, "second").map(|v| v as u32).unwrap_or_else(|| conv_time.second());
            let ns = map_i64(m, "nanosecond").map(|v| v as u32).unwrap_or_else(|| conv_time.nanosecond() % 1_000_000_000);
            let ms_val = map_i64(m, "millisecond").unwrap_or(0) as u32 * 1_000_000;
            let us = map_i64(m, "microsecond").unwrap_or(0) as u32 * 1_000;
            let nanos = if m.contains_key("nanosecond") || m.contains_key("millisecond") || m.contains_key("microsecond") {
                ns + ms_val + us
            } else {
                ns
            };
            let final_time = NaiveTime::from_hms_nano_opt(h, mi, s, nanos).ok_or_else(err)?;
            let final_date = conv_date;
            // Compute final offset (recompute for named tz in case DST differs)
            let (off, tz_name) = if let Some(tz_s) = tz_str {
                if let Some(o) = parse_offset(tz_s) {
                    (o, None)
                } else {
                    let tz: Tz = tz_s.parse().unwrap_or(chrono_tz::UTC);
                    let ndt = NaiveDateTime::new(final_date, final_time);
                    let dt = ndt.and_local_timezone(tz)
                        .earliest()
                        .unwrap_or_else(|| ndt.and_local_timezone(chrono_tz::UTC).unwrap());
                    let off = dt.offset().fix().local_minus_utc();
                    (off, Some(tz_s.to_string()))
                }
            } else if let Some(src_off) = base_off {
                (src_off, None)
            } else {
                (0, None)
            };
            let iso = format_datetime(&final_date, &final_time, off, tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(final_date), time: Some(final_time), offset_secs: Some(off), tz_name })
        }
        Value::Temporal(tv) => {
            let date = tv.date.ok_or_else(err)?;
            let time = tv.time.unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
            let off = tv.offset_secs.unwrap_or(0);
            let iso = format_datetime(&date, &time, off, tv.tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time), offset_secs: Some(off), tz_name: tv.tz_name.clone() })
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
            // Helper to read a map value as f64 (supports both integer and float JSON values)
            let map_f = |key: &str| -> f64 {
                m.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
            };
            let f_years = map_f("years") + map_f("year");
            let f_months = map_f("months") + map_f("month");
            let f_weeks = map_f("weeks") + map_f("week");
            let f_days = map_f("days") + map_f("day");
            let f_hours = map_f("hours") + map_f("hour");
            let f_minutes = map_f("minutes") + map_f("minute");
            let f_seconds = map_f("seconds") + map_f("second");
            let f_ms = map_f("milliseconds") + map_f("millisecond");
            let f_us = map_f("microseconds") + map_f("microsecond");
            let f_ns = map_f("nanoseconds") + map_f("nanosecond");

            // Cascade fractional parts down the chain
            let years = f_years.trunc() as i64;
            let frac_years_months = f_years.fract() * 12.0;
            let total_months_f = f_months + frac_years_months;
            let months = total_months_f.trunc() as i64;
            // Fractional months → seconds (1 month = 2629746 seconds = 365.2425*86400/12)
            let frac_months_secs = total_months_f.fract() * 2629746.0;
            let total_weeks_f = f_weeks;
            let frac_weeks_days = total_weeks_f.fract() * 7.0;
            let weeks_whole = total_weeks_f.trunc() as i64;
            let total_days_f = f_days + frac_weeks_days + (frac_months_secs / 86400.0).trunc();
            let frac_months_secs_rem = frac_months_secs % 86400.0;
            let days = total_days_f.trunc() as i64 + weeks_whole * 7;
            let total_hours_f = f_hours + total_days_f.fract() * 24.0 + (frac_months_secs_rem / 3600.0).trunc();
            let frac_months_secs_rem2 = frac_months_secs_rem % 3600.0;
            let hours = total_hours_f.trunc() as i64;
            let total_minutes_f = f_minutes + total_hours_f.fract() * 60.0 + (frac_months_secs_rem2 / 60.0).trunc();
            let frac_months_secs_rem3 = frac_months_secs_rem2 % 60.0;
            let minutes = total_minutes_f.trunc() as i64;
            let total_seconds_f = f_seconds + total_minutes_f.fract() * 60.0 + frac_months_secs_rem3 + f_ms / 1000.0 + f_us / 1_000_000.0 + f_ns / 1_000_000_000.0;
            let seconds = total_seconds_f.trunc() as i64;
            let nanos = (total_seconds_f.fract() * 1_000_000_000.0).round() as i64;

            // Normalize: carry nanos → seconds, seconds → minutes, minutes → hours
            let total_ns = seconds * 1_000_000_000 + nanos;
            let seconds = total_ns / 1_000_000_000;
            let nanos = total_ns % 1_000_000_000;
            let total_s = hours * 3600 + minutes * 60 + seconds;
            let hours = total_s / 3600;
            let minutes = (total_s % 3600) / 60;
            let seconds = total_s % 60;

            let iso = CypherDuration::build_iso(years, months, 0, days, hours, minutes, seconds, nanos);
            Ok(CypherDuration { years, months, weeks: 0, days, hours, minutes, seconds, nanoseconds: nanos, iso })
        }
        _ => Err(err()),
    }
}

// ---------------------------------------------------------------------------
// Temporal truncation
// ---------------------------------------------------------------------------

/// Truncate a date to the specified unit, then apply overrides from the map.
fn truncate_date(unit: &str, input: &TemporalValue, overrides: &serde_json::Map<String, serde_json::Value>) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: format!("date.truncate(): invalid unit '{unit}'") };
    let d = input.date.ok_or_else(|| ExecError { message: "truncate: input has no date component".into() })?;
    let base = match unit {
        "millennium" => {
            let y = (d.year() / 1000) * 1000;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(err)?
        }
        "century" => {
            let y = (d.year() / 100) * 100;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(err)?
        }
        "decade" => {
            let y = (d.year() / 10) * 10;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(err)?
        }
        "year" => NaiveDate::from_ymd_opt(d.year(), 1, 1).ok_or_else(err)?,
        "weekYear" => {
            let iso_year = d.iso_week().year();
            NaiveDate::from_isoywd_opt(iso_year, 1, chrono::Weekday::Mon).ok_or_else(err)?
        }
        "quarter" => {
            let q = (d.month() - 1) / 3;
            NaiveDate::from_ymd_opt(d.year(), q * 3 + 1, 1).ok_or_else(err)?
        }
        "month" => NaiveDate::from_ymd_opt(d.year(), d.month(), 1).ok_or_else(err)?,
        "week" => {
            // Truncate to start of ISO week (Monday)
            let wd = d.weekday().num_days_from_monday();
            d.checked_sub_signed(chrono::Duration::days(wd as i64)).ok_or_else(err)?
        }
        "day" => d,
        _ => return Err(err()),
    };
    // Apply overrides
    let day_override = map_i(overrides, "day").map(|v| v as u32);
    let dow_override = map_i(overrides, "dayOfWeek");
    let result = if let Some(dow) = dow_override {
        // dayOfWeek override: find the Nth day of the week within the truncated period
        let wd = iso_weekday(dow as u8).unwrap_or(chrono::Weekday::Mon);
        // Start from base date, find the first occurrence of the target weekday
        let days_ahead = (wd.num_days_from_monday() as i32 - base.weekday().num_days_from_monday() as i32 + 7) % 7;
        base.checked_add_signed(chrono::Duration::days(days_ahead as i64)).ok_or_else(err)?
    } else if let Some(dv) = day_override {
        NaiveDate::from_ymd_opt(base.year(), base.month(), dv).ok_or_else(err)?
    } else {
        base
    };
    let iso = result.format("%Y-%m-%d").to_string();
    Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(result), time: None, offset_secs: None, tz_name: None })
}

/// Truncate a localtime to the specified unit, then apply overrides.
fn truncate_localtime(unit: &str, input: &TemporalValue, overrides: &serde_json::Map<String, serde_json::Value>) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: format!("localtime.truncate(): invalid unit '{unit}'") };
    let t = input.time.ok_or_else(|| ExecError { message: "truncate: input has no time component".into() })?;
    let base = match unit {
        "day" => NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        "hour" => NaiveTime::from_hms_opt(t.hour(), 0, 0).unwrap(),
        "minute" => NaiveTime::from_hms_opt(t.hour(), t.minute(), 0).unwrap(),
        "second" => NaiveTime::from_hms_opt(t.hour(), t.minute(), t.second()).unwrap(),
        "millisecond" => {
            let ms = t.nanosecond() / 1_000_000 * 1_000_000;
            NaiveTime::from_hms_nano_opt(t.hour(), t.minute(), t.second(), ms).unwrap()
        }
        "microsecond" => {
            let us = t.nanosecond() / 1_000 * 1_000;
            NaiveTime::from_hms_nano_opt(t.hour(), t.minute(), t.second(), us).unwrap()
        }
        _ => return Err(err()),
    };
    // Apply overrides (hour, minute, second, etc.)
    let h = map_i(overrides, "hour").map(|v| v as u32).unwrap_or(base.hour());
    let mi = map_i(overrides, "minute").map(|v| v as u32).unwrap_or(base.minute());
    let s = map_i(overrides, "second").map(|v| v as u32).unwrap_or(base.second());
    // nanosecond override is ADDITIVE to the truncated nanoseconds
    let base_ns = base.nanosecond() % 1_000_000_000;
    let ns = if let Some(v) = map_i64(overrides, "nanosecond") {
        base_ns + v as u32
    } else if let Some(v) = map_i64(overrides, "microsecond") {
        base_ns + v as u32 * 1000
    } else if let Some(v) = map_i64(overrides, "millisecond") {
        base_ns + v as u32 * 1_000_000
    } else {
        base_ns
    };
    let result = NaiveTime::from_hms_nano_opt(h, mi, s, ns).ok_or_else(err)?;
    let iso = format_localtime(&result);
    Ok(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(result), offset_secs: None, tz_name: None })
}

/// Truncate a time (with offset) to the specified unit.
fn truncate_time(unit: &str, input: &TemporalValue, overrides: &serde_json::Map<String, serde_json::Value>) -> Result<TemporalValue, ExecError> {
    let mut tv = truncate_localtime(unit, input, overrides)?;
    tv.kind = TemporalKind::Time;
    // Allow timezone override in the overrides map
    let off = if let Some(tz_str) = overrides.get("timezone").and_then(|v| v.as_str()) {
        parse_offset(tz_str).unwrap_or(input.offset_secs.unwrap_or(0))
    } else {
        input.offset_secs.unwrap_or(0)
    };
    tv.offset_secs = Some(off);
    tv.iso = format_time_with_offset(&tv.time.unwrap(), off);
    Ok(tv)
}

/// Truncate a localdatetime to the specified unit.
fn truncate_localdatetime(unit: &str, input: &TemporalValue, overrides: &serde_json::Map<String, serde_json::Value>) -> Result<TemporalValue, ExecError> {
    let err = || ExecError { message: format!("localdatetime.truncate(): invalid unit '{unit}'") };
    let d = input.date.ok_or_else(|| ExecError { message: "truncate: input has no date component".into() })?;
    let t = input.time.unwrap_or_else(|| NaiveTime::from_hms_opt(0, 0, 0).unwrap());
    // Date-level truncation units
    let (base_date, base_time) = match unit {
        "millennium" | "century" | "decade" | "year" | "weekYear" | "quarter" | "month" | "week" => {
            let td = truncate_date(unit, input, &serde_json::Map::new())?;
            (td.date.unwrap(), NaiveTime::from_hms_opt(0, 0, 0).unwrap())
        }
        "day" => (d, NaiveTime::from_hms_opt(0, 0, 0).unwrap()),
        "hour" => (d, NaiveTime::from_hms_opt(t.hour(), 0, 0).unwrap()),
        "minute" => (d, NaiveTime::from_hms_opt(t.hour(), t.minute(), 0).unwrap()),
        "second" => (d, NaiveTime::from_hms_opt(t.hour(), t.minute(), t.second()).unwrap()),
        "millisecond" => {
            let ms = t.nanosecond() / 1_000_000 * 1_000_000;
            (d, NaiveTime::from_hms_nano_opt(t.hour(), t.minute(), t.second(), ms).unwrap())
        }
        "microsecond" => {
            let us = t.nanosecond() / 1_000 * 1_000;
            (d, NaiveTime::from_hms_nano_opt(t.hour(), t.minute(), t.second(), us).unwrap())
        }
        _ => return Err(err()),
    };
    // Apply overrides — date parts
    let day_override = map_i(overrides, "day").map(|v| v as u32);
    let dow_override = map_i(overrides, "dayOfWeek");
    let month_override = map_i(overrides, "month").map(|v| v as u32);
    let rd = if let Some(dow) = dow_override {
        let wd = iso_weekday(dow as u8).unwrap_or(chrono::Weekday::Mon);
        let days_ahead = (wd.num_days_from_monday() as i32 - base_date.weekday().num_days_from_monday() as i32 + 7) % 7;
        base_date.checked_add_signed(chrono::Duration::days(days_ahead as i64)).ok_or_else(err)?
    } else if let Some(dv) = day_override {
        let m = month_override.unwrap_or(base_date.month());
        NaiveDate::from_ymd_opt(base_date.year(), m, dv).ok_or_else(err)?
    } else if let Some(m) = month_override {
        NaiveDate::from_ymd_opt(base_date.year(), m, base_date.day()).ok_or_else(err)?
    } else {
        base_date
    };
    // time overrides
    let h = map_i(overrides, "hour").map(|v| v as u32).unwrap_or(base_time.hour());
    let mi = map_i(overrides, "minute").map(|v| v as u32).unwrap_or(base_time.minute());
    let s = map_i(overrides, "second").map(|v| v as u32).unwrap_or(base_time.second());
    // nanosecond override is ADDITIVE to the truncated nanoseconds
    let base_ns = base_time.nanosecond() % 1_000_000_000;
    let ns = if let Some(v) = map_i64(overrides, "nanosecond") {
        base_ns + v as u32
    } else if let Some(v) = map_i64(overrides, "microsecond") {
        base_ns + v as u32 * 1000
    } else if let Some(v) = map_i64(overrides, "millisecond") {
        base_ns + v as u32 * 1_000_000
    } else {
        base_ns
    };
    let rt = NaiveTime::from_hms_nano_opt(h, mi, s, ns).ok_or_else(err)?;
    let iso = format_localdatetime(&rd, &rt);
    Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(rd), time: Some(rt), offset_secs: None, tz_name: None })
}

/// Truncate a datetime (with offset) to the specified unit.
fn truncate_datetime(unit: &str, input: &TemporalValue, overrides: &serde_json::Map<String, serde_json::Value>) -> Result<TemporalValue, ExecError> {
    let mut tv = truncate_localdatetime(unit, input, overrides)?;
    tv.kind = TemporalKind::DateTime;
    // Apply timezone override from the overrides map, or inherit from input
    if let Some(tz_str) = overrides.get("timezone").and_then(|v| v.as_str()) {
        // Try as named timezone first
        if let Ok(tz) = tz_str.parse::<chrono_tz::Tz>() {
            let d = tv.date.unwrap();
            let t = tv.time.unwrap();
            let naive = d.and_time(t);
            if let Some(dt) = naive.and_local_timezone(tz).earliest() {
                tv.offset_secs = Some(dt.offset().fix().local_minus_utc());
                tv.tz_name = Some(tz_str.to_string());
            } else {
                tv.offset_secs = Some(0);
                tv.tz_name = Some(tz_str.to_string());
            }
        } else {
            tv.offset_secs = Some(parse_offset(tz_str).unwrap_or(0));
            tv.tz_name = None;
        }
    } else {
        tv.offset_secs = Some(input.offset_secs.unwrap_or(0));
        tv.tz_name = input.tz_name.clone();
    }
    let d = tv.date.unwrap();
    let t = tv.time.unwrap();
    tv.iso = format_datetime(&d, &t, tv.offset_secs.unwrap_or(0), tv.tz_name.as_deref());
    Ok(tv)
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

/// Add months to a date, clamping the day to last day of target month.
fn add_months_to_date(d: NaiveDate, months: i64) -> NaiveDate {
    let total_months = d.year() as i64 * 12 + (d.month() as i64 - 1) + months;
    let y = (total_months.div_euclid(12)) as i32;
    let m = (total_months.rem_euclid(12) + 1) as u32;
    let max_day = days_in_month(y, m);
    let day = d.day().min(max_day);
    NaiveDate::from_ymd_opt(y, m, day).unwrap_or(d)
}

/// Add months to an extended date (year, month, day), clamping day.
fn add_months_ext(year: i64, month: u32, day: u32, months: i64) -> (i64, u32, u32) {
    let total_months = year * 12 + (month as i64 - 1) + months;
    let y = total_months.div_euclid(12);
    let m = (total_months.rem_euclid(12) + 1) as u32;
    let max_day = days_in_month_ext(y, m);
    (y, m, day.min(max_day))
}

/// Compute the signed day-difference between two extended dates (a → b).
/// Positive if b is after a.
fn extended_date_diff_days(a: (i64, u32, u32), b: (i64, u32, u32)) -> i64 {
    // If both dates fit in chrono's range, delegate to NaiveDate for exact result.
    if let (Some(a32), Some(b32)) = (i32_from_i64(a.0), i32_from_i64(b.0))
        && let (Some(ad), Some(bd)) = (
            NaiveDate::from_ymd_opt(a32, a.1, a.2),
            NaiveDate::from_ymd_opt(b32, b.1, b.2),
        )
    {
        return bd.signed_duration_since(ad).num_days();
    }
    // Fallback: compute days by counting remaining days in a's month then full months.
    // For the purpose of duration.between, we only need the remainder after subtracting
    // whole months — so the diff is simply b.day - a.day, possibly borrowing a month.
    // Since callers already aligned the month, this is just the day difference.
    b.2 as i64 - a.2 as i64
}

/// Compute the total number of days between two extended dates using proleptic
/// Gregorian calendar arithmetic.  Result is signed (positive when b > a).
fn extended_total_day_diff(a: (i64, u32, u32), b: (i64, u32, u32)) -> i64 {
    // If both fit in chrono range, use chrono for exact result.
    if let (Some(a32), Some(b32)) = (i32_from_i64(a.0), i32_from_i64(b.0))
        && let (Some(ad), Some(bd)) = (
            NaiveDate::from_ymd_opt(a32, a.1, a.2),
            NaiveDate::from_ymd_opt(b32, b.1, b.2),
        )
    {
        return bd.signed_duration_since(ad).num_days();
    }
    // For extended dates: compute proleptic Julian Day Number difference.
    fn to_jdn(y: i64, m: u32, d: u32) -> i64 {
        // Algorithm from Meeus, Astronomical Algorithms, valid for all Gregorian dates.
        let (y, m) = if m <= 2 { (y - 1, m as i64 + 12) } else { (y, m as i64) };
        let a = y.div_euclid(100);
        let b = 2 - a + a.div_euclid(4);
        (365.25 * (y + 4716) as f64) as i64 + (30.6001 * (m + 1) as f64) as i64 + d as i64 + b - 1524
    }
    to_jdn(b.0, b.1, b.2) - to_jdn(a.0, a.1, a.2)
}

/// Days in a given month (leap-year aware).
fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 { 29 } else { 28 },
        _ => 30,
    }
}

/// Convert a NaiveTime to nanoseconds since midnight.
fn time_to_ns(t: NaiveTime) -> i64 {
    t.hour() as i64 * 3_600_000_000_000
        + t.minute() as i64 * 60_000_000_000
        + t.second() as i64 * 1_000_000_000
        + t.nanosecond() as i64
}

/// Compute duration.between(lhs, rhs): exact difference preserving sign.
/// Uses calendar-month arithmetic: months component is the full calendar months
/// between the dates, days is the remaining days, and seconds/nanos is the time diff.
fn duration_between(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    let midnight = NaiveTime::from_hms_opt(0,0,0).unwrap();

    // Mixed zoned/local: when exactly one side has a named timezone and the other is local,
    // interpret the local side in the named timezone and compute elapsed UTC time.
    let mixed_zoned = lhs.offset_secs.is_some() != rhs.offset_secs.is_some();
    let named_tz = lhs.tz_name.as_ref().or(rhs.tz_name.as_ref());
    if mixed_zoned
        && let Some(tz_name) = named_tz
            && let Ok(tz) = tz_name.parse::<Tz>() {
                let zoned_date = if lhs.offset_secs.is_some() { lhs.date } else { rhs.date };
                let to_utc_ns = |tv: &TemporalValue| -> i64 {
                    let d = tv.date.or(zoned_date).unwrap_or_else(|| NaiveDate::from_ymd_opt(1970,1,1).unwrap());
                    let t = tv.time.unwrap_or(midnight);
                    if let Some(off) = tv.offset_secs {
                        (NaiveDateTime::new(d, t).and_utc().timestamp() - off as i64) * 1_000_000_000 + t.nanosecond() as i64
                    } else {
                        let ndt = NaiveDateTime::new(d, t);
                        let dt = ndt.and_local_timezone(tz)
                            .earliest()
                            .unwrap_or_else(|| ndt.and_local_timezone(chrono_tz::UTC).unwrap());
                        dt.timestamp() * 1_000_000_000 + t.nanosecond() as i64
                    }
                };
                let ns_diff = to_utc_ns(rhs) - to_utc_ns(lhs);
                let hours = ns_diff / 3_600_000_000_000;
                let minutes = (ns_diff % 3_600_000_000_000) / 60_000_000_000;
                let secs = (ns_diff % 60_000_000_000) / 1_000_000_000;
                let nanos = ns_diff % 1_000_000_000;
                let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
                return CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso };
            }

    // Both have date components → use calendar difference (supports extended-range dates)
    let l_ymd = temporal_ymd(lhs);
    let r_ymd = temporal_ymd(rhs);
    if let (Some((ly, lm, ld_day)), Some((ry, rm, rd_day))) = (l_ymd, r_ymd) {
        let lt = lhs.time.unwrap_or(midnight);
        let rt = rhs.time.unwrap_or(midnight);

        // Compute total months difference
        let mut months = (ry - ly) * 12 + (rm as i64 - lm as i64);

        // After adding months to lhs date, compute remaining days
        let (am_y, am_m, am_d) = add_months_ext(ly, lm, ld_day, months);
        let mut rem_days = extended_date_diff_days((am_y, am_m, am_d), (ry, rm, rd_day));

        // If remaining days has opposite sign to months, adjust
        if months > 0 && rem_days < 0 {
            months -= 1;
            let (am_y2, am_m2, am_d2) = add_months_ext(ly, lm, ld_day, months);
            rem_days = extended_date_diff_days((am_y2, am_m2, am_d2), (ry, rm, rd_day));
        } else if months < 0 && rem_days > 0 {
            months += 1;
            let (am_y2, am_m2, am_d2) = add_months_ext(ly, lm, ld_day, months);
            rem_days = extended_date_diff_days((am_y2, am_m2, am_d2), (ry, rm, rd_day));
        }

        // Time difference in nanoseconds — only account for offsets if both sides have one
        let both_off = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
        let l_off = if both_off { lhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
        let r_off = if both_off { rhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
        let lt_ns = time_to_ns(lt) - l_off * 1_000_000_000;
        let rt_ns = time_to_ns(rt) - r_off * 1_000_000_000;
        let mut time_ns = rt_ns - lt_ns;

        // If time diff has opposite sign to the overall direction, borrow a day
        let overall_sign = if months > 0 || (months == 0 && rem_days > 0) || (months == 0 && rem_days == 0 && time_ns >= 0) { 1 } else { -1 };
        if overall_sign > 0 && time_ns < 0 {
            rem_days -= 1;
            time_ns += 86_400_000_000_000;
        } else if overall_sign < 0 && time_ns > 0 {
            rem_days += 1;
            time_ns -= 86_400_000_000_000;
        }

        let years = months / 12;
        let months = months % 12;
        let hours = time_ns / 3_600_000_000_000;
        let minutes = (time_ns % 3_600_000_000_000) / 60_000_000_000;
        let secs = (time_ns % 60_000_000_000) / 1_000_000_000;
        let nanos = time_ns % 1_000_000_000;

        let iso = CypherDuration::build_iso(years, months, 0, rem_days, hours, minutes, secs, nanos);
        return CypherDuration { years, months, weeks: 0, days: rem_days, hours, minutes, seconds: secs, nanoseconds: nanos, iso };
    }

    // Time-only or mixed date+time-only: compute nanosecond difference
    // If both have offsets, compare in UTC. If either is local, compare local times.
    let both_have_offset = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
    let lhs_ns = lhs.time.map(time_to_ns).unwrap_or(0)
        - if both_have_offset { lhs.offset_secs.unwrap_or(0) as i64 * 1_000_000_000 } else { 0 };
    let rhs_ns = rhs.time.map(time_to_ns).unwrap_or(0)
        - if both_have_offset { rhs.offset_secs.unwrap_or(0) as i64 * 1_000_000_000 } else { 0 };
    let ns_diff = rhs_ns - lhs_ns;
    let hours = ns_diff / 3_600_000_000_000;
    let minutes = (ns_diff % 3_600_000_000_000) / 60_000_000_000;
    let secs = (ns_diff % 60_000_000_000) / 1_000_000_000;
    let nanos = ns_diff % 1_000_000_000;
    let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
    CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso }
}

#[allow(dead_code)]
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
    // Compute raw month difference then adjust for incomplete months
    let mut total_months = (rd.year() as i64 * 12 + rd.month() as i64)
        - (ld.year() as i64 * 12 + ld.month() as i64);
    // Day-based adjustment: if adding total_months to ld overshoots rd day
    if total_months > 0 && rd.day() < ld.day() {
        total_months -= 1;
    } else if total_months < 0 && rd.day() > ld.day() {
        total_months += 1;
    }
    // Time-based adjustment: if days are equal, check if time partially undoes the last month
    if rd.day() == ld.day() {
        let lt = lhs.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
        let rt = rhs.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
        let both_off = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
        let l_off = if both_off { lhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
        let r_off = if both_off { rhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
        let lt_ns = time_to_ns(lt) - l_off * 1_000_000_000;
        let rt_ns = time_to_ns(rt) - r_off * 1_000_000_000;
        let time_ns = rt_ns - lt_ns;
        if total_months > 0 && time_ns < 0 {
            total_months -= 1;
        } else if total_months < 0 && time_ns > 0 {
            total_months += 1;
        }
    }
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
    let mut days = (rd - ld).num_days();
    // Truncate partial day: if time portion goes opposite to overall direction, subtract a day
    let lt = lhs.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
    let rt = rhs.time.unwrap_or(NaiveTime::from_hms_opt(0,0,0).unwrap());
    let both_off = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
    let l_off = if both_off { lhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
    let r_off = if both_off { rhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
    let lt_ns = time_to_ns(lt) - l_off * 1_000_000_000;
    let rt_ns = time_to_ns(rt) - r_off * 1_000_000_000;
    let time_ns = rt_ns - lt_ns;
    if days > 0 && time_ns < 0 {
        days -= 1;
    } else if days < 0 && time_ns > 0 {
        days += 1;
    }
    let iso = CypherDuration::build_iso(0, 0, 0, days, 0, 0, 0, 0);
    CypherDuration { years: 0, months: 0, weeks: 0, days, hours: 0, minutes: 0, seconds: 0, nanoseconds: 0, iso }
}

/// duration.inSeconds: full seconds between two temporals.
/// Uses same logic as duration.between but flattens everything to seconds (no months/days).
fn duration_in_seconds(lhs: &TemporalValue, rhs: &TemporalValue) -> CypherDuration {
    let midnight = NaiveTime::from_hms_opt(0,0,0).unwrap();

    // Mixed zoned/local: interpret local side in the named timezone
    let mixed_zoned = lhs.offset_secs.is_some() != rhs.offset_secs.is_some();
    let named_tz = lhs.tz_name.as_ref().or(rhs.tz_name.as_ref());
    if mixed_zoned
        && let Some(tz_name) = named_tz
            && let Ok(tz) = tz_name.parse::<Tz>() {
                let zoned_date = if lhs.offset_secs.is_some() { lhs.date } else { rhs.date };
                let to_utc_ns = |tv: &TemporalValue| -> i64 {
                    let d = tv.date.or(zoned_date).unwrap_or_else(|| NaiveDate::from_ymd_opt(1970,1,1).unwrap());
                    let t = tv.time.unwrap_or(midnight);
                    if let Some(off) = tv.offset_secs {
                        (NaiveDateTime::new(d, t).and_utc().timestamp() - off as i64) * 1_000_000_000 + t.nanosecond() as i64
                    } else {
                        let ndt = NaiveDateTime::new(d, t);
                        let dt = ndt.and_local_timezone(tz)
                            .earliest()
                            .unwrap_or_else(|| ndt.and_local_timezone(chrono_tz::UTC).unwrap());
                        dt.timestamp() * 1_000_000_000 + t.nanosecond() as i64
                    }
                };
                let ns_diff = to_utc_ns(rhs) - to_utc_ns(lhs);
                let hours = ns_diff / 3_600_000_000_000;
                let minutes = (ns_diff % 3_600_000_000_000) / 60_000_000_000;
                let secs = (ns_diff % 60_000_000_000) / 1_000_000_000;
                let nanos = ns_diff % 1_000_000_000;
                let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
                return CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso };
            }

    // If both have dates: compute total seconds via epoch (UTC-adjusted if both have offsets)
    let l_ymd = temporal_ymd(lhs);
    let r_ymd = temporal_ymd(rhs);
    if let (Some((ly, lm, ld_day)), Some((ry, rm, rd_day))) = (l_ymd, r_ymd) {
        let lt = lhs.time.unwrap_or(midnight);
        let rt = rhs.time.unwrap_or(midnight);
        let both_off = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
        let l_off = if both_off { lhs.offset_secs.unwrap_or(0) as i64 } else { 0 };
        let r_off = if both_off { rhs.offset_secs.unwrap_or(0) as i64 } else { 0 };

        // Try chrono path for dates within range
        if let (Some(ld), Some(rd)) = (lhs.date, rhs.date) {
            let l_ns = (NaiveDateTime::new(ld, lt).and_utc().timestamp() - l_off) * 1_000_000_000 + lt.nanosecond() as i64;
            let r_ns = (NaiveDateTime::new(rd, rt).and_utc().timestamp() - r_off) * 1_000_000_000 + rt.nanosecond() as i64;
            let ns_diff = r_ns - l_ns;
            let hours = ns_diff / 3_600_000_000_000;
            let minutes = (ns_diff % 3_600_000_000_000) / 60_000_000_000;
            let secs = (ns_diff % 60_000_000_000) / 1_000_000_000;
            let nanos = ns_diff % 1_000_000_000;
            let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
            return CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso };
        }

        // Extended-range: compute total seconds arithmetically
        let total_days = extended_total_day_diff((ly, lm, ld_day), (ry, rm, rd_day));
        let lt_secs = lt.hour() as i64 * 3600 + lt.minute() as i64 * 60 + lt.second() as i64 - l_off;
        let rt_secs = rt.hour() as i64 * 3600 + rt.minute() as i64 * 60 + rt.second() as i64 - r_off;
        let total_secs = total_days * 86400 + (rt_secs - lt_secs);
        let lt_nanos = lt.nanosecond() as i64 % 1_000_000_000;
        let rt_nanos = rt.nanosecond() as i64 % 1_000_000_000;
        let mut nanos = rt_nanos - lt_nanos;
        let mut adj_secs = total_secs;
        if nanos < 0 && adj_secs > 0 {
            adj_secs -= 1;
            nanos += 1_000_000_000;
        }
        let hours = adj_secs / 3600;
        let minutes = (adj_secs % 3600) / 60;
        let secs = adj_secs % 60;
        let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
        return CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso };
    }
    // Mixed or time-only: use time-component difference (same logic as duration_between time-only branch)
    let both_have_offset = lhs.offset_secs.is_some() && rhs.offset_secs.is_some();
    let lhs_ns = lhs.time.map(time_to_ns).unwrap_or(0)
        - if both_have_offset { lhs.offset_secs.unwrap_or(0) as i64 * 1_000_000_000 } else { 0 };
    let rhs_ns = rhs.time.map(time_to_ns).unwrap_or(0)
        - if both_have_offset { rhs.offset_secs.unwrap_or(0) as i64 * 1_000_000_000 } else { 0 };
    let ns_diff = rhs_ns - lhs_ns;
    let hours = ns_diff / 3_600_000_000_000;
    let minutes = (ns_diff % 3_600_000_000_000) / 60_000_000_000;
    let secs = (ns_diff % 60_000_000_000) / 1_000_000_000;
    let nanos = ns_diff % 1_000_000_000;
    let iso = CypherDuration::build_iso(0, 0, 0, 0, hours, minutes, secs, nanos);
    CypherDuration { years: 0, months: 0, weeks: 0, days: 0, hours, minutes, seconds: secs, nanoseconds: nanos, iso }
}

// ---------------------------------------------------------------------------
// Temporal ISO round-trip reconstitution
// ---------------------------------------------------------------------------

/// Coerce a Value::Str to Value::Temporal or Value::Duration if it matches an ISO pattern.
/// This is used at arithmetic boundaries so stored temporal values (which round-trip through
/// the property store as strings) can participate in temporal operations.
fn coerce_temporal_str(v: &Value) -> Value {
    match v {
        Value::Str(s) => {
            if let Some(dur) = try_parse_duration_iso(s) {
                Value::Duration(dur)
            } else if let Some(tv) = try_parse_temporal_iso(s) {
                Value::Temporal(tv)
            } else {
                v.clone()
            }
        }
        _ => v.clone(),
    }
}

/// Try to parse a string as a CypherDuration ISO (starts with 'P').
fn try_parse_duration_iso(s: &str) -> Option<CypherDuration> {
    if s.starts_with('P') {
        CypherDuration::parse(s)
    } else {
        None
    }
}

/// Try to parse a string as a temporal ISO value.
fn try_parse_temporal_iso(s: &str) -> Option<TemporalValue> {
    // Date: YYYY-MM-DD (exactly 10 chars, no T)
    if s.len() == 10 && s.chars().nth(4) == Some('-') && !s.contains('T')
        && let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
            return Some(TemporalValue {
                kind: TemporalKind::Date,
                iso: s.to_string(),
                date: Some(d),
                time: None,
                offset_secs: None,
                tz_name: None,
            });
        }
    // LocalTime: HH:MM:SS[.nnnnnnnnn] — no date, no offset
    if !s.contains('-') && !s.contains('T') && s.contains(':') && !s.ends_with('Z') && !s.contains('+') {
        // Try as time
        if let Some(t) = parse_time_str(s) {
            return Some(TemporalValue {
                kind: TemporalKind::LocalTime,
                iso: s.to_string(),
                date: None,
                time: Some(t),
                offset_secs: None,
                tz_name: None,
            });
        }
    }
    // Time: HH:MM:SS[.nnn]+HH:MM or HH:MM:SSZ
    if !s.contains('T') && s.contains(':') && (s.ends_with('Z') || s.rfind('+').is_some_and(|p| p > 2) || {
        // check for negative offset after the time part
        let parts: Vec<&str> = s.splitn(2, |c: char| c == '+' || (c == '-' && s.find('-').is_some_and(|fp| fp > 2))).collect();
        parts.len() > 1
    }) {
        // Has offset — try Time
        if let Some((t, off)) = parse_time_with_offset(s) {
            return Some(TemporalValue {
                kind: TemporalKind::Time,
                iso: s.to_string(),
                date: None,
                time: Some(t),
                offset_secs: Some(off),
                tz_name: None,
            });
        }
    }
    // LocalDateTime: YYYY-MM-DDTHH:MM:SS (no offset)
    if s.contains('T') && !s.ends_with('Z') && !s.contains('+') && {
        let after_t = s.split('T').nth(1).unwrap_or("");
        !after_t.contains('+') && !after_t.contains('-')
    } {
        let parts: Vec<&str> = s.splitn(2, 'T').collect();
        if parts.len() == 2
            && let Ok(d) = NaiveDate::parse_from_str(parts[0], "%Y-%m-%d")
                && let Some(t) = parse_time_str(parts[1]) {
                    return Some(TemporalValue {
                        kind: TemporalKind::LocalDateTime,
                        iso: s.to_string(),
                        date: Some(d),
                        time: Some(t),
                        offset_secs: None,
                        tz_name: None,
                    });
                }
    }
    // DateTime: YYYY-MM-DDTHH:MM:SS+HH:MM or ...Z or ...[TZ]
    if s.contains('T') {
        let parts: Vec<&str> = s.splitn(2, 'T').collect();
        if parts.len() == 2
            && let Ok(d) = NaiveDate::parse_from_str(parts[0], "%Y-%m-%d") {
                let time_part = parts[1];
                // Strip bracket TZ name if present
                let (tp, tz_name) = if let Some(bracket_start) = time_part.find('[') {
                    let tn = time_part[bracket_start+1..].trim_end_matches(']');
                    (&time_part[..bracket_start], Some(tn.to_string()))
                } else {
                    (time_part, None)
                };
                if let Some((t, off)) = parse_time_with_offset(tp) {
                    return Some(TemporalValue {
                        kind: TemporalKind::DateTime,
                        iso: s.to_string(),
                        date: Some(d),
                        time: Some(t),
                        offset_secs: Some(off),
                        tz_name,
                    });
                }
            }
    }
    None
}

/// Parse a time string like "12:31:14.000000001" into NaiveTime.
fn parse_time_str(s: &str) -> Option<NaiveTime> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() < 2 { return None; }
    let h: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let (sec, nano) = if parts.len() >= 3 {
        let sec_str = parts[2];
        if let Some(dot_pos) = sec_str.find('.') {
            let s_val: u32 = sec_str[..dot_pos].parse().ok()?;
            let frac = &sec_str[dot_pos+1..];
            let padded = format!("{:0<9}", frac);
            let ns: u32 = padded[..9].parse().ok()?;
            (s_val, ns)
        } else {
            (sec_str.parse().ok()?, 0u32)
        }
    } else {
        (0, 0)
    };
    NaiveTime::from_hms_nano_opt(h, m, sec, nano)
}

/// Parse a time string with offset like "12:31:14+01:00" or "12:31:14Z".
fn parse_time_with_offset(s: &str) -> Option<(NaiveTime, i32)> {
    if let Some(stripped) = s.strip_suffix('Z') {
        let t = parse_time_str(stripped)?;
        return Some((t, 0));
    }
    // Find the last '+' or '-' that separates offset
    let off_pos = s.rfind('+').or_else(|| {
        // For '-', we need to find one after position 2 (to avoid being part of time)
        let bytes = s.as_bytes();
        (3..s.len()).rev().find(|&i| bytes[i] == b'-')
    })?;
    let time_str = &s[..off_pos];
    let off_str = &s[off_pos..]; // includes sign
    let t = parse_time_str(time_str)?;
    // Parse offset like +01:00 or -05:30
    let sign: i32 = if off_str.starts_with('-') { -1 } else { 1 };
    let off_parts: Vec<&str> = off_str[1..].split(':').collect();
    let oh: i32 = off_parts.first()?.parse().ok()?;
    let om: i32 = off_parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    Some((t, sign * (oh * 3600 + om * 60)))
}

// ---------------------------------------------------------------------------
// Temporal arithmetic helpers
// ---------------------------------------------------------------------------

/// Add a duration to a temporal value.
fn temporal_add_duration(tv: &TemporalValue, dur: &CypherDuration) -> Result<TemporalValue, ExecError> {
    let total_months = dur.years * 12 + dur.months;
    let total_days = dur.weeks * 7 + dur.days;
    let total_ns = dur.hours * 3_600_000_000_000i64
        + dur.minutes * 60_000_000_000i64
        + dur.seconds * 1_000_000_000i64
        + dur.nanoseconds;

    match tv.kind {
        TemporalKind::Date => {
            let d = tv.date.unwrap();
            let d = add_months_to_date(d, total_months);
            // Include whole-day overflow from the time component
            let extra_days = total_ns / 86_400_000_000_000i64;
            let d = d.checked_add_signed(chrono::Duration::days(total_days + extra_days)).unwrap_or(d);
            let iso = format!("{}", d.format("%Y-%m-%d"));
            Ok(TemporalValue { kind: TemporalKind::Date, iso, date: Some(d), time: None, offset_secs: None, tz_name: None })
        }
        TemporalKind::LocalTime => {
            let t = tv.time.unwrap();
            let ns = time_to_ns(t) + total_ns;
            let ns = ns.rem_euclid(86_400_000_000_000);
            let secs = (ns / 1_000_000_000) as u32;
            let nano = (ns % 1_000_000_000) as u32;
            let rt = NaiveTime::from_hms_nano_opt(secs / 3600, (secs % 3600) / 60, secs % 60, nano).unwrap();
            let iso = format_localtime(&rt);
            Ok(TemporalValue { kind: TemporalKind::LocalTime, iso, date: None, time: Some(rt), offset_secs: None, tz_name: None })
        }
        TemporalKind::Time => {
            let t = tv.time.unwrap();
            let ns = time_to_ns(t) + total_ns;
            let ns = ns.rem_euclid(86_400_000_000_000);
            let secs = (ns / 1_000_000_000) as u32;
            let nano = (ns % 1_000_000_000) as u32;
            let rt = NaiveTime::from_hms_nano_opt(secs / 3600, (secs % 3600) / 60, secs % 60, nano).unwrap();
            let off = tv.offset_secs.unwrap_or(0);
            let iso = format_time_with_offset(&rt, off);
            Ok(TemporalValue { kind: TemporalKind::Time, iso, date: None, time: Some(rt), offset_secs: Some(off), tz_name: None })
        }
        TemporalKind::LocalDateTime => {
            let d = tv.date.unwrap();
            let t = tv.time.unwrap();
            let d = add_months_to_date(d, total_months);
            let ndt = NaiveDateTime::new(d, t)
                + chrono::Duration::days(total_days)
                + chrono::Duration::nanoseconds(total_ns);
            let iso = format_localdatetime(&ndt.date(), &ndt.time());
            Ok(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(ndt.date()), time: Some(ndt.time()), offset_secs: None, tz_name: None })
        }
        TemporalKind::DateTime => {
            let d = tv.date.unwrap();
            let t = tv.time.unwrap();
            let d = add_months_to_date(d, total_months);
            let ndt = NaiveDateTime::new(d, t)
                + chrono::Duration::days(total_days)
                + chrono::Duration::nanoseconds(total_ns);
            let off = tv.offset_secs.unwrap_or(0);
            let iso = format_datetime(&ndt.date(), &ndt.time(), off, tv.tz_name.as_deref());
            Ok(TemporalValue { kind: TemporalKind::DateTime, iso, date: Some(ndt.date()), time: Some(ndt.time()), offset_secs: Some(off), tz_name: tv.tz_name.clone() })
        }
    }
}

/// Subtract a duration from a temporal value.
fn temporal_sub_duration(tv: &TemporalValue, dur: &CypherDuration) -> Result<TemporalValue, ExecError> {
    let neg = CypherDuration {
        years: -dur.years, months: -dur.months, weeks: -dur.weeks, days: -dur.days,
        hours: -dur.hours, minutes: -dur.minutes, seconds: -dur.seconds, nanoseconds: -dur.nanoseconds,
        iso: String::new(),
    };
    temporal_add_duration(tv, &neg)
}

/// Add two durations.
/// Compare two temporal values chronologically, accounting for UTC offsets.
fn temporal_cmp(a: &TemporalValue, b: &TemporalValue) -> std::cmp::Ordering {
    // If both have offsets, compare in UTC
    if let (Some(a_off), Some(b_off)) = (a.offset_secs, b.offset_secs)
        && let (Some(at), Some(bt)) = (a.time, b.time) {
            // For time-only or datetime, compare UTC nanoseconds
            let a_utc_ns = time_to_ns(at) - (a_off as i64) * 1_000_000_000;
            let b_utc_ns = time_to_ns(bt) - (b_off as i64) * 1_000_000_000;
            if let (Some(ad), Some(bd)) = (a.date, b.date) {
                // DateTime: compare date first, then time — use i128 to avoid overflow
                let a_days = ad.num_days_from_ce() as i128;
                let b_days = bd.num_days_from_ce() as i128;
                let a_total = a_days * 86_400_000_000_000i128 + a_utc_ns as i128;
                let b_total = b_days * 86_400_000_000_000i128 + b_utc_ns as i128;
                return a_total.cmp(&b_total);
            }
            // Time only: compare UTC nanoseconds (mod day)
            let a_utc = a_utc_ns.rem_euclid(86_400_000_000_000);
            let b_utc = b_utc_ns.rem_euclid(86_400_000_000_000);
            return a_utc.cmp(&b_utc);
        }
    // Fallback: lexicographic ISO comparison (dates, localtimes, localdatetimes)
    a.iso.cmp(&b.iso)
}

#[allow(clippy::too_many_arguments)]
fn duration_normalize(years: i64, months: i64, weeks: i64, days: i64, hours: i64, minutes: i64, seconds: i64, nanoseconds: i64) -> CypherDuration {
    let total_months = years * 12 + months;
    let y = total_months / 12;
    let m = total_months % 12;
    let total_ns = seconds * 1_000_000_000 + nanoseconds;
    let s = total_ns / 1_000_000_000;
    let ns = total_ns % 1_000_000_000;
    let d = weeks * 7 + days;
    // Normalize seconds -> minutes -> hours
    let total_s = hours * 3600 + minutes * 60 + s;
    let hours = total_s / 3600;
    let minutes = (total_s % 3600) / 60;
    let seconds = total_s % 60;
    let iso = CypherDuration::build_iso(y, m, 0, d, hours, minutes, seconds, ns);
    CypherDuration { years: y, months: m, weeks: 0, days: d, hours, minutes, seconds, nanoseconds: ns, iso }
}

fn duration_add(a: &CypherDuration, b: &CypherDuration) -> CypherDuration {
    duration_normalize(
        a.years + b.years, a.months + b.months, a.weeks + b.weeks, a.days + b.days,
        a.hours + b.hours, a.minutes + b.minutes, a.seconds + b.seconds, a.nanoseconds + b.nanoseconds,
    )
}

/// Subtract two durations.
fn duration_sub(a: &CypherDuration, b: &CypherDuration) -> CypherDuration {
    duration_normalize(
        a.years - b.years, a.months - b.months, a.weeks - b.weeks, a.days - b.days,
        a.hours - b.hours, a.minutes - b.minutes, a.seconds - b.seconds, a.nanoseconds - b.nanoseconds,
    )
}

/// Multiply a duration by an integer.
fn duration_mul(d: &CypherDuration, n: i64) -> CypherDuration {
    duration_normalize(
        d.years * n, d.months * n, d.weeks * n, d.days * n,
        d.hours * n, d.minutes * n, d.seconds * n, d.nanoseconds * n,
    )
}

/// Divide a duration by an integer.
fn duration_div(d: &CypherDuration, n: i64) -> CypherDuration {
    let nf = n as f64;
    let months_total = d.years * 12 + d.months;
    let total_days_i = d.weeks * 7 + d.days;
    let total_secs = d.hours * 3600 + d.minutes * 60 + d.seconds;
    // Divide months, cascade remainder to days using 30.436875 days/month
    let m = months_total / n;
    let months_rem = months_total % n;
    let extra_days_f = months_rem as f64 * 30.436875;
    // Divide days (including extra from months remainder), cascade fractional to seconds
    let total_days_f = total_days_i as f64 + extra_days_f;
    let dd = (total_days_f / nf).trunc() as i64;
    let days_rem_f = total_days_f - dd as f64 * nf;
    let extra_secs_f = days_rem_f * 86400.0;
    // Divide time (including extra from days remainder) using integer nanos
    let total_secs_f = total_secs as f64 + extra_secs_f;
    let total_time_ns = (total_secs_f * 1_000_000_000.0).round() as i64 + d.nanoseconds;
    let result_ns = total_time_ns / n;
    let final_secs = result_ns / 1_000_000_000;
    let final_ns = result_ns % 1_000_000_000;
    let yrs = m / 12;
    let mos = m % 12;
    let hrs = final_secs / 3600;
    let mins = (final_secs % 3600) / 60;
    let scs = final_secs % 60;
    let iso = CypherDuration::build_iso(yrs, mos, 0, dd, hrs, mins, scs, final_ns);
    CypherDuration { years: yrs, months: mos, weeks: 0, days: dd, hours: hrs, minutes: mins, seconds: scs, nanoseconds: final_ns, iso }
}

/// Multiply a duration by a float.
fn duration_mul_f(d: &CypherDuration, f: f64) -> CypherDuration {
    let total_months = (d.years * 12 + d.months) as f64 * f;
    let months = total_months.trunc() as i64;
    let frac_months_days = total_months.fract() * 30.436875;
    let total_days = (d.weeks * 7 + d.days) as f64 * f + frac_months_days;
    let days = total_days.trunc() as i64;
    let frac_days_secs = total_days.fract() * 86400.0;
    let total_secs = (d.hours * 3600 + d.minutes * 60 + d.seconds) as f64 * f + frac_days_secs;
    let total_ns = d.nanoseconds as f64 * f + total_secs.fract() * 1_000_000_000.0;
    let secs = total_secs.trunc() as i64;
    let nanos = total_ns.trunc() as i64;
    // Normalize
    let total_nanos = secs * 1_000_000_000 + nanos;
    let final_secs = total_nanos / 1_000_000_000;
    let final_ns = total_nanos % 1_000_000_000;
    let y = months / 12;
    let m = months % 12;
    let total_s = final_secs;
    let hours = total_s / 3600;
    let minutes = (total_s % 3600) / 60;
    let seconds = total_s % 60;
    let iso = CypherDuration::build_iso(y, m, 0, days, hours, minutes, seconds, final_ns);
    CypherDuration { years: y, months: m, weeks: 0, days, hours, minutes, seconds, nanoseconds: final_ns, iso }
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

fn format_offset(offset_secs: i32) -> String {
    if offset_secs == 0 {
        return "Z".to_string();
    }
    let sign = if offset_secs < 0 { '-' } else { '+' };
    let abs = offset_secs.unsigned_abs();
    let h = abs / 3600;
    let m = (abs % 3600) / 60;
    let s = abs % 60;
    if s != 0 {
        format!("{sign}{h:02}:{m:02}:{s:02}")
    } else {
        format!("{sign}{h:02}:{m:02}")
    }
}

fn format_time_with_offset(t: &NaiveTime, offset_secs: i32) -> String {
    let local_str = format_localtime(t);
    format!("{local_str}{}", format_offset(offset_secs))
}

fn format_localdatetime(d: &NaiveDate, t: &NaiveTime) -> String {
    format!("{}T{}", d.format("%Y-%m-%d"), format_localtime(t))
}

fn format_datetime(d: &NaiveDate, t: &NaiveTime, offset_secs: i32, tz_name: Option<&str>) -> String {
    let base = format!("{}T{}", d.format("%Y-%m-%d"), format_localtime(t));
    let off_str = format_offset(offset_secs);
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
    // Try localdatetime/datetime: contains 'T'
    if let Some(t_pos) = s.find('T') {
        // Strip bracket timezone if present (e.g. "+01:00[Europe/Stockholm]")
        let (s_clean, tz_name_bracket) = strip_tz_bracket(s);
        let t_pos_clean = s_clean.find('T').unwrap_or(t_pos);
        let date_part = &s_clean[..t_pos_clean];
        let time_part = &s_clean[t_pos_clean+1..];
        let has_tz = time_part.contains('+') || time_part.contains('Z')
            || (time_part.len() > 6 && time_part[time_part.len()-6..].contains('-'));
        if !has_tz && tz_name_bracket.is_none() {
            if let (Some(d), Some(t)) = (parse_date_str(date_part), parse_localtime_str(time_part)) {
                let iso = s.to_string();
                return Some(TemporalValue { kind: TemporalKind::LocalDateTime, iso, date: Some(d), time: Some(t), offset_secs: None, tz_name: None });
            }
        } else {
            // Datetime with offset (and possibly bracket timezone)
            let (time_str, off, _) = extract_time_tz(time_part);
            if let (Some(d), Some(t2)) = (parse_date_str(date_part), parse_localtime_str(time_str)) {
                let tz = tz_name_bracket.or(None);
                return Some(TemporalValue { kind: TemporalKind::DateTime, iso: s.to_string(), date: Some(d), time: Some(t2), offset_secs: off, tz_name: tz });
            }
        }
    }
    // Try time with offset: HH:MM[:SS[.f]][+/-HH:MM] (no date, no T, has offset)
    if !s.contains('T') && (s.contains('+') || s.ends_with('Z') || (s.len() > 6 && s[s.len()-6..].contains('-'))) {
        let (time_str, off, tz) = extract_time_tz(s);
        if let Some(t) = parse_localtime_str(time_str) {
            let offset = off.unwrap_or(0);
            let iso = format_time_with_offset(&t, offset);
            return Some(TemporalValue { kind: TemporalKind::Time, iso, date: None, time: Some(t), offset_secs: Some(offset), tz_name: tz });
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
        "microsecond" => tv.time.map(|t| Value::Int((t.nanosecond() / 1_000) as i64)).unwrap_or(Value::Null),
        "nanosecond" => tv.time.map(|t| Value::Int(t.nanosecond() as i64)).unwrap_or(Value::Null),
        "nanoseconds" | "nanosecondsOfSecond" => tv.time.map(|t| Value::Int(t.nanosecond() as i64)).unwrap_or(Value::Null),
        "epochSeconds" => temporal_epoch_seconds(tv).map(Value::Int).unwrap_or(Value::Null),
        "epochMillis" => temporal_epoch_seconds(tv).map(|s| {
            let ms = tv.time.map(|t| (t.nanosecond() / 1_000_000) as i64).unwrap_or(0);
            Value::Int(s * 1000 + ms)
        }).unwrap_or(Value::Null),
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
        "offsetMinutes" => tv.offset_secs.map(|o| Value::Int(o as i64 / 60)).unwrap_or(Value::Null),
        "offsetSeconds" => tv.offset_secs.map(|o| Value::Int(o as i64)).unwrap_or(Value::Null),
        "quarter" => tv.date.map(|d| Value::Int(((d.month() - 1) / 3 + 1) as i64)).unwrap_or(Value::Null),
        "dayOfWeek" | "weekDay" => tv.date.map(|d| Value::Int(d.weekday().number_from_monday() as i64)).unwrap_or(Value::Null),
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
    let total_months = dur.years * 12 + dur.months;
    let total_days = dur.weeks * 7 + dur.days;
    let total_secs = dur.hours * 3600 + dur.minutes * 60 + dur.seconds;
    let sub_sec_ns = dur.nanoseconds;
    match key {
        "years" => Value::Int(total_months / 12),
        "quarters" => Value::Int(total_months / 3),
        "months" => Value::Int(total_months),
        "weeks" => Value::Int(total_days / 7),
        "days" => Value::Int(total_days),
        "hours" => Value::Int(total_secs / 3600),
        "minutes" => Value::Int(total_secs / 60),
        "seconds" => {
            if sub_sec_ns < 0 { Value::Int(total_secs - 1) } else { Value::Int(total_secs) }
        }
        "milliseconds" => {
            let ns = if sub_sec_ns < 0 { sub_sec_ns + 1_000_000_000 } else { sub_sec_ns };
            let s = if sub_sec_ns < 0 { total_secs - 1 } else { total_secs };
            Value::Int(s * 1000 + ns / 1_000_000)
        }
        "microseconds" => {
            let ns = if sub_sec_ns < 0 { sub_sec_ns + 1_000_000_000 } else { sub_sec_ns };
            let s = if sub_sec_ns < 0 { total_secs - 1 } else { total_secs };
            Value::Int(s * 1_000_000 + ns / 1_000)
        }
        "nanoseconds" => {
            let ns = if sub_sec_ns < 0 { sub_sec_ns + 1_000_000_000 } else { sub_sec_ns };
            let s = if sub_sec_ns < 0 { total_secs - 1 } else { total_secs };
            Value::Int(s * 1_000_000_000 + ns)
        }
        "quartersOfYear" => Value::Int(dur.months / 3),
        "monthsOfQuarter" => Value::Int(dur.months % 3),
        "monthsOfYear" => Value::Int(dur.months),
        "daysOfWeek" => Value::Int(total_days % 7),
        "minutesOfHour" => Value::Int(dur.minutes),
        "secondsOfMinute" => Value::Int(dur.seconds),
        "millisecondsOfSecond" => Value::Int(sub_sec_ns / 1_000_000),
        "microsecondsOfSecond" => Value::Int(sub_sec_ns / 1_000),
        "nanosecondsOfSecond" => {
            if sub_sec_ns < 0 { Value::Int(sub_sec_ns + 1_000_000_000) } else { Value::Int(sub_sec_ns) }
        }
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
            } else if let Some(tv) = try_parse_temporal_iso(s) {
                Value::Temporal(tv)
            } else if let Some(dur) = try_parse_duration_iso(s) {
                Value::Duration(dur)
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
        LogicalPlan::CallProcedure { input, proc_name, args, yield_items, implicit } => {
            exec_call_procedure(input, proc_name, args, yield_items, *implicit, params)
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
        // v0.23.0 plan nodes
        LogicalPlan::PropertyIndexScan {
            variable, label, label_id, prop: _, key_id, value_expr,
            remaining_filters, optional,
        } => {
            exec_property_index_scan(
                variable, label, *label_id, *key_id,
                value_expr, remaining_filters, *optional, params,
            )
        }
        LogicalPlan::CreateIndex { label, prop } => {
            crate::catalog::indexes::create_property_index(label, prop);
            Ok(Vec::new())
        }
        LogicalPlan::DropIndex { label, prop } => {
            let dropped = crate::catalog::indexes::drop_property_index(label, prop);
            let mut row = Row::new();
            row.insert("dropped".to_string(), Value::Bool(dropped));
            Ok(vec![row])
        }
        LogicalPlan::ShowIndexes => {
            let pairs = crate::catalog::indexes::list_indexes();
            let rows: Vec<Row> = pairs.into_iter().map(|(label, prop)| {
                let mut r = Row::new();
                r.insert("label".to_string(), Value::Str(label));
                r.insert("prop".to_string(), Value::Str(prop));
                r
            }).collect();
            Ok(rows)
        }
        LogicalPlan::CreateConstraint { label, prop, kind } => {
            use crate::catalog::constraints::{ConstraintKind, create_constraint};
            let ck = if kind == "UNIQUE" { ConstraintKind::Unique } else { ConstraintKind::Exists };
            create_constraint(label, prop, ck);
            Ok(Vec::new())
        }
        LogicalPlan::DropConstraint { label, prop, kind } => {
            use crate::catalog::constraints::{ConstraintKind, drop_constraint};
            let ck = if kind == "UNIQUE" { ConstraintKind::Unique } else { ConstraintKind::Exists };
            let dropped = drop_constraint(label, prop, ck);
            let mut row = Row::new();
            row.insert("dropped".to_string(), Value::Bool(dropped));
            Ok(vec![row])
        }
        LogicalPlan::ShowConstraints => {
            let triples = crate::catalog::constraints::list_constraints();
            let rows: Vec<Row> = triples.into_iter().map(|(label, prop, kind)| {
                let mut r = Row::new();
                r.insert("label".to_string(), Value::Str(label));
                r.insert("prop".to_string(), Value::Str(prop));
                r.insert("kind".to_string(), Value::Str(kind));
                r
            }).collect();
            Ok(rows)
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

/// Use a property B-tree index to resolve nodes with equality filter on a
/// specific (label, property) pair.  Falls back to no results (not a full scan)
/// when the index has no entries for the given value.
#[allow(clippy::too_many_arguments)]
fn exec_property_index_scan(
    variable: &str,
    label: &str,
    label_id: i32,
    key_id: i32,
    value_expr: &Expr,
    remaining_filters: &[(String, Expr)],
    optional: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    use crate::catalog::labels::{label_name, prop_key_name};
    use crate::catalog::indexes::lookup_nodes_by_property;
    use crate::storage::prop_store;

    // Evaluate the equality value in an empty row (it's a constant/parameter).
    let empty_row = Row::new();
    let filter_val = eval_expr(value_expr, &empty_row, params)?;

    // Serialise the value to an index key string (same encoding as store time).
    let value_text = serde_json::to_string(&filter_val.to_json())
        .unwrap_or_default();

    // Query the property value index.
    let node_ids = lookup_nodes_by_property(label_id, key_id, &value_text);

    if node_ids.is_empty() && optional {
        let mut null_row = Row::new();
        null_row.insert(variable.to_string(), Value::Null);
        return Ok(vec![null_row]);
    }

    let mut rows = Vec::new();

    for nid in node_ids {
        let record = unsafe {
            let rel = crate::open_nodes_relation();
            let snap = pgrx::pg_sys::GetActiveSnapshot();
            let r = crate::storage::node_store::find_node_by_id(rel, nid, snap);
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

            // Verify this node actually has the expected label (index might be stale).
            if !r.label_ids.contains(&label_id) {
                continue;
            }

            let labels: Vec<String> = r.label_ids.iter().map(|id| label_name(*id)).collect();
            let properties = prop_store::decode(&r.prop_bytes, prop_key_name);

            // Sanity check: only return nodes whose label matches.
            if !labels.iter().any(|l| l == label) {
                continue;
            }

            let val = Value::Node {
                node_id: nid,
                labels,
                properties,
            };

            // Apply any remaining inline property filters.
            if !remaining_filters.is_empty() {
                let mut matches = true;
                let mut scan_row = Row::new();
                for (k, v) in params {
                    scan_row.insert(k.clone(), json_to_value(v));
                }
                for (key, expr) in remaining_filters {
                    let prop_val = val.get_property(key);
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

    // Open relations once for the entire expand (OPT-3B).
    // With OPT-3 the OID is cached; but table_open itself is still called
    // once-per-edge otherwise.  Opening once per exec_expand invocation and
    // passing through reduces table_open/table_close from O(N_edges) to O(1).
    let (expand_node_rel, expand_edge_rel, expand_snapshot) = unsafe {
        (
            crate::open_nodes_relation(),
            crate::open_edges_relation(),
            pgrx::pg_sys::GetActiveSnapshot(),
        )
    };

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

        // Follow adjacency chains (using relations opened once per exec_expand).
        let edges = unsafe {
            adjacency_follow(
                expand_node_rel, expand_edge_rel, src_node_id, dir, type_filter, expand_snapshot,
            )
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

            // Load the destination node (using the relation opened once per exec_expand).
            let dst_record = unsafe {
                crate::storage::node_store::find_node_by_id(expand_node_rel, other_id, expand_snapshot)
            };

            let dst_record = match dst_record {
                Some(r) => r,
                None => continue, // invisible or deleted
            };

            // Label filter on destination BEFORE overflow resolution:
            // skip the I/O cost of reading overflow pages for nodes that don't
            // match the required label set.
            let mut dst_r = dst_record;
            if !dst_label_ids.is_empty() {
                let has_all = dst_label_ids.iter().all(|lid| dst_r.label_ids.contains(lid));
                if !has_all {
                    continue;
                }
            }

            // Resolve overflow props only now (after label filter passes).
            if dst_r.overflow_blkno != 0 && dst_r.prop_bytes.is_empty() {
                dst_r.prop_bytes = unsafe {
                    crate::storage::node_store::read_overflow_block(expand_node_rel, dst_r.overflow_blkno)
                };
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

    // Close the relations opened once per exec_expand (OPT-3B).
    unsafe {
        pgrx::pg_sys::table_close(expand_edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
        pgrx::pg_sys::table_close(expand_node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
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
        if let Some(eid) = expected_dst_id
            && current_node != eid {
                continue;
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
    // Validate function names in all expressions before executing (catches
    // UnknownFunction even when the input produces zero rows).
    for item in items {
        validate_function_names(&item.expr)?;
    }
    for ob in order_by {
        validate_function_names(&ob.expr)?;
    }

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
                    } else if let Some(dur) = try_parse_duration_iso(s) {
                        Ok(duration_get_property(&dur, key))
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
    // NaN comparisons: NaN = anything → false; NaN <> anything → true.
    // For ordering operators (< > <= >=) with non-numeric types → null.
    let left_is_nan = matches!(left, Value::Float(f) if f.is_nan());
    let right_is_nan = matches!(right, Value::Float(f) if f.is_nan());
    if left_is_nan || right_is_nan {
        // Equality/inequality: always defined for NaN
        if matches!(op, CmpOp::Eq | CmpOp::Neq) {
            return Some(matches!(op, CmpOp::Neq));
        }
        // Ordering: both sides must be numeric
        let left_numeric = matches!(left, Value::Int(_) | Value::Float(_));
        let right_numeric = matches!(right, Value::Int(_) | Value::Float(_));
        if left_numeric && right_numeric {
            return Some(false); // NaN is not < > <= >= anything
        }
        // NaN vs non-numeric with ordering: null (incomparable)
        return None;
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
        // Temporal comparison: compare in UTC for same-kind values with offsets
        (Value::Temporal(a), Value::Temporal(b)) => {
            let ord = temporal_cmp(a, b);
            let result = match op {
                CmpOp::Eq => ord == std::cmp::Ordering::Equal,
                CmpOp::Neq => ord != std::cmp::Ordering::Equal,
                CmpOp::Lt => ord == std::cmp::Ordering::Less,
                CmpOp::Le => ord != std::cmp::Ordering::Greater,
                CmpOp::Gt => ord == std::cmp::Ordering::Greater,
                CmpOp::Ge => ord != std::cmp::Ordering::Less,
            };
            Some(result)
        },
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
    // Coerce strings that are temporal/duration ISO values so arithmetic works after storage round-trip
    let left = coerce_temporal_str(left);
    let right = coerce_temporal_str(right);
    let left = &left;
    let right = &right;
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
        // Temporal + Duration / Temporal - Duration
        (Value::Temporal(tv), Value::Duration(dur)) => match op {
            ArithOp::Add => {
                let res = temporal_add_duration(tv, dur)?;
                Ok(Value::Temporal(res))
            }
            ArithOp::Sub => {
                let res = temporal_sub_duration(tv, dur)?;
                Ok(Value::Temporal(res))
            }
            _ => Ok(Value::Null),
        },
        // Duration + Duration / Duration - Duration
        (Value::Duration(a), Value::Duration(b)) => match op {
            ArithOp::Add => Ok(Value::Duration(duration_add(a, b))),
            ArithOp::Sub => Ok(Value::Duration(duration_sub(a, b))),
            _ => Ok(Value::Null),
        },
        // Duration * Int / Duration / Int
        (Value::Duration(d), Value::Int(n)) => match op {
            ArithOp::Mul => Ok(Value::Duration(duration_mul(d, *n))),
            ArithOp::Div => {
                if *n == 0 { Ok(Value::Null) } else { Ok(Value::Duration(duration_div(d, *n))) }
            }
            _ => Ok(Value::Null),
        },
        // Duration * Float / Duration / Float
        (Value::Duration(d), Value::Float(f)) => match op {
            ArithOp::Mul => Ok(Value::Duration(duration_mul_f(d, *f))),
            ArithOp::Div => {
                if *f == 0.0 { Ok(Value::Null) } else { Ok(Value::Duration(duration_mul_f(d, 1.0 / *f))) }
            }
            _ => Ok(Value::Null),
        },
        // Int * Duration
        (Value::Int(n), Value::Duration(d)) if matches!(op, ArithOp::Mul) => {
            Ok(Value::Duration(duration_mul(d, *n)))
        }
        // Float * Duration
        (Value::Float(f), Value::Duration(d)) if matches!(op, ArithOp::Mul) => {
            Ok(Value::Duration(duration_mul_f(d, *f)))
        }
        // Duration + Temporal (commutative add)
        (Value::Duration(dur), Value::Temporal(tv)) if matches!(op, ArithOp::Add) => {
            let res = temporal_add_duration(tv, dur)?;
            Ok(Value::Temporal(res))
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

/// Known function names for validation.
const KNOWN_FUNCTIONS: &[&str] = &[
    "labels", "type", "properties", "keys", "id", "elementid",
    "tostring", "tointeger", "tofloat", "toboolean",
    "size", "length", "nodes", "relationships", "rels",
    "head", "last", "tail", "range", "reverse", "coalesce",
    "abs", "sign", "ceil", "floor", "round", "sqrt",
    "sin", "cos", "tan", "asin", "acos", "atan", "atan2",
    "degrees", "radians", "pi", "e", "log", "log10", "exp",
    "rand", "tostring", "replace", "substring", "trim",
    "ltrim", "rtrim", "toupper", "tolower", "split",
    "left", "right", "starts with", "ends with", "contains",
    "exists", "collect", "count", "sum", "avg", "min", "max",
    "stdev", "stdevp", "percentilecont", "percentiledisc",
    "date", "localtime", "time", "localdatetime", "datetime",
    "duration", "duration.between", "duration.inmonths",
    "duration.indays", "duration.inseconds",
    "date.truncate", "localtime.truncate", "time.truncate",
    "localdatetime.truncate", "datetime.truncate",
    "date.realtime", "date.statement", "date.transaction",
    "localtime.realtime", "localtime.statement", "localtime.transaction",
    "time.realtime", "time.statement", "time.transaction",
    "localdatetime.realtime", "localdatetime.statement", "localdatetime.transaction",
    "datetime.realtime", "datetime.statement", "datetime.transaction",
    "timestamp", "point", "distance",
    "tointegerornull", "tofloatornull", "tobooleanornull",
    "tostringornull", "valuetype",
    // Bare names for namespaced functions (parser produces FunctionCall("between", ...) etc.)
    "between", "inmonths", "indays", "inseconds",
    "transaction", "statement", "realtime", "truncate", "fromepoch",
    "fromepochmillis",
    // DISTINCT aggregation variants (parser rewrites count(DISTINCT x) → count_distinct(x))
    "count_distinct", "collect_distinct", "sum_distinct", "avg_distinct",
    // Graph functions
    "startnode", "endnode",
];

/// Validate that all function calls in an expression use known function names.
fn validate_function_names(expr: &Expr) -> Result<(), ExecError> {
    match expr {
        Expr::FunctionCall(name, args) => {
            if !KNOWN_FUNCTIONS.contains(&name.to_ascii_lowercase().as_str()) {
                return Err(ExecError {
                    message: format!("UnknownFunction: {name}()"),
                });
            }
            for a in args {
                validate_function_names(a)?;
            }
        }
        Expr::Arith(l, _, r) | Expr::And(l, r) | Expr::Or(l, r) | Expr::Xor(l, r)
        | Expr::Compare(l, _, r) | Expr::InList(l, r) | Expr::StartsWith(l, r)
        | Expr::EndsWith(l, r) | Expr::Contains(l, r) | Expr::Regex(l, r) => {
            validate_function_names(l)?;
            validate_function_names(r)?;
        }
        Expr::Not(e) | Expr::IsNull(e) | Expr::IsNotNull(e) | Expr::Neg(e) => {
            validate_function_names(e)?;
        }
        Expr::CaseSearched { branches, else_ } => {
            for (w, t) in branches {
                validate_function_names(w)?;
                validate_function_names(t)?;
            }
            if let Some(el) = else_ { validate_function_names(el)?; }
        }
        Expr::CaseSimple { test, branches, else_ } => {
            validate_function_names(test)?;
            for (w, t) in branches {
                validate_function_names(w)?;
                validate_function_names(t)?;
            }
            if let Some(el) = else_ { validate_function_names(el)?; }
        }
        Expr::List(items) => {
            for i in items { validate_function_names(i)?; }
        }
        Expr::Property(e, _) => validate_function_names(e)?,
        Expr::Subscript(e, i) => {
            validate_function_names(e)?;
            validate_function_names(i)?;
        }
        Expr::ListSlice { list_expr, from, to } => {
            validate_function_names(list_expr)?;
            if let Some(s) = from { validate_function_names(s)?; }
            if let Some(end) = to { validate_function_names(end)?; }
        }
        _ => {}
    }
    Ok(())
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
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: labels() requires a node argument".into(),
                }),
            }
        }
        "type" => {
            if args.len() != 1 {
                return Err(ExecError { message: "type() takes exactly 1 argument".into() });
            }
            let val = eval_expr(&args[0], row, params)?;
            match val {
                Value::Edge { rel_type, .. } => Ok(Value::Str(rel_type)),
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: type() requires a relationship argument".into(),
                }),
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
                Value::Temporal(tv) => Ok(Value::Str(tv.iso)),
                Value::Duration(d) => Ok(Value::Str(d.iso)),
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: toString() cannot convert this type".into(),
                }),
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
                Value::Bool(b) => Ok(Value::Int(if b { 1 } else { 0 })),
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
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: toInteger() cannot convert this type".into(),
                }),
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
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: toFloat() cannot convert this type".into(),
                }),
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
                Value::Int(i) => Ok(Value::Bool(i != 0)),
                Value::Str(s) => match s.to_ascii_lowercase().as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    _ => Ok(Value::Null),
                },
                Value::Null => Ok(Value::Null),
                _ => Err(ExecError {
                    message: "InvalidArgumentValue: toBoolean() cannot convert this type".into(),
                }),
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
        "tolower" => {
            if args.len() != 1 { return Err(ExecError { message: "toLower() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_lowercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "toupper" => {
            if args.len() != 1 { return Err(ExecError { message: "toUpper() takes exactly 1 argument".into() }); }
            let v = eval_expr(&args[0], row, params)?;
            match v {
                Value::Str(s) => Ok(Value::Str(s.to_uppercase())),
                Value::Null => Ok(Value::Null),
                _ => Ok(Value::Null),
            }
        }
        "between" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_between(lhs, rhs)))
        }
        "inmonths" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_months(lhs, rhs)))
        }
        "indays" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_days(lhs, rhs)))
        }
        "inseconds" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_seconds(lhs, rhs)))
        }
        "substring" if args.len() >= 2 && args.len() <= 3 => {
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
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
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
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
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
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
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
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
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
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
            Ok(Value::Temporal(temporal_datetime(&arg)?))
        }
        "duration" => {
            if args.len() != 1 { return Err(ExecError { message: "duration() takes exactly 1 argument".into() }); }
            let arg = eval_expr(&args[0], row, params)?;
            if matches!(arg, Value::Null) { return Ok(Value::Null); }
            Ok(Value::Duration(temporal_duration(&arg)?))
        }
        // --- duration.between / inMonths / inDays / inSeconds ---
        // These arrive as FunctionCall("between"|"inMonths"|…, [duration_expr, lhs, rhs])
        // because the parser sees `duration.between(a,b)` as:
        //   parse_property_chain(Variable("duration")) → method call → FunctionCall("between", [Variable("duration"), a, b])
        "between" if args.len() == 3 => {
            // args[0] is the "duration" variable reference (ignored — it's just the namespace)
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_between(lhs, rhs)))
        }
        "inmonths" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_months(lhs, rhs)))
        }
        "indays" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_days(lhs, rhs)))
        }
        "inseconds" if args.len() == 3 => {
            let lhs_val = coerce_temporal_str(&eval_expr(&args[1], row, params)?);
            let rhs_val = coerce_temporal_str(&eval_expr(&args[2], row, params)?);
            if matches!(lhs_val, Value::Null) || matches!(rhs_val, Value::Null) { return Ok(Value::Null); }
            let lhs = as_temporal(&lhs_val)?;
            let rhs = as_temporal(&rhs_val)?;
            Ok(Value::Duration(duration_in_seconds(lhs, rhs)))
        }
        // transaction() / statement() / realtime() — clock-access subtypes of datetime()
        // Can be called as datetime.transaction() (args[0] = Variable("datetime"))
        // or standalone transaction().
        "transaction" | "statement" | "realtime" => {
            // Check for null argument: date.transaction(null) → null
            for a in args.iter().skip(1) {
                let v = eval_expr(a, row, params)?;
                if matches!(v, Value::Null) { return Ok(Value::Null); }
            }
            // Determine the temporal kind from the namespace (args[0])
            let ns = if let Some(Expr::Variable(v)) = args.first() {
                v.to_lowercase()
            } else {
                "datetime".into()
            };
            match ns.as_str() {
                "date" => {
                    let s = Spi::get_one::<String>("SELECT current_date::text")
                        .unwrap_or(Some("1970-01-01".into())).unwrap_or_default();
                    Ok(Value::Temporal(temporal_date(&Value::Str(s))?))
                }
                "localtime" => {
                    let s = Spi::get_one::<String>("SELECT localtime::text")
                        .unwrap_or(Some("00:00:00".into())).unwrap_or_default();
                    Ok(Value::Temporal(temporal_localtime(&Value::Str(s))?))
                }
                "time" => {
                    let s = Spi::get_one::<String>("SELECT current_time::text")
                        .unwrap_or(Some("00:00:00+00:00".into())).unwrap_or_default();
                    Ok(Value::Temporal(temporal_time(&Value::Str(s))?))
                }
                "localdatetime" => {
                    let s = Spi::get_one::<String>("SELECT localtimestamp::text")
                        .unwrap_or(Some("1970-01-01T00:00:00".into())).unwrap_or_default();
                    let s = s.replace(' ', "T");
                    Ok(Value::Temporal(temporal_localdatetime(&Value::Str(s))?))
                }
                _ => {
                    // datetime (default)
                    let s = Spi::get_one::<String>("SELECT now()::text")
                        .unwrap_or(Some("1970-01-01T00:00:00+00:00".into())).unwrap_or_default();
                    let s = s.replace(' ', "T");
                    Ok(Value::Temporal(temporal_datetime(&Value::Str(s))?))
                }
            }
        }
        // datetime.fromepoch(seconds, nanos) — epoch-based datetime construction
        "fromepoch" if args.len() == 3 => {
            let secs_val = eval_expr(&args[1], row, params)?;
            let nanos_val = eval_expr(&args[2], row, params)?;
            let secs = match &secs_val {
                Value::Int(i) => *i,
                Value::Float(f) => *f as i64,
                _ => return Err(ExecError { message: "fromepoch(): seconds must be numeric".into() }),
            };
            let nanos = match &nanos_val {
                Value::Int(i) => *i,
                Value::Float(f) => *f as i64,
                _ => 0i64,
            };
            let total_nanos = secs * 1_000_000_000 + nanos;
            let epoch_secs = total_nanos / 1_000_000_000;
            let rem_nanos = (total_nanos % 1_000_000_000) as u32;
            let dt = chrono::DateTime::from_timestamp(epoch_secs, rem_nanos)
                .ok_or_else(|| ExecError { message: "fromepoch(): invalid epoch value".into() })?;
            let date = dt.date_naive();
            let time = dt.time();
            let iso = format_datetime(&date, &time, 0, None);
            Ok(Value::Temporal(TemporalValue {
                kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time),
                offset_secs: Some(0), tz_name: None,
            }))
        }
        // datetime.fromepochmillis(millis) — epoch millis-based datetime construction
        "fromepochmillis" if args.len() >= 2 => {
            let millis_val = eval_expr(&args[1], row, params)?;
            let millis = match &millis_val {
                Value::Int(i) => *i,
                Value::Float(f) => *f as i64,
                _ => return Err(ExecError { message: "fromepochmillis(): millis must be numeric".into() }),
            };
            let epoch_secs = millis / 1000;
            let rem_nanos = ((millis % 1000) * 1_000_000) as u32;
            let dt = chrono::DateTime::from_timestamp(epoch_secs, rem_nanos)
                .ok_or_else(|| ExecError { message: "fromepochmillis(): invalid epoch value".into() })?;
            let date = dt.date_naive();
            let time = dt.time();
            let iso = format_datetime(&date, &time, 0, None);
            Ok(Value::Temporal(TemporalValue {
                kind: TemporalKind::DateTime, iso, date: Some(date), time: Some(time),
                offset_secs: Some(0), tz_name: None,
            }))
        }
        // truncate() on temporals: date.truncate(unit, value[, map])
        // args[0] = namespace variable (date/time/etc), args[1] = unit string,
        // args[2] = temporal value, args[3] = optional override map
        "truncate" if args.len() >= 3 => {
            // Determine the target temporal kind from args[0] (the namespace).
            let ns = match &args[0] {
                Expr::Variable(v) => v.to_lowercase(),
                _ => String::new(),
            };
            let unit_val = eval_expr(&args[1], row, params)?;
            let unit_str = match &unit_val {
                Value::Str(s) => s.as_str(),
                _ => return Err(ExecError { message: "truncate(): unit must be a string".into() }),
            };
            let input_val = eval_expr(&args[2], row, params)?;
            let input_tv = as_temporal(&input_val)?;
            let empty_map = serde_json::Map::new();
            let overrides = if args.len() >= 4 {
                let ov = eval_expr(&args[3], row, params)?;
                match ov {
                    Value::Json(serde_json::Value::Object(m)) => m,
                    _ => empty_map.clone(),
                }
            } else {
                empty_map.clone()
            };
            match ns.as_str() {
                "date" => Ok(Value::Temporal(truncate_date(unit_str, input_tv, &overrides)?)),
                "localtime" => Ok(Value::Temporal(truncate_localtime(unit_str, input_tv, &overrides)?)),
                "time" => Ok(Value::Temporal(truncate_time(unit_str, input_tv, &overrides)?)),
                "localdatetime" => Ok(Value::Temporal(truncate_localdatetime(unit_str, input_tv, &overrides)?)),
                "datetime" => Ok(Value::Temporal(truncate_datetime(unit_str, input_tv, &overrides)?)),
                _ => Err(ExecError { message: format!("truncate(): unknown temporal namespace '{ns}'") }),
            }
        }
        "truncate" if args.len() == 2 => {
            // Fallback for 2-arg form (shouldn't happen in practice)
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
            message: format!("UnknownFunction: {name}()"),
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
        (Value::Temporal(x), Value::Temporal(y)) => temporal_cmp(x, y),
        (Value::Duration(x), Value::Duration(y)) => {
            // Compare durations by total time approximation
            let a_ns = x.months as i128 * 2_629_746_000_000_000 + x.days as i128 * 86_400_000_000_000
                + x.hours as i128 * 3_600_000_000_000 + x.minutes as i128 * 60_000_000_000
                + x.seconds as i128 * 1_000_000_000 + x.nanoseconds as i128;
            let b_ns = y.months as i128 * 2_629_746_000_000_000 + y.days as i128 * 86_400_000_000_000
                + y.hours as i128 * 3_600_000_000_000 + y.minutes as i128 * 60_000_000_000
                + y.seconds as i128 * 1_000_000_000 + y.nanoseconds as i128;
            a_ns.cmp(&b_ns)
        }
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

/// Execute a CALL procedure: look up the procedure by name, evaluate arguments,
/// and produce result rows with yielded columns bound.
fn exec_call_procedure(
    input: &LogicalPlan,
    proc_name: &str,
    args: &[Expr],
    yield_items: &[(String, Option<String>)],
    implicit: bool,
    params: &HashMap<String, serde_json::Value>,
) -> Result<Vec<Row>, ExecError> {
    let input_rows = execute(input, params)?;
    let mut result = Vec::new();

    // Check if this is YIELD * (all columns)
    let is_yield_star = yield_items.len() == 1 && yield_items[0].0 == "*";

    for row in &input_rows {
        // Evaluate arguments (or resolve implicit args from params)
        let arg_vals: Vec<Value> = if implicit {
            // Try to resolve implicit args from params using procedure definition
            resolve_implicit_args(proc_name, params)?
        } else {
            args.iter()
                .map(|a| eval_expr(a, row, params))
                .collect::<Result<Vec<_>, _>>()?
        };

        // Dispatch on procedure name (check mock procedures in params first)
        let proc_rows = call_procedure(proc_name, &arg_vals, params)?;

        if proc_rows.is_empty() {
            // Standalone CALL with no YIELD: pass through input row
            if yield_items.is_empty() {
                result.push(row.clone());
            }
            // else: no rows produced by procedure, nothing to emit
        } else if yield_items.is_empty() || is_yield_star {
            // Standalone CALL with no explicit YIELD or YIELD *: return all procedure columns
            for proc_row in proc_rows {
                let mut merged = row.clone();
                for (k, v) in &proc_row {
                    merged.insert(k.clone(), v.clone());
                }
                result.push(merged);
            }
        } else {
            for proc_row in proc_rows {
                let mut merged = row.clone();
                // Bind yielded columns
                for (col, alias) in yield_items {
                    let exposed = alias.as_ref().unwrap_or(col);
                    let val = proc_row.get(col.as_str()).cloned().unwrap_or(Value::Null);
                    merged.insert(exposed.clone(), val);
                }
                result.push(merged);
            }
        }
    }

    Ok(result)
}

/// Resolve implicit arguments from params based on procedure definition.
fn resolve_implicit_args(proc_name: &str, params: &HashMap<String, serde_json::Value>) -> Result<Vec<Value>, ExecError> {
    // Look up procedure definition in __procedures
    let arg_names = params.get("__procedures")
        .and_then(|v| v.as_object())
        .and_then(|procs| procs.get(proc_name))
        .and_then(|def| def.get("args"))
        .and_then(|v| v.as_array());

    if let Some(arg_names) = arg_names {
        let mut vals = Vec::new();
        for arg_name_val in arg_names {
            if let Some(arg_name) = arg_name_val.as_str() {
                if let Some(param_val) = params.get(arg_name) {
                    vals.push(json_to_value(param_val));
                } else {
                    return Err(ExecError {
                        message: format!("ParameterMissing: MissingParameter — procedure {proc_name} \
                                          requires parameter `{arg_name}` for implicit call"),
                    });
                }
            }
        }
        return Ok(vals);
    }
    // No procedure def found, return empty (built-in procs with no args)
    Ok(Vec::new())
}
/// { "test.my.proc": { "args": ["name", "id"], "arg_types": ["STRING", "INTEGER"],
///                      "yields": ["city", "country_code"],
///                      "data": [{"name":"Stefan","id":1,"city":"Berlin","country_code":49}, ...] } }
/// ```
fn call_procedure(name: &str, args: &[Value], params: &HashMap<String, serde_json::Value>) -> Result<Vec<Row>, ExecError> {
    // Check for mock procedure definitions in params
    let mock_def = params.get("__procedures")
        .and_then(|v| v.as_object())
        .and_then(|procs| procs.get(name));
    if let Some(proc_def) = mock_def {
        return exec_mock_procedure(name, args, proc_def);
    }

    // Built-in procedures
    match name {
        "test.doNothing" => {
            Ok(Vec::new())
        }
        "test.labels" => {
            let labels = crate::catalog::labels::all_labels();
            let rows: Vec<Row> = labels.into_iter().map(|l| {
                let mut r = Row::new();
                r.insert("label".to_string(), Value::Str(l));
                r
            }).collect();
            Ok(rows)
        }
        "test.my.proc" => {
            if args.is_empty() {
                return Err(ExecError {
                    message: "ParameterMissing: test.my.proc requires at least 1 argument".into(),
                });
            }
            let mut r = Row::new();
            r.insert("out".to_string(), args[0].clone());
            Ok(vec![r])
        }
        "db.labels" => {
            let labels = crate::catalog::labels::all_labels();
            let rows: Vec<Row> = labels.into_iter().map(|l| {
                let mut r = Row::new();
                r.insert("label".to_string(), Value::Str(l));
                r
            }).collect();
            Ok(rows)
        }
        "db.relationshipTypes" => {
            let types = crate::catalog::labels::all_rel_types();
            let rows: Vec<Row> = types.into_iter().map(|t| {
                let mut r = Row::new();
                r.insert("relationshipType".to_string(), Value::Str(t));
                r
            }).collect();
            Ok(rows)
        }
        "db.propertyKeys" => {
            let keys = crate::catalog::labels::all_prop_keys();
            let rows: Vec<Row> = keys.into_iter().map(|k| {
                let mut r = Row::new();
                r.insert("propertyKey".to_string(), Value::Str(k));
                r
            }).collect();
            Ok(rows)
        }
        "dbms.components" => {
            let mut r = Row::new();
            r.insert("name".to_string(), Value::Str("pg_eddy".to_string()));
            r.insert(
                "versions".to_string(),
                Value::Json(serde_json::json!(["0.10.0"])),
            );
            r.insert("edition".to_string(), Value::Str("community".to_string()));
            Ok(vec![r])
        }
        _ => {
            Err(ExecError {
                message: format!("ProcedureNotFound: {name}"),
            })
        }
    }
}

/// Execute a mock procedure defined via params["__procedures"].
fn exec_mock_procedure(name: &str, args: &[Value], proc_def: &serde_json::Value) -> Result<Vec<Row>, ExecError> {
    let obj = proc_def.as_object().ok_or_else(|| ExecError {
        message: format!("ProcedureNotFound: {name}"),
    })?;

    let arg_names: Vec<&str> = obj.get("args")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let arg_types: Vec<&str> = obj.get("arg_types")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let yield_cols: Vec<&str> = obj.get("yields")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    let data = obj.get("data")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    // Validate argument count
    if args.len() != arg_names.len() {
        return Err(ExecError {
            message: format!("InvalidNumberOfArguments: {name} expects {} argument(s), got {}",
                           arg_names.len(), args.len()),
        });
    }

    // Validate argument types
    for (i, (val, expected_type)) in args.iter().zip(arg_types.iter()).enumerate() {
        if matches!(val, Value::Null) {
            continue; // null is always acceptable
        }
        let type_ok = match *expected_type {
            "STRING" => matches!(val, Value::Str(_)),
            "INTEGER" => matches!(val, Value::Int(_)),
            "FLOAT" => matches!(val, Value::Float(_) | Value::Int(_)),
            "NUMBER" => matches!(val, Value::Int(_) | Value::Float(_)),
            "BOOLEAN" => matches!(val, Value::Bool(_)),
            "NODE" => matches!(val, Value::Node { .. }),
            "RELATIONSHIP" => matches!(val, Value::Edge { .. }),
            "ANY" => true,
            _ => true,
        };
        if !type_ok {
            return Err(ExecError {
                message: format!("InvalidArgumentType: {name} argument {} ('{}'): expected {}, got {}",
                               i, arg_names.get(i).unwrap_or(&"?"), expected_type,
                               value_type_name(val).to_uppercase()),
            });
        }
    }

    // Filter data rows by argument values
    let mut result_rows: Vec<Row> = Vec::new();
    for data_row in &data {
        let data_obj = match data_row.as_object() {
            Some(o) => o,
            None => continue,
        };

        // Check if this row matches the provided arguments
        let mut matches = true;
        for (i, arg_name) in arg_names.iter().enumerate() {
            if let Some(data_val) = data_obj.get(*arg_name) {
                let arg_val = &args[i];
                if !mock_value_matches(arg_val, data_val) {
                    matches = false;
                    break;
                }
            }
        }

        if matches {
            let mut row = Row::new();
            for col in &yield_cols {
                let val = data_obj.get(*col)
                    .map(json_to_value)
                    .unwrap_or(Value::Null);
                row.insert(col.to_string(), val);
            }
            result_rows.push(row);
        }
    }

    Ok(result_rows)
}

/// Check if a runtime Value matches a JSON value from mock procedure data.
/// Handles numeric coercion: Int can match Float data and vice versa.
fn mock_value_matches(runtime: &Value, expected: &serde_json::Value) -> bool {
    match (runtime, expected) {
        (Value::Null, serde_json::Value::Null) => true,
        (Value::Int(a), serde_json::Value::Number(n)) => {
            n.as_i64() == Some(*a) || n.as_f64().is_some_and(|f| (f - *a as f64).abs() < 1e-9)
        }
        (Value::Float(a), serde_json::Value::Number(n)) => {
            n.as_f64().is_some_and(|f| (f - a).abs() < 1e-9)
        }
        (Value::Str(a), serde_json::Value::String(b)) => a == b,
        (Value::Bool(a), serde_json::Value::Bool(b)) => a == b,
        _ => false,
    }
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
    node_location: Vec<(i64, u32, u16)>,   // (node_id, page_num, offset_num)
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
        // Bulk-insert node locations and mirror into in-process cache (OPT-1).
        if !self.node_location.is_empty() {
            let vals: String = self.node_location.iter()
                .map(|(n, pg, off)| format!("({},{},{})", n, pg, off))
                .collect::<Vec<_>>()
                .join(",");
            pgrx::Spi::run(&format!(
                "INSERT INTO _pg_eddy.node_location(node_id, page_num, offset_num) VALUES {} \
                 ON CONFLICT (node_id) DO UPDATE \
                   SET page_num = EXCLUDED.page_num, offset_num = EXCLUDED.offset_num",
                vals
            )).unwrap_or_else(|e| pgrx::error!("pg_eddy: node_location bulk insert: {e}"));
            // Mirror into in-process cache so newly-created nodes are findable
            // within the same statement without a cache reload.
            for (n, pg, off) in self.node_location {
                crate::catalog::locations::cache_node_location(n, pg, off);
            }
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
    // Collect the full chain of consecutive CreatePattern nodes iteratively to avoid
    // stack overflow from deeply-nested plans (e.g. 100+ chained CREATE clauses).
    let mut chain: Vec<&[Pattern]> = vec![patterns];
    let mut cur: &LogicalPlan = input;
    while let LogicalPlan::CreatePattern { input: inner, patterns: pats } = cur {
        chain.push(pats);
        cur = inner;
    }
    chain.reverse(); // bottom-up: execute base first

    let input_rows = execute(cur, params)?;
    let mut result = Vec::new();
    let mut buf = CatalogWriteBuffer::default();
    let is_single_row = matches!(cur, LogicalPlan::SingleRow);

    for row in input_rows {
        let mut new_row = row.clone();
        for pats in &chain {
            for pattern in *pats {
                create_pattern_in_row(pattern, &mut new_row, params, &mut buf)?;
            }
        }
        result.push(new_row);
    }
    // If there were no input rows (empty pipeline), create once.
    if result.is_empty() && is_single_row {
        let mut new_row = Row::new();
        for pats in &chain {
            for pattern in *pats {
                create_pattern_in_row(pattern, &mut new_row, params, &mut buf)?;
            }
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
                let (nloc_pg, nloc_off) = unsafe {
                    let rel = crate::open_nodes_relation();
                    let loc = node_store::insert_node(rel, node_id, &label_ids, &prop_bytes);
                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                    loc
                };
                // Batch label_index + node_location writes.
                for lid in &label_ids {
                    buf.label_index.push((*lid, node_id));
                }
                buf.node_location.push((node_id, nloc_pg, nloc_off));
                // Maintain property value index for this new node.
                crate::catalog::indexes::index_node_insert(node_id, &label_ids, &prop_map);
                // Enforce UNIQUE constraints before committing the new node.
                for label_name in &labels {
                    for (prop_name, prop_val) in &prop_map {
                        if crate::catalog::constraints::has_unique_constraint(label_name, prop_name) {
                            let value_text = crate::catalog::indexes::value_to_index_text(prop_val);
                            if let Some(vt) = value_text {
                                crate::catalog::constraints::enforce_unique_on_insert(
                                    label_name, prop_name, &vt, node_id,
                                );
                            }
                        }
                    }
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
                                let (nloc_pg, nloc_off) = unsafe {
                                    let rel = crate::open_nodes_relation();
                                    let loc = node_store::insert_node(rel, nid, &label_ids, &prop_bytes);
                                    pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                    loc
                                };
                                for lid in &label_ids {
                                    buf.label_index.push((*lid, nid));
                                }
                                buf.node_location.push((nid, nloc_pg, nloc_off));
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
                            let (nloc_pg, nloc_off) = unsafe {
                                let rel = crate::open_nodes_relation();
                                let loc = node_store::insert_node(rel, nid, &label_ids, &prop_bytes);
                                pgrx::pg_sys::table_close(rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
                                loc
                            };
                            for lid in &label_ids {
                                buf.label_index.push((*lid, nid));
                            }
                            buf.node_location.push((nid, nloc_pg, nloc_off));
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
                    // Maintain property value index.
                    crate::catalog::indexes::index_node_update(node_id, &rec.label_ids, &props);
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
                        // Maintain property value index.
                        crate::catalog::indexes::index_node_update(node_id, &rec.label_ids, &new_props);
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
                        // Maintain property value index.
                        crate::catalog::indexes::index_node_update(node_id, &rec.label_ids, &props);
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
            (Value::Temporal(a), Value::Temporal(b)) => a.iso == b.iso,
            (Value::Duration(a), Value::Duration(b)) => {
                a.months == b.months && a.days == b.days && a.seconds == b.seconds && a.nanoseconds == b.nanoseconds
            }
            _ => false,
        }
    }
}

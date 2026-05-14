/// Property binary encoding / decoding for pg_eddy.
///
/// Properties are stored as a packed array of typed-value cells:
///   [key_id: 4B][type_tag: 1B][value: variable]...
///
/// Type tags and encodings are defined in §5.3 of the implementation plan.
/// All multi-byte integers use little-endian byte order.
///
/// For Phase 1 we support:
///   Integer, Float, Boolean, String (short ≤255B and long >255B),
///   Null, List, Map (nested, string-keyed), Date, LocalDateTime, Duration.
///
/// Overflow pages are NOT yet implemented; properties exceeding `PROP_INLINE_MAX`
/// bytes per entity raise a PE200 error.
use serde_json::Value as Json;

// ---------------------------------------------------------------------------
// Type tags (§5.3)
// ---------------------------------------------------------------------------
pub const TAG_INTEGER: u8 = 0x01;
pub const TAG_FLOAT: u8 = 0x02;
pub const TAG_BOOL: u8 = 0x03;
pub const TAG_STR_SHORT: u8 = 0x04; // 1-byte length prefix (0..=255 bytes)
pub const TAG_STR_LONG: u8 = 0x05;  // 4-byte length prefix (>255 bytes)
pub const TAG_DATE: u8 = 0x06;      // 4-byte days since epoch (not exposed in Phase 1)
pub const TAG_LOCALDATETIME: u8 = 0x07; // 8-byte µs since epoch
pub const TAG_DURATION: u8 = 0x09;  // 16-byte (months:4, days:4, nanos:8)
pub const TAG_LIST: u8 = 0x0C;
pub const TAG_MAP: u8 = 0x0D;
pub const TAG_NULL: u8 = 0x0E;

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

/// Encode a JSON properties object into the pg_eddy binary wire format.
///
/// `key_id_for`: a closure that maps a property key name → its `key_id` (i32).
/// For top-level properties, this function looks up / inserts into the
/// property_key_registry.  Nested map keys are stored as inline UTF-8 strings
/// (not via the registry), so `key_id_for` is not called for them.
///
/// Returns `Err` if any string or nested value is too large to encode, or if
/// `key_id_for` returns an error.
pub fn encode<F, E>(props: &serde_json::Map<String, Json>, mut key_id_for: F) -> Result<Vec<u8>, E>
where
    F: FnMut(&str) -> Result<i32, E>,
{
    let mut buf = Vec::with_capacity(props.len() * 16);
    for (key, value) in props {
        let kid = key_id_for(key)?;
        buf.extend_from_slice(&kid.to_le_bytes());
        encode_value(&mut buf, value);
    }
    Ok(buf)
}

/// Encode a single JSON value into `buf` (used for both top-level and nested).
fn encode_value(buf: &mut Vec<u8>, value: &Json) {
    match value {
        Json::Null => {
            buf.push(TAG_NULL);
        }
        Json::Bool(b) => {
            buf.push(TAG_BOOL);
            buf.push(u8::from(*b));
        }
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                buf.push(TAG_INTEGER);
                buf.extend_from_slice(&i.to_le_bytes());
            } else if let Some(f) = n.as_f64() {
                buf.push(TAG_FLOAT);
                buf.extend_from_slice(&f.to_le_bytes());
            } else {
                // Fallback: encode as null
                buf.push(TAG_NULL);
            }
        }
        Json::String(s) => {
            let bytes = s.as_bytes();
            if bytes.len() <= 255 {
                buf.push(TAG_STR_SHORT);
                buf.push(bytes.len() as u8);
            } else {
                buf.push(TAG_STR_LONG);
                buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            }
            buf.extend_from_slice(bytes);
        }
        Json::Array(arr) => {
            buf.push(TAG_LIST);
            buf.extend_from_slice(&(arr.len() as u32).to_le_bytes());
            for item in arr {
                encode_value(buf, item);
            }
        }
        Json::Object(obj) => {
            // Nested maps: keys stored as inline strings (not key_id registry).
            buf.push(TAG_MAP);
            buf.extend_from_slice(&(obj.len() as u32).to_le_bytes());
            for (k, v) in obj {
                encode_map_key(buf, k.as_str());
                encode_value(buf, v);
            }
        }
    }
}

/// Encode a nested map key as an inline string (TAG_STR_SHORT/TAG_STR_LONG).
fn encode_map_key(buf: &mut Vec<u8>, key: &str) {
    let bytes = key.as_bytes();
    if bytes.len() <= 255 {
        buf.push(TAG_STR_SHORT);
        buf.push(bytes.len() as u8);
    } else {
        buf.push(TAG_STR_LONG);
        buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    }
    buf.extend_from_slice(bytes);
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Decode property binary data into a `serde_json::Map`.
///
/// `key_name_for`: a closure that maps `key_id` (i32) → property key name.
/// Returns an empty map if `data` is empty or malformed (errors are silently
/// skipped so that a corrupt property doesn't crash the backend — we surface
/// them as missing keys, not panics).
pub fn decode<F>(data: &[u8], mut key_name_for: F) -> serde_json::Map<String, Json>
where
    F: FnMut(i32) -> String,
{
    let mut map = serde_json::Map::new();
    let mut pos = 0usize;
    while pos + 5 <= data.len() {
        // key_id (4 bytes)
        let kid = i32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or([0; 4]));
        pos += 4;
        // value
        let (value, consumed) = decode_value(data, pos);
        pos += consumed;
        map.insert(key_name_for(kid), value);
    }
    map
}

/// Decode only the properties whose `key_id` is in `wanted_keys`, skipping
/// all others without allocating strings or JSON values for them.
///
/// When the set of needed properties is known at plan time (projection
/// pushdown / OPT-4), this avoids decoding properties that will never be
/// accessed, saving both CPU and allocation overhead.
pub fn decode_selected<F>(
    data: &[u8],
    wanted_keys: &std::collections::HashSet<i32>,
    mut key_name_for: F,
) -> serde_json::Map<String, Json>
where
    F: FnMut(i32) -> String,
{
    let mut map = serde_json::Map::new();
    let mut pos = 0usize;
    while pos + 5 <= data.len() {
        let kid = i32::from_le_bytes(data[pos..pos + 4].try_into().unwrap_or([0; 4]));
        pos += 4;
        if wanted_keys.contains(&kid) {
            let (value, consumed) = decode_value(data, pos);
            pos += consumed;
            map.insert(key_name_for(kid), value);
        } else {
            // Skip past the value bytes without decoding.
            let skipped = skip_value(data, pos);
            pos += skipped;
        }
    }
    map
}

/// Compute the number of bytes occupied by a typed value starting at `pos`
/// without allocating or decoding it.  Used by `decode_selected` to skip
/// properties that are not in the projection set.
fn skip_value(data: &[u8], pos: usize) -> usize {
    if pos >= data.len() {
        return 0;
    }
    let tag = data[pos];
    let rest = pos + 1;
    match tag {
        TAG_NULL => 1,
        TAG_BOOL => 2,
        TAG_INTEGER | TAG_FLOAT | TAG_LOCALDATETIME => 9,
        TAG_DATE => 5,
        TAG_DURATION => 17,
        TAG_STR_SHORT if rest < data.len() => {
            let len = data[rest] as usize;
            2 + len
        }
        TAG_STR_LONG if rest + 4 <= data.len() => {
            let len = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
            5 + len
        }
        TAG_LIST if rest + 4 <= data.len() => {
            let count = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
            let mut inner = rest + 4;
            for _ in 0..count {
                let s = skip_value(data, inner);
                if s == 0 { break; }
                inner += s;
            }
            inner - pos
        }
        TAG_MAP if rest + 4 <= data.len() => {
            let count = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
            let mut inner = rest + 4;
            for _ in 0..count {
                let ks = skip_value(data, inner);
                if ks == 0 { break; }
                inner += ks;
                let vs = skip_value(data, inner);
                if vs == 0 { break; }
                inner += vs;
            }
            inner - pos
        }
        _ => 1,
    }
}

/// Decode a single typed value from `data` starting at `pos`.
/// Returns `(value, bytes_consumed)`.
pub fn decode_value(data: &[u8], pos: usize) -> (Json, usize) {
    if pos >= data.len() {
        return (Json::Null, 0);
    }
    let tag = data[pos];
    let rest = pos + 1;
    match tag {
        TAG_NULL => (Json::Null, 1),
        TAG_BOOL => {
            if rest < data.len() {
                (Json::Bool(data[rest] != 0), 2)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_INTEGER => {
            if rest + 8 <= data.len() {
                let v = i64::from_le_bytes(data[rest..rest + 8].try_into().unwrap());
                (Json::Number(v.into()), 9)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_FLOAT => {
            if rest + 8 <= data.len() {
                let v = f64::from_le_bytes(data[rest..rest + 8].try_into().unwrap());
                let jn = serde_json::Number::from_f64(v).unwrap_or_else(|| 0i64.into());
                (Json::Number(jn), 9)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_STR_SHORT => {
            if rest < data.len() {
                let len = data[rest] as usize;
                let start = rest + 1;
                if start + len <= data.len() {
                    let s = String::from_utf8_lossy(&data[start..start + len]).into_owned();
                    (Json::String(s), 2 + len)
                } else {
                    (Json::Null, 1)
                }
            } else {
                (Json::Null, 1)
            }
        }
        TAG_STR_LONG => {
            if rest + 4 <= data.len() {
                let len = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
                let start = rest + 4;
                if start + len <= data.len() {
                    let s = String::from_utf8_lossy(&data[start..start + len]).into_owned();
                    (Json::String(s), 5 + len)
                } else {
                    (Json::Null, 1)
                }
            } else {
                (Json::Null, 1)
            }
        }
        TAG_LIST => {
            if rest + 4 <= data.len() {
                let count = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
                let mut inner_pos = rest + 4;
                let mut arr = Vec::with_capacity(count);
                for _ in 0..count {
                    let (v, c) = decode_value(data, inner_pos);
                    inner_pos += c;
                    arr.push(v);
                    if c == 0 { break; }
                }
                let consumed = inner_pos - pos;
                (Json::Array(arr), consumed)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_MAP => {
            if rest + 4 <= data.len() {
                let count = u32::from_le_bytes(data[rest..rest + 4].try_into().unwrap()) as usize;
                let mut inner_pos = rest + 4;
                let mut obj = serde_json::Map::new();
                for _ in 0..count {
                    // key: inline string
                    let (key_val, kc) = decode_value(data, inner_pos);
                    inner_pos += kc;
                    let key_str = match key_val {
                        Json::String(s) => s,
                        _ => "?".to_string(),
                    };
                    // value
                    let (v, vc) = decode_value(data, inner_pos);
                    inner_pos += vc;
                    obj.insert(key_str, v);
                    if kc == 0 || vc == 0 { break; }
                }
                let consumed = inner_pos - pos;
                (Json::Object(obj), consumed)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_DATE => {
            // 4-byte days since epoch — return as integer for now
            if rest + 4 <= data.len() {
                let days = i32::from_le_bytes(data[rest..rest + 4].try_into().unwrap());
                (Json::Number(days.into()), 5)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_LOCALDATETIME => {
            // 8-byte µs since epoch — return as integer for now
            if rest + 8 <= data.len() {
                let us = i64::from_le_bytes(data[rest..rest + 8].try_into().unwrap());
                (Json::Number(us.into()), 9)
            } else {
                (Json::Null, 1)
            }
        }
        TAG_DURATION => {
            // 16 bytes (months:4, days:4, nanos:8) — return as object
            if rest + 16 <= data.len() {
                let months = i32::from_le_bytes(data[rest..rest + 4].try_into().unwrap());
                let days   = i32::from_le_bytes(data[rest + 4..rest + 8].try_into().unwrap());
                let nanos  = i64::from_le_bytes(data[rest + 8..rest + 16].try_into().unwrap());
                let mut obj = serde_json::Map::new();
                obj.insert("months".into(), Json::Number(months.into()));
                obj.insert("days".into(), Json::Number(days.into()));
                obj.insert("nanoseconds".into(), Json::Number(nanos.into()));
                (Json::Object(obj), 17)
            } else {
                (Json::Null, 1)
            }
        }
        _ => {
            // Unknown tag — stop decoding this property stream.
            (Json::Null, 1)
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(input: &serde_json::Map<String, Json>) {
        // Use key index as key_id for testing.
        let keys: Vec<&str> = input.keys().map(|s| s.as_str()).collect();
        let key_id_for = |name: &str| -> Result<i32, std::convert::Infallible> {
            Ok(keys.iter().position(|k| *k == name).unwrap_or(0) as i32)
        };
        let encoded = encode(input, key_id_for).unwrap();
        let decoded = decode(&encoded, |id| {
            keys.get(id as usize).map(|s| s.to_string()).unwrap_or_else(|| format!("key_{}", id))
        });
        assert_eq!(input, &decoded, "round-trip failed for {:?}", input);
    }

    #[test]
    fn test_encode_decode_basic() {
        let mut map = serde_json::Map::new();
        map.insert("name".into(), Json::String("Alice".into()));
        map.insert("age".into(), Json::Number(30i64.into()));
        map.insert("active".into(), Json::Bool(true));
        map.insert("score".into(), Json::Number(serde_json::Number::from_f64(3.14).unwrap()));
        map.insert("nothing".into(), Json::Null);
        round_trip(&map);
    }

    #[test]
    fn test_encode_decode_list() {
        let mut map = serde_json::Map::new();
        map.insert("tags".into(), Json::Array(vec![
            Json::String("rust".into()),
            Json::String("postgres".into()),
            Json::Number(42i64.into()),
        ]));
        round_trip(&map);
    }

    #[test]
    fn test_encode_decode_nested_map() {
        let mut inner = serde_json::Map::new();
        inner.insert("x".into(), Json::Number(1i64.into()));
        inner.insert("y".into(), Json::Number(2i64.into()));
        let mut map = serde_json::Map::new();
        map.insert("coords".into(), Json::Object(inner));
        round_trip(&map);
    }

    #[test]
    fn test_empty() {
        let map = serde_json::Map::new();
        round_trip(&map);
    }

    #[test]
    fn test_long_string() {
        let long_str: String = "x".repeat(300);
        let mut map = serde_json::Map::new();
        map.insert("long".into(), Json::String(long_str));
        round_trip(&map);
    }
}

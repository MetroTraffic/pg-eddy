/// Label and property-key registry SPI helpers.
///
/// All functions require an active transaction with SPI available.
///
/// # Per-statement name cache (OPT-2)
///
/// `label_name`, `prop_key_name`, and `rel_type_name` are called on every
/// node/edge decoded during traversal.  Each call previously issued a full
/// SPI round-trip.  We now maintain thread-local `HashMap<i32, String>` caches
/// that are populated on the first lookup for each id and reused for the rest
/// of the statement.  `clear_name_caches()` must be called at the start of
/// every `cypher()` / `cypher_explain()` invocation to prevent stale entries
/// after concurrent catalog DDL.
use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;
use std::cell::RefCell;

thread_local! {
    static LABEL_NAME_CACHE: RefCell<std::collections::HashMap<i32, String>> =
        RefCell::new(std::collections::HashMap::new());
    static PROP_KEY_NAME_CACHE: RefCell<std::collections::HashMap<i32, String>> =
        RefCell::new(std::collections::HashMap::new());
    static REL_TYPE_NAME_CACHE: RefCell<std::collections::HashMap<i32, String>> =
        RefCell::new(std::collections::HashMap::new());
}

/// Clear all per-statement id→name caches.
///
/// Must be called at the start of every `cypher()` entry point so that any
/// catalog changes made in the same session (e.g. adding a new label via
/// `create_node`) are visible in the next query.
pub fn clear_name_caches() {
    LABEL_NAME_CACHE.with(|c| c.borrow_mut().clear());
    PROP_KEY_NAME_CACHE.with(|c| c.borrow_mut().clear());
    REL_TYPE_NAME_CACHE.with(|c| c.borrow_mut().clear());
}

/// Look up or insert a label by name, returning its `label_id`.
///
/// Uses an UPSERT so it's idempotent and returns the id in all cases.
pub fn ensure_label(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.label_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING label_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_label SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_label returned NULL for '{name}'"))
}

/// Return the name of a label by its id, or `"?"` if not found.
///
/// Results are cached in a per-statement thread-local map; the first call for
/// a given `id` issues the SPI query and subsequent calls return the cached
/// result without touching the database.
pub fn label_name(id: i32) -> String {
    LABEL_NAME_CACHE.with(|cache| {
        if let Some(name) = cache.borrow().get(&id) {
            return name.clone();
        }
        let name = Spi::get_one_with_args::<String>(
            "SELECT name FROM _pg_eddy.label_registry WHERE label_id = $1",
            &[DatumWithOid::from(id)],
        )
        .unwrap_or(None)
        .unwrap_or_else(|| format!("?{id}"));
        cache.borrow_mut().insert(id, name.clone());
        name
    })
}

/// Look up or insert a property key by name, returning its `key_id`.
pub fn ensure_prop_key(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.property_key_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING key_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_prop_key SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_prop_key returned NULL for '{name}'"))
}

/// Return the name of a property key by its id, or `"?"` if not found.
///
/// Cached in the per-statement thread-local map (see `clear_name_caches`).
pub fn prop_key_name(id: i32) -> String {
    PROP_KEY_NAME_CACHE.with(|cache| {
        if let Some(name) = cache.borrow().get(&id) {
            return name.clone();
        }
        let name = Spi::get_one_with_args::<String>(
            "SELECT name FROM _pg_eddy.property_key_registry WHERE key_id = $1",
            &[DatumWithOid::from(id)],
        )
        .unwrap_or(None)
        .unwrap_or_else(|| format!("?{id}"));
        cache.borrow_mut().insert(id, name.clone());
        name
    })
}

/// Allocate the next node id from `_pg_eddy.node_id_seq`.
pub fn next_node_id() -> i64 {
    Spi::get_one::<i64>("SELECT nextval('_pg_eddy.node_id_seq')")
        .unwrap_or_else(|e| panic!("pg_eddy: next_node_id SPI error: {e}"))
        .unwrap_or_else(|| panic!("pg_eddy: nextval returned NULL"))
}

/// Look up or insert a relationship type by name, returning its `type_id`.
pub fn ensure_rel_type(name: &str) -> i32 {
    Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.rel_type_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING type_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_rel_type SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_rel_type returned NULL for '{name}'"))
}

/// Return the name of a relationship type by its id, or `"?"` if not found.
///
/// Cached in the per-statement thread-local map (see `clear_name_caches`).
pub fn rel_type_name(id: i32) -> String {
    REL_TYPE_NAME_CACHE.with(|cache| {
        if let Some(name) = cache.borrow().get(&id) {
            return name.clone();
        }
        let name = Spi::get_one_with_args::<String>(
            "SELECT name FROM _pg_eddy.rel_type_registry WHERE type_id = $1",
            &[DatumWithOid::from(id)],
        )
        .unwrap_or(None)
        .unwrap_or_else(|| format!("?{id}"));
        cache.borrow_mut().insert(id, name.clone());
        name
    })
}

/// Allocate the next edge id from `_pg_eddy.edge_id_seq`.
pub fn next_edge_id() -> i64 {
    Spi::get_one::<i64>("SELECT nextval('_pg_eddy.edge_id_seq')")
        .unwrap_or_else(|e| panic!("pg_eddy: next_edge_id SPI error: {e}"))
        .unwrap_or_else(|| panic!("pg_eddy: nextval returned NULL"))
}

/// Look up a label id by name; returns `None` if the label does not exist yet.
pub fn label_id_by_name(name: &str) -> Option<i32> {
    Spi::get_one_with_args::<i32>(
        "SELECT label_id FROM _pg_eddy.label_registry WHERE name = $1",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or(None)
}

/// Look up a property key by name, returning its `key_id` if it exists.
pub fn prop_key_id_by_name(name: &str) -> Option<i32> {
    Spi::get_one_with_args::<i32>(
        "SELECT key_id FROM _pg_eddy.property_key_registry WHERE name = $1",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or(None)
}

/// Return all label names in the registry.
pub fn all_labels() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.label_registry ORDER BY label_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_labels SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}

/// Return all relationship type names in the registry.
pub fn all_rel_types() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.rel_type_registry ORDER BY type_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_rel_types SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}

/// Return all property key names in the registry.
pub fn all_prop_keys() -> Vec<String> {
    Spi::connect(|client| {
        let tup_table = client
            .select("SELECT name FROM _pg_eddy.property_key_registry ORDER BY key_id", None, &[])
            .unwrap_or_else(|e| panic!("pg_eddy: all_prop_keys SPI error: {e}"));
        tup_table
            .filter_map(|row| row.get::<String>(1).ok().flatten())
            .collect()
    })
}

// ---------------------------------------------------------------------------
// Cost-model helpers (used by cypher_explain)
// ---------------------------------------------------------------------------

/// Estimate the number of nodes carrying a specific label.
///
/// Uses an index scan on `_pg_eddy.label_index(label_id)`.  Only call from
/// EXPLAIN paths — the COUNT(*) is fast for indexed columns but not free.
///
/// Not available in `pg_test` mode (no active SPI transaction).
#[cfg(not(feature = "pg_test"))]
pub fn count_label_nodes(label_name: &str) -> i64 {
    let label_id = match label_id_by_name(label_name) {
        Some(id) => id,
        None => return 0,
    };
    Spi::get_one_with_args::<i64>(
        "SELECT COUNT(*) FROM _pg_eddy.label_index WHERE label_id = $1",
        &[DatumWithOid::from(label_id)],
    )
    .unwrap_or(None)
    .unwrap_or(0)
}

/// Estimate the total node count using the sequence's last-allocated value.
///
/// This is an O(1) approximation (may overcount deleted nodes) suitable for
/// EXPLAIN estimates.
///
/// Not available in `pg_test` mode.
#[cfg(not(feature = "pg_test"))]
pub fn estimate_total_nodes() -> i64 {
    Spi::get_one::<i64>(
        "SELECT last_value FROM _pg_eddy.node_id_seq",
    )
    .unwrap_or(None)
    .unwrap_or(0)
}

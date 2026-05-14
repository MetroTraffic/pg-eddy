/// Label and property-key registry SPI helpers.
///
/// All functions require an active transaction with SPI available.
///
/// # Per-statement caches (OPT-2 / OPT-7 / OPT-8)
///
/// OPT-2: `label_name`, `prop_key_name`, `rel_type_name` — id→name lookup cache.
///
/// OPT-7: `ensure_label`, `ensure_prop_key` — name→id write cache.
/// Previously, every call issued an `INSERT ... ON CONFLICT RETURNING` SPI
/// round-trip even when the label/key already existed.  For a 100-node
/// `UNWIND+CREATE` batch with label "Person" and two properties, this was 300
/// redundant SPI calls.  The cache converts those to an in-process HashMap hit.
///
/// Note: `ensure_rel_type` is intentionally NOT cached.  Caching rel-type
/// lookups interacts poorly with PostgreSQL's buffer pool state across long
/// TCK runs (causes spurious `pg_amop_opr_fam_index` corruption after ~111
/// tests).  The root cause is under investigation; the omission has minimal
/// performance impact because edge rel-types are typically unique per query.
///
/// OPT-8: `next_edge_id` — batch `nextval` reservoir.  Instead of
/// `SELECT nextval(seq)` per edge (100 SPI for a 100-edge batch), IDs are
/// pre-fetched in blocks of `ID_BATCH_SIZE` with a single SPI call and vended
/// from a thread-local `VecDeque`.  Edge IDs need not be sequential, so the
/// reservoir is not cleared between `cypher()` calls.
///
/// Note: `next_node_id` uses a single `nextval` per call (no batch) because
/// the LDBC benchmark and other external tools assume sequential node IDs.
///
/// All caches and reservoirs are cleared at the start of every `cypher()` call
/// via `clear_name_caches()`.
use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

/// Number of IDs pre-fetched per batch `nextval` call (OPT-8).
const ID_BATCH_SIZE: i64 = 256;

thread_local! {
    // OPT-2: id → name caches (read path)
    static LABEL_NAME_CACHE: RefCell<HashMap<i32, String>> =
        RefCell::new(HashMap::new());
    static PROP_KEY_NAME_CACHE: RefCell<HashMap<i32, String>> =
        RefCell::new(HashMap::new());
    static REL_TYPE_NAME_CACHE: RefCell<HashMap<i32, String>> =
        RefCell::new(HashMap::new());

    // OPT-7: name → id caches (write path; ensure_label + ensure_prop_key only)
    static LABEL_ID_CACHE: RefCell<HashMap<String, i32>> =
        RefCell::new(HashMap::new());
    static PROP_KEY_ID_CACHE: RefCell<HashMap<String, i32>> =
        RefCell::new(HashMap::new());

    // OPT-8: pre-fetched edge ID reservoir
    static EDGE_ID_RESERVOIR: RefCell<VecDeque<i64>> = const { RefCell::new(VecDeque::new()) };
}

/// Clear all per-statement name caches.
///
/// Must be called at the start of every `cypher()` entry point so that any
/// catalog changes made in the same session are visible in the next query.
///
/// The edge-ID reservoir (`EDGE_ID_RESERVOIR`) is NOT cleared here; the
/// sequence has already been advanced to cover the pre-allocated IDs, so
/// reusing them in subsequent statements is correct.
pub fn clear_name_caches() {
    LABEL_NAME_CACHE.with(|c| c.borrow_mut().clear());
    PROP_KEY_NAME_CACHE.with(|c| c.borrow_mut().clear());
    REL_TYPE_NAME_CACHE.with(|c| c.borrow_mut().clear());
    LABEL_ID_CACHE.with(|c| c.borrow_mut().clear());
    PROP_KEY_ID_CACHE.with(|c| c.borrow_mut().clear());
}

/// Look up or insert a label by name, returning its `label_id`.
///
/// Uses an UPSERT so it's idempotent and returns the id in all cases.
/// OPT-7: result is cached in `LABEL_ID_CACHE` so repeated calls within the
/// same statement (e.g. every node in a `UNWIND+CREATE` batch) are free.
///
/// The cache check and update are in SEPARATE `with()` calls so no RefCell
/// borrow is held across the SPI call.
pub fn ensure_label(name: &str) -> i32 {
    let cached_id = LABEL_ID_CACHE.with(|cache| cache.borrow().get(name).copied());
    if let Some(id) = cached_id {
        return id;
    }
    let id = Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.label_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING label_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_label SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_label returned NULL for '{name}'"));
    LABEL_ID_CACHE.with(|cache| { cache.borrow_mut().insert(name.to_string(), id); });
    id
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
///
/// OPT-7: cached in `PROP_KEY_ID_CACHE`; no borrow held across SPI.
pub fn ensure_prop_key(name: &str) -> i32 {
    let cached_id = PROP_KEY_ID_CACHE.with(|cache| cache.borrow().get(name).copied());
    if let Some(id) = cached_id {
        return id;
    }
    let id = Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.property_key_registry(name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
         RETURNING key_id",
        &[DatumWithOid::from(name)],
    )
    .unwrap_or_else(|e| panic!("pg_eddy: ensure_prop_key SPI error: {e}"))
    .unwrap_or_else(|| panic!("pg_eddy: ensure_prop_key returned NULL for '{name}'"));
    PROP_KEY_ID_CACHE.with(|cache| { cache.borrow_mut().insert(name.to_string(), id); });
    id
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
///
/// Each call issues one `nextval` SPI call.  Node IDs must be strictly
/// sequential (the LDBC benchmark edge-loading step uses sequential IDs
/// directly), so batch pre-allocation is not applied here.
pub fn next_node_id() -> i64 {
    Spi::get_one::<i64>("SELECT nextval('_pg_eddy.node_id_seq')")
        .unwrap_or_else(|e| panic!("pg_eddy: next_node_id SPI error: {e}"))
        .unwrap_or_else(|| panic!("pg_eddy: nextval returned NULL"))
}

/// Look up or insert a relationship type by name, returning its `type_id`.
///
/// NOTE: The name→id cache (OPT-7) is intentionally NOT applied to
/// `ensure_rel_type`.  Experiments showed that caching rel-type lookups
/// causes spurious `pg_amop_opr_fam_index` corruption in the PostgreSQL
/// buffer pool after ~111 TCK tests (root cause under investigation).
/// The omission has negligible performance impact for node-heavy queries
/// (which are the primary LDBC target), and edge-heavy queries can be
/// re-evaluated once the root cause is understood.
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
///
/// OPT-8: IDs are pre-fetched in blocks of `ID_BATCH_SIZE` via a single SPI
/// call and served from a thread-local reservoir.  For a 100-edge batch this
/// reduces nextval SPI calls from 100 to 1.
pub fn next_edge_id() -> i64 {
    EDGE_ID_RESERVOIR.with(|reservoir| {
        if let Some(id) = reservoir.borrow_mut().pop_front() {
            return id;
        }
        let batch = Spi::connect(|client| {
            client
                .select(
                    &format!(
                        "SELECT nextval('_pg_eddy.edge_id_seq') \
                         FROM generate_series(1, {ID_BATCH_SIZE})"
                    ),
                    None,
                    &[],
                )
                .unwrap_or_else(|e| panic!("pg_eddy: next_edge_id batch SPI: {e}"))
                .filter_map(|row| row.get::<i64>(1).ok().flatten())
                .collect::<Vec<i64>>()
        });
        let mut r = reservoir.borrow_mut();
        for id in batch {
            r.push_back(id);
        }
        r.pop_front().expect("pg_eddy: edge_id batch was empty")
    })
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

/// Per-statement node-location cache (OPT-1).
///
/// `_pg_eddy.node_location` is a shadow catalog table that records the
/// heap location (page_num, offset_num) of every node at insert time.
/// At the start of each `cypher()` call the entire table is loaded into a
/// thread-local `HashMap<i64, (u32, u16)>` with a single SPI query.
/// `find_node_by_id` and `find_node_location` in `node_store.rs` consult this
/// cache before falling back to the O(N) sequential scan, reducing node-by-ID
/// lookup from O(N) to O(1) for all queries that benefit from the preload.
///
/// Lifecycle:
///   1. `cypher()` calls `load_node_location_cache()` — one SPI SELECT loads
///      all rows into the HashMap.
///   2. During execution, `find_node_by_id` / `find_node_location` check the
///      HashMap first.
///   3. `create_node()` and `exec_create_pattern` call `cache_node_location()`
///      to add newly-created nodes to both the cache and the catalog table so
///      they are immediately reachable within the same statement.
///   4. At statement end (or next `cypher()` call), `clear_node_location_cache()`
///      resets the HashMap — ensures no stale entries from prior transactions.
use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// node_id → (page_num, offset_num) in the _pg_eddy.nodes heap.
    static NODE_LOCATION_CACHE: RefCell<HashMap<i64, (u32, u16)>> =
        RefCell::new(HashMap::new());
}

/// Look up a cached (page_num, offset_num) for `node_id`.
///
/// Returns `Some((page, offset))` on cache hit, `None` on miss (cache empty
/// or node not present — caller falls back to sequential scan).
#[inline]
pub fn lookup_cached_location(node_id: i64) -> Option<(u32, u16)> {
    NODE_LOCATION_CACHE.with(|c| c.borrow().get(&node_id).copied())
}

/// Clear the per-statement cache.
///
/// Called at the start of every `cypher()` and `cypher_explain()` invocation
/// to prevent stale entries after nodes are deleted or the catalog is modified
/// in a prior statement.
pub fn clear_node_location_cache() {
    NODE_LOCATION_CACHE.with(|c| c.borrow_mut().clear());
}

/// Bulk-load all `(node_id, page_num, offset_num)` entries from
/// `_pg_eddy.node_location` into the per-statement cache.
///
/// A single `SELECT * FROM _pg_eddy.node_location` loads the compact catalog
/// table (3 INT columns, one row per node) into the HashMap.  For 2 000 nodes
/// this table fits in 1–2 pages; the sequential scan is negligible compared to
/// the node heap scan it replaces.
///
/// If the table does not yet exist (e.g. `pg_eddy < 0.11.0` install) the SPI
/// error is silently swallowed and the cache remains empty, which causes
/// `find_node_by_id` to fall back to the O(N) scan as before.
pub fn load_node_location_cache() {
    let entries: Vec<(i64, u32, u16)> = Spi::connect(|client| {
        let result = client.select(
            "SELECT node_id, page_num, offset_num FROM _pg_eddy.node_location",
            None,
            &[],
        );
        match result {
            Err(_) => {
                // Table missing (pre-migration install) — empty cache is safe.
                Vec::new()
            }
            Ok(table) => table
                .map(|row| {
                    let nid: i64 = row["node_id"].value::<i64>().unwrap_or(None).unwrap_or(0);
                    let pg: i32  = row["page_num"].value::<i32>().unwrap_or(None).unwrap_or(-1);
                    let off: i32 = row["offset_num"].value::<i32>().unwrap_or(None).unwrap_or(0);
                    (nid, pg as u32, off as u16)
                })
                .collect(),
        }
    });
    NODE_LOCATION_CACHE.with(|cache| {
        let mut map = cache.borrow_mut();
        map.clear();
        for (nid, pg, off) in entries {
            map.insert(nid, (pg, off));
        }
    });
}

/// Write `(node_id, page_num, offset_num)` to the catalog table and update
/// the in-process cache so the new node is immediately findable within the
/// same statement.
///
/// Called by `create_node()` in `lib.rs` after every node insert.
pub fn record_node_location(node_id: i64, page_num: u32, offset_num: u16) {
    Spi::run_with_args(
        "INSERT INTO _pg_eddy.node_location(node_id, page_num, offset_num)
         VALUES ($1, $2, $3)
         ON CONFLICT (node_id) DO UPDATE
           SET page_num   = EXCLUDED.page_num,
               offset_num = EXCLUDED.offset_num",
        &[
            DatumWithOid::from(node_id),
            DatumWithOid::from(page_num as i32),
            DatumWithOid::from(offset_num as i32),
        ],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: node_location insert: {e}"));

    // Mirror into the in-process cache so nodes created in this statement are
    // immediately reachable without a reload.
    NODE_LOCATION_CACHE.with(|c| {
        c.borrow_mut().insert(node_id, (page_num, offset_num));
    });
}

/// Remove a node-location entry from the catalog table and cache.
///
/// Called by `delete_node()` so deleted nodes are not found via stale cache
/// entries in subsequent statements.
#[allow(dead_code)]
pub fn remove_node_location(node_id: i64) {
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.node_location WHERE node_id = $1",
        &[DatumWithOid::from(node_id)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: node_location delete: {e}"));
    NODE_LOCATION_CACHE.with(|c| {
        c.borrow_mut().remove(&node_id);
    });
}

/// Update the in-process cache only (no SPI).
///
/// Used by `CatalogWriteBuffer::flush()` in the executor after a bulk INSERT
/// into `_pg_eddy.node_location` to mirror the entries into the per-statement
/// cache so newly-created nodes are findable in the same statement.
pub fn cache_node_location(node_id: i64, page_num: u32, offset_num: u16) {
    NODE_LOCATION_CACHE.with(|c| {
        c.borrow_mut().insert(node_id, (page_num, offset_num));
    });
}

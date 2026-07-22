/// Property index catalog — Rust interface for `_pg_eddy.prop_index_catalog`
/// and `_pg_eddy.prop_value_index`.
///
/// A **property index** lets the query planner resolve
/// `MATCH (n:Label {prop: $val})` in O(log N) time instead of scanning all
/// nodes with that label and post-filtering.
///
/// The index stores JSON-serialised property values:
///   - String "Alice" → `"\"Alice\""`
///   - Integer 42     → `"42"`
///   - Float 3.14     → `"3.14"`
///   - Boolean true   → `"true"`
///   - Null           → not indexed
///
/// All public functions must be called inside an active transaction.
///
/// # OPT-10: indexed-props-per-label cache
///
/// `index_node_insert` previously called `SELECT prop_name FROM
/// prop_index_catalog WHERE label_name = $1` for every single node created.
/// For a 100-node `UNWIND+CREATE` batch with no property index, this was 100
/// SPI calls that all returned empty results.  The result is now cached in a
/// thread-local `HashMap<String, Vec<String>>` (keyed by label name).  The
/// cache is cleared at the start of every `cypher()` call via
/// `clear_prop_index_cache()`.
use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;
use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// OPT-10: per-label cache of indexed property names.
    static INDEXED_PROPS_CACHE: RefCell<HashMap<String, Vec<String>>> =
        RefCell::new(HashMap::new());
}

/// Clear the per-statement indexed-props cache.
///
/// Must be called alongside `clear_name_caches()` at the start of every
/// `cypher()` invocation so DDL changes (e.g. `create_node_index`) are
/// reflected immediately.
pub fn clear_prop_index_cache() {
    INDEXED_PROPS_CACHE.with(|c| c.borrow_mut().clear());
}

// ---------------------------------------------------------------------------
// Index catalog queries
// ---------------------------------------------------------------------------

/// Return true if a property index exists for `(label_id, key_id)`.
#[cfg(not(feature = "pg_test"))]
pub fn has_property_index(label_id: i32, key_id: i32) -> bool {
    // We need to join label/key registries to check by id.
    // Efficient: the catalog table has a UNIQUE(label_name, prop_name) and we
    // look up via label_name/prop_name; here we receive numeric ids so we
    // resolve them first.
    let label_name = crate::catalog::labels::label_name(label_id);
    let prop_name  = crate::catalog::labels::prop_key_name(key_id);
    if label_name.starts_with('?') || prop_name.starts_with('?') {
        return false; // Unknown id → definitely no index
    }
    Spi::get_one_with_args::<i32>(
        "SELECT 1 FROM _pg_eddy.prop_index_catalog \
         WHERE label_name = $1 AND prop_name = $2",
        &[DatumWithOid::from(label_name.as_str()), DatumWithOid::from(prop_name.as_str())],
    )
    .unwrap_or(None)
    .is_some()
}

/// Return all registered property indexes as `(label_name, prop_name)` pairs.
pub fn list_indexes() -> Vec<(String, String)> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT label_name, prop_name FROM _pg_eddy.prop_index_catalog \
                 ORDER BY label_name, prop_name",
                None,
                &[],
            )
            .unwrap_or_else(|e| pgrx::error!("pg_eddy: list_indexes SPI: {e}"))
            .filter_map(|row| {
                let label = row.get::<String>(1).ok().flatten()?;
                let prop  = row.get::<String>(2).ok().flatten()?;
                Some((label, prop))
            })
            .collect()
    })
}

/// Estimate the number of distinct values indexed for `(label_id, key_id)`.
///
/// Uses the primary key of `prop_value_index` which is indexed, so this is
/// an O(count) scan but fast for small indexes.  Only call from EXPLAIN paths.
///
/// Not available in `pg_test` mode.
#[cfg(not(feature = "pg_test"))]
pub fn count_index_entries(label_id: i32, key_id: i32) -> i64 {
    Spi::get_one_with_args::<i64>(
        "SELECT COUNT(*) FROM _pg_eddy.prop_value_index \
         WHERE label_id = $1 AND key_id = $2",
        &[DatumWithOid::from(label_id), DatumWithOid::from(key_id)],
    )
    .unwrap_or(None)
    .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Index management
// ---------------------------------------------------------------------------

/// Register a property index for `(label, prop)` and backfill all existing
/// nodes that carry this label.
///
/// If an index already exists for this pair, this is a no-op (idempotent).
/// Returns the `index_id` of the new (or existing) catalog entry.
pub fn create_property_index(label_name: &str, prop_name: &str) -> i32 {
    use crate::catalog::labels::{ensure_label, ensure_prop_key};
    #[cfg(not(feature = "pg_test"))]
    use crate::catalog::labels::prop_key_name;
    #[cfg(not(feature = "pg_test"))]
    use crate::storage::prop_store;

    // Upsert into catalog.
    let index_id: i32 = Spi::get_one_with_args::<i32>(
        "INSERT INTO _pg_eddy.prop_index_catalog(label_name, prop_name) \
         VALUES ($1, $2) \
         ON CONFLICT (label_name, prop_name) DO UPDATE \
           SET label_name = EXCLUDED.label_name \
         RETURNING index_id",
        &[DatumWithOid::from(label_name), DatumWithOid::from(prop_name)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: create_property_index SPI: {e}"))
    .unwrap_or_else(|| pgrx::error!("pg_eddy: create_property_index returned NULL"));

    // Resolve numeric ids.
    let label_id = ensure_label(label_name);
    let key_id   = ensure_prop_key(prop_name);

    // Remove any stale entries first (handles re-index after drop).
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.prop_value_index WHERE label_id = $1 AND key_id = $2",
        &[DatumWithOid::from(label_id), DatumWithOid::from(key_id)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: backfill cleanup SPI: {e}"));

    // Backfill: scan all nodes with this label and index their prop value.
    let node_ids: Vec<i64> = Spi::connect(|client| {
        client
            .select(
                "SELECT node_id FROM _pg_eddy.label_index WHERE label_id = $1",
                None,
                &[DatumWithOid::from(label_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pg_eddy: backfill scan SPI: {e}"))
            .filter_map(|row| row.get::<i64>(1).ok().flatten())
            .collect()
    });

    for nid in node_ids {
        // Read the node's properties and index the relevant prop.
        // In pg_test mode, open_nodes_relation/storage calls are not available
        // so we skip the backfill (tests create fresh databases anyway).
        #[cfg(not(feature = "pg_test"))]
        {
        let record = unsafe {
            use pgrx::pg_sys;
            let rel = crate::open_nodes_relation();
            let snap = pg_sys::GetActiveSnapshot();
            let r = crate::storage::node_store::find_node_by_id(rel, nid, snap);
            pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            r
        };
        if let Some(mut r) = record {
            if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                r.prop_bytes = unsafe {
                    use pgrx::pg_sys;
                    let rel = crate::open_nodes_relation();
                    let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
                    pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
                    bytes
                };
            }
            let props = prop_store::decode(&r.prop_bytes, prop_key_name);
            if let Some(v) = props.get(prop_name)
                && !v.is_null() {
                    let vtext = serde_json::to_string(v).unwrap_or_default();
                    Spi::run_with_args(
                        "INSERT INTO _pg_eddy.prop_value_index(label_id, key_id, value_text, node_id) \
                         VALUES ($1, $2, $3, $4)",
                        &[
                            DatumWithOid::from(label_id),
                            DatumWithOid::from(key_id),
                            DatumWithOid::from(vtext.as_str()),
                            DatumWithOid::from(nid),
                        ],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pg_eddy: backfill insert SPI: {e}"));
                }
        }
        } // end #[cfg(not(feature = "pg_test"))]
        #[cfg(feature = "pg_test")]
        { let _ = nid; } // suppress unused variable warning in test mode
    }

    index_id
}

/// Remove the property index for `(label, prop)`, deleting all index data.
///
/// Returns `true` if an index was found and dropped, `false` if no such
/// index existed.
pub fn drop_property_index(label_name: &str, prop_name: &str) -> bool {
    use crate::catalog::labels::{label_id_by_name, prop_key_id_by_name};

    // Remove from catalog.
    let deleted: i64 = Spi::get_one_with_args::<i64>(
        "WITH del AS (DELETE FROM _pg_eddy.prop_index_catalog \
          WHERE label_name = $1 AND prop_name = $2 RETURNING 1) \
         SELECT COUNT(*) FROM del",
        &[DatumWithOid::from(label_name), DatumWithOid::from(prop_name)],
    )
    .unwrap_or(None)
    .unwrap_or(0);

    if deleted == 0 {
        return false;
    }

    // Also purge the value index if the label/key ids are known.
    if let (Some(lid), Some(kid)) = (label_id_by_name(label_name), prop_key_id_by_name(prop_name)) {
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.prop_value_index WHERE label_id = $1 AND key_id = $2",
            &[DatumWithOid::from(lid), DatumWithOid::from(kid)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: drop_property_index data purge SPI: {e}"));
    }

    true
}

// ---------------------------------------------------------------------------
// Index maintenance — called by create_node / delete_node / update_node
// ---------------------------------------------------------------------------

/// Serialise a JSON value to an index key string.
/// Returns `None` for null (null values are not indexed).
pub fn value_to_index_text(v: &serde_json::Value) -> Option<String> {
    if v.is_null() {
        return None;
    }
    Some(serde_json::to_string(v).unwrap_or_default())
}

/// Insert property index entries for a newly-created node.
///
/// For each (label, prop) pair that has a registered index, and the node's
/// props contain a non-null value for `prop`, insert one row into
/// `prop_value_index`.
pub fn index_node_insert(
    node_id: i64,
    label_ids: &[i32],
    props: &serde_json::Map<String, serde_json::Value>,
) {
    use crate::catalog::labels::{label_name, prop_key_id_by_name};

    // For each label the node has, check if any prop is indexed.
    for &lid in label_ids {
        let lname = label_name(lid);
        // OPT-10: cache indexed props per label — avoids repeated SPI on cache hit.
        let cached = INDEXED_PROPS_CACHE.with(|c| c.borrow().get(&lname).cloned());
        let indexed_props = if let Some(props) = cached {
            props
        } else {
            let props = Spi::connect(|client| {
                client
                    .select(
                        "SELECT prop_name FROM _pg_eddy.prop_index_catalog \
                         WHERE label_name = $1",
                        None,
                        &[DatumWithOid::from(lname.as_str())],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pg_eddy: index_node_insert SPI: {e}"))
                    .filter_map(|row| row.get::<String>(1).ok().flatten())
                    .collect::<Vec<_>>()
            });
            INDEXED_PROPS_CACHE.with(|c| { c.borrow_mut().insert(lname.clone(), props.clone()); });
            props
        };

        for pname in indexed_props {
            if let Some(v) = props.get(&pname)
                && let Some(vtext) = value_to_index_text(v)
                && let Some(kid) = prop_key_id_by_name(&pname) {
                    Spi::run_with_args(
                        "INSERT INTO _pg_eddy.prop_value_index \
                         (label_id, key_id, value_text, node_id) \
                         VALUES ($1, $2, $3, $4)",
                        &[
                            DatumWithOid::from(lid),
                            DatumWithOid::from(kid),
                            DatumWithOid::from(vtext.as_str()),
                            DatumWithOid::from(node_id),
                        ],
                    )
                    .unwrap_or_else(|e| {
                        pgrx::error!("pg_eddy: index_node_insert data SPI: {e}")
                    });
                }
        }
    }
}

/// Remove all property index entries for a node being deleted.
pub fn index_node_delete(node_id: i64) {
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.prop_value_index WHERE node_id = $1",
        &[DatumWithOid::from(node_id)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: index_node_delete SPI: {e}"));
}

/// Update property index entries for a node whose properties have changed.
///
/// Deletes all existing entries for the node, then re-inserts entries for
/// the new property values.  This is correct but not maximally efficient;
/// a differential update can be added later.
pub fn index_node_update(
    node_id: i64,
    label_ids: &[i32],
    new_props: &serde_json::Map<String, serde_json::Value>,
) {
    // Delete the old entries.
    index_node_delete(node_id);
    // Re-insert using the new props.
    index_node_insert(node_id, label_ids, new_props);
}

// ---------------------------------------------------------------------------
// Index lookup — used by the query executor (PropertyIndexScan)
// ---------------------------------------------------------------------------

/// Return the set of node_ids whose `(label, prop)` value equals `value_text`.
pub fn lookup_nodes_by_property(
    label_id: i32,
    key_id: i32,
    value_text: &str,
) -> Vec<i64> {
    Spi::connect(|client| {
        client
            .select(
                "SELECT node_id FROM _pg_eddy.prop_value_index \
                 WHERE label_id = $1 AND key_id = $2 AND value_text = $3",
                None,
                &[
                    DatumWithOid::from(label_id),
                    DatumWithOid::from(key_id),
                    DatumWithOid::from(value_text),
                ],
            )
            .unwrap_or_else(|e| pgrx::error!("pg_eddy: lookup_nodes_by_property SPI: {e}"))
            .filter_map(|row| row.get::<i64>(1).ok().flatten())
            .collect()
    })
}

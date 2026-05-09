// pg_eddy — Phase 4.x v0.5.1: TAP infra, rel-type indexes, find_edges
//
// This is the extension entry point.  At _PG_init we:
//   1. Register the custom WAL resource manager.
//   2. Nothing else; AM objects are created by the SQL script.
//
// shared_preload_libraries = 'pg_eddy'  is required.

use pgrx::prelude::*;
use pgrx::datum::DatumWithOid;

mod catalog;
mod cypher;
mod error;
mod storage;

pgrx::pg_module_magic!();

// ---------------------------------------------------------------------------
// Extension SQL — schemas, registry tables, AM objects, and SQL functions.
// ---------------------------------------------------------------------------
extension_sql_file!("../sql/pg_eddy--0.6.0.sql", name = "pg_eddy_schema", finalize);

// ---------------------------------------------------------------------------
// _PG_init  — runs at postmaster start (shared_preload_libraries)
// ---------------------------------------------------------------------------
#[allow(non_snake_case)]
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    storage::wal::register_rmgr();
}

// ---------------------------------------------------------------------------
// Basic health-check function (smoke-test CREATE EXTENSION worked)
// ---------------------------------------------------------------------------
#[pg_extern]
fn health_check() -> &'static str {
    "pg_eddy OK"
}

// ---------------------------------------------------------------------------
// Node API SQL functions
// ---------------------------------------------------------------------------

/// Create a node in the graph.
///
/// `labels`     — array of label names (may be empty `'{}'`).
/// `properties` — JSONB document of node properties (may be `'{}'`).
///
/// Returns the new node's integer id.
#[pg_extern]
fn create_node(labels: Vec<String>, properties: pgrx::JsonB) -> i64 {
    use crate::catalog::labels::{ensure_label, ensure_prop_key, next_node_id};
    use crate::storage::prop_store;

    // Resolve labels → label_ids
    let label_ids: Vec<i32> = labels.iter().map(|l| ensure_label(l)).collect();

    // Encode properties → binary
    let prop_obj = match &properties.0 {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let prop_bytes = prop_store::encode(&prop_obj, |name| -> Result<i32, std::convert::Infallible> {
        Ok(ensure_prop_key(name))
    })
    .unwrap_or_default();

    // Allocate node id
    let node_id = next_node_id();

    // Open the nodes table and insert
    unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        crate::storage::node_store::insert_node(rel, node_id, &label_ids, &prop_bytes);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    }

    // Maintain label_index: insert one row per label.
    for lid in &label_ids {
        Spi::run_with_args(
            "INSERT INTO _pg_eddy.label_index(label_id, node_id) VALUES ($1, $2)",
            &[DatumWithOid::from(*lid), DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index insert failed: {e}"));
    }

    node_id
}

/// Retrieve a node by id and return its properties as JSONB.
///
/// Returns `NULL` if the node does not exist or is not visible.
#[pg_extern]
fn get_node(node_id: i64) -> Option<pgrx::JsonB> {
    use crate::catalog::labels::{label_name, prop_key_name};
    use crate::storage::prop_store;

    let record = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let result = crate::storage::node_store::find_node_by_id(rel, node_id, snapshot);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        result
    };

    record.map(|mut r| {
        // Resolve overflow props if needed.
        if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
            r.prop_bytes = unsafe {
                use pgrx::pg_sys;
                let rel = open_nodes_relation();
                let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
                pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
                bytes
            };
        }
        let mut out = serde_json::Map::new();
        out.insert(
            "node_id".into(),
            serde_json::Value::Number(r.node_id.into()),
        );
        let label_names: Vec<_> = r.label_ids.iter().map(|id| label_name(*id)).collect();
        out.insert("labels".into(), serde_json::Value::Array(
            label_names.into_iter().map(serde_json::Value::String).collect(),
        ));
        let props = prop_store::decode(&r.prop_bytes, prop_key_name);
        out.insert("properties".into(), serde_json::Value::Object(props));
        pgrx::JsonB(serde_json::Value::Object(out))
    })
}

/// Count all visible nodes in the graph.
#[pg_extern(name = "count_nodes")]
fn node_count() -> i64 {
    unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let count = crate::storage::node_store::count_nodes(rel, snapshot);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        count
    }
}

// ---------------------------------------------------------------------------
// Helper: open the _pg_eddy.nodes relation
// ---------------------------------------------------------------------------

/// Open `_pg_eddy.nodes` with `AccessShareLock` (for reads) or
/// `RowExclusiveLock` (for writes). For Phase 1 we always use `NoLock`
/// because we manage concurrency inside node_store via buffer locks.
///
/// # Safety
/// The returned `Relation` must be closed before the current transaction ends.
unsafe fn open_nodes_relation() -> pgrx::pg_sys::Relation {
    use pgrx::pg_sys;

    // Resolve schema-qualified name.
    let schema_name = std::ffi::CString::new("_pg_eddy").unwrap();
    let rel_name = std::ffi::CString::new("nodes").unwrap();

    let schema_oid = unsafe { pg_sys::get_namespace_oid(schema_name.as_ptr(), false) };
    let rel_oid = unsafe { pg_sys::get_relname_relid(rel_name.as_ptr(), schema_oid) };

    if rel_oid == pg_sys::Oid::INVALID {
        error!("pg_eddy: relation _pg_eddy.nodes not found");
    }

    unsafe { pg_sys::table_open(rel_oid, pg_sys::NoLock as pg_sys::LOCKMODE) }
}

/// Open `_pg_eddy.edges` with `NoLock`.
///
/// # Safety
/// The returned `Relation` must be closed before the current transaction ends.
unsafe fn open_edges_relation() -> pgrx::pg_sys::Relation {
    use pgrx::pg_sys;

    let schema_name = std::ffi::CString::new("_pg_eddy").unwrap();
    let rel_name = std::ffi::CString::new("edges").unwrap();

    let schema_oid = unsafe { pg_sys::get_namespace_oid(schema_name.as_ptr(), false) };
    let rel_oid = unsafe { pg_sys::get_relname_relid(rel_name.as_ptr(), schema_oid) };

    if rel_oid == pg_sys::Oid::INVALID {
        error!("pg_eddy: relation _pg_eddy.edges not found");
    }

    unsafe { pg_sys::table_open(rel_oid, pg_sys::NoLock as pg_sys::LOCKMODE) }
}

// ---------------------------------------------------------------------------
// Edge API SQL functions
// ---------------------------------------------------------------------------

/// Create an edge between two existing nodes.
///
/// `source`     — node_id of the source (start) node.
/// `target`     — node_id of the target (end) node.
/// `rel_type`   — relationship type name (e.g. `'KNOWS'`).
/// `properties` — JSONB property map (may be `'{}'`).
///
/// Returns the new edge's rel_id.
#[pg_extern]
fn create_edge(
    source: i64,
    target: i64,
    rel_type: &str,
    properties: pgrx::JsonB,
) -> i64 {
    use crate::catalog::labels::{ensure_prop_key, ensure_rel_type, next_edge_id};
    use crate::storage::prop_store;

    let type_id = ensure_rel_type(rel_type);

    let prop_obj = match &properties.0 {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let prop_bytes = prop_store::encode(&prop_obj, |name| -> Result<i32, std::convert::Infallible> {
        Ok(ensure_prop_key(name))
    })
    .unwrap_or_default();

    let edge_id = next_edge_id();

    unsafe {
        use pgrx::pg_sys;
        let node_rel = open_nodes_relation();
        let edge_rel = open_edges_relation();
        crate::storage::edge_store::insert_edge(
            node_rel, edge_rel, edge_id, type_id, source, target, &prop_bytes,
        );
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        pg_sys::table_close(node_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    }

    // Maintain rel-type catalog indexes (v0.5.1).
    Spi::run_with_args(
        "INSERT INTO _pg_eddy.edge_type_src(type_id, src_node_id, edge_id)
         VALUES ($1, $2, $3)",
        &[DatumWithOid::from(type_id), DatumWithOid::from(source), DatumWithOid::from(edge_id)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src insert: {e}"));

    Spi::run_with_args(
        "INSERT INTO _pg_eddy.edge_type_dst(type_id, dst_node_id, edge_id)
         VALUES ($1, $2, $3)",
        &[DatumWithOid::from(type_id), DatumWithOid::from(target), DatumWithOid::from(edge_id)],
    )
    .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst insert: {e}"));

    edge_id
}

/// Retrieve an edge by its rel_id, returning its data as JSONB.
///
/// Returns `NULL` if the edge does not exist or has been deleted.
#[pg_extern]
fn get_edge(rel_id: i64) -> Option<pgrx::JsonB> {
    use crate::catalog::labels::{prop_key_name, rel_type_name};
    use crate::storage::prop_store;

    let record = unsafe {
        use pgrx::pg_sys;
        let edge_rel = open_edges_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let result = crate::storage::edge_store::find_edge_by_id(edge_rel, rel_id, snapshot);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        result
    };

    record.map(|r| {
        let mut out = serde_json::Map::new();
        out.insert("rel_id".into(), serde_json::Value::Number(r.edge_id.into()));
        out.insert("rel_type".into(), serde_json::Value::String(rel_type_name(r.rel_type_id)));
        out.insert("source_node_id".into(), serde_json::Value::Number(r.source_node_id.into()));
        out.insert("target_node_id".into(), serde_json::Value::Number(r.target_node_id.into()));
        let props = prop_store::decode(&r.prop_bytes, prop_key_name);
        out.insert("properties".into(), serde_json::Value::Object(props));
        pgrx::JsonB(serde_json::Value::Object(out))
    })
}

/// Logically delete an edge by its rel_id.
///
/// The edge is marked as deleted (xmax set); physical reclamation happens
/// during VACUUM (Phase 3). The adjacency chain is not modified immediately;
/// traversal skips deleted edges via MVCC visibility.
///
/// Returns `true` if the edge was found and deleted, `false` if not found.
#[pg_extern]
fn delete_edge(rel_id: i64) -> bool {
    let found = unsafe {
        use pgrx::pg_sys;
        let edge_rel = open_edges_relation();
        let f = crate::storage::edge_store::delete_edge(edge_rel, rel_id);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };

    if found {
        // Remove from rel-type catalog indexes (v0.5.1).
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.edge_type_src WHERE edge_id = $1",
            &[DatumWithOid::from(rel_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src delete: {e}"));
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.edge_type_dst WHERE edge_id = $1",
            &[DatumWithOid::from(rel_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst delete: {e}"));
    }

    found
}

/// Count all non-deleted edges in the graph.
#[pg_extern(name = "count_edges")]
fn edge_count() -> i64 {
    unsafe {
        use pgrx::pg_sys;
        let edge_rel = open_edges_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let count = crate::storage::edge_store::count_edges(edge_rel, snapshot);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        count
    }
}

/// Return the set of neighbour node_ids reachable from `node_id` in
/// `direction` (`'OUT'`, `'IN'`, or `'BOTH'`), optionally filtered by
/// relationship type name.
///
/// This follows the singly-linked adjacency chain directly — O(degree)
/// with no index scan.
#[pg_extern]
fn neighbours(
    node_id: i64,
    direction: &str,
    rel_type: Option<String>,
) -> SetOfIterator<'static, i64> {
    use crate::catalog::labels::ensure_rel_type;
    use crate::storage::edge_store::{Direction, adjacency_follow};

    let dir = Direction::from_str(direction);
    let rel_type_filter: Option<i32> = rel_type.as_deref().map(ensure_rel_type);

    let edges = unsafe {
        use pgrx::pg_sys;
        let node_rel = open_nodes_relation();
        let edge_rel = open_edges_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let result = adjacency_follow(node_rel, edge_rel, node_id, dir, rel_type_filter, snapshot);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        pg_sys::table_close(node_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        result
    };

    let ids: Vec<i64> = edges.into_iter().map(|e| {
        if matches!(dir, Direction::In) {
            e.source_node_id
        } else {
            // OUT or BOTH: return the "other" end
            if e.source_node_id == node_id { e.target_node_id } else { e.source_node_id }
        }
    }).collect();

    SetOfIterator::new(ids)
}

/// Find edge IDs connecting specific endpoints and/or of a specific type.
///
/// All parameters are optional filters; pass `NULL` to skip a filter:
/// - `src_node_id` — only edges originating from this node
/// - `dst_node_id` — only edges terminating at this node
/// - `rel_type`    — only edges of this relationship type
///
/// When `rel_type` AND either endpoint is given, uses the catalog index tables
/// (`_pg_eddy.edge_type_src` / `_pg_eddy.edge_type_dst`) for an O(|result|)
/// lookup. Without a type filter falls back to the adjacency chain.
///
/// Returns each matching edge's `rel_id`.
#[pg_extern]
fn find_edges(
    src_node_id: Option<i64>,
    dst_node_id: Option<i64>,
    rel_type: Option<String>,
) -> SetOfIterator<'static, i64> {
    use crate::catalog::labels::ensure_rel_type;
    use crate::storage::edge_store::{Direction, adjacency_follow};

    let type_id_opt: Option<i32> = rel_type.as_deref().map(ensure_rel_type);

    // Fast path: type + src → use edge_type_src index.
    if let (Some(tid), Some(src)) = (type_id_opt, src_node_id) {
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT edge_id FROM _pg_eddy.edge_type_src
                      WHERE type_id = $1 AND src_node_id = $2",
                    None,
                    &[DatumWithOid::from(tid), DatumWithOid::from(src)],
                )
                .unwrap_or_else(|e| pgrx::error!("pg_eddy find_edges SPI: {e}"))
                .map(|row| {
                    row["edge_id"]
                        .value::<i64>()
                        .unwrap_or(None)
                        .unwrap_or(0)
                })
                .collect::<Vec<i64>>()
        });
        let filtered: Vec<i64> = if let Some(dst) = dst_node_id {
            // Secondary filter by destination via edge_type_dst.
            let dst_set: std::collections::HashSet<i64> = Spi::connect(|client| {
                client
                    .select(
                        "SELECT edge_id FROM _pg_eddy.edge_type_dst
                          WHERE type_id = $1 AND dst_node_id = $2",
                        None,
                        &[DatumWithOid::from(tid), DatumWithOid::from(dst)],
                    )
                    .unwrap_or_else(|e| pgrx::error!("pg_eddy find_edges SPI: {e}"))
                    .map(|row| row["edge_id"].value::<i64>().unwrap_or(None).unwrap_or(0))
                    .collect()
            });
            rows.into_iter().filter(|eid| dst_set.contains(eid)).collect()
        } else {
            rows
        };
        return SetOfIterator::new(filtered);
    }

    // Fast path: type + dst → use edge_type_dst index.
    if let (Some(tid), Some(dst)) = (type_id_opt, dst_node_id) {
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT edge_id FROM _pg_eddy.edge_type_dst
                      WHERE type_id = $1 AND dst_node_id = $2",
                    None,
                    &[DatumWithOid::from(tid), DatumWithOid::from(dst)],
                )
                .unwrap_or_else(|e| pgrx::error!("pg_eddy find_edges SPI: {e}"))
                .map(|row| row["edge_id"].value::<i64>().unwrap_or(None).unwrap_or(0))
                .collect::<Vec<i64>>()
        });
        return SetOfIterator::new(rows);
    }

    // Fallback: use adjacency chain for endpoint-only filters.
    let anchor_node = src_node_id.or(dst_node_id);
    let dir = if src_node_id.is_some() { Direction::Out } else if dst_node_id.is_some() { Direction::In } else { Direction::Both };

    let edges = if let Some(node) = anchor_node {
        unsafe {
            use pgrx::pg_sys;
            let node_rel = open_nodes_relation();
            let edge_rel = open_edges_relation();
            let snapshot = pg_sys::GetActiveSnapshot();
            let result = adjacency_follow(node_rel, edge_rel, node, dir, type_id_opt, snapshot);
            pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            pg_sys::table_close(node_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            result
        }
    } else if let Some(tid) = type_id_opt {
        // Type-only filter with no endpoint: use the src catalog index (covers all edges of that type).
        let rows = Spi::connect(|client| {
            client
                .select(
                    "SELECT edge_id FROM _pg_eddy.edge_type_src WHERE type_id = $1",
                    None,
                    &[DatumWithOid::from(tid)],
                )
                .unwrap_or_else(|e| pgrx::error!("pg_eddy find_edges SPI: {e}"))
                .map(|row| row["edge_id"].value::<i64>().unwrap_or(None).unwrap_or(0))
                .collect::<Vec<i64>>()
        });
        return SetOfIterator::new(rows);
    } else {
        // No filters at all: full edge scan via adjacency chains from every node.
        // This is O(N+E) — use only for small graphs or tooling purposes.
        Spi::connect(|client| {
            client
                .select("SELECT edge_id FROM _pg_eddy.edge_type_src", None, &[])
                .unwrap_or_else(|e| pgrx::error!("pg_eddy find_edges SPI: {e}"))
                .map(|row| row["edge_id"].value::<i64>().unwrap_or(None).unwrap_or(0))
                .collect::<Vec<i64>>()
        });
        // Return empty — callers should always supply at least one filter.
        return SetOfIterator::new(std::iter::empty());
    };

    // Apply remaining dst filter if we traversed from src.
    let ids: Vec<i64> = edges
        .into_iter()
        .filter(|e| dst_node_id.is_none_or(|d| e.target_node_id == d))
        .filter(|e| src_node_id.is_none_or(|s| e.source_node_id == s))
        .map(|e| e.edge_id)
        .collect();

    SetOfIterator::new(ids)
}

/// Adjacency-follow SRF: follows the adjacency chain from `node_id` in
/// `direction` and returns full edge information for each visible edge.
///
/// `direction` — `'OUT'`, `'IN'`, or `'BOTH'`.
/// `rel_type`  — optional relationship type filter; `NULL` returns all types.
///
/// Returns one row per edge: (rel_id, other_node_id, rel_type_id, rel_properties).
#[pg_extern]
fn expand(
    node_id: i64,
    direction: &str,
    rel_type: Option<String>,
) -> TableIterator<
    'static,
    (
        name!(rel_id, i64),
        name!(other_node_id, i64),
        name!(rel_type_id, i32),
        name!(rel_properties, pgrx::JsonB),
    ),
> {
    use crate::catalog::labels::{ensure_rel_type, prop_key_name, rel_type_name};
    use crate::storage::edge_store::{Direction, adjacency_follow};
    use crate::storage::prop_store;

    let dir = Direction::from_str(direction);
    let rel_type_filter: Option<i32> = rel_type.as_deref().map(ensure_rel_type);

    let edges = unsafe {
        use pgrx::pg_sys;
        let node_rel = open_nodes_relation();
        let edge_rel = open_edges_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let result = adjacency_follow(node_rel, edge_rel, node_id, dir, rel_type_filter, snapshot);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        pg_sys::table_close(node_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        result
    };

    let rows: Vec<(i64, i64, i32, pgrx::JsonB)> = edges
        .into_iter()
        .map(|e| {
            let other = if e.source_node_id == node_id {
                e.target_node_id
            } else {
                e.source_node_id
            };
            let props = prop_store::decode(&e.prop_bytes, prop_key_name);
            let _ = rel_type_name; // suppress unused warning — available if needed
            let props_json = pgrx::JsonB(serde_json::Value::Object(props));
            (e.edge_id, other, e.rel_type_id, props_json)
        })
        .collect();

    TableIterator::new(rows)
}

// ---------------------------------------------------------------------------
// Node update / delete (Phase 3 MVCC)
// ---------------------------------------------------------------------------

/// Update a node's labels and properties.
///
/// The old node record is logically deleted and a new MVCC version is inserted
/// on the **same page** (adj_slot_idx preserved).  If the new record is too
/// large to fit on the same page an error is raised.
///
/// Returns `false` if the node was not found.
#[pg_extern]
fn update_node(node_id: i64, labels: Vec<String>, properties: pgrx::JsonB) -> bool {
    use crate::catalog::labels::{ensure_label, ensure_prop_key};
    use crate::storage::prop_store;

    let label_ids: Vec<i32> = labels.iter().map(|l| ensure_label(l)).collect();
    let prop_obj = match &properties.0 {
        serde_json::Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    let prop_bytes =
        prop_store::encode(&prop_obj, |name| -> Result<i32, std::convert::Infallible> {
            Ok(ensure_prop_key(name))
        })
        .unwrap_or_default();

    let found = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let f = crate::storage::node_store::update_node(rel, node_id, &label_ids, &prop_bytes);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };

    if found {
        // Refresh label_index: delete old rows and insert new ones.
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.label_index WHERE node_id = $1",
            &[DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete failed: {e}"));
        for lid in &label_ids {
            Spi::run_with_args(
                "INSERT INTO _pg_eddy.label_index(label_id, node_id) VALUES ($1, $2)",
                &[DatumWithOid::from(*lid), DatumWithOid::from(node_id)],
            )
            .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index insert failed: {e}"));
        }
    }

    found
}

/// Logically delete a node by id.
///
/// The node is marked as deleted (xmax set); physical reclamation happens
/// during VACUUM. Returns `false` if the node was not found.
#[pg_extern]
fn delete_node(node_id: i64) -> bool {
    let found = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let f = crate::storage::node_store::delete_node_by_id(rel, node_id);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };
    if found {
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.label_index WHERE node_id = $1",
            &[DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete failed: {e}"));
    }
    found
}

// ---------------------------------------------------------------------------
// Phase 4: label management, detach-delete, find_nodes, schema_info
// ---------------------------------------------------------------------------

/// Add a label to an existing node.
///
/// If the node already has this label, returns `false`.
/// Returns `true` if the label was added successfully.
#[pg_extern]
fn add_label(node_id: i64, label: &str) -> bool {
    use crate::catalog::labels::ensure_label;

    let label_id = ensure_label(label);

    // Find the current node.
    let record = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let r = crate::storage::node_store::find_node_by_id(rel, node_id, snapshot);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        r
    };
    let mut r = match record {
        Some(r) => r,
        None => return false,
    };

    // Already has this label?
    if r.label_ids.contains(&label_id) {
        return false;
    }

    // Resolve overflow props.
    if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
        r.prop_bytes = unsafe {
            use pgrx::pg_sys;
            let rel = open_nodes_relation();
            let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
            pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            bytes
        };
    }

    let mut new_labels = r.label_ids.clone();
    new_labels.push(label_id);

    let found = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let f = crate::storage::node_store::update_node(rel, node_id, &new_labels, &r.prop_bytes);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };

    if found {
        Spi::run_with_args(
            "INSERT INTO _pg_eddy.label_index(label_id, node_id) VALUES ($1, $2)",
            &[DatumWithOid::from(label_id), DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index insert failed: {e}"));
    }

    found
}

/// Remove a label from an existing node.
///
/// If the node does not have this label, returns `false`.
/// Returns `true` if the label was removed successfully.
#[pg_extern]
fn remove_label(node_id: i64, label: &str) -> bool {
    use crate::catalog::labels::ensure_label;

    let label_id = ensure_label(label);

    let record = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let snapshot = pg_sys::GetActiveSnapshot();
        let r = crate::storage::node_store::find_node_by_id(rel, node_id, snapshot);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        r
    };
    let mut r = match record {
        Some(r) => r,
        None => return false,
    };

    // Does not have this label?
    if !r.label_ids.contains(&label_id) {
        return false;
    }

    // Resolve overflow props.
    if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
        r.prop_bytes = unsafe {
            use pgrx::pg_sys;
            let rel = open_nodes_relation();
            let bytes = crate::storage::node_store::read_overflow_block(rel, r.overflow_blkno);
            pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            bytes
        };
    }

    let new_labels: Vec<i32> = r.label_ids.iter().copied().filter(|&l| l != label_id).collect();

    let found = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let f = crate::storage::node_store::update_node(rel, node_id, &new_labels, &r.prop_bytes);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };

    if found {
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.label_index WHERE label_id = $1 AND node_id = $2",
            &[DatumWithOid::from(label_id), DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete failed: {e}"));
    }

    found
}

/// Delete a node and all edges connected to it (detach-delete pattern).
///
/// First logically deletes all incoming and outgoing edges, then deletes the
/// node itself. Returns `false` if the node was not found.
#[pg_extern]
fn detach_delete_node(node_id: i64) -> bool {
    use crate::storage::edge_store::{Direction, adjacency_follow};
    use std::collections::HashSet;

    // Collect all edge ids attached to this node.
    let all_edge_ids: Vec<i64> = unsafe {
        use pgrx::pg_sys;
        let node_rel = open_nodes_relation();
        let edge_rel = open_edges_relation();
        let snapshot = pg_sys::GetActiveSnapshot();

        let out_edges = adjacency_follow(node_rel, edge_rel, node_id, Direction::Out, None, snapshot);
        let in_edges = adjacency_follow(node_rel, edge_rel, node_id, Direction::In, None, snapshot);

        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        pg_sys::table_close(node_rel, pg_sys::NoLock as pg_sys::LOCKMODE);

        let mut seen: HashSet<i64> = HashSet::new();
        for e in out_edges.iter().chain(in_edges.iter()) {
            seen.insert(e.edge_id);
        }
        seen.into_iter().collect()
    };

    // Delete each unique edge.
    unsafe {
        use pgrx::pg_sys;
        let edge_rel = open_edges_relation();
        for eid in &all_edge_ids {
            crate::storage::edge_store::delete_edge(edge_rel, *eid);
        }
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
    }

    // Clean up rel-type catalog indexes for deleted edges (v0.5.1).
    if !all_edge_ids.is_empty() {
        // Use ANY($1) to delete all in one round-trip.
        let ids_array: Vec<i64> = all_edge_ids.clone();
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.edge_type_src WHERE edge_id = ANY($1)",
            &[DatumWithOid::from(ids_array.as_slice())],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_src batch delete: {e}"));
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.edge_type_dst WHERE edge_id = ANY($1)",
            &[DatumWithOid::from(ids_array.as_slice())],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: edge_type_dst batch delete: {e}"));
    }

    // Delete the node.
    let found = unsafe {
        use pgrx::pg_sys;
        let rel = open_nodes_relation();
        let f = crate::storage::node_store::delete_node_by_id(rel, node_id);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        f
    };

    if found {
        Spi::run_with_args(
            "DELETE FROM _pg_eddy.label_index WHERE node_id = $1",
            &[DatumWithOid::from(node_id)],
        )
        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index delete failed: {e}"));
    }

    found
}

/// Find nodes, optionally filtered by label name and/or a property sub-document.
///
/// `label`           — if given, only nodes with this label are returned.
/// `property_filter` — if given, only nodes whose properties contain all
///                     key-value pairs from the filter document are returned.
///
/// Returns a set of matching node_ids.
#[pg_extern]
fn find_nodes(
    label: Option<String>,
    property_filter: Option<pgrx::JsonB>,
) -> SetOfIterator<'static, i64> {
    use crate::catalog::labels::{label_id_by_name, prop_key_name};
    use crate::storage::prop_store;

    // Determine candidate node_ids.
    let candidates: Vec<i64> = if let Some(lname) = label {
        // Fast path: query label_index.
        let lid = label_id_by_name(&lname);
        match lid {
            None => return SetOfIterator::new(std::iter::empty()),
            Some(lid) => {
                Spi::connect(|client| {
                    let tup_table = client
                        .select(
                            "SELECT node_id FROM _pg_eddy.label_index WHERE label_id = $1",
                            None,
                            &[DatumWithOid::from(lid)],
                        )
                        .unwrap_or_else(|e| pgrx::error!("pg_eddy: label_index query failed: {e}"));
                    tup_table
                        .filter_map(|row| row.get::<i64>(1).ok().flatten())
                        .collect()
                })
            }
        }
    } else {
        // Slow path: full sequential scan.
        unsafe {
            use pgrx::pg_sys;
            let rel = open_nodes_relation();
            let snapshot = pg_sys::GetActiveSnapshot();
            let mut state = crate::storage::node_store::NodeScanState::begin(rel, snapshot);
            let mut ids = Vec::new();
            while let Some(r) = state.next() {
                ids.push(r.node_id);
            }
            pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
            ids
        }
    };

    // If no property filter, return all candidates.
    let filter_obj: Option<serde_json::Map<String, serde_json::Value>> =
        property_filter.and_then(|f| match f.0 {
            serde_json::Value::Object(m) => Some(m),
            _ => None,
        });

    let result: Vec<i64> = if let Some(filter) = filter_obj {
        candidates
            .into_iter()
            .filter(|&nid| {
                let record = unsafe {
                    use pgrx::pg_sys;
                    let rel = open_nodes_relation();
                    let snapshot = pg_sys::GetActiveSnapshot();
                    let r = crate::storage::node_store::find_node_by_id(rel, nid, snapshot);
                    pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
                    r
                };
                match record {
                    None => false,
                    Some(mut r) => {
                        if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                            r.prop_bytes = unsafe {
                                use pgrx::pg_sys;
                                let rel = open_nodes_relation();
                                let bytes = crate::storage::node_store::read_overflow_block(
                                    rel,
                                    r.overflow_blkno,
                                );
                                pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
                                bytes
                            };
                        }
                        let props =
                            prop_store::decode(&r.prop_bytes, prop_key_name);
                        filter.iter().all(|(k, v)| props.get(k) == Some(v))
                    }
                }
            })
            .collect()
    } else {
        candidates
    };

    SetOfIterator::new(result)
}

/// Return schema information (label, rel-type, and property-key registries) as JSONB.
#[pg_extern]
fn schema_info() -> pgrx::JsonB {
    use crate::catalog::labels::{all_labels, all_rel_types, all_prop_keys};

    let labels = all_labels();
    let rel_types = all_rel_types();
    let prop_keys = all_prop_keys();

    let out = serde_json::json!({
        "label_count": labels.len(),
        "labels": labels,
        "rel_type_count": rel_types.len(),
        "rel_types": rel_types,
        "property_key_count": prop_keys.len(),
        "property_keys": prop_keys,
    });
    pgrx::JsonB(out)
}

// ---------------------------------------------------------------------------
// AM statistics (Phase 3)
// ---------------------------------------------------------------------------

/// Return live/dead tuple counts for nodes and edges as a JSONB document.
///
/// This is a convenience diagnostic function; it does NOT call VACUUM.
/// Counts are approximate: they reflect the current snapshot visibility.
#[pg_extern]
fn am_stats() -> pgrx::JsonB {
    use std::mem::size_of;
    use pgrx::pg_sys;
    use crate::storage::page::NODE_FIXED_DATA_SIZE;

    let (live_nodes, dead_nodes, node_pages) = unsafe {
        let rel = open_nodes_relation();
        let _snapshot = pg_sys::GetActiveSnapshot();
        let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
        let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(
            rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
        );
        let mut live = 0u64;
        let mut dead = 0u64;
        for blkno in 0..nblocks {
            let buf = pg_sys::ReadBufferExtended(
                rel,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                blkno,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            );
            pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
            let page = pg_sys::BufferGetPage(buf);
            let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);
            for off in pg_sys::FirstOffsetNumber..=max_off {
                let iid = pg_sys::PageGetItemId(page, off);
                if (*iid).lp_flags() != pg_sys::LP_NORMAL { continue; }
                let item_len = (*iid).lp_len() as usize;
                if item_len < hdr_size + NODE_FIXED_DATA_SIZE { continue; }
                let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
                let hdr = item as *const pg_sys::HeapTupleHeaderData;
                let xmax_invalid = ((*hdr).t_infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
                if xmax_invalid { live += 1; } else { dead += 1; }
            }
            pg_sys::UnlockReleaseBuffer(buf);
        }
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        (live, dead, nblocks)
    };

    let (live_edges, dead_edges, edge_pages) = unsafe {
        let rel = open_edges_relation();
        let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
        let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(
            rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
        );
        let mut live = 0u64;
        let mut dead = 0u64;
        for blkno in 0..nblocks {
            let buf = pg_sys::ReadBufferExtended(
                rel,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                blkno,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            );
            pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
            let page = pg_sys::BufferGetPage(buf);
            let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);
            for off in pg_sys::FirstOffsetNumber..=max_off {
                let iid = pg_sys::PageGetItemId(page, off);
                let flags = (*iid).lp_flags();
                if flags != pg_sys::LP_NORMAL && flags != pg_sys::LP_DEAD { continue; }
                let item_len = (*iid).lp_len() as usize;
                if item_len < hdr_size + 1 { continue; }
                if flags == pg_sys::LP_DEAD { dead += 1; continue; }
                let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
                let hdr = item as *const pg_sys::HeapTupleHeaderData;
                let xmax_invalid = ((*hdr).t_infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
                if xmax_invalid { live += 1; } else { dead += 1; }
            }
            pg_sys::UnlockReleaseBuffer(buf);
        }
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        (live, dead, nblocks)
    };

    let out = serde_json::json!({
        "node_pages": node_pages,
        "edge_pages": edge_pages,
        "live_nodes": live_nodes,
        "dead_nodes": dead_nodes,
        "live_edges": live_edges,
        "dead_edges": dead_edges,
    });
    pgrx::JsonB(out)
}

// ---------------------------------------------------------------------------
// Cypher query API (Phase 5)
// ---------------------------------------------------------------------------

/// Execute a Cypher query and return results as SETOF JSONB.
///
/// `query`  — a Cypher MATCH…RETURN statement.
/// `params` — optional JSONB object with query parameters ($name references).
///
/// Example:
/// ```sql
/// SELECT * FROM cypher('MATCH (n:Person) WHERE n.age > 30 RETURN n', '{}');
/// ```
#[pg_extern]
fn cypher(query: &str, params: Option<pgrx::JsonB>) -> SetOfIterator<'static, pgrx::JsonB> {
    let param_map: std::collections::HashMap<String, serde_json::Value> = match params {
        Some(pgrx::JsonB(serde_json::Value::Object(m))) => {
            m.into_iter().collect()
        }
        _ => std::collections::HashMap::new(),
    };

    let ast = match cypher::parser::parse(query) {
        Ok(q) => q,
        Err(e) => error!("pg_eddy cypher parse error: {e}"),
    };

    let plan = match cypher::planner::plan(&ast) {
        Ok(p) => p,
        Err(e) => error!("pg_eddy cypher plan error: {e}"),
    };

    let rows = match cypher::executor::execute(&plan, &param_map) {
        Ok(r) => r,
        Err(e) => error!("pg_eddy cypher exec error: {e}"),
    };

    let results = cypher::executor::rows_to_jsonb(rows);
    SetOfIterator::new(results)
}

/// Return the logical execution plan for a Cypher query as text.
///
/// Example:
/// ```sql
/// SELECT cypher_explain('MATCH (a:Person)-[:KNOWS]->(b) RETURN a, b');
/// ```
#[pg_extern]
fn cypher_explain(query: &str) -> String {
    let ast = match cypher::parser::parse(query) {
        Ok(q) => q,
        Err(e) => error!("pg_eddy cypher parse error: {e}"),
    };

    let plan = match cypher::planner::plan(&ast) {
        Ok(p) => p,
        Err(e) => error!("pg_eddy cypher plan error: {e}"),
    };

    cypher::planner::explain(&plan, 0)
}

// ---------------------------------------------------------------------------
// pg_test module — pgrx unit tests
// ---------------------------------------------------------------------------
#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_health_check() {
        assert_eq!("pg_eddy OK", crate::health_check());
    }

    #[pg_test]
    fn test_create_and_get_node() {
        // No labels, no properties.
        let node_id = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        assert!(node_id > 0, "node_id should be positive");

        let result = crate::get_node(node_id);
        assert!(result.is_some(), "get_node should find the inserted node");

        let json = result.unwrap().0;
        assert_eq!(json["node_id"], serde_json::json!(node_id));
    }

    #[pg_test]
    fn test_create_node_with_labels_and_props() {
        let node_id = crate::create_node(
            vec!["Person".into(), "Employee".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice", "age": 30})),
        );
        assert!(node_id > 0);

        let result = crate::get_node(node_id).expect("node should exist");
        let json = result.0;
        let labels = json["labels"].as_array().unwrap();
        assert!(labels.contains(&serde_json::json!("Person")));
        assert!(labels.contains(&serde_json::json!("Employee")));
        assert_eq!(json["properties"]["name"], serde_json::json!("Alice"));
        assert_eq!(json["properties"]["age"], serde_json::json!(30));
    }

    #[pg_test]
    fn test_node_count() {
        let before = crate::node_count();
        crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let after = crate::node_count();
        assert_eq!(after, before + 2, "node_count should increase by 2");
    }

    #[pg_test]
    fn test_create_and_get_edge() {
        let src = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice"})),
        );
        let tgt = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Bob"})),
        );
        assert!(src > 0 && tgt > 0);

        let eid = crate::create_edge(
            src,
            tgt,
            "KNOWS",
            pgrx::JsonB(serde_json::json!({"since": 2020})),
        );
        assert!(eid > 0, "edge_id should be positive");

        let result = crate::get_edge(eid);
        assert!(result.is_some(), "get_edge should find the edge");
        let json = result.unwrap().0;
        assert_eq!(json["rel_id"], serde_json::json!(eid));
        assert_eq!(json["rel_type"], serde_json::json!("KNOWS"));
        assert_eq!(json["source_node_id"], serde_json::json!(src));
        assert_eq!(json["target_node_id"], serde_json::json!(tgt));
        assert_eq!(json["properties"]["since"], serde_json::json!(2020));
    }

    #[pg_test]
    fn test_delete_edge() {
        let src = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let tgt = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let eid = crate::create_edge(src, tgt, "LINK", pgrx::JsonB(serde_json::json!({})));

        // Edge should exist before delete.
        assert!(crate::get_edge(eid).is_some());

        let deleted = crate::delete_edge(eid);
        assert!(deleted, "delete_edge should return true for existing edge");

        // Edge should no longer be visible after logical delete.
        let after = crate::get_edge(eid);
        assert!(after.is_none(), "deleted edge should not be visible");
    }

    #[pg_test]
    fn test_edge_count() {
        let before = crate::edge_count();
        let n1 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let n2 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let n3 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        crate::create_edge(n1, n2, "A", pgrx::JsonB(serde_json::json!({})));
        crate::create_edge(n2, n3, "A", pgrx::JsonB(serde_json::json!({})));
        assert_eq!(crate::edge_count(), before + 2);
    }

    #[pg_test]
    fn test_neighbours() {
        let alice = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice"})),
        );
        let bob = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Bob"})),
        );
        let carol = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Carol"})),
        );

        // Alice → Bob, Alice → Carol
        crate::create_edge(alice, bob, "KNOWS", pgrx::JsonB(serde_json::json!({})));
        crate::create_edge(alice, carol, "KNOWS", pgrx::JsonB(serde_json::json!({})));

        let out_ids: Vec<i64> = crate::neighbours(alice, "OUT", None).collect();
        assert_eq!(out_ids.len(), 2, "Alice should have 2 outgoing neighbours");
        assert!(out_ids.contains(&bob), "Bob should be a neighbour of Alice");
        assert!(out_ids.contains(&carol), "Carol should be a neighbour of Alice");

        // Bob's incoming neighbours should include Alice.
        let in_ids: Vec<i64> = crate::neighbours(bob, "IN", None).collect();
        assert!(in_ids.contains(&alice), "Alice should be an incoming neighbour of Bob");
    }

    #[pg_test]
    fn test_expand() {
        let src = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let tgt = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let eid = crate::create_edge(
            src, tgt, "EDGE", pgrx::JsonB(serde_json::json!({})),
        );

        let rows: Vec<_> = crate::expand(src, "OUT", None).collect();
        assert_eq!(rows.len(), 1, "expand should return 1 row");
        let (rel_id, other, _type_id, _props) = &rows[0];
        assert_eq!(*rel_id, eid);
        assert_eq!(*other, tgt);
    }

    #[pg_test]
    fn test_update_node() {
        // Create a node with initial labels and properties.
        let node_id = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice"})),
        );
        assert!(node_id > 0);

        // Verify initial state.
        let before = crate::get_node(node_id).expect("node should exist");
        assert_eq!(before.0["properties"]["name"], serde_json::json!("Alice"));

        // Update labels and properties.
        let found = crate::update_node(
            node_id,
            vec!["Person".into(), "Engineer".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice", "role": "eng"})),
        );
        assert!(found, "update_node should return true for existing node");

        // Verify updated state.
        let after = crate::get_node(node_id).expect("updated node should still be visible");
        let labels = after.0["labels"].as_array().unwrap();
        assert!(labels.contains(&serde_json::json!("Engineer")), "should have new label");
        assert_eq!(after.0["properties"]["role"], serde_json::json!("eng"));

        // Old version should not be visible (only newest live version).
        let count_alice: Vec<_> = std::iter::once(after)
            .filter(|r| r.0["node_id"] == serde_json::json!(node_id))
            .collect();
        assert_eq!(count_alice.len(), 1, "only one live version should exist");
    }

    #[pg_test]
    fn test_delete_node() {
        let node_id = crate::create_node(
            vec!["Temp".into()],
            pgrx::JsonB(serde_json::json!({"x": 1})),
        );
        assert!(node_id > 0);

        // Should be visible before delete.
        assert!(crate::get_node(node_id).is_some(), "node should exist before delete");

        let deleted = crate::delete_node(node_id);
        assert!(deleted, "delete_node should return true");

        // Should NOT be visible after delete.
        assert!(crate::get_node(node_id).is_none(), "deleted node should not be visible");

        // Deleting again should return false.
        assert!(!crate::delete_node(node_id), "second delete should return false");
    }

    #[pg_test]
    fn test_am_stats() {
        // Create some nodes and edges, then check am_stats reports sensible values.
        let n1 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        let n2 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        crate::create_edge(n1, n2, "X", pgrx::JsonB(serde_json::json!({})));

        let stats = crate::am_stats();
        let live_nodes = stats.0["live_nodes"].as_u64().unwrap();
        let live_edges = stats.0["live_edges"].as_u64().unwrap();
        // We created at least 2 nodes and 1 edge in this test.
        assert!(live_nodes >= 2, "live_nodes should be >= 2, got {live_nodes}");
        assert!(live_edges >= 1, "live_edges should be >= 1, got {live_edges}");
    }

    // -----------------------------------------------------------------------
    // Phase 4 tests
    // -----------------------------------------------------------------------

    #[pg_test]
    fn test_add_label() {
        let node_id = crate::create_node(
            vec!["Person".into()],
            pgrx::JsonB(serde_json::json!({"name": "Dave"})),
        );

        // Add a new label.
        let added = crate::add_label(node_id, "Employee");
        assert!(added, "add_label should return true when label is new");

        let node = crate::get_node(node_id).expect("node should exist");
        let labels = node.0["labels"].as_array().unwrap();
        assert!(labels.contains(&serde_json::json!("Person")));
        assert!(labels.contains(&serde_json::json!("Employee")));

        // Adding the same label again should return false.
        let again = crate::add_label(node_id, "Employee");
        assert!(!again, "add_label should return false for duplicate label");
    }

    #[pg_test]
    fn test_remove_label() {
        let node_id = crate::create_node(
            vec!["Person".into(), "Temp".into()],
            pgrx::JsonB(serde_json::json!({})),
        );

        let removed = crate::remove_label(node_id, "Temp");
        assert!(removed, "remove_label should return true when label exists");

        let node = crate::get_node(node_id).expect("node should exist");
        let labels = node.0["labels"].as_array().unwrap();
        assert!(labels.contains(&serde_json::json!("Person")));
        assert!(!labels.contains(&serde_json::json!("Temp")));

        // Removing a label not present should return false.
        let again = crate::remove_label(node_id, "Temp");
        assert!(!again, "remove_label should return false when label is absent");
    }

    #[pg_test]
    fn test_detach_delete_node() {
        let alice = crate::create_node(vec!["Person".into()], pgrx::JsonB(serde_json::json!({})));
        let bob = crate::create_node(vec!["Person".into()], pgrx::JsonB(serde_json::json!({})));
        let e1 = crate::create_edge(alice, bob, "KNOWS", pgrx::JsonB(serde_json::json!({})));
        let e2 = crate::create_edge(bob, alice, "LIKES", pgrx::JsonB(serde_json::json!({})));

        let deleted = crate::detach_delete_node(alice);
        assert!(deleted, "detach_delete_node should return true");

        // Node should be gone.
        assert!(crate::get_node(alice).is_none(), "alice should be deleted");

        // Both edges should be gone (logically deleted).
        assert!(crate::get_edge(e1).is_none(), "edge e1 should be deleted");
        assert!(crate::get_edge(e2).is_none(), "edge e2 should be deleted");

        // Bob should still exist.
        assert!(crate::get_node(bob).is_some(), "bob should still exist");
    }

    #[pg_test]
    fn test_find_nodes_by_label() {
        let n1 = crate::create_node(vec!["Robot".into()], pgrx::JsonB(serde_json::json!({})));
        let n2 = crate::create_node(vec!["Robot".into()], pgrx::JsonB(serde_json::json!({})));
        crate::create_node(vec!["Human".into()], pgrx::JsonB(serde_json::json!({}))); // should not appear

        let found: Vec<i64> = crate::find_nodes(Some("Robot".into()), None).collect();
        assert!(found.contains(&n1), "n1 should be found");
        assert!(found.contains(&n2), "n2 should be found");
    }

    #[pg_test]
    fn test_find_nodes_by_property() {
        let n1 = crate::create_node(
            vec!["Widget".into()],
            pgrx::JsonB(serde_json::json!({"color": "red"})),
        );
        let _n2 = crate::create_node(
            vec!["Widget".into()],
            pgrx::JsonB(serde_json::json!({"color": "blue"})),
        );

        let filter = pgrx::JsonB(serde_json::json!({"color": "red"}));
        let found: Vec<i64> = crate::find_nodes(None, Some(filter)).collect();
        assert!(found.contains(&n1), "n1 (red) should be found");
    }

    #[pg_test]
    fn test_schema_info() {
        // Create a node and edge to populate registries.
        let n1 = crate::create_node(
            vec!["SchemaTestLabel".into()],
            pgrx::JsonB(serde_json::json!({"schema_key": 1})),
        );
        let n2 = crate::create_node(vec![], pgrx::JsonB(serde_json::json!({})));
        crate::create_edge(n1, n2, "SCHEMA_REL", pgrx::JsonB(serde_json::json!({})));

        let info = crate::schema_info();
        let labels = info.0["labels"].as_array().unwrap();
        let rel_types = info.0["rel_types"].as_array().unwrap();
        let prop_keys = info.0["property_keys"].as_array().unwrap();

        assert!(labels.iter().any(|l| l == "SchemaTestLabel"), "label should be in schema_info");
        assert!(rel_types.iter().any(|r| r == "SCHEMA_REL"), "rel_type should be in schema_info");
        assert!(prop_keys.iter().any(|k| k == "schema_key"), "prop_key should be in schema_info");
    }

    #[pg_test]
    fn test_overflow_props() {
        // Create a node with a large property value that exceeds PROP_INLINE_MAX.
        // PROP_INLINE_MAX is ~2100 bytes. Build a string longer than that.
        let big_value = "x".repeat(2500);
        let node_id = crate::create_node(
            vec!["Big".into()],
            pgrx::JsonB(serde_json::json!({"data": big_value})),
        );
        assert!(node_id > 0);

        // Should be retrievable with full property data.
        let result = crate::get_node(node_id).expect("big node should be visible");
        let data = result.0["properties"]["data"].as_str().unwrap();
        assert_eq!(data.len(), 2500, "overflow props should be fully recovered");
    }

    #[pg_test]
    fn test_find_edges() {
        let a = crate::create_node(vec!["A".into()], pgrx::JsonB(serde_json::json!({})));
        let b = crate::create_node(vec!["B".into()], pgrx::JsonB(serde_json::json!({})));
        let c = crate::create_node(vec!["C".into()], pgrx::JsonB(serde_json::json!({})));

        let e1 = crate::create_edge(a, b, "KNOWS", pgrx::JsonB(serde_json::json!({})));
        let e2 = crate::create_edge(a, c, "KNOWS", pgrx::JsonB(serde_json::json!({})));
        let e3 = crate::create_edge(b, c, "LIKES", pgrx::JsonB(serde_json::json!({})));

        // src filter
        let from_a: Vec<i64> = crate::find_edges(Some(a), None, None).collect();
        assert_eq!(from_a.len(), 2, "2 edges from A");
        assert!(from_a.contains(&e1) && from_a.contains(&e2));

        // dst filter
        let to_c: Vec<i64> = crate::find_edges(None, Some(c), None).collect();
        assert_eq!(to_c.len(), 2, "2 edges to C");
        assert!(to_c.contains(&e2) && to_c.contains(&e3));

        // type + src (index path)
        let a_knows: Vec<i64> = crate::find_edges(Some(a), None, Some("KNOWS".into())).collect();
        assert_eq!(a_knows.len(), 2, "2 KNOWS edges from A");

        // type + dst (index path)
        let likes_to_c: Vec<i64> = crate::find_edges(None, Some(c), Some("LIKES".into())).collect();
        assert_eq!(likes_to_c.len(), 1, "1 LIKES edge to C");
        assert_eq!(likes_to_c[0], e3);

        // src + dst + type (intersection)
        let a_to_b_knows: Vec<i64> = crate::find_edges(Some(a), Some(b), Some("KNOWS".into())).collect();
        assert_eq!(a_to_b_knows.len(), 1, "1 KNOWS edge from A to B");
        assert_eq!(a_to_b_knows[0], e1);

        // delete and verify removal from index
        assert!(crate::delete_edge(e3));
        let likes_after: Vec<i64> = crate::find_edges(None, Some(c), Some("LIKES".into())).collect();
        assert!(likes_after.is_empty(), "LIKES edge should be gone from index after delete");
    }

    // -----------------------------------------------------------------------
    // Phase 5 Cypher engine tests
    // -----------------------------------------------------------------------

    #[pg_test]
    fn test_cypher_match_all_nodes() {
        crate::create_node(
            vec!["CypherTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice"})),
        );
        crate::create_node(
            vec!["CypherTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "Bob"})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:CypherTest) RETURN n",
            None,
        ).collect();
        assert!(results.len() >= 2, "should find at least 2 CypherTest nodes, got {}", results.len());
    }

    #[pg_test]
    fn test_cypher_where_filter() {
        crate::create_node(
            vec!["FilterTest".into()],
            pgrx::JsonB(serde_json::json!({"age": 25})),
        );
        crate::create_node(
            vec!["FilterTest".into()],
            pgrx::JsonB(serde_json::json!({"age": 35})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:FilterTest) WHERE n.age > 30 RETURN n",
            None,
        ).collect();
        assert_eq!(results.len(), 1, "only node with age=35 should match");
    }

    #[pg_test]
    fn test_cypher_expand() {
        let a = crate::create_node(
            vec!["ExpandTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "A"})),
        );
        let b = crate::create_node(
            vec!["ExpandTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "B"})),
        );
        crate::create_edge(a, b, "KNOWS", pgrx::JsonB(serde_json::json!({})));

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (a:ExpandTest)-[:KNOWS]->(b:ExpandTest) RETURN a, b",
            None,
        ).collect();
        assert!(results.len() >= 1, "should find at least 1 KNOWS edge");
    }

    #[pg_test]
    fn test_cypher_return_property() {
        crate::create_node(
            vec!["PropReturn".into()],
            pgrx::JsonB(serde_json::json!({"name": "Charlie"})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:PropReturn) RETURN n.name",
            None,
        ).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, serde_json::json!("Charlie"));
    }

    #[pg_test]
    fn test_cypher_return_id() {
        let nid = crate::create_node(
            vec!["IdTest".into()],
            pgrx::JsonB(serde_json::json!({})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:IdTest) RETURN id(n)",
            None,
        ).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, serde_json::json!(nid));
    }

    #[pg_test]
    fn test_cypher_explain() {
        let plan = crate::cypher_explain(
            "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a, b",
        );
        assert!(plan.contains("LabelScan"), "explain should contain LabelScan");
        assert!(plan.contains("Expand"), "explain should contain Expand");
        assert!(plan.contains("Project"), "explain should contain Project");
    }

    #[pg_test]
    fn test_cypher_inline_properties() {
        crate::create_node(
            vec!["InlineTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "X"})),
        );
        crate::create_node(
            vec!["InlineTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "Y"})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:InlineTest {name: 'X'}) RETURN n.name",
            None,
        ).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, serde_json::json!("X"));
    }

    #[pg_test]
    fn test_cypher_is_null() {
        crate::create_node(
            vec!["NullTest".into()],
            pgrx::JsonB(serde_json::json!({"name": "Alice"})),
        );
        crate::create_node(
            vec!["NullTest".into()],
            pgrx::JsonB(serde_json::json!({})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:NullTest) WHERE n.name IS NULL RETURN n",
            None,
        ).collect();
        assert_eq!(results.len(), 1, "only the node without name should match IS NULL");
    }

    #[pg_test]
    fn test_cypher_labels_function() {
        crate::create_node(
            vec!["LabelFnTest".into(), "Extra".into()],
            pgrx::JsonB(serde_json::json!({})),
        );

        let results: Vec<pgrx::JsonB> = crate::cypher(
            "MATCH (n:LabelFnTest) RETURN labels(n)",
            None,
        ).collect();
        assert_eq!(results.len(), 1);
        let arr = results[0].0.as_array().unwrap();
        assert!(arr.iter().any(|v| v == "LabelFnTest"));
        assert!(arr.iter().any(|v| v == "Extra"));
    }
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_eddy'"]
    }
}

// pg_eddy — Phase 2: Edge Storage + Adjacency Lists
//
// This is the extension entry point.  At _PG_init we:
//   1. Register the custom WAL resource manager.
//   2. Nothing else; AM objects are created by the SQL script.
//
// shared_preload_libraries = 'pg_eddy'  is required.

use pgrx::prelude::*;

mod catalog;
mod error;
mod storage;

pgrx::pg_module_magic!();

// ---------------------------------------------------------------------------
// Extension SQL — schemas, registry tables, AM objects, and SQL functions.
// ---------------------------------------------------------------------------
extension_sql_file!("../sql/pg_eddy--0.3.0.sql", name = "pg_eddy_schema", finalize);

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
        // Look up the _pg_eddy.nodes relation by name via SPI/catalog.
        let rel = open_nodes_relation();
        crate::storage::node_store::insert_node(rel, node_id, &label_ids, &prop_bytes);
        pg_sys::table_close(rel, pg_sys::NoLock as pg_sys::LOCKMODE);
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

    record.map(|r| {
        let mut out = serde_json::Map::new();
        out.insert(
            "node_id".into(),
            serde_json::Value::Number(r.node_id.into()),
        );
        let label_names: Vec<_> = r.label_ids.iter().map(|id| label_name(*id)).collect();
        out.insert("labels".into(), serde_json::Value::Array(
            label_names.into_iter().map(serde_json::Value::String).collect(),
        ));
        let props = prop_store::decode(&r.prop_bytes, |kid| prop_key_name(kid));
        out.insert("properties".into(), serde_json::Value::Object(props));
        pgrx::JsonB(serde_json::Value::Object(out))
    })
}

/// Count all visible nodes in the graph.
#[pg_extern]
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
        let props = prop_store::decode(&r.prop_bytes, |kid| prop_key_name(kid));
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
    unsafe {
        use pgrx::pg_sys;
        let edge_rel = open_edges_relation();
        let found = crate::storage::edge_store::delete_edge(edge_rel, rel_id);
        pg_sys::table_close(edge_rel, pg_sys::NoLock as pg_sys::LOCKMODE);
        found
    }
}

/// Count all non-deleted edges in the graph.
#[pg_extern]
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

    SetOfIterator::new(ids.into_iter())
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
            let props = prop_store::decode(&e.prop_bytes, |kid| prop_key_name(kid));
            let _ = rel_type_name; // suppress unused warning — available if needed
            let props_json = pgrx::JsonB(serde_json::Value::Object(props));
            (e.edge_id, other, e.rel_type_id, props_json)
        })
        .collect();

    TableIterator::new(rows.into_iter())
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
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_eddy'"]
    }
}

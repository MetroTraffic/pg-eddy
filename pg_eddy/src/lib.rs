// pg_eddy — Phase 1: Node Storage
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
extension_sql_file!("../sql/pg_eddy--0.2.0.sql", name = "pg_eddy_schema", finalize);

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
}

#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["shared_preload_libraries = 'pg_eddy'"]
    }
}

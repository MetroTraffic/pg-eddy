use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use std::cell::Cell;

use crate::catalog::labels::{label_name, prop_key_name, rel_type_name};
use crate::storage::{edge_store::EdgeRecord, node_store::NodeRecord, prop_store};

pub const PG_TRICKLE_REPOSITORY: &str = "MetroTraffic/pg-trickle";
pub const PG_TRICKLE_REVISION: &str = "048c180e0b5e83a0f2214f3eabd7d069b6abea49";
pub const PG_TRICKLE_VERSION: &str = "0.82.0";

thread_local! {
    static IVM_NODES_OID: Cell<pgrx::pg_sys::Oid> = const { Cell::new(pgrx::pg_sys::Oid::INVALID) };
    static IVM_EDGES_OID: Cell<pgrx::pg_sys::Oid> = const { Cell::new(pgrx::pg_sys::Oid::INVALID) };
}

fn relation_available(
    cache: &'static std::thread::LocalKey<Cell<pgrx::pg_sys::Oid>>,
    relation_name: &str,
) -> bool {
    cache.with(|cached| {
        if cached.get() != pgrx::pg_sys::Oid::INVALID {
            return true;
        }

        let schema_name = std::ffi::CString::new("_pg_eddy").expect("valid schema name");
        let relation_name = std::ffi::CString::new(relation_name).expect("valid relation name");
        let relation_oid = unsafe {
            let schema_oid = pgrx::pg_sys::get_namespace_oid(schema_name.as_ptr(), true);
            if schema_oid == pgrx::pg_sys::Oid::INVALID {
                return false;
            }
            pgrx::pg_sys::get_relname_relid(relation_name.as_ptr(), schema_oid)
        };
        if relation_oid == pgrx::pg_sys::Oid::INVALID {
            return false;
        }
        cached.set(relation_oid);
        true
    })
}

fn nodes_available() -> bool {
    relation_available(&IVM_NODES_OID, "ivm_nodes")
}

fn edges_available() -> bool {
    relation_available(&IVM_EDGES_OID, "ivm_edges")
}

pub fn upsert_node(
    node_id: i64,
    labels: &[String],
    properties: &serde_json::Map<String, serde_json::Value>,
) {
    if !nodes_available() {
        return;
    }
    let labels = pgrx::JsonB(serde_json::json!(labels));
    let properties = pgrx::JsonB(serde_json::Value::Object(properties.clone()));
    Spi::run_with_args(
        "INSERT INTO _pg_eddy.ivm_nodes(node_id, labels, properties) \
         SELECT $1::bigint, \
                ARRAY(SELECT jsonb_array_elements_text($2::jsonb)), \
                $3::jsonb \
         ON CONFLICT (node_id) DO UPDATE \
         SET labels = EXCLUDED.labels, properties = EXCLUDED.properties",
        &[
            DatumWithOid::from(node_id),
            DatumWithOid::from(labels),
            DatumWithOid::from(properties),
        ],
    )
    .unwrap_or_else(|error| pgrx::error!("pg_eddy: IVM node mirror write failed: {error}"));
}

pub fn upsert_node_record(record: &NodeRecord) {
    let labels: Vec<String> = record.label_ids.iter().map(|id| label_name(*id)).collect();
    let properties = prop_store::decode(&record.prop_bytes, prop_key_name);
    upsert_node(record.node_id, &labels, &properties);
}

pub fn delete_node(node_id: i64) {
    if !nodes_available() {
        return;
    }
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.ivm_nodes WHERE node_id = $1",
        &[DatumWithOid::from(node_id)],
    )
    .unwrap_or_else(|error| pgrx::error!("pg_eddy: IVM node mirror delete failed: {error}"));
}

pub fn upsert_edge(
    rel_id: i64,
    rel_type: &str,
    source_node_id: i64,
    target_node_id: i64,
    properties: &serde_json::Map<String, serde_json::Value>,
) {
    if !edges_available() {
        return;
    }
    let properties = pgrx::JsonB(serde_json::Value::Object(properties.clone()));
    Spi::run_with_args(
        "INSERT INTO _pg_eddy.ivm_edges( \
             rel_id, rel_type, source_node_id, target_node_id, properties \
         ) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (rel_id) DO UPDATE \
         SET rel_type = EXCLUDED.rel_type, \
             source_node_id = EXCLUDED.source_node_id, \
             target_node_id = EXCLUDED.target_node_id, \
             properties = EXCLUDED.properties",
        &[
            DatumWithOid::from(rel_id),
            DatumWithOid::from(rel_type),
            DatumWithOid::from(source_node_id),
            DatumWithOid::from(target_node_id),
            DatumWithOid::from(properties),
        ],
    )
    .unwrap_or_else(|error| pgrx::error!("pg_eddy: IVM edge mirror write failed: {error}"));
}

pub fn upsert_edge_record(record: &EdgeRecord) {
    let properties = prop_store::decode(&record.prop_bytes, prop_key_name);
    upsert_edge(
        record.edge_id,
        &rel_type_name(record.rel_type_id),
        record.source_node_id,
        record.target_node_id,
        &properties,
    );
}

pub fn delete_edge(rel_id: i64) {
    if !edges_available() {
        return;
    }
    Spi::run_with_args(
        "DELETE FROM _pg_eddy.ivm_edges WHERE rel_id = $1",
        &[DatumWithOid::from(rel_id)],
    )
    .unwrap_or_else(|error| pgrx::error!("pg_eddy: IVM edge mirror delete failed: {error}"));
}

pub fn clear() {
    if !nodes_available() || !edges_available() {
        return;
    }
    Spi::run("TRUNCATE _pg_eddy.ivm_edges, _pg_eddy.ivm_nodes")
        .unwrap_or_else(|error| pgrx::error!("pg_eddy: IVM mirror truncate failed: {error}"));
}

pub fn rebuild() -> i64 {
    clear();

    let mut mirrored = 0_i64;
    unsafe {
        let snapshot = pgrx::pg_sys::GetTransactionSnapshot();

        let node_rel = crate::open_nodes_relation();
        let mut scan = crate::storage::node_store::NodeScanState::begin(node_rel, snapshot);
        while let Some(record) = scan.next() {
            upsert_node_record(&record);
            mirrored += 1;
        }
        pgrx::pg_sys::table_close(node_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);

        let edge_rel = crate::open_edges_relation();
        for record in crate::storage::edge_store::scan_all_edges(edge_rel, snapshot) {
            upsert_edge_record(&record);
            mirrored += 1;
        }
        pgrx::pg_sys::table_close(edge_rel, pgrx::pg_sys::NoLock as pgrx::pg_sys::LOCKMODE);
    }

    mirrored
}

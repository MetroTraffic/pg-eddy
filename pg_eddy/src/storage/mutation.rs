//! Authoritative graph mutations with transactional IVM mirror maintenance.

use pgrx::pg_sys;

use crate::catalog::labels::{label_name, prop_key_name, rel_type_name};
use crate::storage::cdc_protocol::{EdgeRow, Mutation, NodeRow};
use crate::storage::{cdc_message, edge_store::EdgeRecord, node_store::NodeRecord, prop_store};

fn node_row(record: &NodeRecord) -> NodeRow {
    NodeRow {
        node_id: record.node_id,
        labels: record.label_ids.iter().map(|id| label_name(*id)).collect(),
        properties: prop_store::decode(&record.prop_bytes, prop_key_name),
    }
}

fn edge_row(record: &EdgeRecord) -> EdgeRow {
    EdgeRow {
        rel_id: record.edge_id,
        rel_type: rel_type_name(record.rel_type_id),
        source_node_id: record.source_node_id,
        target_node_id: record.target_node_id,
        properties: prop_store::decode(&record.prop_bytes, prop_key_name),
    }
}

unsafe fn load_node_record(relation: pg_sys::Relation, node_id: i64) -> Option<NodeRecord> {
    let mut record = unsafe {
        crate::storage::node_store::find_node_by_id(
            relation,
            node_id,
            pg_sys::GetActiveSnapshot(),
        )
    }?;
    if record.overflow_blkno != 0 && record.prop_bytes.is_empty() {
        record.prop_bytes = unsafe {
            crate::storage::node_store::read_overflow_block(relation, record.overflow_blkno)
        };
    }
    Some(record)
}

unsafe fn load_edge_record(relation: pg_sys::Relation, edge_id: i64) -> Option<EdgeRecord> {
    unsafe {
        crate::storage::edge_store::find_edge_by_id(
            relation,
            edge_id,
            pg_sys::GetActiveSnapshot(),
        )
    }
}

pub unsafe fn insert_node(
    relation: pg_sys::Relation,
    node_id: i64,
    label_ids: &[i32],
    properties: &[u8],
) -> (pg_sys::BlockNumber, pg_sys::OffsetNumber) {
    let location = unsafe {
        crate::storage::node_store::insert_node(relation, node_id, label_ids, properties)
    };
    let record = crate::storage::node_store::NodeRecord {
        node_id,
        adj_slot_idx: 0,
        overflow_blkno: 0,
        label_ids: label_ids.to_vec(),
        prop_bytes: properties.to_vec(),
    };
    crate::ivm::mirror::upsert_node_record(&record);
    cdc_message::emit(&Mutation::NodeInsert {
        new: node_row(&record),
    });
    location
}

pub unsafe fn update_node(
    relation: pg_sys::Relation,
    node_id: i64,
    label_ids: &[i32],
    properties: &[u8],
) -> bool {
    let old = unsafe { load_node_record(relation, node_id) };
    let updated = unsafe {
        crate::storage::node_store::update_node(relation, node_id, label_ids, properties)
    };
    if updated {
        let record = crate::storage::node_store::NodeRecord {
            node_id,
            adj_slot_idx: 0,
            overflow_blkno: 0,
            label_ids: label_ids.to_vec(),
            prop_bytes: properties.to_vec(),
        };
        crate::ivm::mirror::upsert_node_record(&record);
        let old = old.unwrap_or_else(|| {
            pgrx::error!("pg_eddy: node {node_id} disappeared before CDC update capture")
        });
        cdc_message::emit(&Mutation::NodeUpdate {
            old: node_row(&old),
            new: node_row(&record),
        });
    }
    updated
}

pub unsafe fn delete_node_by_id(relation: pg_sys::Relation, node_id: i64) -> bool {
    let old = unsafe { load_node_record(relation, node_id) };
    let deleted = unsafe { crate::storage::node_store::delete_node_by_id(relation, node_id) };
    if deleted {
        crate::ivm::mirror::delete_node(node_id);
        let old = old.unwrap_or_else(|| {
            pgrx::error!("pg_eddy: node {node_id} disappeared before CDC delete capture")
        });
        cdc_message::emit(&Mutation::NodeDelete {
            old: node_row(&old),
        });
    }
    deleted
}

#[allow(clippy::too_many_arguments)]
pub unsafe fn insert_edge(
    node_relation: pg_sys::Relation,
    edge_relation: pg_sys::Relation,
    edge_id: i64,
    rel_type_id: i32,
    source_node_id: i64,
    target_node_id: i64,
    properties: &[u8],
) {
    unsafe {
        crate::storage::edge_store::insert_edge(
            node_relation,
            edge_relation,
            edge_id,
            rel_type_id,
            source_node_id,
            target_node_id,
            properties,
        )
    };
    let record = crate::storage::edge_store::EdgeRecord {
        edge_id,
        rel_type_id,
        source_node_id,
        target_node_id,
        prop_bytes: properties.to_vec(),
        block_num: 0,
        offset_num: 0,
        next_out_page: 0,
        next_out_slot: 0,
        next_in_page: 0,
        next_in_slot: 0,
    };
    crate::ivm::mirror::upsert_edge_record(&record);
    cdc_message::emit(&Mutation::EdgeInsert {
        new: edge_row(&record),
    });
}

pub unsafe fn update_edge_props(
    edge_relation: pg_sys::Relation,
    edge_id: i64,
    properties: &[u8],
) -> bool {
    let old = unsafe { load_edge_record(edge_relation, edge_id) };
    let updated = unsafe {
        crate::storage::edge_store::update_edge_props(edge_relation, edge_id, properties)
    };
    if updated {
        let old = old.unwrap_or_else(|| {
            pgrx::error!("pg_eddy: edge {edge_id} disappeared before CDC update capture")
        });
        let mut record = old.clone();
        record.prop_bytes = properties.to_vec();
        crate::ivm::mirror::upsert_edge_record(&record);
        cdc_message::emit(&Mutation::EdgeUpdate {
            old: edge_row(&old),
            new: edge_row(&record),
        });
    }
    updated
}

pub unsafe fn delete_edge(edge_relation: pg_sys::Relation, edge_id: i64) -> bool {
    let old = unsafe { load_edge_record(edge_relation, edge_id) };
    let deleted = unsafe { crate::storage::edge_store::delete_edge(edge_relation, edge_id) };
    if deleted {
        crate::ivm::mirror::delete_edge(edge_id);
        let old = old.unwrap_or_else(|| {
            pgrx::error!("pg_eddy: edge {edge_id} disappeared before CDC delete capture")
        });
        cdc_message::emit(&Mutation::EdgeDelete {
            old: edge_row(&old),
        });
    }
    deleted
}

pub fn emit_graph_reset() {
    cdc_message::emit(&Mutation::GraphReset);
}

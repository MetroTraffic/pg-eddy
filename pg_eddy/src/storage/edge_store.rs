// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

//! Edge storage engine for pg_eddy.
//!
//! Manages reading, writing, scanning, and adjacency-following of edges using
//! the custom edge page layout (see `storage/page.rs`).
//!
//! Key design decisions:
//! - Edge pages use standard `PageInit(page, BLCKSZ, 0)` (no pd_special area).
//! - Adjacency headers live in the NODE page's pd_special area.
//! - Edge deletes are logical only (set xmax); physical cleanup is Phase 3 VACUUM.
//! - Lock ordering: always acquire source node page BEFORE target node page
//!   (by block number) to prevent deadlocks under concurrent edge inserts.
//! - Chain sentinel: `next_out_slot == 0` (or `next_in_slot == 0`) means
//!   end-of-chain. Slot numbering starts at 1 (FirstOffsetNumber).

use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::page::{
    ADJ_HEADER_BYTES, EDGE_FIXED_DATA_SIZE, NodeAdjHeader, OFF_EDGE_NEXT_IN_PAGE,
    OFF_EDGE_NEXT_IN_SLOT, OFF_EDGE_NEXT_OUT_PAGE, OFF_EDGE_NEXT_OUT_SLOT,
    OFF_EDGE_PROP_DATA, OFF_EDGE_PROP_INLINE_LEN, OFF_EDGE_PROP_OVERFLOW_PAGE,
    OFF_EDGE_REL_ID, OFF_EDGE_REL_TYPE_ID, OFF_EDGE_SOURCE_NODE_ID, OFF_EDGE_TARGET_NODE_ID,
    PROP_INLINE_MAX,
};
use crate::storage::node_store::find_node_location;
use crate::storage::wal::{log_adj_update, log_edge_delete, log_edge_insert};

// ---------------------------------------------------------------------------
// Public edge record
// ---------------------------------------------------------------------------

/// A decoded edge record, ready for Rust/SQL consumption.
#[derive(Debug, Clone)]
pub struct EdgeRecord {
    pub edge_id: i64,
    pub rel_type_id: i32,
    pub source_node_id: i64,
    pub target_node_id: i64,
    pub prop_bytes: Vec<u8>,
    /// Stored block + offset (needed to follow chain without re-scanning).
    pub block_num: pg_sys::BlockNumber,
    pub offset_num: pg_sys::OffsetNumber,
    /// Next pointers in the adjacency chains.
    pub next_out_page: u32,
    pub next_out_slot: u16,
    pub next_in_page: u32,
    pub next_in_slot: u16,
}

/// Direction for adjacency-follow operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Out,
    In,
    Both,
}

impl Direction {
    pub fn from_str(s: &str) -> Self {
        match s.to_uppercase().as_str() {
            "IN" => Direction::In,
            "BOTH" => Direction::Both,
            _ => Direction::Out, // default to OUT
        }
    }
}

// ---------------------------------------------------------------------------
// Insert
// ---------------------------------------------------------------------------

/// Insert a new edge into the edge relation, updating source and target
/// adjacency headers in the node relation.
///
/// `node_rel`       — open node relation (pg_eddy_node AM).
/// `edge_rel`       — open edge relation (pg_eddy_edge AM).
/// `edge_id`        — globally unique edge id (from `edge_id_seq`).
/// `rel_type_id`    — resolved relationship type id.
/// `src_node_id`    — source node id.
/// `tgt_node_id`    — target node id.
/// `prop_bytes`     — pre-encoded property bytes (may be empty).
///
/// # Safety
/// Caller must ensure both relations are valid and open.
pub unsafe fn insert_edge(
    node_rel: pg_sys::Relation,
    edge_rel: pg_sys::Relation,
    edge_id: i64,
    rel_type_id: i32,
    src_node_id: i64,
    tgt_node_id: i64,
    prop_bytes: &[u8],
) {
    if prop_bytes.len() > PROP_INLINE_MAX {
        pgrx::error!(
            "pg_eddy PE200: edge property data ({} B) exceeds inline limit ({} B); overflow not yet implemented",
            prop_bytes.len(),
            PROP_INLINE_MAX,
        );
    }

    let snapshot = pg_sys::GetActiveSnapshot();

    // Find locations of source and target nodes using the public node_store helper.
    let (src_blk, src_off, src_adj_idx) = find_node_location(node_rel, src_node_id, snapshot)
        .unwrap_or_else(|| {
            pgrx::error!("pg_eddy PE400: source node {} not found", src_node_id)
        });
    let (tgt_blk, tgt_off, tgt_adj_idx) = find_node_location(node_rel, tgt_node_id, snapshot)
        .unwrap_or_else(|| {
            pgrx::error!("pg_eddy PE400: target node {} not found", tgt_node_id)
        });

    let _ = (src_off, tgt_off); // offsets not needed; we use stored adj indices

    let same_node_page = src_blk == tgt_blk;

    // ----- Lock node pages (in block-number order to prevent deadlocks) -----
    // For same-page case, we hold one buffer; for different pages, lower block first.
    let (src_node_buf, tgt_node_buf) = if same_node_page {
        let buf = pg_sys::ReadBufferExtended(
            node_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            src_blk,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        (buf, buf)
    } else if src_blk < tgt_blk {
        let sbuf = pg_sys::ReadBufferExtended(
            node_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            src_blk,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(sbuf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let tbuf = pg_sys::ReadBufferExtended(
            node_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            tgt_blk,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(tbuf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        (sbuf, tbuf)
    } else {
        // src_blk > tgt_blk — lock tgt first
        let tbuf = pg_sys::ReadBufferExtended(
            node_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            tgt_blk,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(tbuf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let sbuf = pg_sys::ReadBufferExtended(
            node_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            src_blk,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(sbuf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        (sbuf, tbuf)
    };

    // Read current adj headers while holding exclusive locks.
    let src_node_page = pg_sys::BufferGetPage(src_node_buf);
    let tgt_node_page = if same_node_page { src_node_page } else { pg_sys::BufferGetPage(tgt_node_buf) };

    let src_adj = read_adj_header(src_node_page, src_adj_idx);
    let tgt_adj = if same_node_page && src_adj_idx == tgt_adj_idx {
        src_adj.clone() // self-loop
    } else {
        read_adj_header(tgt_node_page, tgt_adj_idx)
    };

    // Build edge item. The chain `next_out/in` pointers point to the current
    // chain head so the new edge is inserted at the head of both lists.
    let edge_item = build_edge_item_bytes(
        edge_id,
        rel_type_id,
        src_node_id,
        tgt_node_id,
        src_adj.out_head_pg(),
        src_adj.out_head_sl(),
        tgt_adj.in_head_pg(),
        tgt_adj.in_head_sl(),
        prop_bytes,
    );

    // Find/extend the edge page with enough free space.
    let edge_buf = find_or_extend_edge_page(edge_rel, edge_item.len());
    let edge_page = pg_sys::BufferGetPage(edge_buf);
    let edge_blk = pg_sys::BufferGetBlockNumber(edge_buf);

    // ---- Critical section: must not error between START and END ----
    pg_sys::CritSectionCount += 1;

    // Insert edge item into edge page.
    let edge_off = pg_sys::PageAddItemExtended(
        edge_page,
        edge_item.as_ptr() as pg_sys::Item,
        edge_item.len() as pg_sys::Size,
        pg_sys::InvalidOffsetNumber,
        0,
    );
    if edge_off == pg_sys::InvalidOffsetNumber {
        pg_sys::CritSectionCount -= 1;
        pgrx::error!("pg_eddy: PageAddItemExtended failed for edge on block {edge_blk}");
    }

    // Set t_ctid self-pointer in edge item.
    {
        let iid = pg_sys::PageGetItemId(edge_page, edge_off);
        let hdr = pg_sys::PageGetItem(edge_page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData;
        pg_sys::ItemPointerSet(&mut (*hdr).t_ctid, edge_blk, edge_off);
    }

    // Compute new adjacency headers with updated head pointers and degrees.
    let mut new_src_adj = src_adj.clone();
    new_src_adj.set_out_head_pg(edge_blk);
    new_src_adj.set_out_head_sl(edge_off);
    new_src_adj.set_out_degree(src_adj.out_degree() + 1);

    // For self-loops, start from new_src_adj (which already has out updated).
    let mut new_tgt_adj = if same_node_page && src_adj_idx == tgt_adj_idx {
        new_src_adj.clone()
    } else {
        tgt_adj.clone()
    };
    new_tgt_adj.set_in_head_pg(edge_blk);
    new_tgt_adj.set_in_head_sl(edge_off);
    new_tgt_adj.set_in_degree(tgt_adj.in_degree() + 1);

    // Write updated adj headers into node pages.
    write_adj_header(src_node_page, src_adj_idx, &new_src_adj);
    if !(same_node_page && src_adj_idx == tgt_adj_idx) {
        write_adj_header(tgt_node_page, tgt_adj_idx, &new_tgt_adj);
    } else {
        // Self-loop: write the combined header (has both out + in updated).
        write_adj_header(src_node_page, src_adj_idx, &new_tgt_adj);
    }

    // WAL log: EDGE_INSERT record.
    let lsn_edge = log_edge_insert(edge_buf, edge_page, edge_off, &edge_item);
    // WAL log: ADJ_UPDATE for source node.
    let final_src_adj = if same_node_page && src_adj_idx == tgt_adj_idx { &new_tgt_adj } else { &new_src_adj };
    let lsn_src = log_adj_update(src_node_buf, src_adj_idx as u16, final_src_adj);
    // WAL log: ADJ_UPDATE for target node (skip for self-loop — already covered by src).
    let lsn_tgt = if same_node_page && src_adj_idx == tgt_adj_idx {
        lsn_src
    } else {
        log_adj_update(tgt_node_buf, tgt_adj_idx as u16, &new_tgt_adj)
    };

    // Set LSNs and mark dirty.
    pg_sys::PageSetLSN(edge_page, lsn_edge);
    pg_sys::MarkBufferDirty(edge_buf);
    pg_sys::PageSetLSN(src_node_page, lsn_src);
    pg_sys::MarkBufferDirty(src_node_buf);
    if !same_node_page {
        pg_sys::PageSetLSN(tgt_node_page, lsn_tgt);
        pg_sys::MarkBufferDirty(tgt_node_buf);
    }

    pg_sys::CritSectionCount -= 1;
    // ---- End critical section ----

    // Release edge buffer.
    pg_sys::UnlockReleaseBuffer(edge_buf);
    // Release node buffers (avoid double-release for same-page case).
    if same_node_page {
        pg_sys::UnlockReleaseBuffer(src_node_buf);
    } else {
        pg_sys::UnlockReleaseBuffer(src_node_buf);
        pg_sys::UnlockReleaseBuffer(tgt_node_buf);
    }
}

// ---------------------------------------------------------------------------
// Logical delete
// ---------------------------------------------------------------------------

/// Logically delete an edge by setting its xmax.
///
/// The edge remains in its position in the adjacency chain; it will be
/// skipped during traversal via MVCC visibility and reclaimed by VACUUM.
///
/// # Safety
/// Caller must ensure `edge_rel` is valid and open.
pub unsafe fn delete_edge(
    edge_rel: pg_sys::Relation,
    edge_id: i64,
) -> bool {
    let snapshot = pg_sys::GetActiveSnapshot();
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(
        edge_rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
    );
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();

    for blkno in 0..nblocks {
        let buf = pg_sys::ReadBufferExtended(
            edge_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            blkno,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let page = pg_sys::BufferGetPage(buf);
        let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);

        let mut found_off = pg_sys::InvalidOffsetNumber;
        for off in pg_sys::FirstOffsetNumber..=max_off {
            let iid = pg_sys::PageGetItemId(page, off);
            if (*iid).lp_flags() != pg_sys::LP_NORMAL {
                continue;
            }
            let item_len = (*iid).lp_len() as usize;
            if item_len < hdr_size + EDGE_FIXED_DATA_SIZE {
                continue;
            }
            let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
            let raw = std::slice::from_raw_parts(item, item_len);
            let data = &raw[hdr_size..];
            let eid = i64::from_le_bytes(
                data[OFF_EDGE_REL_ID..OFF_EDGE_REL_ID + 8].try_into().unwrap(),
            );

            // Check MVCC: skip already-deleted edges.
            let hdr = item as *const pg_sys::HeapTupleHeaderData;
            let xmax_invalid = ((*hdr).t_infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;

            if eid == edge_id && xmax_invalid {
                found_off = off;
                break;
            }
            // Skip: either wrong edge or already deleted.
            let _ = snapshot;
        }

        if found_off != pg_sys::InvalidOffsetNumber {
            // Apply logical delete under critical section.
            pg_sys::CritSectionCount += 1;
            let iid = pg_sys::PageGetItemId(page, found_off);
            let hdr =
                pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData;
            let xmax = pg_sys::GetCurrentTransactionId();
            pg_sys::HeapTupleHeaderSetXmax(hdr, xmax);
            (*hdr).t_infomask &= !(pg_sys::HEAP_XMAX_INVALID as u16);

            let lsn = log_edge_delete(buf, page, found_off, xmax);
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
            pg_sys::CritSectionCount -= 1;

            pg_sys::UnlockReleaseBuffer(buf);
            return true;
        }

        pg_sys::UnlockReleaseBuffer(buf);
    }
    false
}

// ---------------------------------------------------------------------------
// Find edge by id
// ---------------------------------------------------------------------------

/// Scan the edge relation for an edge with `edge_id` and return it.
///
/// Phase 2: simplified visibility — returns LP_NORMAL, non-deleted edges.
pub unsafe fn find_edge_by_id(
    edge_rel: pg_sys::Relation,
    edge_id: i64,
    _snapshot: pg_sys::Snapshot,
) -> Option<EdgeRecord> {
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(
        edge_rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
    );
    for blkno in 0..nblocks {
        let buf = pg_sys::ReadBufferExtended(
            edge_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            blkno,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
        let page = pg_sys::BufferGetPage(buf);
        let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);

        for off in pg_sys::FirstOffsetNumber..=max_off {
            if let Some(rec) = read_edge_at_offset(page, blkno, off) {
                if rec.edge_id == edge_id {
                    pg_sys::UnlockReleaseBuffer(buf);
                    return Some(rec);
                }
            }
        }
        pg_sys::UnlockReleaseBuffer(buf);
    }
    None
}

/// Count all non-deleted edges in the relation.
pub unsafe fn count_edges(
    edge_rel: pg_sys::Relation,
    _snapshot: pg_sys::Snapshot,
) -> i64 {
    let nblocks = pg_sys::RelationGetNumberOfBlocksInFork(
        edge_rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
    );
    let mut count: i64 = 0;
    for blkno in 0..nblocks {
        let buf = pg_sys::ReadBufferExtended(
            edge_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            blkno,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
        let page = pg_sys::BufferGetPage(buf);
        let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);
        for off in pg_sys::FirstOffsetNumber..=max_off {
            if read_edge_at_offset(page, blkno, off).is_some() {
                count += 1;
            }
        }
        pg_sys::UnlockReleaseBuffer(buf);
    }
    count
}

// ---------------------------------------------------------------------------
// Adjacency-follow scan
// ---------------------------------------------------------------------------

/// Follow the adjacency chain for `node_id` in the given `direction`.
///
/// Returns all visible edges (LP_NORMAL, not logically deleted) connected
/// to the node in the requested direction. Optionally filters by `rel_type_id`.
///
/// Chain traversal reads each edge slot even if invisible, to retrieve the
/// `next_*` pointer. Only visible slots are included in the result.
///
/// # Safety
/// Caller must ensure both relations are valid and open.
pub unsafe fn adjacency_follow(
    node_rel: pg_sys::Relation,
    edge_rel: pg_sys::Relation,
    node_id: i64,
    direction: Direction,
    rel_type_filter: Option<i32>,
    snapshot: pg_sys::Snapshot,
) -> Vec<EdgeRecord> {
    // Find the node using the public helper.
    let (node_blk, _node_off, adj_idx) = match find_node_location(node_rel, node_id, snapshot) {
        Some(loc) => loc,
        None => return Vec::new(),
    };

    // Read the adjacency header (brief shared lock).
    let node_buf = pg_sys::ReadBufferExtended(
        node_rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        node_blk,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(node_buf, pg_sys::BUFFER_LOCK_SHARE as i32);
    let node_page = pg_sys::BufferGetPage(node_buf);
    let adj = read_adj_header(node_page, adj_idx);
    pg_sys::UnlockReleaseBuffer(node_buf);

    let mut results = Vec::new();

    // Follow out-chain.
    if matches!(direction, Direction::Out | Direction::Both) {
        follow_chain(
            edge_rel,
            adj.out_head_pg(),
            adj.out_head_sl(),
            true, // is_out_chain
            rel_type_filter,
            &mut results,
        );
    }
    // Follow in-chain.
    if matches!(direction, Direction::In | Direction::Both) {
        follow_chain(
            edge_rel,
            adj.in_head_pg(),
            adj.in_head_sl(),
            false, // is_in_chain
            rel_type_filter,
            &mut results,
        );
    }

    results
}

/// Walk one adjacency chain and collect visible edge records.
///
/// We always read each slot (even invisible ones or LP_DEAD) to extract the
/// next pointer; only visible LP_NORMAL edges are appended to `out`.
unsafe fn follow_chain(
    edge_rel: pg_sys::Relation,
    mut head_pg: u32,
    mut head_sl: u16,
    is_out_chain: bool,
    rel_type_filter: Option<i32>,
    out: &mut Vec<EdgeRecord>,
) {
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();

    while head_sl != 0 {
        // head_sl != 0 means there is an edge here.
        let buf = pg_sys::ReadBufferExtended(
            edge_rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            head_pg,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32);
        let page = pg_sys::BufferGetPage(buf);

        // Safety: head_sl is 1-based (valid item offset).
        let off = head_sl;
        let iid = pg_sys::PageGetItemId(page, off);
        let flags = (*iid).lp_flags();
        let item_len = (*iid).lp_len() as usize;

        // LP_DEAD: VACUUM has marked this slot dead but kept data for chain following.
        // We skip it (don't yield) but still follow the next pointer.
        let is_lp_dead = flags == pg_sys::LP_DEAD;
        let is_lp_normal = flags == pg_sys::LP_NORMAL;

        if !is_lp_normal && !is_lp_dead {
            // LP_UNUSED or other invalid — stop traversal (broken chain).
            pg_sys::UnlockReleaseBuffer(buf);
            break;
        }
        if item_len < hdr_size + EDGE_FIXED_DATA_SIZE {
            pg_sys::UnlockReleaseBuffer(buf);
            break;
        }

        let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
        let raw = std::slice::from_raw_parts(item, item_len);
        let data = &raw[hdr_size..];

        // Read next pointer BEFORE releasing the lock.
        let (next_pg, next_sl) = if is_out_chain {
            let pg = u32::from_le_bytes(data[OFF_EDGE_NEXT_OUT_PAGE..OFF_EDGE_NEXT_OUT_PAGE + 4].try_into().unwrap());
            let sl = u16::from_le_bytes(data[OFF_EDGE_NEXT_OUT_SLOT..OFF_EDGE_NEXT_OUT_SLOT + 2].try_into().unwrap());
            (pg, sl)
        } else {
            let pg = u32::from_le_bytes(data[OFF_EDGE_NEXT_IN_PAGE..OFF_EDGE_NEXT_IN_PAGE + 4].try_into().unwrap());
            let sl = u16::from_le_bytes(data[OFF_EDGE_NEXT_IN_SLOT..OFF_EDGE_NEXT_IN_SLOT + 2].try_into().unwrap());
            (pg, sl)
        };

        // Check visibility: skip logically-deleted edges.
        let hdr = item as *const pg_sys::HeapTupleHeaderData;
        let xmax_invalid = ((*hdr).t_infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
        if xmax_invalid && is_lp_normal {
            // Edge is alive — decode and maybe yield it.
            if let Some(rec) = decode_edge_record(data, item_len - hdr_size, head_pg, off) {
                let passes_filter = rel_type_filter.map_or(true, |t| t == rec.rel_type_id);
                if passes_filter {
                    out.push(rec);
                }
            }
        }
        // If xmax is set (edge deleted) or LP_DEAD, we still follow the chain pointer.

        pg_sys::UnlockReleaseBuffer(buf);

        head_pg = next_pg;
        head_sl = next_sl;
    }
}

// ---------------------------------------------------------------------------
// Edge page initialization
// ---------------------------------------------------------------------------

/// Initialize `page` as a fresh edge page (no pd_special area).
///
/// # Safety
/// `page` must be a writeable, exclusively-locked buffer page.
pub unsafe fn init_edge_page(page: pg_sys::Page) {
    pg_sys::PageInit(page, pg_sys::BLCKSZ as pg_sys::Size, 0);
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Read the adj header at `idx` from the pd_special area of a node page.
///
/// # Safety
/// `page` must be a valid node page with `PD_NODE_SPECIAL_SIZE` special area.
unsafe fn read_adj_header(page: pg_sys::Page, idx: usize) -> NodeAdjHeader {
    let special = pg_sys::PageGetSpecialPointer(page) as *const u8;
    let offset = idx * ADJ_HEADER_BYTES;
    let bytes = std::slice::from_raw_parts(special.add(offset), ADJ_HEADER_BYTES);
    NodeAdjHeader::from_bytes(bytes.try_into().unwrap())
}

/// Write `hdr` at `idx` into the pd_special area of a node page.
///
/// # Safety
/// `page` must be an exclusively-locked node page.
unsafe fn write_adj_header(page: pg_sys::Page, idx: usize, hdr: &NodeAdjHeader) {
    let special = pg_sys::PageGetSpecialPointer(page) as *mut u8;
    let offset = idx * ADJ_HEADER_BYTES;
    std::ptr::copy_nonoverlapping(hdr.as_bytes().as_ptr(), special.add(offset), ADJ_HEADER_BYTES);
}

/// Build the raw bytes for an edge item (including HeapTupleHeaderData).
unsafe fn build_edge_item_bytes(
    edge_id: i64,
    rel_type_id: i32,
    source_node_id: i64,
    target_node_id: i64,
    next_out_pg: u32,
    next_out_sl: u16,
    next_in_pg: u32,
    next_in_sl: u16,
    prop_bytes: &[u8],
) -> Vec<u8> {
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    let total = hdr_size + EDGE_FIXED_DATA_SIZE + prop_bytes.len();
    let mut buf = vec![0u8; total];

    // Fill HeapTupleHeaderData.
    let hdr = buf.as_mut_ptr() as *mut pg_sys::HeapTupleHeaderData;
    (*hdr).t_infomask2 = 0;
    (*hdr).t_infomask = pg_sys::HEAP_XMAX_INVALID as u16;
    (*hdr).t_hoff = hdr_size as u8;
    pg_sys::ItemPointerSetInvalid(&mut (*hdr).t_ctid);
    let xid = pg_sys::GetCurrentTransactionId();
    let cid = pg_sys::GetCurrentCommandId(true);
    pg_sys::HeapTupleHeaderSetXmin(hdr, xid);
    pg_sys::HeapTupleHeaderSetCmin(hdr, cid);
    pg_sys::HeapTupleHeaderSetXmax(hdr, pg_sys::InvalidTransactionId);

    // Fill data portion.
    let data = &mut buf[hdr_size..];
    data[OFF_EDGE_REL_ID..OFF_EDGE_REL_ID + 8].copy_from_slice(&edge_id.to_le_bytes());
    data[OFF_EDGE_REL_TYPE_ID..OFF_EDGE_REL_TYPE_ID + 4]
        .copy_from_slice(&rel_type_id.to_le_bytes());
    data[OFF_EDGE_SOURCE_NODE_ID..OFF_EDGE_SOURCE_NODE_ID + 8]
        .copy_from_slice(&source_node_id.to_le_bytes());
    data[OFF_EDGE_TARGET_NODE_ID..OFF_EDGE_TARGET_NODE_ID + 8]
        .copy_from_slice(&target_node_id.to_le_bytes());
    data[OFF_EDGE_NEXT_OUT_PAGE..OFF_EDGE_NEXT_OUT_PAGE + 4]
        .copy_from_slice(&next_out_pg.to_le_bytes());
    data[OFF_EDGE_NEXT_OUT_SLOT..OFF_EDGE_NEXT_OUT_SLOT + 2]
        .copy_from_slice(&next_out_sl.to_le_bytes());
    data[OFF_EDGE_NEXT_IN_PAGE..OFF_EDGE_NEXT_IN_PAGE + 4]
        .copy_from_slice(&next_in_pg.to_le_bytes());
    data[OFF_EDGE_NEXT_IN_SLOT..OFF_EDGE_NEXT_IN_SLOT + 2]
        .copy_from_slice(&next_in_sl.to_le_bytes());
    let plen = prop_bytes.len() as u16;
    data[OFF_EDGE_PROP_INLINE_LEN..OFF_EDGE_PROP_INLINE_LEN + 2]
        .copy_from_slice(&plen.to_le_bytes());
    data[OFF_EDGE_PROP_OVERFLOW_PAGE..OFF_EDGE_PROP_OVERFLOW_PAGE + 4]
        .copy_from_slice(&0u32.to_le_bytes());
    if !prop_bytes.is_empty() {
        data[OFF_EDGE_PROP_DATA..OFF_EDGE_PROP_DATA + prop_bytes.len()]
            .copy_from_slice(prop_bytes);
    }

    buf
}

/// Decode an edge data slice into an `EdgeRecord`.
///
/// Returns `None` if the data is too short or malformed.
fn decode_edge_record(
    data: &[u8],
    data_len: usize,
    blkno: pg_sys::BlockNumber,
    off: pg_sys::OffsetNumber,
) -> Option<EdgeRecord> {
    if data_len < EDGE_FIXED_DATA_SIZE {
        return None;
    }
    let edge_id = i64::from_le_bytes(data[OFF_EDGE_REL_ID..OFF_EDGE_REL_ID + 8].try_into().ok()?);
    let rel_type_id =
        i32::from_le_bytes(data[OFF_EDGE_REL_TYPE_ID..OFF_EDGE_REL_TYPE_ID + 4].try_into().ok()?);
    let source_node_id = i64::from_le_bytes(
        data[OFF_EDGE_SOURCE_NODE_ID..OFF_EDGE_SOURCE_NODE_ID + 8].try_into().ok()?,
    );
    let target_node_id = i64::from_le_bytes(
        data[OFF_EDGE_TARGET_NODE_ID..OFF_EDGE_TARGET_NODE_ID + 8].try_into().ok()?,
    );
    let next_out_page = u32::from_le_bytes(
        data[OFF_EDGE_NEXT_OUT_PAGE..OFF_EDGE_NEXT_OUT_PAGE + 4].try_into().ok()?,
    );
    let next_out_slot = u16::from_le_bytes(
        data[OFF_EDGE_NEXT_OUT_SLOT..OFF_EDGE_NEXT_OUT_SLOT + 2].try_into().ok()?,
    );
    let next_in_page = u32::from_le_bytes(
        data[OFF_EDGE_NEXT_IN_PAGE..OFF_EDGE_NEXT_IN_PAGE + 4].try_into().ok()?,
    );
    let next_in_slot = u16::from_le_bytes(
        data[OFF_EDGE_NEXT_IN_SLOT..OFF_EDGE_NEXT_IN_SLOT + 2].try_into().ok()?,
    );
    let prop_len = u16::from_le_bytes(
        data[OFF_EDGE_PROP_INLINE_LEN..OFF_EDGE_PROP_INLINE_LEN + 2].try_into().ok()?,
    ) as usize;

    if data_len < EDGE_FIXED_DATA_SIZE + prop_len {
        return None;
    }
    let prop_bytes = data[OFF_EDGE_PROP_DATA..OFF_EDGE_PROP_DATA + prop_len].to_vec();

    Some(EdgeRecord {
        edge_id,
        rel_type_id,
        source_node_id,
        target_node_id,
        prop_bytes,
        block_num: blkno,
        offset_num: off,
        next_out_page,
        next_out_slot,
        next_in_page,
        next_in_slot,
    })
}

/// Read an edge at a specific offset in the page.
///
/// Phase 2: simplified visibility — returns live (not deleted, xmin visible)
/// LP_NORMAL edges only. Checks xmin against the commit log so that ghost
/// tuples from rolled-back transactions are correctly filtered out.
unsafe fn read_edge_at_offset(
    page: pg_sys::Page,
    blkno: pg_sys::BlockNumber,
    off: pg_sys::OffsetNumber,
) -> Option<EdgeRecord> {
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    let iid = pg_sys::PageGetItemId(page, off);
    if (*iid).lp_flags() != pg_sys::LP_NORMAL {
        return None;
    }
    let item_len = (*iid).lp_len() as usize;
    if item_len < hdr_size + EDGE_FIXED_DATA_SIZE {
        return None;
    }
    let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
    let hdr = item as *const pg_sys::HeapTupleHeaderData;

    // Check xmin: skip tuples from aborted or in-progress-other transactions.
    // Union access is safe here — we know this is a heap tuple.
    let xmin = (*hdr).t_choice.t_heap.t_xmin;
    if xmin == pg_sys::InvalidTransactionId {
        return None;
    }
    // Fast path: use hint bits when they've been set.
    let infomask = (*hdr).t_infomask;
    let xmin_committed = (infomask & pg_sys::HEAP_XMIN_COMMITTED as u16) != 0;
    let xmin_invalid_flag = (infomask & pg_sys::HEAP_XMIN_INVALID as u16) != 0;
    let xmin_visible = if xmin_committed {
        true
    } else if xmin_invalid_flag {
        false
    } else {
        // No hint bit: check the transaction status directly.
        pg_sys::TransactionIdIsCurrentTransactionId(xmin)
            || pg_sys::TransactionIdDidCommit(xmin)
    };
    if !xmin_visible {
        return None;
    }

    // Skip logically deleted edges (xmax is set and valid).
    let xmax_invalid = (infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
    if !xmax_invalid {
        return None;
    }

    let raw = std::slice::from_raw_parts(item, item_len);
    let data = &raw[hdr_size..];
    decode_edge_record(data, item_len - hdr_size, blkno, off)
}

/// Find or extend an edge page with enough free space.
///
/// Returns an exclusively-locked buffer.
unsafe fn find_or_extend_edge_page(rel: pg_sys::Relation, item_size: usize) -> pg_sys::Buffer {
    let nblocks =
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM);

    if nblocks > 0 {
        let last = nblocks - 1;
        let buf = pg_sys::ReadBufferExtended(
            rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            last,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let page = pg_sys::BufferGetPage(buf);
        let free = page_free_space(page);
        if free >= item_size + size_of::<pg_sys::ItemIdData>() {
            return buf;
        }
        pg_sys::UnlockReleaseBuffer(buf);
    }

    // Extend with a new page.
    let buf = pg_sys::ReadBufferExtended(
        rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        pg_sys::InvalidBlockNumber,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let page = pg_sys::BufferGetPage(buf);
    init_edge_page(page);
    buf
}

/// Return free bytes available on a page.
#[inline]
unsafe fn page_free_space(page: pg_sys::Page) -> usize {
    let phdr = page as *mut pg_sys::PageHeaderData;
    let upper = (*phdr).pd_upper as usize;
    let lower = (*phdr).pd_lower as usize;
    if upper >= lower { upper - lower } else { 0 }
}

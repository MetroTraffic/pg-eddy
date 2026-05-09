// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

/// VACUUM support for pg_eddy (Phase 3).
///
/// Strategy
/// --------
/// Dead tuples are identified by a non-zero xmax that is visible to all
/// current and future snapshots (i.e., xmax < OldestNonRemovableXid).
///
/// For each such dead item pointer we:
///   1. Set the ItemId flags to LP_DEAD (preserves the data in-page so that
///      the adjacency chain follower can still read the `next_*` pointers).
///   2. WAL-log the batch of LP_DEAD changes via XLOG_PG_EDDY_VACUUM_PAGE.
///
/// Physical page compaction (PageRepairFragmentation) is deferred to Phase 4;
/// VACUUM only marks slots LP_DEAD in this release.
use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::page::{OFF_EDGE_NEXT_IN_PAGE, OFF_EDGE_NEXT_IN_SLOT, OFF_EDGE_NEXT_OUT_PAGE, OFF_EDGE_NEXT_OUT_SLOT};
use crate::storage::wal::log_vacuum_page;

// ---------------------------------------------------------------------------
// Public stats
// ---------------------------------------------------------------------------

/// Statistics returned from `vacuum_relation`.
#[derive(Debug, Default, Clone, Copy)]
pub struct VacuumStats {
    pub dead_nodes: u64,
    pub live_nodes: u64,
    pub dead_edges: u64,
    pub live_edges: u64,
}

// ---------------------------------------------------------------------------
// vacuum_relation
// ---------------------------------------------------------------------------

/// Scan every page of `rel` and mark dead tuples LP_DEAD.
///
/// # Safety
/// * `rel` must be a valid, open relation that pg_eddy manages.
/// * Should only be called from the AM `vacuum` callback (or `VACUUM` command).
pub unsafe fn vacuum_relation(rel: pg_sys::Relation) -> VacuumStats {
    let mut stats = VacuumStats::default();

    // Obtain the oldest XID that is still visible to at least one snapshot.
    // Any tuple whose xmax < oldest_xmin is dead to all current and future
    // transactions and can be reclaimed.
    let oldest_xmin = pg_sys::GetOldestNonRemovableTransactionId(rel);

    let nblocks =
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM);

    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();

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

        // Collect dead offsets under shared lock first (avoids long exclusive hold).
        let mut dead_offsets: Vec<u16> = Vec::new();
        let mut page_live: u64 = 0;
        let mut page_dead: u64 = 0;

        for off in pg_sys::FirstOffsetNumber..=max_off {
            let iid = pg_sys::PageGetItemId(page, off);
            let flags = (*iid).lp_flags();
            if flags != pg_sys::LP_NORMAL {
                continue;
            }
            let item_len = (*iid).lp_len() as usize;
            // Need at least a HeapTupleHeader + some data.
            if item_len < hdr_size + 1 {
                continue;
            }
            let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
            let hdr = item as *const pg_sys::HeapTupleHeaderData;
            let infomask = (*hdr).t_infomask;

            // Check xmax.
            let xmax_invalid = (infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
            if xmax_invalid {
                // No deleter — live tuple.
                page_live += 1;
                continue;
            }
            // Deleter exists; check if it committed before oldest_xmin.
            let xmax = (*hdr).t_choice.t_heap.t_xmax;
            if xmax != pg_sys::InvalidTransactionId
                && pg_sys::TransactionIdPrecedes(xmax, oldest_xmin)
                && pg_sys::TransactionIdDidCommit(xmax)
            {
                dead_offsets.push(off as u16);
                page_dead += 1;
            } else {
                page_live += 1;
            }
        }

        pg_sys::UnlockReleaseBuffer(buf);

        // Determine whether this is a node or edge page and update stats.
        // We use NODE_FIXED_DATA_SIZE as a heuristic: node pages have
        // pd_special > 0, edge pages have pd_special == sizeof(PageHeaderData).
        // For now, accumulate totals without per-type split — callers in lib.rs
        // will run vacuum on nodes and edges separately.
        stats.live_nodes += page_live; // will be corrected by caller
        stats.dead_nodes += page_dead;

        if dead_offsets.is_empty() {
            continue;
        }

        // Re-acquire exclusive lock to mark LP_DEAD.
        let buf = pg_sys::ReadBufferExtended(
            rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            blkno,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        );
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let page = pg_sys::BufferGetPage(buf);

        pg_sys::CritSectionCount += 1;
        for &off in &dead_offsets {
            let iid = pg_sys::PageGetItemId(page, off as pg_sys::OffsetNumber);
            if (*iid).lp_flags() == pg_sys::LP_NORMAL {
                (*iid).set_lp_flags(pg_sys::LP_DEAD);
            }
        }

        let lsn = log_vacuum_page(buf, &dead_offsets);
        pg_sys::PageSetLSN(page, lsn);
        pg_sys::MarkBufferDirty(buf);
        pg_sys::CritSectionCount -= 1;

        pg_sys::UnlockReleaseBuffer(buf);
    }

    stats
}

// ---------------------------------------------------------------------------
// Accessor helpers for edge chain pointers (used by vacuum to verify chains)
// ---------------------------------------------------------------------------

/// Read the `next_out_page` and `next_out_slot` from an edge item data slice.
#[inline]
pub fn edge_next_out(data: &[u8]) -> (u32, u16) {
    let pg = u32::from_le_bytes(data[OFF_EDGE_NEXT_OUT_PAGE..OFF_EDGE_NEXT_OUT_PAGE + 4].try_into().unwrap());
    let sl = u16::from_le_bytes(data[OFF_EDGE_NEXT_OUT_SLOT..OFF_EDGE_NEXT_OUT_SLOT + 2].try_into().unwrap());
    (pg, sl)
}

/// Read the `next_in_page` and `next_in_slot` from an edge item data slice.
#[inline]
pub fn edge_next_in(data: &[u8]) -> (u32, u16) {
    let pg = u32::from_le_bytes(data[OFF_EDGE_NEXT_IN_PAGE..OFF_EDGE_NEXT_IN_PAGE + 4].try_into().unwrap());
    let sl = u16::from_le_bytes(data[OFF_EDGE_NEXT_IN_SLOT..OFF_EDGE_NEXT_IN_SLOT + 2].try_into().unwrap());
    (pg, sl)
}

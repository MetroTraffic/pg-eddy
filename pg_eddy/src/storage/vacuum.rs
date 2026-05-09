// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

/// VACUUM support for pg_eddy (Phase 3/4).
///
/// Phase 3: mark dead slots LP_DEAD.
/// Phase 4: physical compaction (PageRepairFragmentation) for node pages after
/// LP_DEAD marking; adj headers cleared for dead nodes; WAL-logged as a full
/// page image (XLOG_PG_EDDY_NODE_COMPACT).
///
/// Edge pages are still only marked LP_DEAD (physical compaction requires
/// adj-chain repair, deferred to a future release).
use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::node_store::{compact_node_page, is_overflow_page};
use crate::storage::page::NODE_FIXED_DATA_SIZE;
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

        // Skip overflow blocks (plain pages with no special area).
        if is_overflow_page(page) {
            pg_sys::UnlockReleaseBuffer(buf);
            continue;
        }

        // Detect node page vs edge page:
        // Node pages: pd_special < BLCKSZ (they have a special area).
        // Edge pages: pd_special == BLCKSZ (no special area, but they ARE data pages).
        // We detect by checking the pd_special offset: node pages have
        // pd_special = PD_NODE_SPECIAL_OFFSET (5792), edge pages have 8192.
        // But `is_overflow_page` already returned false, so a page with
        // pd_special == 8192 here is an EDGE page.
        let phdr = page as *const pg_sys::PageHeaderData;
        let is_node_page = (*phdr).pd_special as usize != pg_sys::BLCKSZ as usize;

        let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);

        // Collect dead offsets under shared lock first (avoids long exclusive hold).
        let mut dead_offsets: Vec<u16> = Vec::new();
        let mut dead_adj_slots: Vec<usize> = Vec::new(); // for node pages only
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
                dead_offsets.push(off);
                page_dead += 1;
                // For node pages, collect the adj_slot_idx so we can clear the adj header.
                if is_node_page && item_len >= hdr_size + NODE_FIXED_DATA_SIZE {
                    let data = std::slice::from_raw_parts(item.add(hdr_size), item_len - hdr_size);
                    let adj_slot_idx = u16::from_le_bytes(
                        data[crate::storage::page::OFF_ADJ_SLOT..crate::storage::page::OFF_ADJ_SLOT + 2]
                            .try_into()
                            .unwrap_or([0, 0]),
                    ) as usize;
                    dead_adj_slots.push(adj_slot_idx);
                }
            } else {
                page_live += 1;
            }
        }

        pg_sys::UnlockReleaseBuffer(buf);

        if is_node_page {
            stats.live_nodes += page_live;
            stats.dead_nodes += page_dead;
        } else {
            stats.live_edges += page_live;
            stats.dead_edges += page_dead;
        }

        if dead_offsets.is_empty() {
            continue;
        }

        // Re-acquire exclusive lock to mark LP_DEAD and (for node pages) compact.
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

        let lsn = if is_node_page {
            // Node pages: physically compact (clears adj headers + PageRepairFragmentation)
            // and WAL-log as a full-page image. The LP_DEAD marking above is a
            // prerequisite so PageRepairFragmentation knows which slots to reclaim.
            compact_node_page(buf, &dead_adj_slots)
        } else {
            // Edge pages: LP_DEAD only (chain repair deferred to future release).
            let l = log_vacuum_page(buf, &dead_offsets);
            pg_sys::PageSetLSN(page, l);
            pg_sys::MarkBufferDirty(buf);
            l
        };

        if is_node_page {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
        pg_sys::CritSectionCount -= 1;

        pg_sys::UnlockReleaseBuffer(buf);
    }

    stats
}


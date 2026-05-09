// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

//! Node storage engine for pg_eddy.
//!
//! Manages reading, writing, and scanning nodes from PostgreSQL buffer-manager
//! pages using our custom page layout (see `storage/page.rs`).
//!
//! Safety rules:
//! - All buffer access MUST hold the appropriate lock.
//! - WAL logging MUST happen inside a critical section
//!   (CritSectionCount incremented).
//! - Callers are responsible for supplying a valid, open `Relation`.

use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::page::{
    MAX_LABELS_PER_NODE, NODE_FIXED_DATA_SIZE, OFF_ADJ_SLOT, OFF_LABEL_COUNT,
    OFF_LABEL_IDS, OFF_NODE_ID, OFF_PROP_INLINE_LEN, OFF_PROP_OVERFLOW_PAGE,
    PD_NODE_SPECIAL_OFFSET, PD_NODE_SPECIAL_SIZE, PROP_INLINE_MAX,
};
use crate::storage::wal::{log_node_compact, log_node_delete, log_node_insert};

// ---------------------------------------------------------------------------
// Public node record
// ---------------------------------------------------------------------------

/// A decoded node record, ready for Rust/SQL consumption.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub node_id: i64,
    pub adj_slot_idx: u16,
    /// When non-zero, `prop_bytes` is empty and the actual properties are
    /// stored in this block of the node relation. Callers must call
    /// `read_overflow_block(rel, overflow_blkno)` to get the full data.
    pub overflow_blkno: u32,
    pub label_ids: Vec<i32>,
    pub prop_bytes: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Insert
// ---------------------------------------------------------------------------

/// Insert a new node into the relation.
///
/// `rel`       — open relation (must use pg_eddy_node AM).  
/// `node_id`   — globally unique node id (from `node_id_seq`).  
/// `label_ids` — resolved label ids (may be empty).  
/// `prop_bytes`— pre-encoded property bytes (may be empty).
///
/// Returns the `BlockNumber` where the node was stored (informational).
///
/// # Safety
/// Caller must ensure `rel` is valid and open.
pub unsafe fn insert_node(
    rel: pg_sys::Relation,
    node_id: i64,
    label_ids: &[i32],
    prop_bytes: &[u8],
) -> pg_sys::BlockNumber {
    // Guard: labels
    if label_ids.len() > MAX_LABELS_PER_NODE {
        pgrx::error!("pg_eddy PE101: node has {} labels, max is {}", label_ids.len(), MAX_LABELS_PER_NODE);
    }

    let needs_overflow = prop_bytes.len() > PROP_INLINE_MAX;

    // Compute item size (inline_props = empty when overflow; size is the same
    // regardless of overflow_blkno value since it's a fixed-size u32 field).
    let inline_props_for_size: &[u8] = if needs_overflow { &[] } else { prop_bytes };
    let item_size_estimate = build_node_item_bytes_ovf(
        node_id, 0, label_ids, inline_props_for_size, 0,
    ).len();

    // Step 1: Find/extend the NODE page FIRST (exclusive lock acquired here).
    // We MUST do this before write_overflow_block to avoid a deadlock:
    // write_overflow_block creates a new block (P_NEW) and holds it exclusively.
    // If find_or_extend_page runs after, it tries the last block first, which
    // would be the newly-created overflow block — causing a self-deadlock.
    let buf = find_or_extend_page(rel, item_size_estimate);

    // Step 2: Write overflow data if needed. The overflow block is always a
    // NEW block (P_NEW = block after the node page we just found), so there
    // is no lock conflict with the node page buffer we're already holding.
    let ovf_result: Option<(pg_sys::Buffer, pg_sys::BlockNumber)> =
        if needs_overflow {
            Some(write_overflow_block(rel, prop_bytes))
        } else {
            None
        };
    let (inline_props, overflow_blkno) = match &ovf_result {
        Some((_, blk)) => (&[][..], *blk),
        None            => (prop_bytes, 0u32),
    };

    // Build final item bytes with correct overflow_blkno.
    let item_bytes = build_node_item_bytes_ovf(
        node_id, 0 /*adj placeholder*/, label_ids, inline_props, overflow_blkno,
    );

    let page = pg_sys::BufferGetPage(buf);
    let blkno = pg_sys::BufferGetBlockNumber(buf);

    // ----- Critical section: must not error between START and END -----
    // ALL page modifications (overflow page + node page) happen here, under a
    // single CritSectionCount guard, so that:
    //   1. PageAddItemExtended for overflow data runs inside the critical section.
    //   2. XLogInsert registers and images both buffers atomically.
    //   3. PageSetLSN + MarkBufferDirty for both pages happen before
    //      CritSectionCount drops to zero.
    // This is required by PostgreSQL's WAL protocol: between any page
    // modification and the corresponding MarkBufferDirty + PageSetLSN, no
    // error must be able to escape (otherwise the page is left in a modified
    // but un-WAL-logged state).
    unsafe {
        pg_sys::CritSectionCount += 1;
    }

    // First, write the prop data into the overflow page (if used).
    // This runs inside the critical section so the modification is covered.
    let ovf_item_ok = if let Some((ovf_buf, _)) = ovf_result.as_ref() {
        let ovf_page = unsafe { pg_sys::BufferGetPage(*ovf_buf) };
        let ovf_off = unsafe {
            pg_sys::PageAddItemExtended(
                ovf_page,
                prop_bytes.as_ptr() as pg_sys::Item,
                prop_bytes.len() as pg_sys::Size,
                pg_sys::InvalidOffsetNumber,
                0,
            )
        };
        ovf_off != pg_sys::InvalidOffsetNumber
    } else {
        true
    };

    if !ovf_item_ok {
        unsafe { pg_sys::CritSectionCount -= 1; }
        if let Some((ovf_buf, _)) = ovf_result {
            unsafe { pg_sys::UnlockReleaseBuffer(ovf_buf) };
        }
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
        pgrx::error!("pg_eddy: PageAddItemExtended failed for overflow block");
    }

    let off = unsafe {
        pg_sys::PageAddItemExtended(
            page,
            item_bytes.as_ptr() as pg_sys::Item,
            item_bytes.len() as pg_sys::Size,
            pg_sys::InvalidOffsetNumber,
            0, // flags: 0 = find next free slot
        )
    };
    if off == pg_sys::InvalidOffsetNumber {
        // Roll back critical section before panic
        unsafe { pg_sys::CritSectionCount -= 1; }
        if let Some((ovf_buf, _)) = ovf_result {
            unsafe { pg_sys::UnlockReleaseBuffer(ovf_buf) };
        }
        pgrx::error!("pg_eddy: PageAddItemExtended failed on block {blkno}");
    }

    // Fix adj_slot_idx in the in-page copy to the actual slot index (off - 1).
    // This is permanent: it does not change when properties are updated.
    let adj_slot_idx = (off - pg_sys::FirstOffsetNumber) as u16;
    unsafe {
        let iid = pg_sys::PageGetItemId(page, off);
        let item_in_page = pg_sys::PageGetItem(page as *const _, iid) as *mut u8;
        let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
        let data_ptr = item_in_page.add(hdr_size);
        std::ptr::copy_nonoverlapping(
            adj_slot_idx.to_le_bytes().as_ptr(),
            data_ptr.add(OFF_ADJ_SLOT),
            2,
        );
    }

    // Set the self-pointer (t_ctid) in the in-page copy of the header.
    unsafe {
        let iid = pg_sys::PageGetItemId(page, off);
        let item_in_page = pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData;
        pg_sys::ItemPointerSet(&mut (*item_in_page).t_ctid, blkno, off);
    }

    // WAL-log the insert (with optional overflow block).
    // log_node_insert registers both buffers and takes a full-page image of
    // the overflow block (REGBUF_FORCE_IMAGE). Both pages are registered here,
    // inside the critical section, so the WAL record covers all modifications.
    let overflow_buf_opt: Option<pg_sys::Buffer> = ovf_result.as_ref().map(|(b, _)| *b);
    let lsn = unsafe { log_node_insert(buf, page, off, &item_bytes, overflow_buf_opt) };

    unsafe {
        pg_sys::PageSetLSN(page, lsn);
        pg_sys::MarkBufferDirty(buf);
        // Also set LSN and mark dirty for the overflow page (if any).
        // Must be done inside the same critical section, after XLogInsert.
        if let Some(ovf_buf) = overflow_buf_opt {
            let ovf_page = pg_sys::BufferGetPage(ovf_buf);
            pg_sys::PageSetLSN(ovf_page, lsn);
            pg_sys::MarkBufferDirty(ovf_buf);
        }
        pg_sys::CritSectionCount -= 1;
    }
    // ----- End critical section -----

    // Release overflow buffer (after WAL critical section).
    if let Some(ovf_buf) = overflow_buf_opt {
        unsafe { pg_sys::UnlockReleaseBuffer(ovf_buf) };
    }

    unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    blkno
}

// ---------------------------------------------------------------------------
// Scan state
// ---------------------------------------------------------------------------

/// Sequential scan state for node pages.
///
/// Usage:
/// ```ignore
/// let mut state = NodeScanState::begin(rel, snapshot);
/// while let Some(rec) = state.next() { ... }
/// state.end();
/// ```
pub struct NodeScanState {
    rel: pg_sys::Relation,
    snapshot: pg_sys::Snapshot,
    current_blk: pg_sys::BlockNumber,
    current_off: pg_sys::OffsetNumber,
    nblocks: pg_sys::BlockNumber,
}

impl NodeScanState {
    /// Begin a sequential scan. Does NOT hold any buffer pin between calls.
    pub unsafe fn begin(rel: pg_sys::Relation, snapshot: pg_sys::Snapshot) -> Self {
        let nblocks = unsafe {
            pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
        };
        NodeScanState {
            rel,
            snapshot,
            current_blk: 0,
            current_off: pg_sys::FirstOffsetNumber,
            nblocks,
        }
    }

    /// Return the next visible node, or `None` when exhausted.
    pub unsafe fn next(&mut self) -> Option<NodeRecord> {
        loop {
            if self.current_blk >= self.nblocks {
                return None;
            }
            let buf = unsafe {
                pg_sys::ReadBufferExtended(
                    self.rel,
                    pg_sys::ForkNumber::MAIN_FORKNUM,
                    self.current_blk,
                    pg_sys::ReadBufferMode::RBM_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            unsafe { pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32) };
            let page = unsafe { pg_sys::BufferGetPage(buf) };

            // Skip overflow blocks (plain pages with no special area).
            if unsafe { is_overflow_page(page) } {
                unsafe { pg_sys::UnlockReleaseBuffer(buf) };
                self.current_blk += 1;
                self.current_off = pg_sys::FirstOffsetNumber;
                continue;
            }

            let max_off = unsafe { pg_sys::PageGetMaxOffsetNumber(page as *const _) };

            let mut found: Option<NodeRecord> = None;

            while self.current_off <= max_off {
                let off = self.current_off;
                self.current_off += 1;

                let rec = unsafe { read_node_at_offset(page, buf, self.snapshot, off) };
                if let Some(r) = rec {
                    found = Some(r);
                    break;
                }
            }

            unsafe { pg_sys::UnlockReleaseBuffer(buf) };

            // Resolve overflow props outside the buffer lock.
            if let Some(mut r) = found {
                if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                    r.prop_bytes = unsafe { read_overflow_block(self.rel, r.overflow_blkno) };
                }
                return Some(r);
            }
            // Move to next block
            self.current_blk += 1;
            self.current_off = pg_sys::FirstOffsetNumber;
        }
    }

    /// End the scan (currently a no-op since we don't hold persistent pins).
    #[allow(dead_code)]
    pub fn end(self) {
        // Nothing to clean up.
    }
}

// ---------------------------------------------------------------------------
// Find a node by id (sequential scan — O(n), Phase 1 only)
// ---------------------------------------------------------------------------

/// Scan the relation for a node with `node_id`, return it if visible.
/// Uses `SnapshotSelf` semantics (finds the node just inserted in this xact).
pub unsafe fn find_node_by_id(
    rel: pg_sys::Relation,
    node_id: i64,
    snapshot: pg_sys::Snapshot,
) -> Option<NodeRecord> {
    let nblocks = unsafe {
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
    };
    for blkno in 0..nblocks {
        let buf = unsafe {
            pg_sys::ReadBufferExtended(
                rel,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                blkno,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            )
        };
        unsafe { pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32) };
        let page = unsafe { pg_sys::BufferGetPage(buf) };

        // Skip overflow blocks.
        if unsafe { is_overflow_page(page) } {
            unsafe { pg_sys::UnlockReleaseBuffer(buf) };
            continue;
        }

        let max_off = unsafe { pg_sys::PageGetMaxOffsetNumber(page as *const _) };

        let mut found: Option<NodeRecord> = None;
        for off in pg_sys::FirstOffsetNumber..=max_off {
            if let Some(rec) = unsafe { read_node_at_offset(page, buf, snapshot, off) } {
                if rec.node_id == node_id {
                    found = Some(rec);
                    break;
                }
            }
        }
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
        if let Some(mut r) = found {
            if r.overflow_blkno != 0 && r.prop_bytes.is_empty() {
                r.prop_bytes = unsafe { read_overflow_block(rel, r.overflow_blkno) };
            }
            return Some(r);
        }
    }
    None
}

/// Count all visible nodes in the relation.
pub unsafe fn count_nodes(rel: pg_sys::Relation, snapshot: pg_sys::Snapshot) -> i64 {
    let nblocks = unsafe {
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
    };
    let mut count: i64 = 0;
    for blkno in 0..nblocks {
        let buf = unsafe {
            pg_sys::ReadBufferExtended(
                rel,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                blkno,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            )
        };
        unsafe { pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_SHARE as i32) };
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        // Skip overflow pages.
        if unsafe { is_overflow_page(page) } {
            unsafe { pg_sys::UnlockReleaseBuffer(buf) };
            continue;
        }
        let max_off = unsafe { pg_sys::PageGetMaxOffsetNumber(page as *const _) };
        for off in pg_sys::FirstOffsetNumber..=max_off {
            if unsafe { read_node_at_offset(page, buf, snapshot, off) }.is_some() {
                count += 1;
            }
        }
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
    count
}

// ---------------------------------------------------------------------------
// Initialize a new node page (call from relation_set_new_filelocator and
// when extending the relation).
// ---------------------------------------------------------------------------

/// Initialize `page` as a fresh node page (zero pd_special area).
///
/// # Safety
/// `page` must be a writeable page buffer, locked exclusively.
pub unsafe fn init_node_page(page: pg_sys::Page) {
    unsafe {
        pg_sys::PageInit(
            page,
            pg_sys::BLCKSZ as pg_sys::Size,
            PD_NODE_SPECIAL_SIZE as pg_sys::Size,
        );
        // Zero out the pd_special area (adjacency headers — Region 1).
        let special = pg_sys::PageGetSpecialPointer(page) as *mut u8;
        std::ptr::write_bytes(special, 0, PD_NODE_SPECIAL_SIZE);
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Build the raw bytes for a node item (without overflow support — kept for
/// update_node which handles overflow separately).
///
/// `adj_slot_idx` — the permanent adjacency-header slot index (0-based); pass
/// 0 as a placeholder in `insert_node` and overwrite in the in-page copy
/// after `PageAddItemExtended` returns the actual offset.
unsafe fn build_node_item_bytes(
    node_id: i64,
    adj_slot_idx: u16,
    label_ids: &[i32],
    prop_bytes: &[u8],
) -> Vec<u8> {
    build_node_item_bytes_ovf(node_id, adj_slot_idx, label_ids, prop_bytes, 0)
}

/// Build the raw bytes for a node item with explicit overflow block control.
///
/// When `overflow_blkno != 0`, `prop_bytes` should be empty (the data is in
/// the overflow block), and `prop_inline_len = 0` with
/// `prop_overflow_page = overflow_blkno` will be stored.
unsafe fn build_node_item_bytes_ovf(
    node_id: i64,
    adj_slot_idx: u16,
    label_ids: &[i32],
    prop_bytes: &[u8],
    overflow_blkno: u32,
) -> Vec<u8> {
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    let total = hdr_size + NODE_FIXED_DATA_SIZE + label_ids.len() * 4 + prop_bytes.len();
    let mut buf = vec![0u8; total];

    // --- Fill in HeapTupleHeaderData ---
    let hdr = buf.as_mut_ptr() as *mut pg_sys::HeapTupleHeaderData;
    unsafe {
        // 0 attributes in our custom tuple (we manage the layout ourselves)
        (*hdr).t_infomask2 = 0;
        // HEAP_XMAX_INVALID: no deleter yet
        (*hdr).t_infomask = pg_sys::HEAP_XMAX_INVALID as u16;
        // t_hoff: offset from start of header to tuple data (= full header size)
        (*hdr).t_hoff = hdr_size as u8;
        // t_ctid: self-pointer; filled in after PageAddItemExtended returns.
        pg_sys::ItemPointerSetInvalid(&mut (*hdr).t_ctid);

        let xid = pg_sys::GetCurrentTransactionId();
        let cid = pg_sys::GetCurrentCommandId(true);
        pg_sys::HeapTupleHeaderSetXmin(hdr, xid);
        pg_sys::HeapTupleHeaderSetCmin(hdr, cid);
        pg_sys::HeapTupleHeaderSetXmax(hdr, pg_sys::InvalidTransactionId);
    }

    // --- Fill in the data portion ---
    let data = &mut buf[hdr_size..];
    // node_id (8)
    data[OFF_NODE_ID..OFF_NODE_ID + 8].copy_from_slice(&node_id.to_le_bytes());
    // adj_slot_idx (2)
    data[OFF_ADJ_SLOT..OFF_ADJ_SLOT + 2].copy_from_slice(&adj_slot_idx.to_le_bytes());
    // label_count (1)
    data[OFF_LABEL_COUNT] = label_ids.len() as u8;
    // prop_inline_len (2) — 0 when overflow
    let nprop = if overflow_blkno == 0 { prop_bytes.len() as u16 } else { 0u16 };
    data[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2].copy_from_slice(&nprop.to_le_bytes());
    // prop_overflow_page (4)
    data[OFF_PROP_OVERFLOW_PAGE..OFF_PROP_OVERFLOW_PAGE + 4].copy_from_slice(&overflow_blkno.to_le_bytes());
    // _pad (1) — already 0 from vec![]
    // label_ids (4 each)
    for (i, lid) in label_ids.iter().enumerate() {
        let off = OFF_LABEL_IDS + i * 4;
        data[off..off + 4].copy_from_slice(&lid.to_le_bytes());
    }
    // prop_bytes (only when inline)
    if overflow_blkno == 0 {
        let prop_start = OFF_LABEL_IDS + label_ids.len() * 4;
        data[prop_start..prop_start + prop_bytes.len()].copy_from_slice(prop_bytes);
    }

    buf
}

// ---------------------------------------------------------------------------
// Overflow block helpers
// ---------------------------------------------------------------------------

/// Write a plain "overflow" block to the node relation containing `prop_bytes`.
///
/// Overflow blocks use PageInit(BLCKSZ, 0) — no pd_special area (pd_special
/// offset = BLCKSZ = 8192).  A single item at offset 1 holds the raw bytes.
/// Callers MUST release the returned buffer after WAL-logging.
///
/// # Safety
/// `rel` must be a valid, open node relation with an exclusive lock held.
pub unsafe fn write_overflow_block(
    rel: pg_sys::Relation,
    prop_bytes: &[u8],
) -> (pg_sys::Buffer, pg_sys::BlockNumber) {
    // Extend the relation with a new block.
    // MUST be called OUTSIDE any critical section (ReadBufferExtended can error).
    let buf = pg_sys::ReadBufferExtended(
        rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        pg_sys::InvalidBlockNumber,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    let blkno = pg_sys::BufferGetBlockNumber(buf);
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let page = pg_sys::BufferGetPage(buf);
    // Initialise as a plain page (no special area).
    // pd_special = BLCKSZ after PageInit(page, BLCKSZ, 0), which is how we
    // identify overflow pages in is_overflow_page().
    // PageInit is safe here (outside critical section) because we hold the
    // exclusive buffer lock and haven't registered this buffer for WAL yet.
    pg_sys::PageInit(page, pg_sys::BLCKSZ as pg_sys::Size, 0);

    // DO NOT call PageAddItemExtended, PageSetLSN, or MarkBufferDirty here.
    // The caller (insert_node) MUST do all page modifications for this buffer
    // inside its own critical section, so that all page changes (node page +
    // overflow page) and WAL registration happen atomically under a single
    // CritSectionCount guard.

    (buf, blkno)
}

/// Read raw property bytes from an overflow block.
///
/// Returns an empty `Vec` if the block is absent, uninitialised, or has no
/// items (should not happen in normal operation).
///
/// # Safety
/// `rel` must be a valid, open node relation.
pub unsafe fn read_overflow_block(
    rel: pg_sys::Relation,
    blkno: pg_sys::BlockNumber,
) -> Vec<u8> {
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
    let result = if max_off >= pg_sys::FirstOffsetNumber {
        let iid = pg_sys::PageGetItemId(page, pg_sys::FirstOffsetNumber);
        let len = (*iid).lp_len() as usize;
        let data = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
        std::slice::from_raw_parts(data, len).to_vec()
    } else {
        Vec::new()
    };
    pg_sys::UnlockReleaseBuffer(buf);
    result
}

/// Returns `true` if `page` is an overflow block (pd_special == BLCKSZ).
///
/// Node pages have `pd_special == PD_NODE_SPECIAL_OFFSET (5792)`.
/// Overflow pages are initialised with `PageInit(BLCKSZ, 0)` so
/// `pd_special == BLCKSZ (8192)`.
#[inline]
pub unsafe fn is_overflow_page(page: pg_sys::Page) -> bool {
    let phdr = page as *const pg_sys::PageHeaderData;
    (*phdr).pd_special as usize == pg_sys::BLCKSZ as usize
        && (*phdr).pd_special as usize != PD_NODE_SPECIAL_OFFSET
}

/// Compact a node page: clear adj headers for dead items, call
/// PageRepairFragmentation, and WAL-log the result as a full-page image.
///
/// `dead_adj_slots` — adj_slot_idx values of the LP_DEAD items being reclaimed.
///
/// # Safety
/// Called inside vacuum_relation with the buffer held exclusively and inside a
/// critical section.
pub unsafe fn compact_node_page(
    buf: pg_sys::Buffer,
    dead_adj_slots: &[usize],
) -> pg_sys::XLogRecPtr {
    let page = pg_sys::BufferGetPage(buf);

    // Clear adj headers for each dead item (zero the pd_special entry).
    if !dead_adj_slots.is_empty() {
        let special = pg_sys::PageGetSpecialPointer(page) as *mut u8;
        for &slot in dead_adj_slots {
            let offset = slot * crate::storage::page::ADJ_HEADER_BYTES;
            std::ptr::write_bytes(special.add(offset), 0, crate::storage::page::ADJ_HEADER_BYTES);
        }
    }

    // Physically compact the page (reclaim LP_DEAD slot space).
    pg_sys::PageRepairFragmentation(page);

    // WAL-log the full compacted page image.
    log_node_compact(buf)
}

/// Find a buffer with enough free space, or extend the relation.
///
/// Returns an exclusively-locked buffer ready for writing.
/// Skips overflow blocks (which have pd_special == BLCKSZ).
unsafe fn find_or_extend_page(rel: pg_sys::Relation, item_size: usize) -> pg_sys::Buffer {
    let nblocks = unsafe {
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
    };

    // Scan backwards from the last block looking for a node page with space.
    // Skip overflow blocks (pd_special == BLCKSZ, created by write_overflow_block).
    if nblocks > 0 {
        let mut blk = nblocks - 1;
        loop {
            let buf = unsafe {
                pg_sys::ReadBufferExtended(
                    rel,
                    pg_sys::ForkNumber::MAIN_FORKNUM,
                    blk,
                    pg_sys::ReadBufferMode::RBM_NORMAL,
                    std::ptr::null_mut(),
                )
            };
            unsafe { pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32) };
            let page = unsafe { pg_sys::BufferGetPage(buf) };
            // Skip overflow blocks.
            if unsafe { is_overflow_page(page) } {
                unsafe { pg_sys::UnlockReleaseBuffer(buf) };
                if blk == 0 {
                    break;
                }
                blk -= 1;
                continue;
            }
            let free = unsafe { page_free_space(page) };
            if free >= item_size + size_of::<pg_sys::ItemIdData>() {
                return buf;
            }
            unsafe { pg_sys::UnlockReleaseBuffer(buf) };
            break;
        }
    }

    // Extend the relation with a new page.
    let buf = unsafe {
        pg_sys::ReadBufferExtended(
            rel,
            pg_sys::ForkNumber::MAIN_FORKNUM,
            pg_sys::InvalidBlockNumber,
            pg_sys::ReadBufferMode::RBM_NORMAL,
            std::ptr::null_mut(),
        )
    };
    unsafe {
        pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
        let page = pg_sys::BufferGetPage(buf);
        init_node_page(page);
    }
    buf
}

/// Read and decode a node item at `off` in `page`.
///
/// Phase 3: checks xmin visibility and xmax (logical delete) for correct
/// MVCC semantics.
unsafe fn read_node_at_offset(
    page: pg_sys::Page,
    _buf: pg_sys::Buffer,
    _snapshot: pg_sys::Snapshot,
    off: pg_sys::OffsetNumber,
) -> Option<NodeRecord> {
    let iid = unsafe { pg_sys::PageGetItemId(page, off) };
    // Only read LP_NORMAL items.
    let flags = unsafe { (*iid).lp_flags() };
    if flags != pg_sys::LP_NORMAL {
        return None;
    }
    let item_len = unsafe { (*iid).lp_len() } as usize;
    let item = unsafe { pg_sys::PageGetItem(page as *const _, iid) as *const u8 };

    // MVCC xmin check: exclude tuples from aborted or in-progress transactions.
    let hdr = item as *const pg_sys::HeapTupleHeaderData;
    let infomask = unsafe { (*hdr).t_infomask };
    let xmin_committed = (infomask & pg_sys::HEAP_XMIN_COMMITTED as u16) != 0;
    let xmin_invalid_flag = (infomask & pg_sys::HEAP_XMIN_INVALID as u16) != 0;
    let xmin_visible = if xmin_committed {
        true
    } else if xmin_invalid_flag {
        false
    } else {
        let xmin = unsafe { (*hdr).t_choice.t_heap.t_xmin };
        xmin != pg_sys::InvalidTransactionId
            && (unsafe { pg_sys::TransactionIdIsCurrentTransactionId(xmin) }
                || unsafe { pg_sys::TransactionIdDidCommit(xmin) })
    };
    if !xmin_visible {
        return None;
    }

    // MVCC xmax check: exclude logically deleted tuples.
    let xmax_invalid = (infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
    if !xmax_invalid {
        return None; // logically deleted
    }

    // Decode the data portion.
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    if item_len < hdr_size + NODE_FIXED_DATA_SIZE {
        return None; // Malformed
    }
    let raw = unsafe { std::slice::from_raw_parts(item, item_len) };
    let data = &raw[hdr_size..];

    let node_id = i64::from_le_bytes(data[OFF_NODE_ID..OFF_NODE_ID + 8].try_into().ok()?);
    let adj_slot_idx = u16::from_le_bytes(data[OFF_ADJ_SLOT..OFF_ADJ_SLOT + 2].try_into().ok()?);
    let label_count = data[OFF_LABEL_COUNT] as usize;
    let prop_len = u16::from_le_bytes(
        data[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2].try_into().ok()?,
    ) as usize;
    let overflow_blkno = u32::from_le_bytes(
        data[OFF_PROP_OVERFLOW_PAGE..OFF_PROP_OVERFLOW_PAGE + 4].try_into().ok()?,
    );

    let labels_end = OFF_LABEL_IDS + label_count * 4;
    if data.len() < labels_end + prop_len {
        return None; // Malformed
    }
    let mut label_ids = Vec::with_capacity(label_count);
    for i in 0..label_count {
        let lo = OFF_LABEL_IDS + i * 4;
        label_ids.push(i32::from_le_bytes(data[lo..lo + 4].try_into().ok()?));
    }

    // Resolve properties: inline or from overflow block.
    let prop_bytes = if prop_len > 0 {
        data[labels_end..labels_end + prop_len].to_vec()
    } else if overflow_blkno != 0 {
        // We need to open the relation again for the overflow read. Since we
        // already hold a shared lock on the NODE page's buffer, and the
        // overflow block is a different block, this is safe.
        // SAFETY: `_buf` parameter gives us access to the relation via rel_lookup;
        // but we don't have rel here. For now, re-derive from the buffer.
        // Actually: read_node_at_offset doesn't have access to `rel`.
        // Store overflow_blkno in the returned record and let callers resolve it.
        // See NodeRecord.overflow_blkno — callers call read_overflow_block if non-zero.
        Vec::new() // placeholder; caller checks overflow_blkno
    } else {
        Vec::new()
    };

    Some(NodeRecord { node_id, adj_slot_idx, overflow_blkno, label_ids, prop_bytes })
}

/// Return the number of free bytes available for new items on `page`.
#[inline]
unsafe fn page_free_space(page: pg_sys::Page) -> usize {
    let phdr = page as *mut pg_sys::PageHeaderData;
    let upper = unsafe { (*phdr).pd_upper as usize };
    let lower = unsafe { (*phdr).pd_lower as usize };
    if upper >= lower { upper - lower } else { 0 }
}

// ---------------------------------------------------------------------------
// Public: find_node_location
// ---------------------------------------------------------------------------

/// Locate a live node by id in the node relation.
///
/// Returns `(block_number, item_offset, adj_slot_idx)` where `adj_slot_idx`
/// is the **stored** (permanent) index into the pd_special adjacency header
/// array on that page. This is the canonical location — used by edge_store
/// to read and write adjacency headers.
///
/// # Safety
/// `rel` must be a valid, open node relation.
pub unsafe fn find_node_location(
    rel: pg_sys::Relation,
    node_id: i64,
    _snapshot: pg_sys::Snapshot,
) -> Option<(pg_sys::BlockNumber, pg_sys::OffsetNumber, usize)> {
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    let nblocks =
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM);

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

        // Skip overflow blocks.
        if is_overflow_page(page) {
            pg_sys::UnlockReleaseBuffer(buf);
            continue;
        }

        let max_off = pg_sys::PageGetMaxOffsetNumber(page as *const _);

        let mut found: Option<(pg_sys::BlockNumber, pg_sys::OffsetNumber, usize)> = None;
        for off in pg_sys::FirstOffsetNumber..=max_off {
            let iid = pg_sys::PageGetItemId(page, off);
            if (*iid).lp_flags() != pg_sys::LP_NORMAL {
                continue;
            }
            let item_len = (*iid).lp_len() as usize;
            if item_len < hdr_size + NODE_FIXED_DATA_SIZE {
                continue;
            }
            let item = pg_sys::PageGetItem(page as *const _, iid) as *const u8;
            let raw = std::slice::from_raw_parts(item, item_len);
            let data = &raw[hdr_size..];
            let nid = i64::from_le_bytes(data[OFF_NODE_ID..OFF_NODE_ID + 8].try_into().unwrap());
            // Skip logically deleted nodes.
            let hdr = item as *const pg_sys::HeapTupleHeaderData;
            let xmax_invalid = ((*hdr).t_infomask & pg_sys::HEAP_XMAX_INVALID as u16) != 0;
            if nid == node_id && xmax_invalid {
                let adj_slot_idx = u16::from_le_bytes(
                    data[OFF_ADJ_SLOT..OFF_ADJ_SLOT + 2].try_into().unwrap(),
                ) as usize;
                found = Some((blkno, off, adj_slot_idx));
                break;
            }
        }
        pg_sys::UnlockReleaseBuffer(buf);
        if found.is_some() {
            return found;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Node delete (logical)
// ---------------------------------------------------------------------------

/// Logically delete a node by setting its xmax.
///
/// The node remains in place; physical reclamation happens during VACUUM.
/// Adjacency headers are NOT cleared here (VACUUM does that after all
/// incident edges are also dead-to-all).
///
/// Returns `true` if the node was found and deleted, `false` if not found.
///
/// # Safety
/// Caller must ensure `rel` is valid and open.
pub unsafe fn delete_node_by_id(rel: pg_sys::Relation, node_id: i64) -> bool {
    let snapshot = pg_sys::GetActiveSnapshot();
    let (blkno, off, _adj) = match find_node_location(rel, node_id, snapshot) {
        Some(loc) => loc,
        None => return false,
    };

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
    let iid = pg_sys::PageGetItemId(page, off);
    let hdr = pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData;
    let xmax = pg_sys::GetCurrentTransactionId();
    pg_sys::HeapTupleHeaderSetXmax(hdr, xmax);
    (*hdr).t_infomask &= !(pg_sys::HEAP_XMAX_INVALID as u16);

    let lsn = log_node_delete(buf, page, off, xmax);
    pg_sys::PageSetLSN(page, lsn);
    pg_sys::MarkBufferDirty(buf);
    pg_sys::CritSectionCount -= 1;

    pg_sys::UnlockReleaseBuffer(buf);
    true
}

// ---------------------------------------------------------------------------
// Node update (new MVCC version on same page)
// ---------------------------------------------------------------------------

/// Update a node's labels and/or properties, creating a new MVCC version.
///
/// The old record is logically deleted (xmax set) and a new record is
/// inserted on the SAME page to preserve the adj_slot_idx (which is
/// page-relative).  If the new record is too large to fit on the same page,
/// an error is raised (Phase 3 limitation; Phase 4 will add cross-page
/// update support).
///
/// Adjacency headers (Region 1) are untouched.
///
/// Returns `false` if the node was not found.
///
/// # Safety
/// Caller must ensure `rel` is valid and open.
pub unsafe fn update_node(
    rel: pg_sys::Relation,
    node_id: i64,
    new_label_ids: &[i32],
    new_prop_bytes: &[u8],
) -> bool {
    if new_label_ids.len() > MAX_LABELS_PER_NODE {
        pgrx::error!("pg_eddy PE101: node has {} labels, max is {}", new_label_ids.len(), MAX_LABELS_PER_NODE);
    }
    if new_prop_bytes.len() > PROP_INLINE_MAX {
        pgrx::error!(
            "pg_eddy PE200: property data ({} B) exceeds inline limit ({} B)",
            new_prop_bytes.len(), PROP_INLINE_MAX
        );
    }

    let snapshot = pg_sys::GetActiveSnapshot();
    let (blkno, old_off, adj_slot_idx) = match find_node_location(rel, node_id, snapshot) {
        Some(loc) => loc,
        None => return false,
    };

    // Build new item with the stored adj_slot_idx so it stays page-relative.
    let new_item = build_node_item_bytes(node_id, adj_slot_idx as u16, new_label_ids, new_prop_bytes);

    let buf = pg_sys::ReadBufferExtended(
        rel,
        pg_sys::ForkNumber::MAIN_FORKNUM,
        blkno,
        pg_sys::ReadBufferMode::RBM_NORMAL,
        std::ptr::null_mut(),
    );
    pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32);
    let page = pg_sys::BufferGetPage(buf);

    // Check that the new item fits on this page.
    let free = page_free_space(page);
    if free < new_item.len() + size_of::<pg_sys::ItemIdData>() {
        pg_sys::UnlockReleaseBuffer(buf);
        pgrx::error!(
            "pg_eddy PE201: updated node properties too large to fit on same page (need {} B free, have {}); \
             shrink properties or wait for Phase 4 cross-page update support",
            new_item.len() + size_of::<pg_sys::ItemIdData>(),
            free,
        );
    }

    pg_sys::CritSectionCount += 1;

    // 1. Logically delete the old record.
    let old_iid = pg_sys::PageGetItemId(page, old_off);
    let old_hdr = pg_sys::PageGetItem(page as *const _, old_iid) as *mut pg_sys::HeapTupleHeaderData;
    let xmax = pg_sys::GetCurrentTransactionId();
    pg_sys::HeapTupleHeaderSetXmax(old_hdr, xmax);
    (*old_hdr).t_infomask &= !(pg_sys::HEAP_XMAX_INVALID as u16);

    // 2. Insert new record on the same page.
    let new_off = pg_sys::PageAddItemExtended(
        page,
        new_item.as_ptr() as pg_sys::Item,
        new_item.len() as pg_sys::Size,
        pg_sys::InvalidOffsetNumber,
        0,
    );
    if new_off == pg_sys::InvalidOffsetNumber {
        pg_sys::CritSectionCount -= 1;
        pg_sys::UnlockReleaseBuffer(buf);
        pgrx::error!("pg_eddy: PageAddItemExtended failed for node update on block {blkno}");
    }

    // Fix t_ctid in new record.
    {
        let new_iid = pg_sys::PageGetItemId(page, new_off);
        let new_hdr = pg_sys::PageGetItem(page as *const _, new_iid) as *mut pg_sys::HeapTupleHeaderData;
        pg_sys::ItemPointerSet(&mut (*new_hdr).t_ctid, blkno, new_off);
    }

    // WAL-log: delete old, insert new.
    let lsn_del = log_node_delete(buf, page, old_off, xmax);
    let lsn_ins = log_node_insert(buf, page, new_off, &new_item, None);
    let lsn = if lsn_ins > lsn_del { lsn_ins } else { lsn_del };
    pg_sys::PageSetLSN(page, lsn);
    pg_sys::MarkBufferDirty(buf);
    pg_sys::CritSectionCount -= 1;

    pg_sys::UnlockReleaseBuffer(buf);
    true
}


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
    PD_NODE_SPECIAL_SIZE, PROP_INLINE_MAX,
};
use crate::storage::wal::log_node_insert;

// ---------------------------------------------------------------------------
// Public node record
// ---------------------------------------------------------------------------

/// A decoded node record, ready for Rust/SQL consumption.
#[derive(Debug, Clone)]
pub struct NodeRecord {
    pub node_id: i64,
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
    // Guard: property size
    if prop_bytes.len() > PROP_INLINE_MAX {
        pgrx::error!(
            "pg_eddy PE200: property data ({} B) exceeds inline limit ({} B); overflow pages not yet implemented",
            prop_bytes.len(),
            PROP_INLINE_MAX,
        );
    }

    let item_bytes = build_node_item_bytes(node_id, label_ids, prop_bytes);

    // Find a page with enough free space, or extend.
    let buf = find_or_extend_page(rel, item_bytes.len());

    let page = pg_sys::BufferGetPage(buf);
    let blkno = pg_sys::BufferGetBlockNumber(buf);

    // ----- Critical section: must not error between START and END -----
    unsafe {
        pg_sys::CritSectionCount += 1;
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
        pgrx::error!("pg_eddy: PageAddItemExtended failed on block {blkno}");
    }

    // Set the self-pointer (t_ctid) in the in-page copy of the header.
    unsafe {
        let iid = pg_sys::PageGetItemId(page, off);
        let item_in_page = pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData;
        pg_sys::ItemPointerSet(&mut (*item_in_page).t_ctid, blkno, off);
    }

    // WAL-log the insert, get the LSN.
    let lsn = unsafe { log_node_insert(buf, page, off, &item_bytes) };

    unsafe {
        pg_sys::PageSetLSN(page, lsn);
        pg_sys::MarkBufferDirty(buf);
        pg_sys::CritSectionCount -= 1;
    }
    // ----- End critical section -----

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

            if found.is_some() {
                return found;
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
        let max_off = unsafe { pg_sys::PageGetMaxOffsetNumber(page as *const _) };

        for off in pg_sys::FirstOffsetNumber..=max_off {
            if let Some(rec) = unsafe { read_node_at_offset(page, buf, snapshot, off) } {
                if rec.node_id == node_id {
                    unsafe { pg_sys::UnlockReleaseBuffer(buf) };
                    return Some(rec);
                }
            }
        }
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
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

/// Build the raw bytes for a node item.
unsafe fn build_node_item_bytes(node_id: i64, label_ids: &[i32], prop_bytes: &[u8]) -> Vec<u8> {
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
    // adj_slot_idx (2) — 0 for Phase 1 (no edges)
    data[OFF_ADJ_SLOT..OFF_ADJ_SLOT + 2].copy_from_slice(&0u16.to_le_bytes());
    // label_count (1)
    data[OFF_LABEL_COUNT] = label_ids.len() as u8;
    // prop_inline_len (2)
    let nprop = prop_bytes.len() as u16;
    data[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2].copy_from_slice(&nprop.to_le_bytes());
    // prop_overflow_page (4) — 0 = none
    data[OFF_PROP_OVERFLOW_PAGE..OFF_PROP_OVERFLOW_PAGE + 4].copy_from_slice(&0u32.to_le_bytes());
    // _pad (1) — already 0 from vec![]
    // label_ids (4 each)
    for (i, lid) in label_ids.iter().enumerate() {
        let off = OFF_LABEL_IDS + i * 4;
        data[off..off + 4].copy_from_slice(&lid.to_le_bytes());
    }
    // prop_bytes
    let prop_start = OFF_LABEL_IDS + label_ids.len() * 4;
    data[prop_start..prop_start + prop_bytes.len()].copy_from_slice(prop_bytes);

    buf
}

/// Find a buffer with enough free space, or extend the relation.
///
/// Returns an exclusively-locked buffer ready for writing.
unsafe fn find_or_extend_page(rel: pg_sys::Relation, item_size: usize) -> pg_sys::Buffer {
    let nblocks = unsafe {
        pg_sys::RelationGetNumberOfBlocksInFork(rel, pg_sys::ForkNumber::MAIN_FORKNUM)
    };

    // Try the last block first (or block 0 if only one block).
    if nblocks > 0 {
        let last = nblocks - 1;
        let buf = unsafe {
            pg_sys::ReadBufferExtended(
                rel,
                pg_sys::ForkNumber::MAIN_FORKNUM,
                last,
                pg_sys::ReadBufferMode::RBM_NORMAL,
                std::ptr::null_mut(),
            )
        };
        unsafe { pg_sys::LockBuffer(buf, pg_sys::BUFFER_LOCK_EXCLUSIVE as i32) };
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        let free = unsafe { page_free_space(page) };
        if free >= item_size + size_of::<pg_sys::ItemIdData>() {
            return buf;
        }
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
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
/// Phase 1: skips full MVCC visibility check — returns all LP_NORMAL items.
/// Full MVCC (HeapTupleSatisfiesVisibility) will be added in Phase 3.
unsafe fn read_node_at_offset(
    page: pg_sys::Page,
    _buf: pg_sys::Buffer,
    _snapshot: pg_sys::Snapshot,
    off: pg_sys::OffsetNumber,
) -> Option<NodeRecord> {
    let iid = unsafe { pg_sys::PageGetItemId(page, off) };
    // Skip empty/dead item pointers (LP_NORMAL = 1).
    let flags = unsafe { (*iid).lp_flags() };
    if flags != pg_sys::LP_NORMAL {
        return None;
    }
    let item_len = unsafe { (*iid).lp_len() } as usize;
    let item = unsafe { pg_sys::PageGetItem(page as *const _, iid) as *const u8 };

    // Decode the data portion.
    let hdr_size = size_of::<pg_sys::HeapTupleHeaderData>();
    if item_len < hdr_size + NODE_FIXED_DATA_SIZE {
        return None; // Malformed
    }
    let raw = std::slice::from_raw_parts(item, item_len);
    let data = &raw[hdr_size..];

    let node_id = i64::from_le_bytes(data[OFF_NODE_ID..OFF_NODE_ID + 8].try_into().ok()?);
    let label_count = data[OFF_LABEL_COUNT] as usize;
    let prop_len = u16::from_le_bytes(
        data[OFF_PROP_INLINE_LEN..OFF_PROP_INLINE_LEN + 2].try_into().ok()?,
    ) as usize;

    let labels_end = OFF_LABEL_IDS + label_count * 4;
    if data.len() < labels_end + prop_len {
        return None; // Malformed
    }
    let mut label_ids = Vec::with_capacity(label_count);
    for i in 0..label_count {
        let lo = OFF_LABEL_IDS + i * 4;
        label_ids.push(i32::from_le_bytes(data[lo..lo + 4].try_into().ok()?));
    }
    let prop_bytes = data[labels_end..labels_end + prop_len].to_vec();

    Some(NodeRecord { node_id, label_ids, prop_bytes })
}

/// Return the number of free bytes available for new items on `page`.
#[inline]
unsafe fn page_free_space(page: pg_sys::Page) -> usize {
    let phdr = page as *mut pg_sys::PageHeaderData;
    let upper = unsafe { (*phdr).pd_upper as usize };
    let lower = unsafe { (*phdr).pd_lower as usize };
    if upper >= lower { upper - lower } else { 0 }
}

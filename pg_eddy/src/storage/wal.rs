// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

/// Custom WAL resource manager — Phase 3.
///
/// Uses `RM_EXPERIMENTAL_ID` (128) during development.  Before publishing any
/// release that users might run in production, reserve a permanent ID on the
/// PostgreSQL Custom RMGRs wiki page.
///
/// WAL record types (opcode in HIGH nibble; bits 2-3 of low nibble are
/// forbidden by PostgreSQL's XLogInsert — see page.rs for details):
///   0x00 — XLOG_PG_EDDY_NODE_INSERT
///   0x10 — XLOG_PG_EDDY_NODE_INSERT_OVF  (node + overflow full-page image)
///   0x20 — XLOG_PG_EDDY_NODE_DELETE
///   0x30 — XLOG_PG_EDDY_NODE_COMPACT     (FPI after PageRepairFragmentation)
///   0x40 — XLOG_PG_EDDY_EDGE_INSERT
///   0x50 — XLOG_PG_EDDY_EDGE_DELETE
///   0x60 — XLOG_PG_EDDY_ADJ_UPDATE
///   0x70 — XLOG_PG_EDDY_VACUUM_PAGE
///
/// WAL record layout for NODE_DELETE:
///   Block 0:    node page (REGBUF_STANDARD)
///   Main data:  XLogNodeDelete { offset_number: u16, _pad: u16, xmax: u32 }
///
/// WAL record layout for VACUUM_PAGE:
///   Block 0:    node or edge page (REGBUF_STANDARD)
///   Main data:  XLogVacuumPage { n_dead: u16 } + n_dead * u16 offset numbers
use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::page::{
    ADJ_HEADER_BYTES, NodeAdjHeader, XLOG_PG_EDDY_ADJ_UPDATE, XLOG_PG_EDDY_EDGE_DELETE,
    XLOG_PG_EDDY_EDGE_INSERT, XLOG_PG_EDDY_NODE_COMPACT, XLOG_PG_EDDY_NODE_DELETE,
    XLOG_PG_EDDY_NODE_INSERT, XLOG_PG_EDDY_NODE_INSERT_OVF, XLOG_PG_EDDY_VACUUM_PAGE,
};

/// Development RMGR ID.  Replace with a reserved ID before production use.
const RMGR_ID: pg_sys::RmgrId = 128;

const RMGR_NAME: &std::ffi::CStr = c"pg_eddy";

/// Main data appended to XLOG_PG_EDDY_NODE_INSERT records.
#[repr(C)]
struct XLogNodeInsert {
    /// The item offset at which the node record was placed.
    offset_number: u16,
    _pad: u16,
}

/// Main data appended to XLOG_PG_EDDY_NODE_DELETE records.
#[repr(C)]
struct XLogNodeDelete {
    /// The item offset of the node being logically deleted.
    offset_number: u16,
    _pad: u16,
    /// The xmax transaction id being set.
    xmax: pg_sys::TransactionId,
}

/// Main data header for XLOG_PG_EDDY_VACUUM_PAGE records.
/// Followed immediately by `n_dead` u16 offset numbers.
#[repr(C)]
struct XLogVacuumPage {
    n_dead: u16,
}

/// Main data appended to XLOG_PG_EDDY_EDGE_INSERT records.
#[repr(C)]
struct XLogEdgeInsert {
    /// The item offset at which the edge record was placed.
    offset_number: u16,
    _pad: u16,
}

/// Main data appended to XLOG_PG_EDDY_EDGE_DELETE records.
#[repr(C)]
struct XLogEdgeDelete {
    /// The item offset of the edge being deleted.
    offset_number: u16,
    _pad: u16,
    /// The xmax transaction id being set.
    xmax: pg_sys::TransactionId,
}

// ---------------------------------------------------------------------------
// log_node_insert — called by node_store::insert_node inside the critical
// section (CritSectionCount already incremented by the caller).
// ---------------------------------------------------------------------------

/// WAL-log a node insert and return the resulting LSN.
///
/// If `overflow_buf` is `Some(buf)`, a second block (block id 1) is registered
/// for the overflow page using `REGBUF_FORCE_IMAGE`.  The overflow page image
/// is sufficient for redo; no additional main-data is needed for it.
///
/// # Safety
/// Must be called:
/// - While `buf` (and `overflow_buf` if any) are held exclusively locked.
/// - Inside a critical section (caller has incremented `CritSectionCount`).
pub unsafe fn log_node_insert(
    buf: pg_sys::Buffer,
    _page: pg_sys::Page,
    off: pg_sys::OffsetNumber,
    item_bytes: &[u8],
    overflow_buf: Option<pg_sys::Buffer>,
) -> pg_sys::XLogRecPtr {
    let xlrec = XLogNodeInsert { offset_number: off, _pad: 0 };
    let info = if overflow_buf.is_some() { XLOG_PG_EDDY_NODE_INSERT_OVF } else { XLOG_PG_EDDY_NODE_INSERT };

    unsafe {
        pg_sys::XLogBeginInsert();
        pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
        pg_sys::XLogRegisterBufData(
            0,
            item_bytes.as_ptr() as *const _,
            item_bytes.len() as u32,
        );
        pg_sys::XLogRegisterData(
            &xlrec as *const XLogNodeInsert as *const _,
            size_of::<XLogNodeInsert>() as u32,
        );
        if let Some(ovf_buf) = overflow_buf {
            pg_sys::XLogRegisterBuffer(
                1,
                ovf_buf,
                (pg_sys::REGBUF_FORCE_IMAGE | pg_sys::REGBUF_STANDARD) as u8,
            );
        }
        pg_sys::XLogInsert(RMGR_ID, info)
    }
}

// ---------------------------------------------------------------------------
// log_node_compact — called by vacuum after PageRepairFragmentation.
// Uses REGBUF_FORCE_IMAGE so the full compacted page is captured; no custom
// redo logic is needed (redo restores the image automatically).
// ---------------------------------------------------------------------------

/// WAL-log a node page compaction (full page image).
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_node_compact(buf: pg_sys::Buffer) -> pg_sys::XLogRecPtr {
    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(
        0,
        buf,
        (pg_sys::REGBUF_FORCE_IMAGE | pg_sys::REGBUF_STANDARD) as u8,
    );
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_NODE_COMPACT)
}

// ---------------------------------------------------------------------------
// log_node_delete — called by node_store::delete_node_by_id and
// node_store::update_node inside the critical section.
// ---------------------------------------------------------------------------

/// WAL-log a node logical delete (xmax set) and return the resulting LSN.
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_node_delete(
    buf: pg_sys::Buffer,
    _page: pg_sys::Page,
    off: pg_sys::OffsetNumber,
    xmax: pg_sys::TransactionId,
) -> pg_sys::XLogRecPtr {
    let xlrec = XLogNodeDelete { offset_number: off, _pad: 0, xmax };

    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
    pg_sys::XLogRegisterData(
        &xlrec as *const XLogNodeDelete as *const _,
        size_of::<XLogNodeDelete>() as u32,
    );
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_NODE_DELETE)
}

// ---------------------------------------------------------------------------
// log_vacuum_page — called by vacuum::vacuum_relation after marking dead slots.
// ---------------------------------------------------------------------------

/// WAL-log a VACUUM page operation (marking dead slots LP_DEAD).
///
/// `dead_offsets` must be non-empty.
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_vacuum_page(
    buf: pg_sys::Buffer,
    dead_offsets: &[u16],
) -> pg_sys::XLogRecPtr {
    let hdr = XLogVacuumPage { n_dead: dead_offsets.len() as u16 };

    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
    pg_sys::XLogRegisterData(
        &hdr as *const XLogVacuumPage as *const _,
        size_of::<XLogVacuumPage>() as u32,
    );
    pg_sys::XLogRegisterData(
        dead_offsets.as_ptr() as *const _,
        std::mem::size_of_val(dead_offsets) as u32,
    );
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_VACUUM_PAGE)
}

// ---------------------------------------------------------------------------
// log_edge_insert — called by edge_store::insert_edge inside the critical
// section.
// ---------------------------------------------------------------------------

/// WAL-log an edge insert and return the resulting LSN.
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_edge_insert(
    buf: pg_sys::Buffer,
    _page: pg_sys::Page,
    off: pg_sys::OffsetNumber,
    item_bytes: &[u8],
) -> pg_sys::XLogRecPtr {
    let xlrec = XLogEdgeInsert { offset_number: off, _pad: 0 };

    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
    pg_sys::XLogRegisterBufData(0, item_bytes.as_ptr() as *const _, item_bytes.len() as u32);
    pg_sys::XLogRegisterData(
        &xlrec as *const XLogEdgeInsert as *const _,
        size_of::<XLogEdgeInsert>() as u32,
    );
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_EDGE_INSERT)
}

// ---------------------------------------------------------------------------
// log_edge_delete — called by edge_store::delete_edge inside the critical
// section.
// ---------------------------------------------------------------------------

/// WAL-log an edge logical delete (xmax set) and return the resulting LSN.
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_edge_delete(
    buf: pg_sys::Buffer,
    _page: pg_sys::Page,
    off: pg_sys::OffsetNumber,
    xmax: pg_sys::TransactionId,
) -> pg_sys::XLogRecPtr {
    let xlrec = XLogEdgeDelete { offset_number: off, _pad: 0, xmax };

    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
    pg_sys::XLogRegisterData(
        &xlrec as *const XLogEdgeDelete as *const _,
        size_of::<XLogEdgeDelete>() as u32,
    );
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_EDGE_DELETE)
}

// ---------------------------------------------------------------------------
// log_adj_update — called by edge_store::insert_edge inside the critical
// section, once for source node and once for target node.
// ---------------------------------------------------------------------------

/// WAL-log an adjacency header update and return the resulting LSN.
///
/// The block 0 data encodes: [adj_slot_idx: u16][new_adj_header: 24 bytes].
///
/// # Safety
/// Must be called while `buf` is held exclusively locked and inside a critical
/// section.
pub unsafe fn log_adj_update(
    buf: pg_sys::Buffer,
    adj_slot_idx: u16,
    new_adj: &NodeAdjHeader,
) -> pg_sys::XLogRecPtr {
    // 26 bytes: 2 (adj_slot_idx) + 24 (NodeAdjHeader)
    let mut payload = [0u8; 2 + ADJ_HEADER_BYTES];
    payload[0..2].copy_from_slice(&adj_slot_idx.to_le_bytes());
    payload[2..].copy_from_slice(new_adj.as_bytes());

    pg_sys::XLogBeginInsert();
    pg_sys::XLogRegisterBuffer(0, buf, pg_sys::REGBUF_STANDARD as u8);
    pg_sys::XLogRegisterBufData(0, payload.as_ptr() as *const _, payload.len() as u32);
    pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_ADJ_UPDATE)
}

// ---------------------------------------------------------------------------
// Redo
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Helper macros implemented as inline fns (XLogRecGet* in C are macros).
// ---------------------------------------------------------------------------

/// Returns the `xl_info` byte from the WAL record (= the XLOG_PG_EDDY_* constant
/// combined with high bits like XLR_INFO_MASK).
#[inline]
unsafe fn xlog_rec_get_info(record: *mut pg_sys::XLogReaderState) -> u8 {
    unsafe { (*(*record).record).header.xl_info }
}

/// Returns a pointer to the main data appended via `XLogRegisterData`.
#[inline]
unsafe fn xlog_rec_get_data(record: *mut pg_sys::XLogReaderState) -> *mut std::ffi::c_char {
    unsafe { (*(*record).record).main_data }
}

/// Returns the length of the main data.
#[inline]
unsafe fn xlog_rec_get_data_len(record: *mut pg_sys::XLogReaderState) -> u32 {
    unsafe { (*(*record).record).main_data_len }
}

/// Returns the LSN of the record.
#[inline]
unsafe fn xlog_rec_get_lsn(record: *mut pg_sys::XLogReaderState) -> pg_sys::XLogRecPtr {
    unsafe { (*(*record).record).lsn }
}

// ---------------------------------------------------------------------------
// Redo dispatch
// ---------------------------------------------------------------------------

/// Redo a single WAL record.
///
/// # Safety
/// Called by PostgreSQL's recovery machinery with a valid record pointer.
unsafe extern "C-unwind" fn rmgr_redo(record: *mut pg_sys::XLogReaderState) {
    let info = unsafe { xlog_rec_get_info(record) } & !(pg_sys::XLR_INFO_MASK as u8);

    match info {
        XLOG_PG_EDDY_NODE_INSERT | XLOG_PG_EDDY_NODE_INSERT_OVF => unsafe { redo_node_insert(record) },
        XLOG_PG_EDDY_NODE_COMPACT => unsafe { redo_node_compact(record) },
        XLOG_PG_EDDY_NODE_DELETE => unsafe { redo_node_delete(record) },
        XLOG_PG_EDDY_EDGE_INSERT => unsafe { redo_edge_insert(record) },
        XLOG_PG_EDDY_EDGE_DELETE => unsafe { redo_edge_delete(record) },
        XLOG_PG_EDDY_ADJ_UPDATE  => unsafe { redo_adj_update(record) },
        XLOG_PG_EDDY_VACUUM_PAGE => unsafe { redo_vacuum_page(record) },
        _ => {
            pgrx::error!("pg_eddy: unknown WAL record type 0x{:02x}", info);
        }
    }
}

unsafe fn redo_node_insert(record: *mut pg_sys::XLogReaderState) {
    use crate::storage::node_store::init_node_page;

    // Reconstruct the main data (XLogNodeInsert).
    let main_data_len = unsafe { xlog_rec_get_data_len(record) } as usize;
    if main_data_len < size_of::<XLogNodeInsert>() {
        pgrx::error!("pg_eddy redo: NODE_INSERT main data too short ({} bytes)", main_data_len);
    }
    let xlrec = unsafe {
        let ptr = xlog_rec_get_data(record) as *const XLogNodeInsert;
        ptr.read_unaligned()
    };
    let off = xlrec.offset_number;

    // Get block 0 data (the item bytes).
    let mut item_len: pg_sys::Size = 0;
    let item_data = unsafe { pg_sys::XLogRecGetBlockData(record, 0, &mut item_len) };

    // Acquire the buffer for redo.
    // XLogReadBufferForRedo(record, block_id, *mut Buffer) -> XLogRedoAction::Type
    let mut buf: pg_sys::Buffer = 0; // 0 = InvalidBuffer (as i32)
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };

        // If the page looks un-initialised (e.g. after a failed partial write)
        // re-initialise it before replaying the insert.
        let phdr = page as *mut pg_sys::PageHeaderData;
        let pd_upper = unsafe { (*phdr).pd_upper };
        if pd_upper == 0 {
            unsafe { init_node_page(page) };
        }

        let result = unsafe {
            pg_sys::PageAddItemExtended(
                page,
                item_data as pg_sys::Item,
                item_len,
                off,
                pg_sys::PAI_OVERWRITE as i32,
            )
        };
        if result == pg_sys::InvalidOffsetNumber {
            pgrx::error!("pg_eddy redo: PageAddItemExtended failed at offset {}", off);
        }

        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    } else if action == pg_sys::XLogRedoAction::BLK_RESTORED {
        // Full page image was restored; just mark dirty.
        unsafe { pg_sys::MarkBufferDirty(buf) };
    }

    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }

    // If this was a NODE_INSERT_OVF record, handle the overflow block (block 1).
    // With REGBUF_FORCE_IMAGE the full overflow page image was captured;
    // XLogReadBufferForRedo restores it automatically.
    // IMPORTANT: only do this for OVF records; regular NODE_INSERT records have
    // no block 1 and calling XLogReadBufferForRedo for it would PANIC.
    let is_ovf = unsafe { xlog_rec_get_info(record) } & !(pg_sys::XLR_INFO_MASK as u8)
        == XLOG_PG_EDDY_NODE_INSERT_OVF;
    if is_ovf {
        let mut ovf_buf: pg_sys::Buffer = 0;
        let ovf_action = unsafe { pg_sys::XLogReadBufferForRedo(record, 1, &mut ovf_buf) };
        if ovf_action == pg_sys::XLogRedoAction::BLK_RESTORED {
            // Overflow page restored from full page image; mark dirty.
            let lsn = unsafe { xlog_rec_get_lsn(record) };
            let ovf_page = unsafe { pg_sys::BufferGetPage(ovf_buf) };
            unsafe {
                pg_sys::PageSetLSN(ovf_page, lsn);
                pg_sys::MarkBufferDirty(ovf_buf);
            }
        }
        if ovf_buf != 0 {
            unsafe { pg_sys::UnlockReleaseBuffer(ovf_buf) };
        }
    }
}

/// Redo a node page compaction (full page image — image restores automatically).
unsafe fn redo_node_compact(record: *mut pg_sys::XLogReaderState) {
    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };
    if action == pg_sys::XLogRedoAction::BLK_RESTORED {
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Redo a node logical delete (xmax set).
unsafe fn redo_node_delete(record: *mut pg_sys::XLogReaderState) {
    let main_data_len = unsafe { xlog_rec_get_data_len(record) } as usize;
    if main_data_len < size_of::<XLogNodeDelete>() {
        pgrx::error!("pg_eddy redo: NODE_DELETE main data too short ({} bytes)", main_data_len);
    }
    let xlrec = unsafe {
        let ptr = xlog_rec_get_data(record) as *const XLogNodeDelete;
        ptr.read_unaligned()
    };
    let off = xlrec.offset_number;
    let xmax = xlrec.xmax;

    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        let iid = unsafe { pg_sys::PageGetItemId(page, off) };
        if unsafe { (*iid).lp_flags() } == pg_sys::LP_NORMAL {
            let hdr = unsafe {
                pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData
            };
            unsafe {
                pg_sys::HeapTupleHeaderSetXmax(hdr, xmax);
                (*hdr).t_infomask &= !(pg_sys::HEAP_XMAX_INVALID as u16);
            }
        }
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Redo a vacuum page (mark dead slots LP_DEAD).
unsafe fn redo_vacuum_page(record: *mut pg_sys::XLogReaderState) {
    let main_data_len = unsafe { xlog_rec_get_data_len(record) } as usize;
    if main_data_len < size_of::<XLogVacuumPage>() {
        pgrx::error!("pg_eddy redo: VACUUM_PAGE main data too short ({} bytes)", main_data_len);
    }
    let data_ptr = unsafe { xlog_rec_get_data(record) as *const u8 };
    let hdr = unsafe { (data_ptr as *const XLogVacuumPage).read_unaligned() };
    let n_dead = hdr.n_dead as usize;

    let offsets_ptr = unsafe { data_ptr.add(size_of::<XLogVacuumPage>()) as *const u16 };
    let expected_len = size_of::<XLogVacuumPage>() + n_dead * size_of::<u16>();
    if main_data_len < expected_len {
        pgrx::error!("pg_eddy redo: VACUUM_PAGE main data too short for {} offsets", n_dead);
    }

    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        for i in 0..n_dead {
            let off = unsafe { offsets_ptr.add(i).read_unaligned() } as pg_sys::OffsetNumber;
            let iid = unsafe { pg_sys::PageGetItemId(page, off) };
            if unsafe { (*iid).lp_flags() } == pg_sys::LP_NORMAL {
                unsafe { (*iid).set_lp_flags(pg_sys::LP_DEAD) };
            }
        }
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Redo an edge insert.
unsafe fn redo_edge_insert(record: *mut pg_sys::XLogReaderState) {
    use crate::storage::edge_store::init_edge_page;

    let main_data_len = unsafe { xlog_rec_get_data_len(record) } as usize;
    if main_data_len < size_of::<XLogEdgeInsert>() {
        pgrx::error!("pg_eddy redo: EDGE_INSERT main data too short ({} bytes)", main_data_len);
    }
    let xlrec = unsafe {
        let ptr = xlog_rec_get_data(record) as *const XLogEdgeInsert;
        ptr.read_unaligned()
    };
    let off = xlrec.offset_number;

    let mut item_len: pg_sys::Size = 0;
    let item_data = unsafe { pg_sys::XLogRecGetBlockData(record, 0, &mut item_len) };

    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        let phdr = page as *mut pg_sys::PageHeaderData;
        if unsafe { (*phdr).pd_upper } == 0 {
            unsafe { init_edge_page(page) };
        }
        let result = unsafe {
            pg_sys::PageAddItemExtended(
                page,
                item_data as pg_sys::Item,
                item_len,
                off,
                pg_sys::PAI_OVERWRITE as i32,
            )
        };
        if result == pg_sys::InvalidOffsetNumber {
            pgrx::error!("pg_eddy redo: EDGE_INSERT PageAddItemExtended failed at offset {}", off);
        }
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Redo an edge logical delete (xmax set).
unsafe fn redo_edge_delete(record: *mut pg_sys::XLogReaderState) {
    let main_data_len = unsafe { xlog_rec_get_data_len(record) } as usize;
    if main_data_len < size_of::<XLogEdgeDelete>() {
        pgrx::error!("pg_eddy redo: EDGE_DELETE main data too short ({} bytes)", main_data_len);
    }
    let xlrec = unsafe {
        let ptr = xlog_rec_get_data(record) as *const XLogEdgeDelete;
        ptr.read_unaligned()
    };
    let off = xlrec.offset_number;
    let xmax = xlrec.xmax;

    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };
        let iid = unsafe { pg_sys::PageGetItemId(page, off) };
        if unsafe { (*iid).lp_flags() } == pg_sys::LP_NORMAL {
            let hdr = unsafe {
                pg_sys::PageGetItem(page as *const _, iid) as *mut pg_sys::HeapTupleHeaderData
            };
            unsafe {
                pg_sys::HeapTupleHeaderSetXmax(hdr, xmax);
                (*hdr).t_infomask &= !(pg_sys::HEAP_XMAX_INVALID as u16);
            }
        }
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Redo an adjacency header update.
unsafe fn redo_adj_update(record: *mut pg_sys::XLogReaderState) {
    let mut buf_data_len: pg_sys::Size = 0;
    let buf_data = unsafe { pg_sys::XLogRecGetBlockData(record, 0, &mut buf_data_len) };
    if (buf_data_len as usize) < 2 + ADJ_HEADER_BYTES {
        pgrx::error!(
            "pg_eddy redo: ADJ_UPDATE block data too short ({} bytes)",
            buf_data_len
        );
    }

    let payload = unsafe { std::slice::from_raw_parts(buf_data as *const u8, buf_data_len as usize) };
    let adj_slot_idx = u16::from_le_bytes(payload[0..2].try_into().unwrap()) as usize;
    let new_adj_bytes: &[u8; ADJ_HEADER_BYTES] = payload[2..2 + ADJ_HEADER_BYTES].try_into().unwrap();

    let mut buf: pg_sys::Buffer = 0;
    let action = unsafe { pg_sys::XLogReadBufferForRedo(record, 0, &mut buf) };

    if action == pg_sys::XLogRedoAction::BLK_NEEDS_REDO {
        let page = unsafe { pg_sys::BufferGetPage(buf) };
                let special = unsafe { pg_sys::PageGetSpecialPointer(page) };
        let offset = adj_slot_idx * ADJ_HEADER_BYTES;
        unsafe {
            std::ptr::copy_nonoverlapping(new_adj_bytes.as_ptr(), special.add(offset), ADJ_HEADER_BYTES);
        }
        let lsn = unsafe { xlog_rec_get_lsn(record) };
        unsafe {
            pg_sys::PageSetLSN(page, lsn);
            pg_sys::MarkBufferDirty(buf);
        }
    }
    if buf != 0 {
        unsafe { pg_sys::UnlockReleaseBuffer(buf) };
    }
}

/// Describe callback (for pg_waldump).
///
/// # Safety
/// Called by PostgreSQL; buf is a valid StringInfo allocated by PG.
unsafe extern "C-unwind" fn rmgr_desc(
    buf: pg_sys::StringInfo,
    record: *mut pg_sys::XLogReaderState,
) {
    if buf.is_null() {
        return;
    }
    let info = unsafe { xlog_rec_get_info(record) } & !(pg_sys::XLR_INFO_MASK as u8);
    let msg = match info {
        XLOG_PG_EDDY_NODE_INSERT    => c"node_insert",
        XLOG_PG_EDDY_NODE_INSERT_OVF => c"node_insert_ovf",
        XLOG_PG_EDDY_NODE_COMPACT   => c"node_compact",
        XLOG_PG_EDDY_NODE_DELETE    => c"node_delete",
        XLOG_PG_EDDY_EDGE_INSERT    => c"edge_insert",
        XLOG_PG_EDDY_EDGE_DELETE    => c"edge_delete",
        XLOG_PG_EDDY_ADJ_UPDATE     => c"adj_update",
        XLOG_PG_EDDY_VACUUM_PAGE    => c"vacuum_page",
        _ => c"unknown",
    };
    unsafe { pg_sys::appendStringInfoString(buf, msg.as_ptr()) };
}

/// Identify callback — returns the record type name for pg_waldump.
///
/// # Safety
/// Called by PostgreSQL.
unsafe extern "C-unwind" fn rmgr_identify(info: u8) -> *const std::ffi::c_char {
    let info = info & !(pg_sys::XLR_INFO_MASK as u8);
    match info {
        XLOG_PG_EDDY_NODE_INSERT    => c"NODE_INSERT".as_ptr(),
        XLOG_PG_EDDY_NODE_INSERT_OVF => c"NODE_INSERT_OVF".as_ptr(),
        XLOG_PG_EDDY_NODE_COMPACT   => c"NODE_COMPACT".as_ptr(),
        XLOG_PG_EDDY_NODE_DELETE    => c"NODE_DELETE".as_ptr(),
        XLOG_PG_EDDY_EDGE_INSERT    => c"EDGE_INSERT".as_ptr(),
        XLOG_PG_EDDY_EDGE_DELETE    => c"EDGE_DELETE".as_ptr(),
        XLOG_PG_EDDY_ADJ_UPDATE     => c"ADJ_UPDATE".as_ptr(),
        XLOG_PG_EDDY_VACUUM_PAGE    => c"VACUUM_PAGE".as_ptr(),
        _ => c"UNKNOWN".as_ptr(),
    }
}

/// Register the pg_eddy custom WAL resource manager with PostgreSQL.
///
/// Must be called from `_PG_init` (which runs inside `shared_preload_libraries`
/// processing at postmaster start).
pub fn register_rmgr() {
    let rmgr = pg_sys::RmgrData {
        rm_name: RMGR_NAME.as_ptr(),
        rm_redo: Some(rmgr_redo),
        rm_desc: Some(rmgr_desc),
        rm_identify: Some(rmgr_identify),
        rm_startup: None,
        rm_cleanup: None,
        rm_mask: None,
        rm_decode: None,
    };

    // Safety: RegisterCustomRmgr is safe to call from _PG_init.
    // PostgreSQL copies the RmgrData struct internally.
    unsafe {
        pg_sys::RegisterCustomRmgr(RMGR_ID, &rmgr as *const _ as *mut _);
    }
}


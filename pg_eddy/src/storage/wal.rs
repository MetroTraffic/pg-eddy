// In Rust 2024, unsafe ops inside unsafe fns still need explicit unsafe {}.
// We allow the old implicit-unsafe behavior here to keep the code readable.
#![allow(unsafe_op_in_unsafe_fn)]

/// Custom WAL resource manager — Phase 2.
///
/// Uses `RM_EXPERIMENTAL_ID` (128) during development.  Before publishing any
/// release that users might run in production, reserve a permanent ID on the
/// PostgreSQL Custom RMGRs wiki page.
///
/// WAL record types:
///   0x00 — XLOG_PG_EDDY_NODE_INSERT
///   0x10 — XLOG_PG_EDDY_EDGE_INSERT
///   0x11 — XLOG_PG_EDDY_EDGE_DELETE
///   0x20 — XLOG_PG_EDDY_ADJ_UPDATE
///
/// WAL record layout for NODE_INSERT:
///   Block 0:    target page (registered with REGBUF_STANDARD for full-page-write support)
///   Block 0 data: the complete node item bytes
///   Main data:  XLogNodeInsert { offset_number: u16, _pad: u16 }
///
/// WAL record layout for EDGE_INSERT:
///   Block 0:    edge page (REGBUF_STANDARD)
///   Block 0 data: the complete edge item bytes
///   Main data:  XLogEdgeInsert { offset_number: u16, _pad: u16 }
///
/// WAL record layout for EDGE_DELETE:
///   Block 0:    edge page (REGBUF_STANDARD)
///   Main data:  XLogEdgeDelete { offset_number: u16, _pad: u16, xmax: u32 }
///
/// WAL record layout for ADJ_UPDATE:
///   Block 0:    node page (REGBUF_STANDARD)
///   Block 0 data: u16 adj_slot_idx + 24 bytes new NodeAdjHeader (26 bytes total)
///   Main data:  (none)
use pgrx::pg_sys;
use std::mem::size_of;

use crate::storage::page::{
    ADJ_HEADER_BYTES, NodeAdjHeader, XLOG_PG_EDDY_ADJ_UPDATE, XLOG_PG_EDDY_EDGE_DELETE,
    XLOG_PG_EDDY_EDGE_INSERT, XLOG_PG_EDDY_NODE_INSERT,
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
/// # Safety
/// Must be called:
/// - While `buf` is held exclusively locked.
/// - Inside a critical section (caller has incremented `CritSectionCount`).
pub unsafe fn log_node_insert(
    buf: pg_sys::Buffer,
    _page: pg_sys::Page,
    off: pg_sys::OffsetNumber,
    item_bytes: &[u8],
) -> pg_sys::XLogRecPtr {
    let xlrec = XLogNodeInsert { offset_number: off, _pad: 0 };

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
        pg_sys::XLogInsert(RMGR_ID, XLOG_PG_EDDY_NODE_INSERT)
    }
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
        XLOG_PG_EDDY_NODE_INSERT => unsafe { redo_node_insert(record) },
        XLOG_PG_EDDY_EDGE_INSERT => unsafe { redo_edge_insert(record) },
        XLOG_PG_EDDY_EDGE_DELETE => unsafe { redo_edge_delete(record) },
        XLOG_PG_EDDY_ADJ_UPDATE => unsafe { redo_adj_update(record) },
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
        let special = unsafe { pg_sys::PageGetSpecialPointer(page) as *mut u8 };
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
        XLOG_PG_EDDY_NODE_INSERT => c"node_insert",
        XLOG_PG_EDDY_EDGE_INSERT => c"edge_insert",
        XLOG_PG_EDDY_EDGE_DELETE => c"edge_delete",
        XLOG_PG_EDDY_ADJ_UPDATE  => c"adj_update",
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
        XLOG_PG_EDDY_NODE_INSERT => c"NODE_INSERT".as_ptr(),
        XLOG_PG_EDDY_EDGE_INSERT => c"EDGE_INSERT".as_ptr(),
        XLOG_PG_EDDY_EDGE_DELETE => c"EDGE_DELETE".as_ptr(),
        XLOG_PG_EDDY_ADJ_UPDATE  => c"ADJ_UPDATE".as_ptr(),
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


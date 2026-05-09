/// Custom WAL resource manager skeleton — Phase 0.
///
/// Uses `RM_EXPERIMENTAL_ID` (128) during development.  Before publishing any
/// release that users might run in production, reserve a permanent ID on the
/// PostgreSQL Custom RMGRs wiki page.
///
/// The redo callback is a no-op for Phase 0.  Correct redo functions are added
/// in Phase 1 (NODE_INSERT) and Phase 2 (EDGE_INSERT, ADJ_UPDATE, …).
use pgrx::pg_sys;

/// Development RMGR ID.  Replace with a reserved ID before production use.
const RMGR_ID: pg_sys::RmgrId = 128;

const RMGR_NAME: &std::ffi::CStr = c"pg_eddy";

/// No-op redo: Phase 0 has no real WAL records.
///
/// # Safety
/// Called by PostgreSQL's recovery machinery.
unsafe extern "C-unwind" fn rmgr_redo(_record: *mut pg_sys::XLogReaderState) {
    // Phase 0: no records to redo.
}

/// Describe callback (for pg_waldump).
/// `buf` is a `StringInfo` (i.e. `*mut StringInfoData`).
///
/// # Safety
/// Called by PostgreSQL; buf is a valid StringInfo allocated by PG.
unsafe extern "C-unwind" fn rmgr_desc(
    buf: pg_sys::StringInfo,
    _record: *mut pg_sys::XLogReaderState,
) {
    if buf.is_null() {
        return;
    }
    let msg = c"pg_eddy: unknown record";
    // Safety: appendStringInfoString appends to a valid StringInfo.
    unsafe { pg_sys::appendStringInfoString(buf, msg.as_ptr()) };
}

/// Identify callback — returns the record type name for pg_waldump.
///
/// # Safety
/// Called by PostgreSQL.
unsafe extern "C-unwind" fn rmgr_identify(_info: u8) -> *const std::ffi::c_char {
    c"UNKNOWN".as_ptr()
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


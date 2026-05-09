/// Table Access Method handler — Phase 1.
///
/// Two AM objects are registered in the extension SQL:
///   CREATE ACCESS METHOD pg_eddy_node TYPE TABLE HANDLER pg_eddy_node_handler;
///   CREATE ACCESS METHOD pg_eddy_edge TYPE TABLE HANDLER pg_eddy_edge_handler;
///
/// Phase 1: node scan and filelocator-init are wired to real storage.
/// All edge callbacks remain "not implemented" until Phase 2.
use pgrx::prelude::*;
use pgrx::pg_sys;

use crate::storage::node_store::NodeScanState;

// ---------------------------------------------------------------------------
// Scan state — embeds NodeScanState after TableScanDescData header.
// We use a separate Box to avoid alignment issues.
// ---------------------------------------------------------------------------

/// Opaque scan state allocated by `scan_begin`.
struct PgEddyScanDesc {
    base: pg_sys::TableScanDescData,
    /// The actual node scan iterator; `None` for edge tables (Phase 2).
    state: Option<NodeScanState>,
}

/// scan_begin — allocates a PgEddyScanDesc and starts node scan.
///
/// # Safety
/// Called by PostgreSQL's executor with valid, non-null pointers.
unsafe extern "C-unwind" fn stub_scan_begin(
    rel: pg_sys::Relation,
    snapshot: pg_sys::Snapshot,
    nkeys: std::ffi::c_int,
    key: *mut pg_sys::ScanKeyData,
    pscan: pg_sys::ParallelTableScanDesc,
    flags: u32,
) -> pg_sys::TableScanDesc {
    let state = unsafe { NodeScanState::begin(rel, snapshot) };
    let desc = Box::new(PgEddyScanDesc {
        base: pg_sys::TableScanDescData {
            rs_rd: rel,
            rs_snapshot: snapshot,
            rs_nkeys: nkeys,
            rs_key: key,
            rs_flags: flags,
            rs_parallel: pscan,
            ..unsafe { std::mem::zeroed() }
        },
        state: Some(state),
    });
    // Safety: PG will pass this back to us; we own the memory via Box::into_raw.
    Box::into_raw(desc) as pg_sys::TableScanDesc
}

/// scan_end — drops the PgEddyScanDesc.
///
/// # Safety
/// Called by PostgreSQL's executor; scan must be a valid PgEddyScanDesc.
unsafe extern "C-unwind" fn stub_scan_end(scan: pg_sys::TableScanDesc) {
    // Reclaim the Box we allocated in scan_begin.
    let _ = unsafe { Box::from_raw(scan as *mut PgEddyScanDesc) };
}

/// scan_rescan — restart iteration from block 0.
///
/// # Safety
/// Called by PostgreSQL's executor.
unsafe extern "C-unwind" fn stub_scan_rescan(
    scan: pg_sys::TableScanDesc,
    _key: *mut pg_sys::ScanKeyData,
    _set_params: bool,
    _allow_strat: bool,
    _allow_sync: bool,
    _allow_pagemode: bool,
) {
    let desc = unsafe { &mut *(scan as *mut PgEddyScanDesc) };
    let rel = desc.base.rs_rd;
    let snapshot = desc.base.rs_snapshot;
    desc.state = Some(unsafe { NodeScanState::begin(rel, snapshot) });
}

/// scan_getnextslot — fetch next visible node into `slot`.
///
/// Returns `true` if a node was returned, `false` when exhausted.
///
/// # Safety
/// Called by PostgreSQL's executor.
unsafe extern "C-unwind" fn stub_scan_getnextslot(
    scan: pg_sys::TableScanDesc,
    _direction: pg_sys::ScanDirection::Type,
    slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    // `pg_eddy.nodes` has 0 columns, so we just return an empty virtual slot
    // to signal "there is a row" — the user sees no columns anyway.
    let desc = unsafe { &mut *(scan as *mut PgEddyScanDesc) };
    let state = match &mut desc.state {
        Some(s) => s,
        None => return false,
    };
    let maybe = unsafe { state.next() };
    if maybe.is_none() {
        return false;
    }
    // Materialise an empty virtual tuple (no columns to fill).
    unsafe {
        pg_sys::ExecClearTuple(slot);
        (*slot).tts_nvalid = 0;
        pg_sys::ExecStoreVirtualTuple(slot);
    }
    true
}

// ---------------------------------------------------------------------------
// "Not implemented" stubs for every other required callback.
// ---------------------------------------------------------------------------

macro_rules! unimplemented_callback {
    ($name:ident ( $($arg:ident : $ty:ty),* ) $(-> $ret:ty)?) => {
        unsafe extern "C-unwind" fn $name( $($arg : $ty),* ) $(-> $ret)? {
            error!(
                "pg_eddy: {} is not yet implemented (Phase 0 stub)",
                stringify!($name)
            );
        }
    };
}

unimplemented_callback!(stub_parallelscan_estimate(_rel: pg_sys::Relation) -> pg_sys::Size);
unimplemented_callback!(stub_parallelscan_initialize(
    _rel: pg_sys::Relation,
    _pscan: pg_sys::ParallelTableScanDesc
) -> pg_sys::Size);
unimplemented_callback!(stub_parallelscan_reinitialize(
    _rel: pg_sys::Relation,
    _pscan: pg_sys::ParallelTableScanDesc
));
unimplemented_callback!(stub_index_fetch_begin(_rel: pg_sys::Relation) -> *mut pg_sys::IndexFetchTableData);
unimplemented_callback!(stub_index_fetch_reset(_data: *mut pg_sys::IndexFetchTableData));
unimplemented_callback!(stub_index_fetch_end(_data: *mut pg_sys::IndexFetchTableData));
unimplemented_callback!(stub_index_fetch_tuple(
    _scan: *mut pg_sys::IndexFetchTableData,
    _tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    _slot: *mut pg_sys::TupleTableSlot,
    _call_again: *mut bool,
    _all_dead: *mut bool
) -> bool);
unimplemented_callback!(stub_tuple_insert(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _options: std::ffi::c_int,
    _bistate: *mut pg_sys::BulkInsertStateData
));
unimplemented_callback!(stub_tuple_insert_speculative(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _options: std::ffi::c_int,
    _bistate: *mut pg_sys::BulkInsertStateData,
    _spec_token: pg_sys::uint32
));
unimplemented_callback!(stub_tuple_complete_speculative(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _spec_token: pg_sys::uint32,
    _succeeded: bool
));
unimplemented_callback!(stub_multi_insert(
    _rel: pg_sys::Relation,
    _slots: *mut *mut pg_sys::TupleTableSlot,
    _nslots: std::ffi::c_int,
    _cid: pg_sys::CommandId,
    _options: std::ffi::c_int,
    _bistate: *mut pg_sys::BulkInsertStateData
));
unimplemented_callback!(stub_tuple_delete(
    _rel: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    _cid: pg_sys::CommandId,
    _snapshot: pg_sys::Snapshot,
    _crosscheck: pg_sys::Snapshot,
    _wait: bool,
    _tmfd: *mut pg_sys::TM_FailureData,
    _changing_part: bool
) -> pg_sys::TM_Result::Type);
unimplemented_callback!(stub_tuple_update(
    _rel: pg_sys::Relation,
    _otid: pg_sys::ItemPointer,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _snapshot: pg_sys::Snapshot,
    _crosscheck: pg_sys::Snapshot,
    _wait: bool,
    _tmfd: *mut pg_sys::TM_FailureData,
    _lockmode: *mut pg_sys::LockTupleMode::Type,
    _update_indexes: *mut pg_sys::TU_UpdateIndexes::Type
) -> pg_sys::TM_Result::Type);
unimplemented_callback!(stub_tuple_lock(
    _rel: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    _slot: *mut pg_sys::TupleTableSlot,
    _cid: pg_sys::CommandId,
    _mode: pg_sys::LockTupleMode::Type,
    _wait_policy: pg_sys::LockWaitPolicy::Type,
    _flags: pg_sys::uint8,
    _tmfd: *mut pg_sys::TM_FailureData
) -> pg_sys::TM_Result::Type);
unimplemented_callback!(stub_finish_bulk_insert(
    _rel: pg_sys::Relation,
    _options: std::ffi::c_int
));
/// relation_set_new_filelocator — create the physical storage file.
///
/// PostgreSQL calls this when a new relation is created (e.g. `CREATE TABLE
/// ... USING pg_eddy_node`).  We create the underlying storage file and
/// set the freeze/minmulti out-params.  Pages are lazily allocated by
/// `node_store::find_or_extend_page` when the first insert arrives.
unsafe extern "C-unwind" fn stub_relation_set_new_filelocator(
    _rel: pg_sys::Relation,
    newrlocator: *const pg_sys::RelFileLocator,
    persistence: std::ffi::c_char,
    freeze_xid: *mut pg_sys::TransactionId,
    minmulti: *mut pg_sys::MultiXactId,
) {
    unsafe {
        // Set out-params (PG18 asserts TransactionIdIsNormal for permanent rels).
        if !freeze_xid.is_null() {
            *freeze_xid = pg_sys::GetCurrentTransactionId();
        }
        if !minmulti.is_null() {
            *minmulti = pg_sys::GetOldestMultiXactId();
        }
        // Create the underlying storage file (0 blocks initially).
        pg_sys::RelationCreateStorage(*newrlocator, persistence, true);
    }
}

/// relation_nontransactional_truncate — no-op for logical AM.
unsafe extern "C-unwind" fn stub_relation_nontransactional_truncate(
    _rel: pg_sys::Relation,
) {
    // Phase 0: no storage to truncate.
}
unimplemented_callback!(stub_relation_copy_data(
    _rel: pg_sys::Relation,
    _newrlocator: *const pg_sys::RelFileLocator
));
unimplemented_callback!(stub_relation_copy_for_cluster(
    _old_table: pg_sys::Relation,
    _new_table: pg_sys::Relation,
    _old_index: pg_sys::Relation,
    _use_sort: bool,
    _oldest_xmin: pg_sys::TransactionId,
    _xid_cutoff: *mut pg_sys::TransactionId,
    _multi_cutoff: *mut pg_sys::MultiXactId,
    _num_tuples: *mut f64,
    _tups_vacuumed: *mut f64,
    _tups_recently_dead: *mut f64
));
unimplemented_callback!(stub_relation_vacuum(
    _rel: pg_sys::Relation,
    _params: *mut pg_sys::VacuumParams,
    _bstrategy: pg_sys::BufferAccessStrategy
));
unimplemented_callback!(stub_scan_analyze_next_block(
    _scan: pg_sys::TableScanDesc,
    _stream: *mut pg_sys::ReadStream
) -> bool);
unimplemented_callback!(stub_scan_analyze_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _oldest_xmin: pg_sys::TransactionId,
    _liverows: *mut f64,
    _deadrows: *mut f64,
    _slot: *mut pg_sys::TupleTableSlot
) -> bool);
unimplemented_callback!(stub_index_build_range_scan(
    _table_rel: pg_sys::Relation,
    _index_rel: pg_sys::Relation,
    _index_info: *mut pg_sys::IndexInfo,
    _allow_sync: bool,
    _anyvisible: bool,
    _progress: bool,
    _start_blockno: pg_sys::BlockNumber,
    _numblocks: pg_sys::BlockNumber,
    _callback: pg_sys::IndexBuildCallback,
    _callback_state: *mut std::ffi::c_void,
    _scan: pg_sys::TableScanDesc
) -> f64);
unimplemented_callback!(stub_index_validate_scan(
    _table_rel: pg_sys::Relation,
    _index_rel: pg_sys::Relation,
    _index_info: *mut pg_sys::IndexInfo,
    _snapshot: pg_sys::Snapshot,
    _state: *mut pg_sys::ValidateIndexState
));
/// relation_size — returns the physical size of the main fork in bytes.
///
/// IMPORTANT: must NOT call `RelationGetNumberOfBlocksInFork` here — that
/// function calls `table_relation_size` which calls us back, causing infinite
/// recursion and a stack-overflow SIGSEGV.  Use `smgrnblocks` directly.
unsafe extern "C-unwind" fn stub_relation_size(
    rel: pg_sys::Relation,
    fork_number: pg_sys::ForkNumber::Type,
) -> pg_sys::uint64 {
    let smgr = unsafe { pg_sys::RelationGetSmgr(rel) };
    let nblocks = unsafe { pg_sys::smgrnblocks(smgr, fork_number) };
    nblocks as pg_sys::uint64 * pg_sys::BLCKSZ as pg_sys::uint64
}

/// relation_needs_toast_table — logical AM, no TOAST needed.
unsafe extern "C-unwind" fn stub_relation_needs_toast_table(
    _rel: pg_sys::Relation,
) -> bool {
    false
}

/// relation_estimate_size — planner hint; report 0 pages / 0 tuples.
unsafe extern "C-unwind" fn stub_relation_estimate_size(
    _rel: pg_sys::Relation,
    _attr_widths: *mut pg_sys::int32,
    pages: *mut pg_sys::BlockNumber,
    tuples: *mut f64,
    allvisfrac: *mut f64,
) {
    unsafe {
        if !pages.is_null()      { *pages = 0; }
        if !tuples.is_null()     { *tuples = 0.0; }
        if !allvisfrac.is_null() { *allvisfrac = 0.0; }
    }
}
// PG18: scan_bitmap_next_block removed; scan_bitmap_next_tuple has new signature
unimplemented_callback!(stub_scan_bitmap_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _slot: *mut pg_sys::TupleTableSlot,
    _recheck: *mut bool,
    _lossy_pages: *mut pg_sys::uint64,
    _exact_pages: *mut pg_sys::uint64
) -> bool);
unimplemented_callback!(stub_scan_sample_next_block(
    _scan: pg_sys::TableScanDesc,
    _scanstate: *mut pg_sys::SampleScanState
) -> bool);
unimplemented_callback!(stub_scan_sample_next_tuple(
    _scan: pg_sys::TableScanDesc,
    _scanstate: *mut pg_sys::SampleScanState,
    _slot: *mut pg_sys::TupleTableSlot
) -> bool);
/// slot_callbacks — return the virtual-tuple slot ops (no physical tuple
/// format; Phase 0 returns empty scans so this is sufficient).
unsafe extern "C-unwind" fn stub_slot_callbacks(
    _rel: pg_sys::Relation,
) -> *const pg_sys::TupleTableSlotOps {
    &raw const pg_sys::TTSOpsVirtual
}
unimplemented_callback!(stub_tuple_fetch_row_version(
    _rel: pg_sys::Relation,
    _tid: pg_sys::ItemPointer,
    _snapshot: pg_sys::Snapshot,
    _slot: *mut pg_sys::TupleTableSlot
) -> bool);
unimplemented_callback!(stub_tuple_tid_valid(
    _scan: pg_sys::TableScanDesc,
    _tid: pg_sys::ItemPointer
) -> bool);
unimplemented_callback!(stub_tuple_get_latest_tid(
    _scan: pg_sys::TableScanDesc,
    _tid: pg_sys::ItemPointer
));
unimplemented_callback!(stub_tuple_satisfies_snapshot(
    _rel: pg_sys::Relation,
    _slot: *mut pg_sys::TupleTableSlot,
    _snapshot: pg_sys::Snapshot
) -> bool);
unimplemented_callback!(stub_index_delete_tuples(
    _rel: pg_sys::Relation,
    _delstate: *mut pg_sys::TM_IndexDeleteOp
) -> pg_sys::TransactionId);

// ---------------------------------------------------------------------------
// Build the TableAmRoutine with all Phase-0 callbacks installed.
// ---------------------------------------------------------------------------

fn build_am_routine() -> pg_sys::TableAmRoutine {
    pg_sys::TableAmRoutine {
        type_: pg_sys::NodeTag::T_TableAmRoutine,

        slot_callbacks: Some(stub_slot_callbacks),

        scan_begin: Some(stub_scan_begin),
        scan_end: Some(stub_scan_end),
        scan_rescan: Some(stub_scan_rescan),
        scan_getnextslot: Some(stub_scan_getnextslot),

        scan_set_tidrange: None,
        scan_getnextslot_tidrange: None,

        parallelscan_estimate: Some(stub_parallelscan_estimate),
        parallelscan_initialize: Some(stub_parallelscan_initialize),
        parallelscan_reinitialize: Some(stub_parallelscan_reinitialize),

        index_fetch_begin: Some(stub_index_fetch_begin),
        index_fetch_reset: Some(stub_index_fetch_reset),
        index_fetch_end: Some(stub_index_fetch_end),
        index_fetch_tuple: Some(stub_index_fetch_tuple),

        tuple_insert: Some(stub_tuple_insert),
        tuple_insert_speculative: Some(stub_tuple_insert_speculative),
        tuple_complete_speculative: Some(stub_tuple_complete_speculative),
        multi_insert: Some(stub_multi_insert),
        tuple_delete: Some(stub_tuple_delete),
        tuple_update: Some(stub_tuple_update),
        tuple_lock: Some(stub_tuple_lock),
        finish_bulk_insert: Some(stub_finish_bulk_insert),

        relation_set_new_filelocator: Some(stub_relation_set_new_filelocator),
        relation_nontransactional_truncate: Some(stub_relation_nontransactional_truncate),
        relation_copy_data: Some(stub_relation_copy_data),
        relation_copy_for_cluster: Some(stub_relation_copy_for_cluster),
        relation_vacuum: Some(stub_relation_vacuum),

        scan_analyze_next_block: Some(stub_scan_analyze_next_block),
        scan_analyze_next_tuple: Some(stub_scan_analyze_next_tuple),

        index_build_range_scan: Some(stub_index_build_range_scan),
        index_validate_scan: Some(stub_index_validate_scan),

        relation_size: Some(stub_relation_size),
        relation_needs_toast_table: Some(stub_relation_needs_toast_table),
        relation_toast_am: None,
        relation_fetch_toast_slice: None,

        relation_estimate_size: Some(stub_relation_estimate_size),

        tuple_fetch_row_version: Some(stub_tuple_fetch_row_version),
        tuple_tid_valid: Some(stub_tuple_tid_valid),
        tuple_get_latest_tid: Some(stub_tuple_get_latest_tid),

        scan_bitmap_next_tuple: Some(stub_scan_bitmap_next_tuple),

        scan_sample_next_block: Some(stub_scan_sample_next_block),
        scan_sample_next_tuple: Some(stub_scan_sample_next_tuple),

        tuple_satisfies_snapshot: Some(stub_tuple_satisfies_snapshot),
        index_delete_tuples: Some(stub_index_delete_tuples),
    }
}

// ---------------------------------------------------------------------------
// Handler entry points — registered by CREATE ACCESS METHOD ... HANDLER.
// Must use #[unsafe(no_mangle)] (Rust 2024) and extern "C-unwind".
//
// PostgreSQL requires a `pg_finfo_<funcname>` FUNCTION (not a data variable)
// for every C function called as a SQL function.  pgrx auto-generates this
// via a #[pg_extern] wrapper.  Since table_am_handler is a pseudo-type
// unsupported by #[pg_extern], we emit the finfo FUNCTION manually.
//
// The C macro expands to:
//   Pg_finfo_record *pg_finfo_myfunc(void) {
//       static const Pg_finfo_record rec = { 1 };
//       return &rec;
//   }
//
// PostgreSQL's fmgr.c calls (*infofunc)() — so it MUST be a function.
// Declaring it as a static data variable causes SIGSEGV when PG tries to
// execute the data bytes as code.
// ---------------------------------------------------------------------------

static FINFO_NODE_HANDLER: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };
static FINFO_EDGE_HANDLER: pg_sys::Pg_finfo_record = pg_sys::Pg_finfo_record { api_version: 1 };

/// Function-info callback for pg_eddy_node_handler.
/// PostgreSQL calls this (via dlsym + indirect call) to verify API version.
///
/// # Safety
/// Called by PostgreSQL's fmgr machinery during function loading.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pg_finfo_pg_eddy_node_handler() -> *const pg_sys::Pg_finfo_record {
    &raw const FINFO_NODE_HANDLER
}

/// Function-info callback for pg_eddy_edge_handler.
///
/// # Safety
/// Called by PostgreSQL's fmgr machinery during function loading.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pg_finfo_pg_eddy_edge_handler() -> *const pg_sys::Pg_finfo_record {
    &raw const FINFO_EDGE_HANDLER
}

/// Handler for `pg_eddy_node` access method.
///
/// # Safety
/// Called by PostgreSQL's CREATE ACCESS METHOD machinery.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pg_eddy_node_handler(
    _fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    let routine = build_am_routine();
    let ptr: *mut pg_sys::TableAmRoutine = unsafe {
        pg_sys::MemoryContextAlloc(
            pg_sys::TopMemoryContext,
            std::mem::size_of::<pg_sys::TableAmRoutine>(),
        )
        .cast()
    };
    unsafe { ptr.write(routine) };
    pg_sys::Datum::from(ptr as usize)
}

/// Handler for `pg_eddy_edge` access method.
/// Reuses the same stub routine as the node AM for Phase 0.
///
/// # Safety
/// Called by PostgreSQL's CREATE ACCESS METHOD machinery.
#[unsafe(no_mangle)]
pub unsafe extern "C-unwind" fn pg_eddy_edge_handler(
    fcinfo: pg_sys::FunctionCallInfo,
) -> pg_sys::Datum {
    unsafe { pg_eddy_node_handler(fcinfo) }
}


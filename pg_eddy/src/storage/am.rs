/// Table Access Method handler stubs — Phase 0.
///
/// Two AM objects are registered in the extension SQL:
///   CREATE ACCESS METHOD pg_eddy_node TYPE TABLE HANDLER pg_eddy_node_handler;
///   CREATE ACCESS METHOD pg_eddy_edge TYPE TABLE HANDLER pg_eddy_edge_handler;
///
/// All callbacks return "not implemented" for Phase 0, except scan_begin /
/// scan_getnextslot / scan_end which return an empty result set.
///
/// Real implementations are added in Phase 1 (nodes) and Phase 2 (edges).
use pgrx::prelude::*;
use pgrx::pg_sys;

// ---------------------------------------------------------------------------
// Scan stubs — return empty rather than erroring, so SELECT * FROM nodes works.
// ---------------------------------------------------------------------------

/// scan_begin stub — allocates a minimal TableScanDescData.
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
    let desc: *mut pg_sys::TableScanDescData = unsafe {
        pg_sys::palloc0(std::mem::size_of::<pg_sys::TableScanDescData>()).cast()
    };
    unsafe {
        (*desc).rs_rd = rel;
        (*desc).rs_snapshot = snapshot;
        (*desc).rs_nkeys = nkeys;
        (*desc).rs_key = key;
        (*desc).rs_flags = flags;
        (*desc).rs_parallel = pscan;
    }
    desc
}

/// scan_end stub — pfrees the descriptor.
///
/// # Safety
/// Called by PostgreSQL's executor.
unsafe extern "C-unwind" fn stub_scan_end(scan: pg_sys::TableScanDesc) {
    unsafe { pg_sys::pfree(scan.cast()) };
}

/// scan_rescan stub — no-op for Phase 0.
///
/// # Safety
/// Called by PostgreSQL's executor.
unsafe extern "C-unwind" fn stub_scan_rescan(
    _scan: pg_sys::TableScanDesc,
    _key: *mut pg_sys::ScanKeyData,
    _set_params: bool,
    _allow_strat: bool,
    _allow_sync: bool,
    _allow_pagemode: bool,
) {
}

/// scan_getnextslot stub — always returns false (empty table).
///
/// # Safety
/// Called by PostgreSQL's executor.
unsafe extern "C-unwind" fn stub_scan_getnextslot(
    _scan: pg_sys::TableScanDesc,
    _direction: pg_sys::ScanDirection::Type,
    _slot: *mut pg_sys::TupleTableSlot,
) -> bool {
    false
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
/// relation_set_new_filelocator — pg_eddy is a logical AM with no physical
/// file-based storage, so there is nothing to initialize.  However PostgreSQL
/// asserts that the out-params `freeze_xid` and `minmulti` are valid after
/// this call (for permanent relations), so we must populate them.
unsafe extern "C-unwind" fn stub_relation_set_new_filelocator(
    _rel: pg_sys::Relation,
    _newrlocator: *const pg_sys::RelFileLocator,
    _persistence: std::ffi::c_char,
    freeze_xid: *mut pg_sys::TransactionId,
    minmulti: *mut pg_sys::MultiXactId,
) {
    // Phase 0: no storage to initialize.
    // Must set out-params to valid values; PG18 asserts TransactionIdIsNormal
    // for permanent relations before storing them in pg_class.
    unsafe {
        if !freeze_xid.is_null() {
            *freeze_xid = pg_sys::GetCurrentTransactionId();
        }
        if !minmulti.is_null() {
            *minmulti = pg_sys::GetOldestMultiXactId();
        }
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
/// relation_size — Phase 0 has no physical storage; report 0 blocks.
unsafe extern "C-unwind" fn stub_relation_size(
    _rel: pg_sys::Relation,
    _fork_number: pg_sys::ForkNumber::Type,
) -> pg_sys::uint64 {
    0
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


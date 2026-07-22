//! Logical decoding output plugin for pg_eddy semantic CDC messages.

use std::ffi::{CStr, c_char, c_void};

use pgrx::pg_guard;
use pgrx::pg_sys;

use crate::storage::cdc_protocol::{
    Frame, LOGICAL_MESSAGE_PREFIX, decode_logical_message, encode_frame,
};

static TXN_HAS_PG_EDDY_MESSAGES: u8 = 0;

fn transaction_marker() -> *mut c_void {
    std::ptr::from_ref(&TXN_HAS_PG_EDDY_MESSAGES)
        .cast_mut()
        .cast::<c_void>()
}

/// Register the callbacks PostgreSQL invokes for a logical slot using plugin
/// `pg_eddy`.
///
/// # Safety
/// `callbacks` is allocated and owned by PostgreSQL for the duration of plugin
/// initialization.
#[unsafe(no_mangle)]
#[pg_guard]
pub unsafe extern "C-unwind" fn _PG_output_plugin_init(
    callbacks: *mut pg_sys::OutputPluginCallbacks,
) {
    if callbacks.is_null() {
        pgrx::error!("pg_eddy output plugin received a NULL callback table");
    }

    // SAFETY: PostgreSQL passes a valid writable callback table here.
    unsafe {
        *callbacks = pg_sys::OutputPluginCallbacks::default();
        (*callbacks).startup_cb = Some(startup);
        (*callbacks).begin_cb = Some(begin);
        (*callbacks).change_cb = Some(change);
        (*callbacks).commit_cb = Some(commit);
        (*callbacks).message_cb = Some(message);
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn startup(
    context: *mut pg_sys::LogicalDecodingContext,
    options: *mut pg_sys::OutputPluginOptions,
    _is_init: bool,
) {
    if context.is_null() || options.is_null() {
        pgrx::error!("pg_eddy output plugin startup received NULL state");
    }

    // v1 intentionally has no plugin options. Rejecting unknown options keeps
    // future protocol negotiation explicit instead of silently misdecoding.
    let option_count = unsafe { pg_sys::list_length((*context).output_plugin_options) };
    if option_count != 0 {
        pgrx::error!("pg_eddy output plugin v1 accepts no options");
    }

    // SAFETY: Both pointers were checked above and are PostgreSQL-owned for
    // this callback.
    unsafe {
        (*options).output_type =
            pg_sys::OutputPluginOutputType::OUTPUT_PLUGIN_BINARY_OUTPUT;
        (*options).receive_rewrites = false;
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn begin(
    _context: *mut pg_sys::LogicalDecodingContext,
    transaction: *mut pg_sys::ReorderBufferTXN,
) {
    if transaction.is_null() {
        pgrx::error!("pg_eddy output plugin BEGIN received a NULL transaction");
    }

    // BEGIN is emitted lazily from the first matching message callback so
    // transactions containing no pg_eddy semantic events produce no output.
    unsafe {
        (*transaction).output_plugin_private = std::ptr::null_mut();
    }
}

#[pg_guard]
unsafe extern "C-unwind" fn change(
    _context: *mut pg_sys::LogicalDecodingContext,
    _transaction: *mut pg_sys::ReorderBufferTXN,
    _relation: pg_sys::Relation,
    _change: *mut pg_sys::ReorderBufferChange,
) {
    // pg_eddy v1 is semantic-message-only. PostgreSQL requires this callback
    // to be registered, but ordinary heap changes are intentionally ignored.
}

#[pg_guard]
unsafe extern "C-unwind" fn message(
    context: *mut pg_sys::LogicalDecodingContext,
    transaction: *mut pg_sys::ReorderBufferTXN,
    message_lsn: pg_sys::XLogRecPtr,
    transactional: bool,
    prefix: *const c_char,
    message_size: pg_sys::Size,
    message_data: *const c_char,
) {
    if !transactional || prefix.is_null() {
        return;
    }

    // SAFETY: PostgreSQL supplies a NUL-terminated prefix for this callback.
    let prefix = unsafe { CStr::from_ptr(prefix) };
    if prefix.to_bytes() != LOGICAL_MESSAGE_PREFIX.as_bytes() {
        return;
    }
    if context.is_null() || transaction.is_null() {
        pgrx::error!("pg_eddy transactional message received NULL decoding state");
    }
    if message_size > 0 && message_data.is_null() {
        pgrx::error!("pg_eddy transactional message has a NULL payload");
    }

    // SAFETY: PostgreSQL guarantees `message_size` readable bytes for the
    // duration of the callback. A zero-length payload may use a NULL pointer.
    let payload = if message_size == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(message_data.cast::<u8>(), message_size) }
    };
    let mutation = decode_logical_message(payload)
        .unwrap_or_else(|error| pgrx::error!("pg_eddy output plugin decode failed: {error}"));

    // SAFETY: The transaction pointer was checked above and remains valid for
    // this callback.
    let first_message = unsafe { (*transaction).output_plugin_private.is_null() };
    if first_message {
        let begin = Frame::Begin {
            xid: unsafe { (*transaction).xid.into() },
        };
        write_frame(context, &begin, false);
        unsafe {
            (*transaction).output_plugin_private = transaction_marker();
        }
    }

    write_frame(
        context,
        &Frame::Mutation {
            event_lsn: message_lsn,
            mutation,
        },
        false,
    );
}

#[pg_guard]
unsafe extern "C-unwind" fn commit(
    context: *mut pg_sys::LogicalDecodingContext,
    transaction: *mut pg_sys::ReorderBufferTXN,
    commit_lsn: pg_sys::XLogRecPtr,
) {
    if context.is_null() || transaction.is_null() {
        pgrx::error!("pg_eddy output plugin COMMIT received NULL state");
    }

    // Transactions without matching semantic messages remain invisible.
    if unsafe { (*transaction).output_plugin_private != transaction_marker() } {
        return;
    }

    let frame = Frame::Commit {
        xid: unsafe { (*transaction).xid.into() },
        commit_lsn,
        end_lsn: unsafe { (*transaction).end_lsn },
    };
    write_frame(context, &frame, true);
}

fn write_frame(context: *mut pg_sys::LogicalDecodingContext, frame: &Frame, last_write: bool) {
    let encoded = encode_frame(frame)
        .unwrap_or_else(|error| pgrx::error!("pg_eddy output plugin encode failed: {error}"));
    let length = i32::try_from(encoded.len())
        .unwrap_or_else(|_| pgrx::error!("pg_eddy output frame exceeds PostgreSQL's limit"));

    // SAFETY: `context` is a live decoding context. PrepareWrite initializes
    // its StringInfo output buffer, append copies the bytes, and Write hands
    // the complete binary frame to PostgreSQL before `encoded` is dropped.
    unsafe {
        pg_sys::OutputPluginPrepareWrite(context, last_write);
        pg_sys::appendBinaryStringInfo(
            (*context).out,
            encoded.as_ptr().cast::<c_void>(),
            length,
        );
        pg_sys::OutputPluginWrite(context, last_write);
    }
}
//! Transactional semantic CDC message producer.

use std::ffi::{c_char, c_void};

use pgrx::pg_sys;

use crate::storage::cdc_protocol::{LOGICAL_MESSAGE_PREFIX, Mutation, encode_logical_message};

unsafe extern "C-unwind" {
    fn LogLogicalMessage(
        prefix: *const c_char,
        message: *const c_char,
        size: usize,
        transactional: bool,
        flush: bool,
    ) -> pg_sys::XLogRecPtr;
}

/// Emit one semantic graph mutation as a transactional logical message.
/// PostgreSQL exposes it to output plugins only if the surrounding transaction
/// commits; transaction and savepoint rollbacks discard it automatically.
pub fn emit(mutation: &Mutation) -> pg_sys::XLogRecPtr {
    let encoded = encode_logical_message(mutation)
        .unwrap_or_else(|error| pgrx::error!("pg_eddy: cannot encode CDC mutation: {error}"));

    // SAFETY: The prefix is a static NUL-terminated C string. `encoded` stays
    // alive for the duration of the call, and PostgreSQL copies `size` bytes
    // into WAL before returning. The payload is binary and need not end in NUL.
    unsafe {
        LogLogicalMessage(
            c"pg_eddy/v1".as_ptr(),
            encoded.as_ptr().cast::<c_void>().cast::<c_char>(),
            encoded.len(),
            true,
            false,
        )
    }
}

const _: () = assert!(LOGICAL_MESSAGE_PREFIX.len() == b"pg_eddy/v1".len());
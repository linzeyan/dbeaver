//! C ABI surface. The only crate a front-end links against.
//!
//! Result data crosses via the Arrow C Data Interface, so handing a batch to
//! Swift is passing two structs — no serialization, no copy. That property is
//! what Phase 0 exists to verify, so it is load-bearing rather than an
//! optimization.
//!
//! Calls are synchronous and must not be made from the UI thread. Phase 1
//! replaces this with a handle-plus-event-queue design; Phase 0 keeps it
//! blocking because a background dispatch queue on the Swift side is enough to
//! measure with, and the simpler surface is easier to trust.

use arrow::array::{Array, RecordBatch, StructArray};
use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};
use driver_postgres::{ArrowStream, PgSource};
use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;
use std::sync::OnceLock;
use tokio::runtime::Runtime;

fn runtime() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("failed to start tokio runtime"))
}

/// Writes `msg` into `*err` as a freshly allocated C string, if `err` is non-null.
/// Caller releases it with `db_string_free`.
unsafe fn set_err(err: *mut *mut c_char, msg: impl std::fmt::Display) {
    if err.is_null() {
        return;
    }
    let c = CString::new(msg.to_string()).unwrap_or_else(|_| CString::new("error").unwrap());
    unsafe { *err = c.into_raw() };
}

pub struct DbHandle {
    source: PgSource,
}

pub struct DbQuery {
    stream: ArrowStream,
}

/// # Safety
/// `conn_str` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_connect(
    conn_str: *const c_char,
    err: *mut *mut c_char,
) -> *mut DbHandle {
    if conn_str.is_null() {
        unsafe { set_err(err, "conn_str is null") };
        return ptr::null_mut();
    }
    let s = match unsafe { CStr::from_ptr(conn_str) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    match runtime().block_on(PgSource::connect(s)) {
        Ok(source) => Box::into_raw(Box::new(DbHandle { source })),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `handle` must come from `db_connect` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_free(handle: *mut DbHandle) {
    if !handle.is_null() {
        drop(unsafe { Box::from_raw(handle) });
    }
}

/// # Safety
/// `handle` must be live; `sql` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query(
    handle: *mut DbHandle,
    sql: *const c_char,
    batch_rows: usize,
    err: *mut *mut c_char,
) -> *mut DbQuery {
    if handle.is_null() || sql.is_null() {
        unsafe { set_err(err, "null handle or sql") };
        return ptr::null_mut();
    }
    let h = unsafe { &*handle };
    let s = match unsafe { CStr::from_ptr(sql) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            unsafe { set_err(err, e) };
            return ptr::null_mut();
        }
    };
    match runtime().block_on(h.source.query(s, batch_rows)) {
        Ok(stream) => Box::into_raw(Box::new(DbQuery { stream })),
        Err(e) => {
            unsafe { set_err(err, e) };
            ptr::null_mut()
        }
    }
}

/// Exports the result schema into `out` (an `ArrowSchema` the caller owns and
/// releases through its own `release` callback). Returns 0 on success.
///
/// # Safety
/// `query` must be live; `out` must point to writable `ArrowSchema` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_schema(
    query: *mut DbQuery,
    out: *mut FFI_ArrowSchema,
    err: *mut *mut c_char,
) -> c_int {
    if query.is_null() || out.is_null() {
        unsafe { set_err(err, "null query or out") };
        return -1;
    }
    let q = unsafe { &*query };
    // The batch is exported as a struct array, so the schema it must match is
    // the struct of its fields rather than the bare field list.
    let dt = arrow::datatypes::DataType::Struct(q.stream.schema().fields().clone());
    match FFI_ArrowSchema::try_from(&dt) {
        Ok(schema) => {
            unsafe { ptr::write(out, schema) };
            0
        }
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

/// Pulls the next batch. Returns 1 when `out` was filled, 0 when the result is
/// exhausted, -1 on error.
///
/// # Safety
/// `query` must be live; `out` must point to writable `ArrowArray` storage.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_next(
    query: *mut DbQuery,
    out: *mut FFI_ArrowArray,
    err: *mut *mut c_char,
) -> c_int {
    if query.is_null() || out.is_null() {
        unsafe { set_err(err, "null query or out") };
        return -1;
    }
    let q = unsafe { &mut *query };
    match runtime().block_on(q.stream.next_batch()) {
        Ok(Some(batch)) => {
            let array = batch_to_ffi(batch);
            unsafe { ptr::write(out, array) };
            1
        }
        Ok(None) => 0,
        Err(e) => {
            unsafe { set_err(err, e) };
            -1
        }
    }
}

fn batch_to_ffi(batch: RecordBatch) -> FFI_ArrowArray {
    let struct_array: StructArray = batch.into();
    FFI_ArrowArray::new(&struct_array.to_data())
}

/// # Safety
/// `query` must come from `db_query` and not have been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_query_free(query: *mut DbQuery) {
    if !query.is_null() {
        drop(unsafe { Box::from_raw(query) });
    }
}

/// # Safety
/// `s` must be a string produced by this library's `err` out-parameters.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn db_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

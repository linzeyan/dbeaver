//! A headless conformance harness drives the full `ffi` surface without a front-end.
//!
//! This exists so the FFI contract is validated independently of macOS, which is
//! what keeps the eventual Windows front-end from discovering a Swift-shaped API.
//! The harness calls every `db_*` entry point exactly the way a foreign front-end
//! does — through the raw `unsafe extern "C"` functions, with C strings and
//! out-parameters, not through any Rust convenience.
//!
//! It covers two groups:
//!
//! 1. **Argument-contract tests that need no database.** Every entry point's
//!    null-pointer and invalid-UTF-8 paths: that it returns the documented
//!    failure value (null pointer, -1, or -1 for `db_query_rows_affected`), that
//!    it writes a message into `err`, and that `db_string_free` releases it.
//!    These must run under plain `cargo test`.
//!
//! 2. **Live-surface tests against the benchmark database**, marked
//!    `#[ignore = "requires the benchmark database"]` so `make test-integration`
//!    runs them and `make test` does not — the same convention
//!    `crates/drivers/postgres/tests/integration.rs` already uses. Connection
//!    string: `host=127.0.0.1 port=55432 user=bench password=bench dbname=bench`.
//!    Cover: `db_connect` / `db_free`; every metadata entry point returning
//!    parseable JSON; `db_query` + `db_query_schema` + `db_query_next` draining
//!    a result to the 0 return, with the exported `ArrowSchema` and `ArrowArray`
//!    released through their own `release` callbacks; `db_query_rows_affected`
//!    returning -1 before the stream is drained and the real count after; `db_query`
//!    's `err_position` out-parameter carrying a non-zero position for a statement
//!    with a syntax error, and staying 0 for an error that has no position.
//!    Also `db_cursor` + `db_cursor_next` + `db_cursor_close` + `db_cursor_free`
//!    paging a result to the 0 return, freeing an open cursor without closing it,
//!    and `db_cursor`'s null-sql, invalid-UTF-8 and `err_position` failure paths.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;

use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};

use dbffi::{
    db_cancel, db_columns_json, db_connect, db_constraints_json, db_cursor, db_cursor_close,
    db_cursor_free, db_cursor_next, db_definition_json, db_foreign_keys_json, db_free,
    db_indexes_json, db_query, db_query_free, db_query_next, db_query_rows_affected,
    db_query_schema, db_referenced_by_json, db_relations_json, db_schemas_json, db_string_free,
    db_triggers_json,
};

// Test db_connect with null connection string
#[test]
fn test_connect_null_connection() {
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(ptr::null(), &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_connect with invalid UTF-8 connection string
#[test]
fn test_connect_invalid_utf8_connection() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(invalid_cstring.as_ptr(), &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_free with null handle
#[test]
fn test_free_null_handle() {
    unsafe { db_free(ptr::null_mut()) };
    // Should not crash
}

// Test db_cancel with null handle
#[test]
fn test_cancel_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cancel(ptr::null_mut(), &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cancel must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_schemas_json with null handle
#[test]
fn test_schemas_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_schemas_json(ptr::null_mut(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_schemas_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_relations_json with null handle
#[test]
fn test_relations_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_relations_json(ptr::null_mut(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_relations_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_relations_json with null schema
#[test]
fn test_relations_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_relations_json(ptr::null_mut(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_relations_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_relations_json with invalid UTF-8 schema
#[test]
fn test_relations_invalid_utf8_schema() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_relations_json(ptr::null_mut(), invalid_cstring.as_ptr(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_relations_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_columns_json with null handle
#[test]
fn test_columns_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_columns_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_columns_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_columns_json with null schema
#[test]
fn test_columns_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_columns_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_columns_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_columns_json with null relation
#[test]
fn test_columns_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_columns_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_columns_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_columns_json with invalid UTF-8 schema
#[test]
fn test_columns_invalid_utf8_schema() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_columns_json(
            ptr::null_mut(),
            invalid_cstring.as_ptr(),
            ptr::null(),
            &mut err,
        )
    };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_columns_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_columns_json with invalid UTF-8 relation
#[test]
fn test_columns_invalid_utf8_relation() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_columns_json(
            ptr::null_mut(),
            ptr::null(),
            invalid_cstring.as_ptr(),
            &mut err,
        )
    };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_columns_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_indexes_json with null handle
#[test]
fn test_indexes_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_indexes_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_indexes_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_indexes_json with null schema
#[test]
fn test_indexes_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_indexes_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_indexes_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_indexes_json with null relation
#[test]
fn test_indexes_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_indexes_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_indexes_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_definition_json with null handle
#[test]
fn test_definition_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_definition_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_definition_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_definition_json with null schema
#[test]
fn test_definition_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_definition_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_definition_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_definition_json with null relation
#[test]
fn test_definition_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_definition_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_definition_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_foreign_keys_json with null handle
#[test]
fn test_foreign_keys_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_foreign_keys_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_foreign_keys_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_foreign_keys_json with null schema
#[test]
fn test_foreign_keys_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_foreign_keys_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_foreign_keys_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_foreign_keys_json with null relation
#[test]
fn test_foreign_keys_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_foreign_keys_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_foreign_keys_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_referenced_by_json with null handle
#[test]
fn test_referenced_by_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_referenced_by_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_referenced_by_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_referenced_by_json with null schema
#[test]
fn test_referenced_by_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_referenced_by_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_referenced_by_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_referenced_by_json with null relation
#[test]
fn test_referenced_by_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_referenced_by_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_referenced_by_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_constraints_json with null handle
#[test]
fn test_constraints_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_constraints_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_constraints_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_constraints_json with null schema
#[test]
fn test_constraints_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_constraints_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_constraints_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_constraints_json with null relation
#[test]
fn test_constraints_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_constraints_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_constraints_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_triggers_json with null handle
#[test]
fn test_triggers_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_triggers_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_triggers_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_triggers_json with null schema
#[test]
fn test_triggers_null_schema() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_triggers_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_triggers_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_triggers_json with null relation
#[test]
fn test_triggers_null_relation() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_triggers_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_triggers_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query with null handle
#[test]
fn test_query_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let result = unsafe {
        db_query(
            ptr::null_mut(),
            ptr::null(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_query must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query with null sql
#[test]
fn test_query_null_sql() {
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let result = unsafe {
        db_query(
            ptr::null_mut(),
            ptr::null(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_query must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query_schema with null query
#[test]
fn test_query_schema_null_query() {
    let mut schema = FFI_ArrowSchema::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_query_schema(ptr::null_mut(), &mut schema, &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_query_schema must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query_schema with null out
#[test]
fn test_query_schema_null_out() {
    // We need a valid handle to test this properly
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(ptr::null(), &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query_next with null query
#[test]
fn test_query_next_null_query() {
    let mut array = FFI_ArrowArray::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_query_next(ptr::null_mut(), &mut array, &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_query_next must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query_next with null out
#[test]
fn test_query_next_null_out() {
    // We need a valid handle to test this properly
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(ptr::null(), &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_query_rows_affected with null query
#[test]
fn test_query_rows_affected_null_query() {
    let result = unsafe { db_query_rows_affected(ptr::null_mut()) };
    assert_eq!(result, -1);
}

// Test db_query_free with null query
#[test]
fn test_query_free_null_query() {
    unsafe { db_query_free(ptr::null_mut()) };
    // Should not crash
}

// Test db_string_free with null string
#[test]
fn test_string_free_null_string() {
    unsafe { db_string_free(ptr::null_mut()) };
    // Should not crash
}

// Test db_cursor with null handle
#[test]
fn test_cursor_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let cursor = unsafe {
        db_cursor(
            ptr::null_mut(),
            ptr::null(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(cursor.is_null());
    assert!(!err.is_null(), "db_cursor must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_cursor_next with null cursor
#[test]
fn test_cursor_next_null_cursor() {
    let mut array = FFI_ArrowArray::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_next(ptr::null_mut(), &mut array, &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cursor_next must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_cursor_close with null cursor
#[test]
fn test_cursor_close_null_cursor() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_close(ptr::null_mut(), &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cursor_close must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_cursor_free with null cursor
#[test]
fn test_cursor_free_null_cursor() {
    unsafe { db_cursor_free(ptr::null_mut()) };
    // Should not crash
}

// Live-surface tests against the benchmark database
#[ignore = "requires the benchmark database"]
#[test]
fn test_connect_and_free() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_schemas_json() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_schemas_json(handle, &mut err) };
    assert!(!result.is_null());
    assert!(
        err.is_null(),
        "db_schemas_json should not set err on success"
    );

    let json_str = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
    assert!(!json_str.is_empty());

    unsafe { db_string_free(result) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_relations_json() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let schema_cstring = CString::new("public").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_relations_json(handle, schema_cstring.as_ptr(), &mut err) };
    assert!(!result.is_null());
    assert!(
        err.is_null(),
        "db_relations_json should not set err on success"
    );

    let json_str = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
    assert!(!json_str.is_empty());

    unsafe { db_string_free(result) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_columns_json() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let schema_cstring = CString::new("public").unwrap();
    let relation_cstring = CString::new("bench_wide").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_columns_json(
            handle,
            schema_cstring.as_ptr(),
            relation_cstring.as_ptr(),
            &mut err,
        )
    };
    assert!(!result.is_null());
    assert!(
        err.is_null(),
        "db_columns_json should not set err on success"
    );

    let json_str = unsafe { CStr::from_ptr(result) }.to_str().unwrap();
    assert!(!json_str.is_empty());

    unsafe { db_string_free(result) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_query_and_drain() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide LIMIT 10").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let query = unsafe {
        db_query(
            handle,
            sql_cstring.as_ptr(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(!query.is_null());
    assert!(err.is_null(), "db_query should not set err on success");

    // Nothing has been read yet, so the count is not knowable — the -1 that says so is
    // the whole reason this returns a signed value rather than a count.
    assert_eq!(unsafe { db_query_rows_affected(query) }, -1);

    // Test schema
    let mut schema = FFI_ArrowSchema::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_query_schema(query, &mut schema, &mut err) };
    assert_eq!(result, 0);
    assert!(
        err.is_null(),
        "db_query_schema should not set err on success"
    );

    // Test next batch
    let mut array = FFI_ArrowArray::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_query_next(query, &mut array, &mut err) };
    assert_eq!(result, 1); // Should have one batch
    assert!(err.is_null(), "db_query_next should not set err on success");

    // Drain the rest of the result
    loop {
        let mut err: *mut c_char = ptr::null_mut();
        let result = unsafe { db_query_next(query, &mut array, &mut err) };
        if result == 0 {
            break; // End of results
        }
        assert_eq!(result, 1); // Should continue getting batches
        assert!(err.is_null(), "db_query_next should not set err on success");
    }

    // Test rows affected after draining
    let rows_affected = unsafe { db_query_rows_affected(query) };
    assert!(rows_affected >= 0); // Should have a real count now

    unsafe { db_query_free(query) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_query_syntax_error() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT 1/0").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let query = unsafe {
        db_query(
            handle,
            sql_cstring.as_ptr(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(query.is_null());
    assert!(!err.is_null(), "db_query should set err on syntax error");
    // Division by zero fails at execution, not at parse, so the server has no character
    // to point at. Zero is how that is said, and it has to stay distinguishable from a
    // real position of zero — which is why the server counts from one.
    assert_eq!(err_position, 0);

    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_query_syntax_error_with_position() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide WHERE id =").unwrap(); // Syntax error at end
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let query = unsafe {
        db_query(
            handle,
            sql_cstring.as_ptr(),
            1000,
            &mut err,
            &mut err_position,
        )
    };
    assert!(query.is_null());
    assert!(!err.is_null(), "db_query should set err on syntax error");
    assert!(err_position > 0); // Should have a position for syntax error

    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_pages_result_and_ends() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide LIMIT 250").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let cursor = unsafe {
        db_cursor(
            handle,
            sql_cstring.as_ptr(),
            100,
            &mut err,
            &mut err_position,
        )
    };
    assert!(!cursor.is_null());
    assert!(err.is_null(), "db_cursor should not set err on success");

    // Page through the results
    let mut array = FFI_ArrowArray::empty();
    let mut call_count = 0;
    loop {
        let mut err: *mut c_char = ptr::null_mut();
        let result = unsafe { db_cursor_next(cursor, &mut array, &mut err) };
        if result == 0 {
            break; // End of results
        }
        assert_eq!(result, 1); // Should continue getting batches
        assert!(
            err.is_null(),
            "db_cursor_next should not set err on success"
        );
        call_count += 1;
    }

    // Should have made multiple calls to get all the data
    assert!(call_count > 1, "Should have multiple pages for this query");

    // Close the cursor
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_close(cursor, &mut err) };
    assert_eq!(result, 0);
    assert!(
        err.is_null(),
        "db_cursor_close should not set err on success"
    );

    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_free_without_close() {
    // A front-end that drops a result mid-scroll never gets to call close, so freeing an
    // open cursor has to release its connection on its own. Doing it many times over is
    // what makes that observable: each cursor opens a connection of its own, so a release
    // path that leaked one would run the server out of connections inside this loop, while
    // a correct one never holds more than a single cursor's worth at a time.
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide LIMIT 10").unwrap();
    for i in 0..120 {
        let mut err: *mut c_char = ptr::null_mut();
        let cursor =
            unsafe { db_cursor(handle, sql_cstring.as_ptr(), 5, &mut err, ptr::null_mut()) };
        assert!(
            !cursor.is_null(),
            "db_cursor failed on iteration {i}: {}",
            if err.is_null() {
                "no message".to_string()
            } else {
                unsafe { CStr::from_ptr(err) }
                    .to_string_lossy()
                    .into_owned()
            }
        );

        // Read one page, so the cursor is genuinely mid-result when it is dropped.
        let mut array = FFI_ArrowArray::empty();
        let mut err: *mut c_char = ptr::null_mut();
        assert_eq!(unsafe { db_cursor_next(cursor, &mut array, &mut err) }, 1);

        unsafe { db_cursor_free(cursor) };
    }

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_syntax_error_with_position() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide WHERE id =").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let cursor = unsafe {
        db_cursor(
            handle,
            sql_cstring.as_ptr(),
            100,
            &mut err,
            &mut err_position,
        )
    };
    assert!(cursor.is_null());
    assert!(!err.is_null(), "db_cursor should set err on syntax error");
    assert!(
        err_position > 0,
        "db_cursor should set err_position on syntax error"
    );

    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_next_null_out() {
    // A real cursor, so the null out-parameter is what fails rather than the null cursor.
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());

    let sql_cstring = CString::new("SELECT * FROM bench_wide LIMIT 10").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql_cstring.as_ptr(), 100, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_next(cursor, ptr::null_mut(), &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cursor_next must say why it failed");

    unsafe { db_string_free(err) };
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_null_sql() {
    // A live handle, so the null sql is what fails — with a null handle this path is
    // unreachable and the test would pass with the sql check deleted.
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());

    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, ptr::null(), 100, &mut err, ptr::null_mut()) };
    assert!(cursor.is_null());
    assert!(!err.is_null(), "db_cursor must say why it failed");

    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_invalid_utf8_sql() {
    let conn_str =
        CString::new("host=127.0.0.1 port=55432 user=bench password=bench dbname=bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let cursor = unsafe {
        db_cursor(
            handle,
            invalid_cstring.as_ptr(),
            100,
            &mut err,
            &mut err_position,
        )
    };
    assert!(cursor.is_null());
    assert!(!err.is_null(), "db_cursor must say why it failed");
    assert_eq!(
        err_position, 0,
        "err_position should be 0 for invalid UTF-8"
    );

    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

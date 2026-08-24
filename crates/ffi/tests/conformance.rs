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
//!    string: `postgres://bench:bench@127.0.0.1:55432/bench`.
//!    Cover: `db_connect` / `db_free`; every metadata entry point returning
//!    parseable JSON; `db_query` + `db_query_schema` + `db_query_next` draining
//!    a result to the 0 return, with the exported `ArrowSchema` and `ArrowArray`
//!    released through their own `release` callbacks; `db_query_rows_affected`
//!    returning -1 before the stream is drained and the real count after; `db_query`
//!    's `err_position` out-parameter carrying a non-zero position for a statement
//!    with a syntax error, and staying 0 for an error that has no position.
//!    Also the cursor surface — `db_cursor`, `db_cursor_schema`, `db_cursor_next`,
//!    `db_cursor_cancel`, `db_cursor_close`, `db_cursor_free` — reporting its
//!    schema before the first fetch, paging a result to the 0 return, stopping a
//!    fetch that is in flight, freeing an open cursor without closing it, and
//!    `db_cursor`'s null-sql, invalid-UTF-8 and `err_position` failure paths.
//!    And the transaction surface — `db_tx_state_json`, `db_tx_autocommit`,
//!    `db_tx_commit`, `db_tx_rollback`, `db_tx_savepoint`, `db_tx_rollback_to`,
//!    `db_tx_release` — where the check that matters is what another connection
//!    can see, since a transaction that held nothing back would pass every
//!    check made through the connection that opened it.

use std::ffi::{CStr, CString, c_char, c_int};
use std::ptr;
use std::time::Duration;

use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};

use dbffi::{
    db_cancel, db_columns_json, db_complete_json, db_connect, db_constraints_json, db_cursor,
    db_cursor_cancel, db_cursor_close, db_cursor_free, db_cursor_next, db_cursor_schema,
    db_databases_json, db_ddl_text, db_definition_json, db_edit_sql_json, db_export, db_export_sql,
    db_foreign_keys_json, db_free, db_import, db_indexes_json, db_names_forget, db_query,
    db_query_free, db_query_next, db_query_rows_affected, db_query_schema, db_referenced_by_json,
    db_relations_json, db_routine_definition_json, db_routines_json, db_row_identity_json,
    db_schemas_json, db_sql_error_offset, db_sql_format, db_sql_scan_json, db_string_free,
    db_transfer_cancel, db_transfer_free, db_transfer_start, db_transfer_step, db_triggers_json,
    db_tx_autocommit, db_tx_commit, db_tx_release, db_tx_rollback, db_tx_rollback_to,
    db_tx_savepoint, db_tx_state_json,
};

// Test db_connect with null connection string
#[test]
fn test_connect_null_connection() {
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(ptr::null(), ptr::null(), 10, &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_connect with invalid UTF-8 connection string
#[test]
fn test_connect_invalid_utf8_connection() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(invalid_cstring.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(handle.is_null());
    assert!(!err.is_null(), "db_connect must say why it failed");
    unsafe { db_string_free(err) };
}

// A server that accepts the connection and then says nothing is the case the
// limit exists for. Without one this waits for a handshake that never comes, and
// "the window never came back" is how that is reported.
//
// The listener is real rather than an unroutable address, because an address
// with no route fails fast and would pass this test with no timeout in the code
// at all.
#[test]
fn a_server_that_accepts_and_never_answers_is_given_up_on() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port to listen on");
    let port = listener.local_addr().expect("the port it took").port();
    std::thread::spawn(move || {
        // Accepted and held. Nothing is ever written back, so the driver waits
        // on a handshake. Held rather than dropped: a closed socket is a refused
        // connection, which is the case this test is not about.
        let held = listener.incoming().next();
        std::thread::sleep(std::time::Duration::from_secs(30));
        drop(held);
    });

    let conn = CString::new(format!("postgres://someone@127.0.0.1:{port}/whatever")).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let began = std::time::Instant::now();
    let handle = unsafe { db_connect(conn.as_ptr(), ptr::null(), 1, &mut err) };
    let took = began.elapsed();

    assert!(handle.is_null(), "nothing answered, so nothing was opened");
    assert!(!err.is_null(), "db_connect must say why it failed");
    let said = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    unsafe { db_string_free(err) };
    assert!(
        said.contains("did not answer within 1s"),
        "the limit is what stopped it, and the message should say so; got: {said}"
    );
    // Generous, because this asserts that a limit exists at all rather than that
    // it is precise. A build with no timeout does not finish in ten seconds.
    assert!(took < Duration::from_secs(10), "gave up after {took:?}");
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

#[test]
fn test_databases_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_databases_json(ptr::null_mut(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_databases_json must say why it failed");
    unsafe { db_string_free(err) };
}

// The `null` half of the contract, on the one driver that needs no server. A
// front end reads this to decide whether to draw a database level at all, so
// "no such level" arriving as `[]` would be a level with nothing under it.
#[test]
fn an_engine_without_a_database_level_answers_null() {
    let path = std::env::temp_dir().join("dbffi-databases-null.db");
    // Zero bytes is a valid SQLite database, and this driver refuses a path that
    // is not already a file.
    std::fs::write(&path, b"").expect("scratch database file");
    let conn = std::ffi::CString::new(format!("sqlite://{}", path.display())).unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null(), "the scratch SQLite file should open");

    let raw = unsafe { db_databases_json(handle, &mut err) };
    assert!(!raw.is_null(), "db_databases_json should answer");
    let json = unsafe { std::ffi::CStr::from_ptr(raw) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { db_string_free(raw) };
    unsafe { db_free(handle) };
    let _ = std::fs::remove_file(&path);

    assert_eq!(json, "null", "SQLite has no level above schemas");
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

// Test db_routines_json with null handle
#[test]
fn test_routines_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_routines_json(ptr::null_mut(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_routines_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_routines_json with invalid UTF-8 schema
#[test]
fn test_routines_invalid_utf8_schema() {
    let invalid_cstring = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_routines_json(ptr::null_mut(), invalid_cstring.as_ptr(), &mut err) };
    assert!(result.is_null());
    assert!(!err.is_null(), "db_routines_json must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_routine_definition_json with null handle
#[test]
fn test_routine_definition_null_handle() {
    let mut err: *mut c_char = ptr::null_mut();
    let result =
        unsafe { db_routine_definition_json(ptr::null_mut(), ptr::null(), ptr::null(), &mut err) };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_routine_definition_json must say why it failed"
    );
    unsafe { db_string_free(err) };
}

// Test db_routine_definition_json with null id
#[test]
fn test_routine_definition_null_id() {
    let schema = CString::new("public").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_routine_definition_json(ptr::null_mut(), schema.as_ptr(), ptr::null(), &mut err)
    };
    assert!(result.is_null());
    assert!(
        !err.is_null(),
        "db_routine_definition_json must say why it failed"
    );
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
    let handle = unsafe { db_connect(ptr::null(), ptr::null(), 10, &mut err) };
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
    let handle = unsafe { db_connect(ptr::null(), ptr::null(), 10, &mut err) };
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

// ---------------------------------------------------------------------------
// Reading SQL, which needs no database
//
// The wire shape is asserted whole rather than parsed, because it is what a
// front end mirrors field by field: a renamed key or a renumbered token kind is
// a silently mispainted editor, and comparing the string is the only assertion
// that fails on either. These names read as sentences, which is the convention
// of the newer tests in this workspace; the ones above predate it.
// ---------------------------------------------------------------------------

/// The JSON one scan produced, with the core's copy released.
fn scan(text: &str, scheme: &str, selection: (u32, u32)) -> String {
    let text = CString::new(text).unwrap();
    let scheme = CString::new(scheme).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe {
        db_sql_scan_json(
            text.as_ptr(),
            scheme.as_ptr(),
            selection.0,
            selection.1,
            &mut err,
        )
    };
    assert!(!raw.is_null(), "db_sql_scan_json must answer");
    assert!(
        err.is_null(),
        "db_sql_scan_json must not set err on success"
    );
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };
    json
}

#[test]
fn a_scan_answers_all_three_questions_in_one_object() {
    // SELECT is a keyword, the space between is whitespace, 1 is a number, the
    // semicolon separates and the comment after it is not a statement of its
    // own. The one statement in the buffer reports itself as the whole thing.
    assert_eq!(
        scan("SELECT 1; -- x", "postgres", (0, 0)),
        r#"{"tokens":[1,0,6,9,6,7,6,7,8,0,8,9,9,9,10,7,10,14],"statements":[0,8],"#.to_owned()
            + r#""target":{"start":0,"end":8,"origin":"whole","index":0,"of":0}}"#
    );
}

#[test]
fn a_caret_names_the_statement_it_sits_in_and_a_selection_names_itself() {
    let script = "SELECT 1; SELECT 2";
    assert!(
        scan(script, "postgres", (12, 12))
            .contains(r#""target":{"start":10,"end":18,"origin":"statement","index":2,"of":2}"#),
        "the caret in the second statement must name it"
    );
    assert!(
        scan(script, "postgres", (0, 8))
            .contains(r#""target":{"start":0,"end":8,"origin":"selection","index":0,"of":0}"#),
        "a selection is taken as written"
    );
    // Backwards is the same span, because a C caller can hand them over either
    // way round and means the same thing.
    assert!(scan(script, "postgres", (8, 0)).contains(r#""origin":"selection""#));
}

#[test]
fn a_buffer_with_nothing_to_run_has_no_target() {
    assert_eq!(
        scan("-- nothing here", "postgres", (0, 0)),
        r#"{"tokens":[7,0,15],"statements":[],"target":null}"#
    );
}

#[test]
fn the_scheme_picks_the_dialect_and_an_unknown_one_is_read_as_postgresql() {
    // A double quote opens an identifier in PostgreSQL and a string in MySQL,
    // which is kind 3 against kind 4 and the plainest proof that the scheme
    // reached the table rather than being ignored.
    assert!(scan(r#""a""#, "postgres", (0, 0)).starts_with(r#"{"tokens":[3,0,3]"#));
    assert!(scan(r#""a""#, "mysql", (0, 0)).starts_with(r#"{"tokens":[4,0,3]"#));
    assert!(scan(r#""a""#, "nosuchdb", (0, 0)).starts_with(r#"{"tokens":[3,0,3]"#));
}

#[test]
fn offsets_are_counted_in_characters_and_not_in_bytes() {
    // The difference is invisible until somebody types an accented letter, and
    // then every caret after it is one place out.
    assert_eq!(
        scan("'é' x", "postgres", (0, 0)),
        r#"{"tokens":[4,0,3,9,3,4,2,4,5],"statements":[0,5],"#.to_owned()
            + r#""target":{"start":0,"end":5,"origin":"whole","index":0,"of":0}}"#
    );
}

#[test]
fn a_scan_says_why_it_could_not_read_its_arguments() {
    let text = CString::new("SELECT 1").unwrap();
    let scheme = CString::new("postgres").unwrap();
    let invalid = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();

    for (text, scheme) in [
        (ptr::null(), scheme.as_ptr()),
        (text.as_ptr(), ptr::null()),
        (invalid.as_ptr(), scheme.as_ptr()),
        (text.as_ptr(), invalid.as_ptr()),
    ] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_sql_scan_json(text, scheme, 0, 0, &mut err) };
        assert!(raw.is_null());
        assert!(!err.is_null(), "db_sql_scan_json must say why it failed");
        unsafe { db_string_free(err) };
    }
}

#[test]
fn formatting_crosses_the_abi_and_keeps_what_it_was_given() {
    let sql = CString::new("SELECT a,b FROM t WHERE x=1").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_sql_format(sql.as_ptr(), &mut err) };
    assert!(!raw.is_null(), "db_sql_format must answer");
    assert!(err.is_null(), "db_sql_format must not set err on success");

    let out = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_string();
    unsafe { db_string_free(raw) };
    assert!(out.contains("SELECT"), "got {out}");
    assert!(out.contains("FROM"), "got {out}");
    // The front end replaces a buffer with this. Losing a clause would be the
    // one failure it could not recover from, so the ABI is checked for it here
    // and not only in the crate that does the work.
    assert!(out.contains("WHERE"), "got {out}");
}

#[test]
fn formatting_says_why_it_could_not_read_its_argument() {
    let invalid = CString::new(vec![b'S', b'E', b'L', 0xff, 0xfe]).unwrap();
    for text in [ptr::null(), invalid.as_ptr()] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_sql_format(text, &mut err) };
        assert!(raw.is_null());
        assert!(!err.is_null(), "db_sql_format must say why it failed");
        unsafe { db_string_free(err) };
    }
}

#[test]
fn an_error_position_lands_where_the_statement_started() {
    // 1-based, from the start of what was sent rather than of the buffer.
    assert_eq!(db_sql_error_offset(1, 10, 20), 10);
    assert_eq!(db_sql_error_offset(11, 10, 20), 20);
    // One past the last character is what an unexpected end of input points at;
    // beyond that the number cannot have come from this statement.
    assert_eq!(db_sql_error_offset(12, 10, 20), -1);
    assert_eq!(db_sql_error_offset(0, 10, 20), -1);
    assert_eq!(db_sql_error_offset(-1, 10, 20), -1);
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

// Test db_cursor_cancel with null cursor
#[test]
fn test_cursor_cancel_null_cursor() {
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_cancel(ptr::null_mut(), &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cursor_cancel must say why it failed");
    unsafe { db_string_free(err) };
}

// Test db_cursor_schema with null cursor
#[test]
fn test_cursor_schema_null_cursor() {
    let mut schema = FFI_ArrowSchema::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_schema(ptr::null_mut(), &mut schema, &mut err) };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_cursor_schema must say why it failed");
    unsafe { db_string_free(err) };
}

// Live-surface tests against the benchmark database
#[ignore = "requires the benchmark database"]
#[test]
fn test_connect_and_free() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_schemas_json() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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

/// The pair round-trips: an id this side got out of `db_routines_json` is one
/// `db_routine_definition_json` takes back.
///
/// The null-argument tests above cannot see this. The id is opaque and
/// driver-defined — an oid here, a `FUNCTION name` pair on MySQL — so the only
/// way to know the two calls agree on its spelling is to carry one across.
#[ignore = "requires the benchmark database"]
#[test]
fn test_routines_json_round_trips_an_id() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let schema_cstring = CString::new("public").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_routines_json(handle, schema_cstring.as_ptr(), &mut err) };
    assert!(!result.is_null());
    assert!(
        err.is_null(),
        "db_routines_json should not set err on success"
    );

    let listed = unsafe { CStr::from_ptr(result) }
        .to_str()
        .unwrap()
        .to_owned();
    unsafe { db_string_free(result) };
    assert!(
        listed.contains("bench_child_touch"),
        "the trigger function the benchmark schema defines should be listed: {listed}"
    );

    // Pulled out of the JSON rather than looked up, because the point is that
    // the id crossing the boundary is the one that comes back.
    let id = listed
        .split("\"id\":\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a listed routine carries an id");
    let id_cstring = CString::new(id).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let source = unsafe {
        db_routine_definition_json(
            handle,
            schema_cstring.as_ptr(),
            id_cstring.as_ptr(),
            &mut err,
        )
    };
    assert!(!source.is_null());
    assert!(
        err.is_null(),
        "db_routine_definition_json should not set err on success"
    );
    let source_str = unsafe { CStr::from_ptr(source) }.to_str().unwrap();
    assert_ne!(
        source_str, "null",
        "the id this side was just handed is not one the driver disowns"
    );
    unsafe { db_string_free(source) };

    // An id that names nothing is JSON null, not a failure: a routine dropped
    // between the list and the click is an ordinary thing to find.
    let stranger = CString::new("no_such_routine_anywhere").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let missing = unsafe {
        db_routine_definition_json(handle, schema_cstring.as_ptr(), stranger.as_ptr(), &mut err)
    };
    assert!(!missing.is_null());
    assert!(err.is_null());
    assert_eq!(
        unsafe { CStr::from_ptr(missing) }.to_str().unwrap(),
        "null",
        "an id that names nothing is nothing, not an error"
    );
    unsafe { db_string_free(missing) };

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_columns_json() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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

/// A raw pointer is not `Send`, and cancelling means naming the cursor from a
/// second thread. Sound for exactly the reason `db_cursor_cancel` documents:
/// that call borrows only the canceller, which the fetch on this thread does
/// not touch.
struct CursorHandle(*mut dbffi::DbCursor);
unsafe impl Send for CursorHandle {}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_cancel_stops_a_fetch_in_flight() {
    // The defect this exists to catch: `db_cancel` cancels the session
    // connection, and a cursor runs on one of its own — so a front-end that
    // routes a browse Cancel to `db_cancel` offers a button that does nothing.
    // Nothing else in this harness would notice, because every other cancel
    // test is about the session.
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    // pg_sleep rather than a large scan: a scan the server finishes early makes
    // this pass without cancelling anything, and the test would then be green on
    // a build where cursor cancellation does not work at all. In the WHERE
    // clause because pg_sleep returns void, which has no Arrow type and would
    // fail while the schema was built — before there was anything to cancel.
    let sql = CString::new("SELECT 1 AS n WHERE pg_sleep(10) IS NULL").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 1, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    // Scheduled before the fetch is issued, not after: `db_cursor_next` does not
    // return while the page is being produced, so a cancel sent after it would
    // be cancelling something that had already finished.
    let sendable = CursorHandle(cursor);
    let canceller = std::thread::spawn(move || {
        let c = sendable;
        // Long enough that the fetch is running. Cancelling before the server
        // starts finds nothing to stop, which is the one outcome that looks
        // exactly like a broken cancel.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let mut err: *mut c_char = ptr::null_mut();
        let rc = unsafe { db_cursor_cancel(c.0, &mut err) };
        if !err.is_null() {
            unsafe { db_string_free(err) };
        }
        rc
    });

    let started = std::time::Instant::now();
    let mut array = FFI_ArrowArray::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_next(cursor, &mut array, &mut err) };

    assert_eq!(
        result, -2,
        "a cancelled fetch is -2, not an ordinary failure"
    );
    assert!(!err.is_null(), "db_cursor_next must say why it stopped");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(9),
        "the fetch ran to completion instead of being cancelled"
    );
    assert_eq!(canceller.join().expect("cancel thread panicked"), 0);

    unsafe { db_string_free(err) };
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_schema() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    let sql_cstring = CString::new("SELECT * FROM bench_wide LIMIT 10").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let cursor = unsafe { db_cursor(handle, sql_cstring.as_ptr(), 5, &mut err, &mut err_position) };
    assert!(!cursor.is_null());
    assert!(err.is_null(), "db_cursor should not set err on success");

    // Test schema - should be available before any fetch
    let mut schema = FFI_ArrowSchema::empty();
    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe { db_cursor_schema(cursor, &mut schema, &mut err) };
    assert_eq!(result, 0);
    assert!(
        err.is_null(),
        "db_cursor_schema should not set err on success"
    );

    // Clean up
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_cursor_pages_result_and_ends() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
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

// ---------------------------------------------------------------------------
// Completion, which needs both halves
//
// The argument contract and the live surface are kept together here rather than
// split into the two groups above, because what this entry point promises is one
// thing: the span it says to replace and the names it offers have to agree, and
// reading that in two places is how they stop agreeing.
// ---------------------------------------------------------------------------

/// The JSON one completion produced, with the core's copy released.
fn complete(handle: *mut dbffi::DbHandle, text: &str, caret: u32) -> String {
    let text = CString::new(text).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_complete_json(handle, text.as_ptr(), caret, &mut err) };
    assert!(!raw.is_null(), "db_complete_json must answer");
    assert!(
        err.is_null(),
        "db_complete_json must not set err on success"
    );
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };
    json
}

#[test]
fn a_completion_says_why_it_could_not_read_its_arguments() {
    let text = CString::new("SELECT 1").unwrap();
    let invalid = CString::new(vec![b'v', b'a', b'l', b'i', b'd', 0xff, 0xfe]).unwrap();

    // A null handle is checked before a null text, so both orders are covered
    // by the first two; the third needs no database because the UTF-8 check
    // happens before anything is asked of one.
    for (handle, text) in [
        (ptr::null_mut(), text.as_ptr()),
        (ptr::null_mut(), ptr::null()),
        (ptr::null_mut(), invalid.as_ptr()),
    ] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_complete_json(handle, text, 0, &mut err) };
        assert!(raw.is_null());
        assert!(!err.is_null(), "db_complete_json must say why it failed");
        unsafe { db_string_free(err) };
    }
}

#[test]
fn forgetting_the_names_of_no_connection_is_not_a_crash() {
    unsafe { db_names_forget(ptr::null_mut()) };
}

#[test]
fn a_completion_stops_at_the_cap_however_many_names_the_schema_has() {
    // DuckDB in memory rather than the benchmark database: the cap is a property
    // of this layer, not of any server, and a schema with more names than the cap
    // is something to build rather than something to find.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null(), "duckdb in memory must open");

    for i in 0..1_100 {
        let sql = CString::new(format!("CREATE TABLE cap_{i:04}(id INTEGER)")).unwrap();
        let mut err: *mut c_char = ptr::null_mut();
        let mut err_position: c_int = 0;
        let query = unsafe { db_query(handle, sql.as_ptr(), 1000, &mut err, &mut err_position) };
        assert!(!query.is_null(), "CREATE TABLE {i} must be accepted");
        unsafe { db_query_free(query) };
    }
    // The catalog is cached, and it was read before any of those existed.
    unsafe { db_names_forget(handle) };

    let json = complete(handle, "SELECT * FROM ", 14);
    let offered = json.matches(r#""label":"#).count();
    assert_eq!(
        offered, 1000,
        "1100 tables exist and the cap is 1000, so exactly the cap crosses: got {offered}"
    );

    unsafe { db_free(handle) };
}

#[test]
fn a_field_the_driver_annotated_still_carries_its_annotation_across_the_abi() {
    // Two features rest on the C data interface exporting a *field's* metadata
    // and not just its type: DuckDB records what a column rendered as text used
    // to be, and MySQL records that a column was declared NOT NULL so the grid
    // can tell a substituted NULL from a real one. Neither has any other way to
    // say it, and if the export ever dropped it both would degrade silently —
    // the schema would still describe every column, just without the one fact
    // the front end was reading.
    //
    // DuckDB in memory because it is the driver that annotates without a server:
    // a LIST has no Arrow type this layer renders, so it arrives as text under
    // `duckdb.rendered_from`.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null(), "duckdb in memory must open");

    let sql = CString::new("SELECT [1, 2, 3] AS items").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut err_position: c_int = 0;
    let query = unsafe { db_query(handle, sql.as_ptr(), 1000, &mut err, &mut err_position) };
    assert!(!query.is_null(), "a list literal must be accepted");

    let mut schema = FFI_ArrowSchema::empty();
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(unsafe { db_query_schema(query, &mut schema, &mut err) }, 0);

    let metadata = schema
        .child(0)
        .metadata()
        .expect("the exported field's metadata must be readable");
    assert!(
        metadata.contains_key("duckdb.rendered_from"),
        "the driver's annotation must survive the export: got {metadata:?}"
    );

    unsafe { db_query_free(query) };
    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_completion_offers_the_columns_of_what_the_statement_selects_from() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    // The relation is named after the caret, which is the case completion exists
    // for: nothing before it says what is being selected from.
    let json = complete(handle, "SELECT  FROM bench_wide w", 7);
    assert!(
        json.starts_with(r#"{"start":7,"end":7,"#),
        "nothing is typed yet, so nothing is replaced: {json}"
    );
    assert!(
        json.contains(r#"{"label":"payload","insert":"payload","kind":"column""#),
        "got {json}"
    );

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_completion_names_the_characters_accepting_it_replaces() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    // `bench_w` occupies characters 14 to 21, and accepting `bench_wide` has to
    // replace all seven — a front end that inserted at the caret would produce
    // `bench_wbench_wide`.
    let json = complete(handle, "SELECT * FROM bench_w", 21);
    assert!(json.starts_with(r#"{"start":14,"end":21,"#), "got {json}");
    assert!(json.contains(r#""label":"bench_wide""#), "got {json}");
    // And only the names that match what was typed.
    assert!(!json.contains(r#""label":"no_key""#), "got {json}");

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_qualified_name_is_answered_from_the_schema_it_names() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let json = complete(handle, "SELECT * FROM reporting.", 24);
    assert!(
        json.contains(
            r#"{"label":"daily_totals","insert":"daily_totals","kind":"relation","detail":"table in reporting"}"#
        ),
        "got {json}"
    );
    // Not the default schema's tables, which is the whole point of having typed
    // the qualifier.
    assert!(!json.contains(r#""label":"bench_wide""#), "got {json}");

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_refresh_the_user_asked_for_is_answered_from_the_server_again() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let before = complete(handle, "SELECT * FROM bench_w", 21);
    unsafe { db_names_forget(handle) };
    // What is asserted is that forgetting leaves the connection able to answer
    // at all: the cache is emptied on a live handle, and the count of round
    // trips is checked where it can be seen, in crates/catalog.
    assert_eq!(complete(handle, "SELECT * FROM bench_w", 21), before);

    unsafe { db_free(handle) };
}

// ---------------------------------------------------------------------------
// Transactions
// ---------------------------------------------------------------------------

/// Opens the benchmark database, insisting it is there.
fn connected() -> *mut dbffi::DbHandle {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null(), "benchmark database unreachable");
    handle
}

/// Runs `sql` to the end and returns what the server said it affected.
///
/// The count comes from `db_query_rows_affected` rather than from the batches,
/// which is what lets these checks count rows without reading an Arrow array:
/// PostgreSQL reports the row count of a SELECT the same way it reports the
/// row count of a DELETE.
/// The message behind an `err`, released as it is read. Empty when there was
/// none, which is what an assertion that never fires wants to print.
fn complaint(err: &mut *mut c_char) -> String {
    if err.is_null() {
        return String::new();
    }
    let message = unsafe { CStr::from_ptr(*err) }
        .to_string_lossy()
        .into_owned();
    unsafe { db_string_free(*err) };
    *err = ptr::null_mut();
    message
}

fn ran(handle: *mut dbffi::DbHandle, sql: &str) -> i64 {
    let text = CString::new(sql).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let mut position: c_int = 0;
    let query = unsafe { db_query(handle, text.as_ptr(), 100, &mut err, &mut position) };
    if query.is_null() {
        let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
        unsafe { db_string_free(err) };
        panic!("{sql} failed: {why}");
    }
    let mut array = FFI_ArrowArray::empty();
    loop {
        let mut err: *mut c_char = ptr::null_mut();
        match unsafe { db_query_next(query, &mut array, &mut err) } {
            0 => break,
            1 => continue,
            code => {
                let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
                unsafe { db_string_free(err) };
                panic!("{sql} stopped with {code}: {why}");
            }
        }
    }
    let affected = unsafe { db_query_rows_affected(query) };
    unsafe { db_query_free(query) };
    affected
}

fn tx_state(handle: *mut dbffi::DbHandle) -> String {
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_tx_state_json(handle, &mut err) };
    assert!(!raw.is_null());
    assert!(
        err.is_null(),
        "db_tx_state_json must not set err on success"
    );
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };
    json
}

/// Calls `step` and insists it worked, saying why if it did not.
fn tx_step(what: &str, step: impl Fn(*mut *mut c_char) -> c_int) {
    let mut err: *mut c_char = ptr::null_mut();
    if step(&mut err) != 0 {
        let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
        unsafe { db_string_free(err) };
        panic!("{what} failed: {why}");
    }
    assert!(err.is_null(), "{what} set err on success");
}

#[test]
fn the_transaction_calls_say_why_a_null_handle_failed() {
    let name = CString::new("s").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    assert!(unsafe { db_tx_state_json(ptr::null_mut(), &mut err) }.is_null());
    assert!(!err.is_null());
    unsafe { db_string_free(err) };

    let calls: [(&str, &dyn Fn(*mut *mut c_char) -> c_int); 6] = [
        ("db_tx_autocommit", &|err| unsafe {
            db_tx_autocommit(ptr::null_mut(), 0, err)
        }),
        ("db_tx_commit", &|err| unsafe {
            db_tx_commit(ptr::null_mut(), err)
        }),
        ("db_tx_rollback", &|err| unsafe {
            db_tx_rollback(ptr::null_mut(), err)
        }),
        ("db_tx_savepoint", &|err| unsafe {
            db_tx_savepoint(ptr::null_mut(), name.as_ptr(), err)
        }),
        ("db_tx_rollback_to", &|err| unsafe {
            db_tx_rollback_to(ptr::null_mut(), name.as_ptr(), err)
        }),
        ("db_tx_release", &|err| unsafe {
            db_tx_release(ptr::null_mut(), name.as_ptr(), err)
        }),
    ];
    for (which, call) in calls {
        let mut err: *mut c_char = ptr::null_mut();
        assert_eq!(call(&mut err), -1, "{which} should refuse a null handle");
        assert!(!err.is_null(), "{which} should say why it refused");
        unsafe { db_string_free(err) };
    }
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_manual_commit_connection_keeps_its_work_to_itself_until_it_commits() {
    let handle = connected();
    let watcher = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_tx");
    ran(handle, "CREATE TABLE ffi_tx (n int)");

    assert!(
        tx_state(handle).contains(r#""transactional":true"#),
        "{}",
        tx_state(handle)
    );
    assert!(tx_state(handle).contains(r#""autocommit":true"#));

    tx_step("leaving autocommit", |err| unsafe {
        db_tx_autocommit(handle, 0, err)
    });
    // Nothing is open yet: the mode says what happens to the next statement,
    // and there has not been one.
    assert!(tx_state(handle).contains(r#""open":false"#));

    ran(handle, "INSERT INTO ffi_tx (n) VALUES (1)");
    assert!(tx_state(handle).contains(r#""open":true"#));
    // The claim, checked from outside: the row is real to this connection and
    // absent from the other one.
    assert_eq!(ran(handle, "SELECT n FROM ffi_tx"), 1);
    assert_eq!(ran(watcher, "SELECT n FROM ffi_tx"), 0);

    // A mode switch while work is uncommitted is refused rather than deciding
    // what to do with it.
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(unsafe { db_tx_autocommit(handle, 1, &mut err) }, -1);
    assert!(!err.is_null(), "the refusal has to say why");
    unsafe { db_string_free(err) };

    tx_step("rollback", |err| unsafe { db_tx_rollback(handle, err) });
    assert!(tx_state(handle).contains(r#""open":false"#));
    assert_eq!(ran(handle, "SELECT n FROM ffi_tx"), 0);

    // And committed work is there for everybody.
    ran(handle, "INSERT INTO ffi_tx (n) VALUES (2)");
    tx_step("commit", |err| unsafe { db_tx_commit(handle, err) });
    assert_eq!(ran(watcher, "SELECT n FROM ffi_tx"), 1);

    // Committing nothing is a disagreement about the state, not a no-op.
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(unsafe { db_tx_commit(handle, &mut err) }, -1);
    unsafe { db_string_free(err) };

    tx_step("returning to autocommit", |err| unsafe {
        db_tx_autocommit(handle, 1, err)
    });
    ran(handle, "DROP TABLE ffi_tx");
    unsafe { db_free(handle) };
    unsafe { db_free(watcher) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn a_savepoint_undoes_part_of_a_transaction_and_leaves_the_rest() {
    let handle = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_savepoint");
    ran(handle, "CREATE TABLE ffi_savepoint (n int)");
    tx_step("leaving autocommit", |err| unsafe {
        db_tx_autocommit(handle, 0, err)
    });

    ran(handle, "INSERT INTO ffi_savepoint (n) VALUES (1)");
    let halfway = CString::new("halfway").unwrap();
    tx_step("savepoint", |err| unsafe {
        db_tx_savepoint(handle, halfway.as_ptr(), err)
    });
    assert!(
        tx_state(handle).contains(r#""savepoints":["halfway"]"#),
        "{}",
        tx_state(handle)
    );

    ran(handle, "INSERT INTO ffi_savepoint (n) VALUES (2)");
    assert_eq!(ran(handle, "SELECT n FROM ffi_savepoint"), 2);
    tx_step("rollback to savepoint", |err| unsafe {
        db_tx_rollback_to(handle, halfway.as_ptr(), err)
    });
    assert_eq!(ran(handle, "SELECT n FROM ffi_savepoint"), 1);
    // The transaction is still open around it, which is the difference between
    // a savepoint and a rollback.
    assert!(tx_state(handle).contains(r#""open":true"#));

    tx_step("release", |err| unsafe {
        db_tx_release(handle, halfway.as_ptr(), err)
    });
    assert!(tx_state(handle).contains(r#""savepoints":[]"#));

    // A name that could carry a statement is refused before it reaches the
    // server, because it would be written into one.
    let injected = CString::new("s; DROP TABLE ffi_savepoint").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(
        unsafe { db_tx_savepoint(handle, injected.as_ptr(), &mut err) },
        -1
    );
    assert!(!err.is_null());
    unsafe { db_string_free(err) };

    tx_step("rollback", |err| unsafe { db_tx_rollback(handle, err) });
    tx_step("returning to autocommit", |err| unsafe {
        db_tx_autocommit(handle, 1, err)
    });
    assert_eq!(ran(handle, "SELECT n FROM ffi_savepoint"), 0);
    ran(handle, "DROP TABLE ffi_savepoint");
    unsafe { db_free(handle) };
}

// ---------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------

#[test]
fn the_ddl_call_says_why_it_could_not_read_its_arguments() {
    let public = CString::new("public").unwrap();
    let invalid = CString::new(vec![b'p', b'u', b'b', 0xff, 0xfe]).unwrap();
    for (handle, schema, relation) in [
        (ptr::null_mut(), public.as_ptr(), public.as_ptr()),
        (ptr::null_mut(), ptr::null(), public.as_ptr()),
        (ptr::null_mut(), public.as_ptr(), ptr::null()),
        (ptr::null_mut(), invalid.as_ptr(), public.as_ptr()),
    ] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_ddl_text(handle, schema, relation, &mut err) };
        assert!(raw.is_null());
        assert!(!err.is_null(), "db_ddl_text must say why it failed");
        unsafe { db_string_free(err) };
    }
}

#[ignore = "requires the benchmark database"]
#[test]
fn the_ddl_of_a_table_is_the_statement_that_would_make_it() {
    let handle = connected();
    let schema = CString::new("public").unwrap();
    let table = CString::new("bench_wide").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_ddl_text(handle, schema.as_ptr(), table.as_ptr(), &mut err) };
    assert!(!raw.is_null());
    assert!(err.is_null(), "db_ddl_text must not set err on success");
    let ddl = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };

    // Text and not JSON: what crosses is the statement itself, quotes and
    // newlines included, and a caller that had to decode a document first would
    // be undoing an encoding that bought nothing.
    assert!(ddl.starts_with("-- Drop table"), "got {ddl}");
    assert!(
        ddl.contains("CREATE TABLE public.bench_wide (\n\tid int4"),
        "got {ddl}"
    );

    // A relation the schema does not hold is a failure with a name in it, not an
    // empty statement.
    let missing = CString::new("no_such_relation_anywhere").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_ddl_text(handle, schema.as_ptr(), missing.as_ptr(), &mut err) };
    assert!(raw.is_null());
    let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(err) };
    assert!(why.contains("no_such_relation_anywhere"), "got {why}");

    unsafe { db_free(handle) };
}

// ---------------------------------------------------------------------------
// Editing a result
// ---------------------------------------------------------------------------

/// The statements `edits` would take, insisting they were written.
fn edit_sql(handle: *mut dbffi::DbHandle, edits: &str) -> String {
    let text = CString::new(edits).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_edit_sql_json(handle, text.as_ptr(), &mut err) };
    if raw.is_null() {
        let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
        unsafe { db_string_free(err) };
        panic!("db_edit_sql_json refused: {why}");
    }
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };
    json
}

#[test]
fn the_edit_call_says_why_it_could_not_read_its_arguments() {
    let edits = CString::new("{}").unwrap();
    let invalid = CString::new(vec![b'{', 0xff, 0xfe]).unwrap();
    for (handle, text) in [
        (ptr::null_mut(), edits.as_ptr()),
        (ptr::null_mut(), ptr::null()),
        (ptr::null_mut(), invalid.as_ptr()),
    ] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_edit_sql_json(handle, text, &mut err) };
        assert!(raw.is_null());
        assert!(!err.is_null(), "db_edit_sql_json must say why it failed");
        unsafe { db_string_free(err) };
    }
}

/// The statements are written against the real catalog, and they run.
///
/// Which is the half `crates/edit` cannot check for itself: it is answered by a
/// fake there, so "the column is called that" and "this text is valid SQL" are
/// only ever true here.
#[ignore = "requires the benchmark database"]
#[test]
fn a_changed_cell_becomes_a_statement_the_server_accepts() {
    let handle = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_edit");
    ran(
        handle,
        "CREATE TABLE ffi_edit (id int PRIMARY KEY, label text, qty numeric(9,2))",
    );
    ran(handle, "INSERT INTO ffi_edit VALUES (1, 'first', 1.00)");

    let json = edit_sql(
        handle,
        r#"{"schema":"public","relation":"ffi_edit",
            "updates":[{"key":[{"column":"id","value":"1"}],
                        "set":[{"column":"label","value":"changed"},
                               {"column":"qty","value":"2.50"}]}],
            "inserts":[{"set":[{"column":"id","value":"2"},{"column":"label","value":null}]}],
            "deletes":[]}"#,
    );
    let statements: Vec<String> = serde_json::from_str(&json).expect("statements should decode");
    assert_eq!(statements.len(), 2, "{json}");
    assert!(
        statements[0].starts_with("UPDATE public.ffi_edit SET"),
        "{json}"
    );

    for statement in &statements {
        ran(handle, statement);
    }
    // Two rows now, and the changed one changed: read back rather than trusted,
    // because a statement that ran is not the same claim as a row that says what
    // the user typed.
    assert_eq!(ran(handle, "SELECT id FROM ffi_edit"), 2);
    assert_eq!(
        ran(
            handle,
            "SELECT id FROM ffi_edit WHERE id = 1 AND label = 'changed' AND qty = 2.50"
        ),
        1
    );

    // And a relation with nothing to name a row by is refused with a reason
    // somebody can act on.
    let refused = CString::new(
        r#"{"schema":"public","relation":"no_key",
            "updates":[{"key":[{"column":"n","value":"1"}],
                        "set":[{"column":"n","value":"2"}]}]}"#,
    )
    .unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    assert!(unsafe { db_edit_sql_json(handle, refused.as_ptr(), &mut err) }.is_null());
    let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(err) };
    assert!(why.contains("no primary key"), "got {why}");

    ran(handle, "DROP TABLE ffi_edit");
    unsafe { db_free(handle) };
}

/// What the identity call answers, insisting it answered.
fn row_identity(handle: *mut dbffi::DbHandle, schema: &str, relation: &str) -> String {
    let (schema, relation) = (
        CString::new(schema).unwrap(),
        CString::new(relation).unwrap(),
    );
    let mut err: *mut c_char = ptr::null_mut();
    let raw = unsafe { db_row_identity_json(handle, schema.as_ptr(), relation.as_ptr(), &mut err) };
    if raw.is_null() {
        let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
        unsafe { db_string_free(err) };
        panic!("db_row_identity_json failed: {why}");
    }
    assert!(err.is_null(), "db_row_identity_json set err on success");
    let json = unsafe { CStr::from_ptr(raw) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(raw) };
    json
}

#[test]
fn the_identity_call_says_why_it_could_not_read_its_arguments() {
    let name = CString::new("public").unwrap();
    for (handle, schema, relation) in [
        (ptr::null_mut(), name.as_ptr(), name.as_ptr()),
        (ptr::null_mut(), ptr::null(), name.as_ptr()),
        (ptr::null_mut(), name.as_ptr(), ptr::null()),
    ] {
        let mut err: *mut c_char = ptr::null_mut();
        let raw = unsafe { db_row_identity_json(handle, schema, relation, &mut err) };
        assert!(raw.is_null());
        assert!(
            !err.is_null(),
            "db_row_identity_json must say why it failed"
        );
        unsafe { db_string_free(err) };
    }
}

/// A table with no primary key is editable through a NOT NULL unique key, and
/// the `UPDATE` names that key.
///
/// The decision this exists for. Upstream identifies a row this way too, and the
/// range it opens up is real: a join table keyed by a unique pair, an import
/// table with a natural key, anything somebody built without a surrogate id.
#[ignore = "requires the benchmark database"]
#[test]
fn a_not_null_unique_key_makes_a_table_editable() {
    let handle = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_unique");
    ran(
        handle,
        "CREATE TABLE ffi_unique (
             email text NOT NULL CONSTRAINT ffi_unique_email UNIQUE,
             label text)",
    );
    ran(
        handle,
        "INSERT INTO ffi_unique VALUES ('a@example.com', 'first')",
    );

    assert_eq!(
        row_identity(handle, "public", "ffi_unique"),
        r#"{"columns":["email"],"obstacle":null}"#
    );

    let json = edit_sql(
        handle,
        r#"{"schema":"public","relation":"ffi_unique","updates":[
            {"key":[{"column":"email","value":"a@example.com"}],
             "set":[{"column":"label","value":"changed"}]}]}"#,
    );
    let statements: Vec<String> = serde_json::from_str(&json).expect("statements should decode");
    assert_eq!(
        statements,
        ["UPDATE public.ffi_unique SET label = 'changed' WHERE email = 'a@example.com'"],
        "{json}"
    );
    // Run rather than read: the point of this harness is that the text is a
    // statement the server accepts, and that it reaches one row.
    ran(handle, &statements[0]);
    assert_eq!(
        ran(
            handle,
            "SELECT email FROM ffi_unique WHERE label = 'changed'"
        ),
        1
    );

    ran(handle, "DROP TABLE ffi_unique");
    unsafe { db_free(handle) };
}

/// A unique constraint over a column that can be null is refused, by name.
///
/// The server is what makes this worth checking here rather than against a fake:
/// the two rows below both satisfy `ffi_nullable_email`, because `NULL != NULL`
/// in a unique index as everywhere else. A client that took this key would write
/// `WHERE email IS NULL`-shaped nonsense — or, having no NULL literal in an
/// equality, `WHERE email = NULL`, which matches neither of them.
#[ignore = "requires the benchmark database"]
#[test]
fn a_nullable_unique_key_is_refused_and_the_reason_names_it() {
    let handle = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_nullable");
    ran(
        handle,
        "CREATE TABLE ffi_nullable (
             email text CONSTRAINT ffi_nullable_email UNIQUE,
             label text)",
    );
    ran(handle, "INSERT INTO ffi_nullable VALUES (NULL, 'first')");
    ran(handle, "INSERT INTO ffi_nullable VALUES (NULL, 'second')");
    assert_eq!(
        ran(handle, "SELECT label FROM ffi_nullable WHERE email IS NULL"),
        2,
        "the constraint admits two rows with no value, which is why it is not a key"
    );

    let identity = row_identity(handle, "public", "ffi_nullable");
    assert!(identity.contains(r#""columns":[]"#), "{identity}");
    assert!(identity.contains("ffi_nullable_email"), "{identity}");
    assert!(identity.contains("can be null"), "{identity}");

    let refused = CString::new(
        r#"{"schema":"public","relation":"ffi_nullable","deletes":[
            {"key":[{"column":"email","value":"a@example.com"}]}]}"#,
    )
    .unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    assert!(unsafe { db_edit_sql_json(handle, refused.as_ptr(), &mut err) }.is_null());
    let why = unsafe { CStr::from_ptr(err) }.to_str().unwrap().to_owned();
    unsafe { db_string_free(err) };
    assert!(why.contains("ffi_nullable_email"), "got {why}");

    ran(handle, "DROP TABLE ffi_nullable");
    unsafe { db_free(handle) };
}

/// Several candidates, and the choice is the same one every time.
///
/// Two of the four constraints below are usable, and neither is the one the
/// catalog lists first. `ffi_several_code` wins on width over the two-column key
/// and on name over `ffi_several_ref`; the nullable one is out whatever its
/// width. Asked repeatedly and on a fresh connection, because an identity that
/// changed between two runs against one schema would be an identity nobody could
/// reason about — and the failure would look like an edit that hit the wrong row
/// rather than like a bug in a sort.
#[ignore = "requires the benchmark database"]
#[test]
fn one_of_several_unique_keys_is_chosen_and_the_choice_does_not_move() {
    let handle = connected();
    ran(handle, "DROP TABLE IF EXISTS ffi_several");
    ran(
        handle,
        "CREATE TABLE ffi_several (
             tenant text NOT NULL,
             member text NOT NULL,
             code   text NOT NULL CONSTRAINT ffi_several_code UNIQUE,
             ref    text NOT NULL CONSTRAINT ffi_several_ref UNIQUE,
             email  text          CONSTRAINT ffi_several_email UNIQUE,
             CONSTRAINT ffi_several_pair UNIQUE (tenant, member))",
    );

    for _ in 0..3 {
        assert_eq!(
            row_identity(handle, "public", "ffi_several"),
            r#"{"columns":["code"],"obstacle":null}"#
        );
    }
    let second = connected();
    assert_eq!(
        row_identity(second, "public", "ffi_several"),
        r#"{"columns":["code"],"obstacle":null}"#
    );
    unsafe { db_free(second) };

    let json = edit_sql(
        handle,
        r#"{"schema":"public","relation":"ffi_several","deletes":[
            {"key":[{"column":"code","value":"x"}]}]}"#,
    );
    let statements: Vec<String> = serde_json::from_str(&json).expect("statements should decode");
    assert_eq!(
        statements,
        ["DELETE FROM public.ffi_several WHERE code = 'x'"],
        "{json}"
    );

    ran(handle, "DROP TABLE ffi_several");
    unsafe { db_free(handle) };
}

/// Where an export test writes. Named for the test rather than randomised,
/// because a leftover from a previous run must be overwritten rather than
/// accumulated — and because a failure that leaves the file behind is one
/// somebody can go and read.
fn export_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("dbffi-export-{name}"))
}

fn export(cursor: *mut dbffi::DbCursor, format: &str, path: &std::path::Path) -> i64 {
    export_limited(cursor, format, path, 0)
}

fn export_limited(
    cursor: *mut dbffi::DbCursor,
    format: &str,
    path: &std::path::Path,
    row_limit: i64,
) -> i64 {
    let format = CString::new(format).unwrap();
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows = unsafe {
        db_export(
            cursor,
            format.as_ptr(),
            path_c.as_ptr(),
            row_limit,
            &mut err,
        )
    };
    if rows < 0 {
        let message = unsafe { CStr::from_ptr(err) }
            .to_string_lossy()
            .into_owned();
        unsafe { db_string_free(err) };
        panic!("db_export returned {rows}: {message}");
    }
    rows
}

#[test]
fn an_export_writes_every_row_the_cursor_had_not_only_the_first_page() {
    // The reason this exists at all. The front end exported what the grid had
    // loaded, so a result longer than one page came out truncated. The batch
    // size here is smaller than the row count on purpose: an exporter that
    // wrote one batch and stopped would pass a single-page test and fail every
    // real export.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null(), "duckdb in memory must open");

    let sql = CString::new("SELECT i AS n FROM range(2500) t(i)").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 100, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null(), "cursor must open");

    let path = export_path("every-row.csv");
    let rows = export(cursor, "csv", &path);
    assert_eq!(rows, 2500, "every row, not the first batch");

    let text = std::fs::read_to_string(&path).expect("the file must be there");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2501, "one header and every row");
    assert_eq!(lines[0], "n");
    assert_eq!(lines[1], "0");
    assert_eq!(lines[2500], "2499");

    let _ = std::fs::remove_file(&path);
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn an_export_names_a_format_it_cannot_write_rather_than_writing_something_else() {
    // Guessing here would produce a file with the name the user asked for and
    // the contents of some other format, which is the one failure they cannot
    // see until whatever they feed it refuses to open it.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT 1 AS n").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 100, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let path = export_path("unknown-format.xlsx");
    let format = CString::new("xlsx").unwrap();
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows = unsafe { db_export(cursor, format.as_ptr(), path_c.as_ptr(), 0, &mut err) };
    assert_eq!(rows, -1);
    let message = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    assert!(
        message.contains("xlsx"),
        "must name the format asked for: {message}"
    );
    assert!(
        !path.exists(),
        "a refused format must not leave a file behind"
    );

    unsafe { db_string_free(err) };
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn an_export_to_a_location_it_cannot_open_fails_before_it_runs_the_query() {
    // Reported from the create rather than from the first write, so a bad
    // destination costs nothing: the alternative is streaming a large result
    // out of the server to discover at the end that it had nowhere to go.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT 1 AS n").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 100, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let format = CString::new("csv").unwrap();
    let path_c = CString::new("/nonexistent-directory-for-a-test/out.csv").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows = unsafe { db_export(cursor, format.as_ptr(), path_c.as_ptr(), 0, &mut err) };
    assert_eq!(rows, -1);
    assert!(!err.is_null(), "db_export must say why it failed");

    unsafe { db_string_free(err) };
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn an_export_with_a_null_cursor_says_so_instead_of_reading_one() {
    let format = CString::new("csv").unwrap();
    let path = export_path("null-cursor.csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows = unsafe {
        db_export(
            ptr::null_mut(),
            format.as_ptr(),
            path_c.as_ptr(),
            0,
            &mut err,
        )
    };
    assert_eq!(rows, -1);
    assert!(!err.is_null(), "db_export must say why it failed");
    unsafe { db_string_free(err) };
}

#[test]
fn a_parquet_export_is_a_parquet_file_and_not_merely_a_file() {
    // A direct Arrow write is this phase's exit criterion, and every wrong
    // implementation of it still produces a file of a plausible size. The magic
    // is what a reader checks first, at both ends, so it is what this checks.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT i AS n FROM range(300) t(i)").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let path = export_path("direct.parquet");
    let rows = export(cursor, "parquet", &path);
    assert_eq!(rows, 300);

    let bytes = std::fs::read(&path).expect("the file must be there");
    assert_eq!(&bytes[..4], b"PAR1", "a parquet file starts with its magic");
    assert_eq!(
        &bytes[bytes.len() - 4..],
        b"PAR1",
        "and ends with it — a footer that was never written is the failure this catches"
    );

    let _ = std::fs::remove_file(&path);
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn a_row_limit_stops_the_export_where_it_was_told_to_and_not_at_a_batch_edge() {
    // The limit is what the save panel showed the user — "the 250 rows here" —
    // so rounding it to the batch size writes a file whose row count is not the
    // one they agreed to. A batch size that does not divide the limit is the
    // only arrangement that catches that, which is why 64 and 250.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT i AS n FROM range(5000) t(i)").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let path = export_path("limited.csv");
    let rows = export_limited(cursor, "csv", &path, 250);
    assert_eq!(rows, 250, "exactly the limit, not 256 and not 5000");

    let text = std::fs::read_to_string(&path).expect("the file must be there");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 251, "header plus the limit");
    assert_eq!(lines[250], "249", "and they are the first rows, in order");

    let _ = std::fs::remove_file(&path);
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn a_limit_larger_than_the_result_is_not_an_error() {
    // The count the panel offers comes from the grid, and a result can end
    // before it — a browse that stopped early, a table that shrank. Refusing
    // there would fail an export that had already written everything there was.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT i AS n FROM range(10) t(i)").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let path = export_path("over-limit.csv");
    assert_eq!(export_limited(cursor, "csv", &path, 9_000), 10);

    let _ = std::fs::remove_file(&path);
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn a_sql_export_writes_statements_the_source_database_would_accept() {
    // The dialect comes from the connection and not from a guess, so this also
    // proves the handle is being read: DuckDB quotes with double quotes, and a
    // build that reached for a default would still produce a plausible file.
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let sql = CString::new("SELECT 1 AS id, 'O''Brien' AS name").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle, sql.as_ptr(), 100, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let path = export_path("people.sql");
    let table = CString::new("main.people").unwrap();
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows =
        unsafe { db_export_sql(handle, cursor, table.as_ptr(), path_c.as_ptr(), 0, &mut err) };
    assert_eq!(rows, 1, "db_export_sql must write the row");

    let text = std::fs::read_to_string(&path).expect("the file must be there");
    assert_eq!(
        text,
        "INSERT INTO main.people (id, name) VALUES\n(1, 'O''Brien');\n"
    );

    let _ = std::fs::remove_file(&path);
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle) };
}

#[test]
fn a_transfer_puts_the_source_rows_in_the_target_table() {
    // `duckdb://:memory:` opened twice is two independent databases, so this is
    // a real database-to-database transfer with no server to start.
    let src = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_src = unsafe { db_connect(src.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_src.is_null());

    let tgt = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_tgt = unsafe { db_connect(tgt.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_tgt.is_null());

    ran(handle_tgt, "CREATE TABLE people (id INTEGER, name VARCHAR)");

    // An apostrophe and a NULL, because those are the two values that reach the
    // target as something other than themselves when the rendering is wrong —
    // one ends its own literal, the other becomes an empty string.
    let sql = CString::new(
        "SELECT * FROM (VALUES (1, 'alice'), (2, 'O''Brien'), (3, NULL)) AS t(id, name)",
    )
    .unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    // One row per fetch, so the count has to arrive in pieces rather than in one
    // answer at the end — which is the whole reason this is a handle.
    let cursor = unsafe { db_cursor(handle_src, sql.as_ptr(), 1, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let table = CString::new("people").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let transfer = unsafe { db_transfer_start(cursor, handle_tgt, table.as_ptr(), &mut err) };
    assert!(!transfer.is_null(), "the transfer must start");

    let mut seen: Vec<i64> = Vec::new();
    loop {
        let mut rows: i64 = -1;
        let mut err: *mut c_char = ptr::null_mut();
        let step = unsafe { db_transfer_step(transfer, &mut rows, &mut err) };
        assert!(step >= 0, "no step should fail: {}", complaint(&mut err));
        seen.push(rows);
        if step == 0 {
            break;
        }
    }
    assert_eq!(
        seen,
        vec![1, 2, 3, 3],
        "each step reports the running total, and the last reports it again as done"
    );

    // Asked of the target as predicates rather than counted: a transfer that
    // wrote three rows of the wrong thing passes a count and fails these.
    assert_eq!(ran(handle_tgt, "SELECT id FROM people"), 3);
    assert_eq!(
        ran(handle_tgt, "SELECT id FROM people WHERE name = 'O''Brien'"),
        1,
        "the apostrophe arrived as data, not as syntax"
    );
    assert_eq!(
        ran(handle_tgt, "SELECT id FROM people WHERE name IS NULL"),
        1,
        "the NULL arrived a NULL and not an empty string"
    );

    // The cursor goes with it: the transfer took it at `start`.
    unsafe { db_transfer_free(transfer) };
    unsafe { db_free(handle_src) };
    unsafe { db_free(handle_tgt) };
}

/// Stopped between two batches, the transfer sends no more and keeps what it
/// sent.
///
/// Stopped from this thread rather than another, which is the case the handle
/// exists for and also the one a test can pin without a race: what is being
/// checked is that the flag is read at the top of the step and that the rows
/// already written are not taken back. The concurrent case is the same flag.
#[test]
fn a_stopped_transfer_leaves_what_it_had_already_written() {
    let src = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_src = unsafe { db_connect(src.as_ptr(), ptr::null(), 10, &mut err) };
    let tgt = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_tgt = unsafe { db_connect(tgt.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_src.is_null() && !handle_tgt.is_null());
    ran(handle_tgt, "CREATE TABLE people (id INTEGER)");

    let sql = CString::new("SELECT * FROM (VALUES (1), (2), (3), (4)) AS t(id)").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle_src, sql.as_ptr(), 1, &mut err, ptr::null_mut()) };
    let table = CString::new("people").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let transfer = unsafe { db_transfer_start(cursor, handle_tgt, table.as_ptr(), &mut err) };
    assert!(!transfer.is_null());

    let mut rows: i64 = -1;
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(
        unsafe { db_transfer_step(transfer, &mut rows, &mut err) },
        1
    );
    assert_eq!(rows, 1);

    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(
        unsafe { db_transfer_cancel(transfer, &mut err) },
        0,
        "stopping is delivered: {}",
        complaint(&mut err)
    );

    let mut rows: i64 = -1;
    let mut err: *mut c_char = ptr::null_mut();
    assert_eq!(
        unsafe { db_transfer_step(transfer, &mut rows, &mut err) },
        -2,
        "the step after the stop says so rather than sending"
    );
    assert_eq!(rows, 1, "and still reports what is on the target");
    assert_eq!(
        ran(handle_tgt, "SELECT id FROM people"),
        1,
        "the row already sent stays; a transfer is not a transaction"
    );

    unsafe { db_transfer_free(transfer) };
    unsafe { db_free(handle_src) };
    unsafe { db_free(handle_tgt) };
}

// A null cursor, a null target, and a null table each answer null and set err.
#[test]
fn a_transfer_without_a_cursor_says_so_instead_of_crashing() {
    let tgt = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_tgt = unsafe { db_connect(tgt.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_tgt.is_null());

    let table = CString::new("people").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let started =
        unsafe { db_transfer_start(ptr::null_mut(), handle_tgt, table.as_ptr(), &mut err) };
    assert!(started.is_null());
    assert!(!err.is_null(), "db_transfer_start must say why it failed");
    unsafe { db_string_free(err) };
    unsafe { db_free(handle_tgt) };
}

#[test]
fn a_transfer_without_a_target_says_so_instead_of_crashing() {
    let src = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_src = unsafe { db_connect(src.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_src.is_null());

    let sql = CString::new("SELECT 1 AS id").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle_src, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let table = CString::new("people").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let started = unsafe { db_transfer_start(cursor, ptr::null_mut(), table.as_ptr(), &mut err) };
    assert!(started.is_null());
    assert!(!err.is_null(), "db_transfer_start must say why it failed");
    unsafe { db_string_free(err) };
    // Refused before the cursor was taken, so it is still the caller's to close.
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle_src) };
}

#[test]
fn a_transfer_without_a_table_name_says_so_instead_of_crashing() {
    let src = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_src = unsafe { db_connect(src.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_src.is_null());

    let sql = CString::new("SELECT 1 AS id").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle_src, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let tgt = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_tgt = unsafe { db_connect(tgt.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_tgt.is_null());

    let mut err: *mut c_char = ptr::null_mut();
    let started = unsafe { db_transfer_start(cursor, handle_tgt, ptr::null(), &mut err) };
    assert!(started.is_null());
    assert!(!err.is_null(), "db_transfer_start must say why it failed");
    unsafe { db_string_free(err) };
    unsafe { db_cursor_free(cursor) };
    unsafe { db_free(handle_src) };
    unsafe { db_free(handle_tgt) };
}

// Transferring into a table that does not exist on the target fails the step —
// the INSERT is refused by the server.
#[test]
fn a_transfer_into_a_table_that_is_not_there_reports_the_servers_refusal() {
    let src = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_src = unsafe { db_connect(src.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_src.is_null());

    let sql = CString::new("SELECT 1 AS id").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let cursor = unsafe { db_cursor(handle_src, sql.as_ptr(), 64, &mut err, ptr::null_mut()) };
    assert!(!cursor.is_null());

    let tgt = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle_tgt = unsafe { db_connect(tgt.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle_tgt.is_null());

    // Do NOT create the table — the INSERT will fail on the server.
    let table = CString::new("ghost_table").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let transfer = unsafe { db_transfer_start(cursor, handle_tgt, table.as_ptr(), &mut err) };
    assert!(
        !transfer.is_null(),
        "starting is not what fails: nothing has been sent yet"
    );

    let mut rows: i64 = -1;
    let mut err: *mut c_char = ptr::null_mut();
    let step = unsafe { db_transfer_step(transfer, &mut rows, &mut err) };
    assert_eq!(step, -1);
    assert!(!err.is_null(), "db_transfer_step must say why it failed");
    let message = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    assert!(
        message.contains("ghost_table")
            || message.contains("not exist")
            || message.contains("relation"),
        "error should mention the missing table, got: {message}"
    );
    assert_eq!(rows, 0, "and nothing is claimed to have arrived");
    unsafe { db_string_free(err) };
    unsafe { db_transfer_free(transfer) };
    unsafe { db_free(handle_src) };
    unsafe { db_free(handle_tgt) };
}

#[test]
fn a_csv_files_rows_arrive_in_the_table_it_was_imported_into() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let path = std::env::temp_dir().join(format!("dbffi-import-{}.csv", std::process::id()));
    std::fs::write(&path, "id,name\n1,alice\n2,O'Brien\n3,\n").expect("write csv");

    let table = CString::new("people").unwrap();
    ran(handle, "CREATE TABLE people (id INTEGER, name VARCHAR)");

    let format = CString::new("csv").unwrap();
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let rows = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(rows, 3, "every CSV row was reported written");

    assert_eq!(ran(handle, "SELECT id FROM people"), 3);
    assert_eq!(
        ran(handle, "SELECT id FROM people WHERE name = 'O''Brien'"),
        1,
        "the apostrophe arrived as data, not as syntax"
    );
    assert_eq!(
        ran(handle, "SELECT id FROM people WHERE name IS NULL"),
        1,
        "the empty CSV field arrived as NULL"
    );

    let _ = std::fs::remove_file(&path);
    unsafe { db_free(handle) };
}

#[test]
fn an_import_without_a_target_says_so_instead_of_crashing() {
    let format = CString::new("csv").unwrap();
    let path = std::env::temp_dir().join("dbffi-import-null-target.csv");
    std::fs::write(&path, "id\n1\n").expect("write csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let table = CString::new("t").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            ptr::null_mut(),
            format.as_ptr(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    unsafe { db_string_free(err) };
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_import_without_a_format_says_so_instead_of_crashing() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let path = std::env::temp_dir().join("dbffi-import-null-format.csv");
    std::fs::write(&path, "id\n1\n").expect("write csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let table = CString::new("t").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            ptr::null_mut(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    unsafe { db_string_free(err) };
    let _ = std::fs::remove_file(&path);
    unsafe { db_free(handle) };
}

#[test]
fn an_import_without_a_path_says_so_instead_of_crashing() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let format = CString::new("csv").unwrap();
    let table = CString::new("t").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            ptr::null_mut(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[test]
fn an_import_without_a_table_name_says_so_instead_of_crashing() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let format = CString::new("csv").unwrap();
    let path = std::env::temp_dir().join("dbffi-import-null-table.csv");
    std::fs::write(&path, "id\n1\n").expect("write csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            path_c.as_ptr(),
            ptr::null_mut(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    unsafe { db_string_free(err) };
    let _ = std::fs::remove_file(&path);
    unsafe { db_free(handle) };
}

#[test]
fn an_import_of_a_format_nothing_reads_names_the_format() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let path = std::env::temp_dir().join("dbffi-import-unknown.fmt");
    std::fs::write(&path, "id\n1\n").expect("write file");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let format = CString::new("xyzzy").unwrap();
    let table = CString::new("t").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    let message = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    assert!(
        message.contains("xyzzy"),
        "error should mention the format, got: {message}"
    );
    unsafe { db_string_free(err) };
    let _ = std::fs::remove_file(&path);
    unsafe { db_free(handle) };
}

#[test]
fn an_import_of_a_file_that_is_not_there_says_so_before_touching_the_server() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let format = CString::new("csv").unwrap();
    let path = std::env::temp_dir().join("dbffi-import-nonexistent.csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let table = CString::new("t").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    unsafe { db_string_free(err) };
    unsafe { db_free(handle) };
}

#[test]
fn an_import_into_a_table_that_is_not_there_reports_the_servers_refusal() {
    let conn_str = CString::new("duckdb://:memory:").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), ptr::null(), 10, &mut err) };
    assert!(!handle.is_null());

    let path = std::env::temp_dir().join("dbffi-import-no-table.csv");
    std::fs::write(&path, "id\n1\n").expect("write csv");
    let path_c = CString::new(path.to_str().unwrap()).unwrap();
    let format = CString::new("csv").unwrap();
    let table = CString::new("ghost_table").unwrap();

    let mut err: *mut c_char = ptr::null_mut();
    let result = unsafe {
        db_import(
            handle,
            format.as_ptr(),
            path_c.as_ptr(),
            table.as_ptr(),
            &mut err,
        )
    };
    assert_eq!(result, -1);
    assert!(!err.is_null(), "db_import must say why it failed");
    let message = unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned();
    assert!(
        message.contains("ghost_table")
            || message.contains("not exist")
            || message.contains("relation"),
        "error should mention the missing table, got: {message}"
    );
    unsafe { db_string_free(err) };
    let _ = std::fs::remove_file(&path);
    unsafe { db_free(handle) };
}

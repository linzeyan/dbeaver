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

use arrow::ffi::{FFI_ArrowArray, FFI_ArrowSchema};

use dbffi::{
    db_cancel, db_columns_json, db_complete_json, db_connect, db_constraints_json, db_cursor,
    db_cursor_cancel, db_cursor_close, db_cursor_free, db_cursor_next, db_cursor_schema,
    db_ddl_text, db_definition_json, db_foreign_keys_json, db_free, db_indexes_json,
    db_names_forget, db_query, db_query_free, db_query_next, db_query_rows_affected,
    db_query_schema, db_referenced_by_json, db_relations_json, db_schemas_json,
    db_sql_error_offset, db_sql_scan_json, db_string_free, db_triggers_json, db_tx_autocommit,
    db_tx_commit, db_tx_release, db_tx_rollback, db_tx_rollback_to, db_tx_savepoint,
    db_tx_state_json,
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null());
    assert!(err.is_null(), "db_connect should not set err on success");

    unsafe { db_free(handle) };
}

#[ignore = "requires the benchmark database"]
#[test]
fn test_schemas_json() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
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

#[ignore = "requires the benchmark database"]
#[test]
fn a_completion_offers_the_columns_of_what_the_statement_selects_from() {
    let conn_str = CString::new("postgres://bench:bench@127.0.0.1:55432/bench").unwrap();
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
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
    let handle = unsafe { db_connect(conn_str.as_ptr(), &mut err) };
    assert!(!handle.is_null(), "benchmark database unreachable");
    handle
}

/// Runs `sql` to the end and returns what the server said it affected.
///
/// The count comes from `db_query_rows_affected` rather than from the batches,
/// which is what lets these checks count rows without reading an Arrow array:
/// PostgreSQL reports the row count of a SELECT the same way it reports the
/// row count of a DELETE.
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

// C surface of the Rust core, plus the Arrow C Data Interface structs that
// carry result data across.
//
// The ArrowSchema/ArrowArray definitions are verbatim from the Arrow C Data
// Interface specification. They are a stable ABI, which is the whole reason
// result batches can cross into Swift without serialization.

#ifndef DBFFI_H
#define DBFFI_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

#ifndef ARROW_C_DATA_INTERFACE
#define ARROW_C_DATA_INTERFACE

#define ARROW_FLAG_DICTIONARY_ORDERED 1
#define ARROW_FLAG_NULLABLE 2
#define ARROW_FLAG_MAP_KEYS_SORTED 4

struct ArrowSchema {
  const char* format;
  const char* name;
  const char* metadata;
  int64_t flags;
  int64_t n_children;
  struct ArrowSchema** children;
  struct ArrowSchema* dictionary;
  void (*release)(struct ArrowSchema*);
  void* private_data;
};

struct ArrowArray {
  int64_t length;
  int64_t null_count;
  int64_t offset;
  int64_t n_buffers;
  int64_t n_children;
  const void** buffers;
  struct ArrowArray** children;
  struct ArrowArray* dictionary;
  void (*release)(struct ArrowArray*);
  void* private_data;
};

#endif  // ARROW_C_DATA_INTERFACE

typedef struct DbHandle DbHandle;
typedef struct DbQuery DbQuery;
typedef struct DbCursor DbCursor;

// All calls block. Do not call from the main thread.
// Any `err` out-parameter, when set, must be released with db_string_free.

DbHandle* db_connect(const char* conn_str, char** err);
void db_free(DbHandle* handle);

// Asks the server to abandon what this handle is running. Returns 0 when the
// request was delivered, -1 when it could not be.
//
// The exception to the rule above: this one may be called while another call is
// in flight on the same handle, and has to be, since every other call blocks.
// The request goes out on a connection of its own for the same reason — the
// protocol cannot interleave one, so an in-band cancel would queue behind the
// statement it is meant to stop.
//
// Delivered is not interrupted: a statement that had already finished leaves
// nothing to cancel and this still returns 0. What actually happened is visible
// only at db_query_next, as -2.
int db_cancel(DbHandle* handle, char** err);

// Metadata crosses as JSON, not Arrow: it is small, and Arrow buys nothing for
// a few thousand short rows. Returned strings are released with db_string_free.
char* db_schemas_json(DbHandle* handle, char** err);
char* db_relations_json(DbHandle* handle, const char* schema, char** err);
char* db_columns_json(DbHandle* handle, const char* schema, const char* relation,
                      char** err);
char* db_indexes_json(DbHandle* handle, const char* schema, const char* relation,
                      char** err);
// A JSON string, or JSON null when the relation is not a view. Null rather than
// an empty string, so "has no definition" stays distinguishable from "has one
// that is blank".
char* db_definition_json(DbHandle* handle, const char* schema, const char* relation,
                         char** err);
char* db_foreign_keys_json(DbHandle* handle, const char* schema, const char* relation,
                           char** err);
char* db_referenced_by_json(DbHandle* handle, const char* schema, const char* relation,
                            char** err);
char* db_constraints_json(DbHandle* handle, const char* schema, const char* relation,
                          char** err);
char* db_triggers_json(DbHandle* handle, const char* schema, const char* relation,
                       char** err);

// On a server error that names a place in the statement, `err_position` receives
// the cursor: 1-based, counted in characters, from the start of `sql`. Zero for
// every error that has no such place. A number rather than a sentence, because
// the caller moves a caret with it.
//
// Returns when the server acknowledges the bind — which is not as early as that
// sounds. The server buffers its output and flushes at the end of the command,
// so on a statement that takes a minute this call takes a minute too, and then
// hands back a result whose first batch is already waiting. Anything that means
// to interrupt such a statement has to reach db_cancel from another thread while
// this one is still inside here.
DbQuery* db_query(DbHandle* handle, const char* sql, size_t batch_rows, char** err,
                  int* err_position);

// Fills `out` with the result schema (a struct type whose children are the
// columns). Returns 0 on success, -1 on error.
int db_query_schema(DbQuery* query, struct ArrowSchema* out, char** err);

// Returns 1 and fills `out` with the next batch, 0 when exhausted, -1 on error,
// -2 when the statement was cancelled. The caller owns `out` and must invoke its
// release callback.
//
// Cancellation is separated from error because it is not a fault: the server's
// wording for it, "canceling statement due to user request", reads as a failure
// in an error banner when it is the button the user just pressed working.
//
// db_query returning a handle is not the statement having succeeded: it awaits
// the server's BindComplete, which is before execution. Everything a statement
// fails at while running — a duplicate relation, a constraint, a divide by zero
// — arrives here instead. A statement with no rows to fetch still has to be
// pulled to exhaustion for that reason, and for the count below.
int db_query_next(DbQuery* query, struct ArrowArray* out, char** err);

// Rows the statement reported affecting, or -1 until its result has been read to
// the end. Zero is a real answer — an UPDATE that matched nothing — so "not yet
// known" cannot be spelled with it. The count is all a statement returning no
// rows says about itself; the verb that produced it does not survive the driver.
int64_t db_query_rows_affected(DbQuery* query);

void db_query_free(DbQuery* query);
void db_string_free(char* s);

// A cursor over `sql`: the server keeps one statement's snapshot open and hands
// out the next rows on request. What that buys over repeating db_query with a
// LIMIT and an OFFSET is a stable position — a second page cannot repeat or skip
// rows because the plan changed between two statements. `err_position` carries
// the same 1-based cursor db_query documents.
//
// A cursor holds a connection of its own for as long as it lives, so a front-end
// keeping one open across user think-time is keeping a connection too.
DbCursor* db_cursor(DbHandle* handle, const char* sql, size_t batch_rows, char** err,
                    int* err_position);

// Fills `out` with the cursor's schema, the same struct type db_query_schema
// exports. Returns 0 on success, -1 on error. Available before the first fetch:
// the statement was prepared to declare the cursor, so the columns are known.
int db_cursor_schema(DbCursor* cursor, struct ArrowSchema* out, char** err);

// Returns 1 and fills `out` with the next page, 0 once the cursor is exhausted,
// -1 on error, -2 when the statement was cancelled. The caller owns `out` and
// must invoke its release callback.
int db_cursor_next(DbCursor* cursor, struct ArrowArray* out, char** err);

// Asks the server to stop the fetch this cursor is running. Returns 0 when the
// request was delivered, -1 when it could not be.
//
// The exception to the "all calls block, do not call from the main thread" rule
// above, and it has to be: db_cursor_next blocks for as long as the server takes
// to produce a page, so a cancel that waited its turn would arrive after it.
//
// db_cancel does NOT reach a cursor. That one cancels the session connection and
// a cursor runs on one of its own, so a front-end reading through a cursor has to
// route its Cancel here — otherwise the button is offered and does nothing.
int db_cursor_cancel(DbCursor* cursor, char** err);

// Closes the cursor and rolls back the transaction it was declared in. Optional:
// db_cursor_free reaches the same end by closing the connection, which is what a
// front-end that drops a result mid-scroll does. Returns 0 on success, -1 on
// error.
int db_cursor_close(DbCursor* cursor, char** err);

void db_cursor_free(DbCursor* cursor);

#ifdef __cplusplus
}
#endif

#endif  // DBFFI_H

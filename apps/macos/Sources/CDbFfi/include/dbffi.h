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
// A handle is several connections: statements run on the session and metadata
// reads on a pooled one, so all of them are named rather than the caller being
// asked which is busy. Not cursors — those are handed out to be held, and carry
// db_cursor_cancel instead.
//
// Delivered is not interrupted: a statement that had already finished leaves
// nothing to cancel and this still returns 0. What actually happened is visible
// only at db_query_next, as -2.
int db_cancel(DbHandle* handle, char** err);

// Every database this build can open. Takes no handle, because the connection
// form asks this before there is one: it needs to know which databases to offer
// and what each of them needs asked for. Each entry has a scheme, a label, a
// shape ("server" or "file") and a default_port.
//
// Exported rather than written out again in Swift, so a driver added to the core
// appears in the form without anybody remembering to do it twice — and so the
// form cannot offer one this build does not have.
char* db_drivers_json(char** err);

// One reading of an editor buffer: what to paint, where the statements are, and
// which one a run would send. Takes no handle for the reason db_drivers_json
// does not — reading SQL needs the dialect and not the connection, and an editor
// holds text before anything is open.
//
// `scheme` is the connection's ("postgres", "mysql", "sqlite", …). One this
// build does not know is read as PostgreSQL rather than refused: a wrong guess
// costs colour, not correctness, since the statement is sent as typed either
// way.
//
// All offsets, in and out, are counted in characters from zero — the unit a
// Swift String.unicodeScalars index is, and the unit db_query's err_position is
// counted in. Bytes would be cheaper and would put every offset after an
// accented letter in the wrong place. `selection_start` and `selection_end` are
// equal for a caret, and may be given in either order.
//
// The answer is one JSON object, released with db_string_free:
//
//   {"tokens":    [kind, start, end, …],
//    "statements":[start, end, …],
//    "target":    {"start":…, "end":…, "origin":"whole"|"statement"|"selection",
//                  "index":…, "of":…}   // null when there is nothing to run
//   }
//
// Flat arrays rather than arrays of objects because this crosses on every
// keystroke, and an object per token would spend most of the payload repeating
// three field names. The tokens cover the buffer exactly once, in order, so a
// caller can find the one at an offset without scanning the text itself.
//
// `kind` is one of, and these numbers are the contract:
//
//   0 terminator   3 quoted identifier  6 number     9  whitespace
//   1 keyword      4 string             7 comment    10 other
//   2 identifier   5 dollar-quoted      8 parameter
//
// `index` and `of` count from 1 and are zero for the origins that number
// nothing, which is unambiguous because there is no zeroth statement.
//
// One call rather than three, because the three questions are asked about the
// same text at the same moment and one scan answers all of them.
char* db_sql_scan_json(const char* text, const char* scheme, uint32_t selection_start,
                       uint32_t selection_end, char** err);

// Where a server error position lands in the buffer, or -1 when the number could
// not have come from what was sent.
//
// The position is db_query's `err_position`: 1-based, in characters, counted
// from the start of the SQL that was sent — which is one statement, not the
// buffer it was cut from. Applying it to the buffer instead points confidently
// at a character in the wrong statement, and looks right every time the one that
// failed happened to be the first. One past the last character is a real answer,
// being what an unexpected end of input points at; anything beyond it is not.
//
// Takes no pointers and cannot fail, so it has no `err`.
int64_t db_sql_error_offset(int position, uint32_t sent_start, uint32_t sent_end);

// What could be typed at `caret`, best first. Released with db_string_free.
//
// Takes a handle where db_sql_scan_json does not, because this is the half of
// completion that needs the catalog: what belongs at the caret — a column of
// these three relations, a table of this schema, nothing at all inside a string
// — is read from the text, and the names that answer it belong to one
// connection.
//
// `caret` is counted in characters from zero, like every other offset here, and
// may sit past the end of `text` — a front end that rounds a selection is
// answered rather than refused.
//
//   {"start":…, "end":…,          // the characters accepting an offer replaces
//    "offers":[{"label":…, "insert":…, "kind":…, "detail":…}, …]}
//
// `label` is the name as the catalog holds it and `insert` is that name written
// so this database reads it as itself — they differ exactly when the name needs
// quoting, and a front end that inserts the label instead produces SQL that
// finds nothing. The span is answered here rather than worked out in the editor
// because where a name begins is the lexer's rule: "Order Lines" is one name,
// and walking back over word characters would replace half of it.
//
// `kind` is "keyword", "schema", "relation", "column" or "local" — the last
// being a name this statement invented, a CTE or a derived table, which the
// catalog has never heard of and which cannot be browsed.
//
// The first call on a connection costs the metadata round trips it takes to
// learn the names; every one after it is answered from memory until
// db_names_forget. It blocks like everything else here, so it belongs off the
// main thread — the first one is a network call wearing a keystroke's clothes.
char* db_complete_json(DbHandle* handle, const char* text, uint32_t caret, char** err);

// Forgets the names this connection has been told, so the next db_complete_json
// asks the server again. For the refresh the user presses.
//
// Nothing expires on a timer, deliberately: a table appearing in the list at a
// moment nobody chose is worse than one that is a few minutes stale, and the
// user is the one who knows a migration just ran.
void db_names_forget(DbHandle* handle);

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

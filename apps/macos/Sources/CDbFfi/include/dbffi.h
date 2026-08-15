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

// `text` laid out again. Released with db_string_free.
//
// Takes no scheme, unlike its neighbours: the formatter treats every quoted
// region — backticks, [brackets], $tag$…$tag$ — as one opaque token whichever
// database wrote it, so there is no dialect to tell it about.
//
// Never fails on the text itself. SQL it cannot read comes back as it arrived,
// because this runs on a buffer somebody is editing, where the worst outcome is
// not an ugly result but a lost one.
char* db_sql_format(const char* text, char** err);

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

// The statements that would recreate one relation, as plain text — released with
// db_string_free like the JSON above, and unlike it in being the value itself.
// Wrapping one string in a document would make a caller decode to reach the only
// field in it.
//
// The output is DBeaver's: upstream is the specification for this, so the text
// is what its DDL tab shows for the same object, differences recorded in the
// core where they are made.
//
// What kind of relation this is gets read here rather than passed in, at the
// cost of one metadata call. A caller that passed it would be handing back
// something this side told it, and the day the two disagree the answer is a
// CREATE TABLE for a view — a statement that runs and makes the wrong object.
//
// Fails for a database whose DDL has not been written yet, and for a kind whose
// statement needs facts the metadata does not carry — a materialized view's
// WITH DATA, a partitioned table's PARTITION BY. Both say so; neither guesses,
// because a statement that looks complete and is not is worse than a refusal.
char* db_ddl_text(DbHandle* handle, const char* schema, const char* relation, char** err);

// The statement that reads one relation's rows, as plain text — released with
// db_string_free, and written rather than run, like db_edit_sql_json.
//
//   {"schema": …, "relation": …, "filter": …, "order": …, "keys": […], "limit": …}
//
// `filter` and `order` are the filter bar's two fields as the user typed them,
// in whatever language this database reads. `keys` are the columns the catalog
// calls a key, added to the ordering so that a browse looks the same twice.
// `limit` is for a caller seeding an editor; the Content tab leaves it out,
// because its bound is the cursor.
//
// Here rather than in the window, which is where it was until a window that had
// only ever met PostgreSQL wrote SELECT * FROM "bench"."orders" for every
// database it could open: MySQL reads those quotes as a string, SQL Server's
// depend on a session setting, and MongoDB has no SELECT at all.
char* db_browse_statement(DbHandle* handle, const char* what, char** err);

// The statements a grid's pending changes would take, as a JSON array of
// strings. Written here and run by the caller, through db_query like anything
// else: that is what puts an edit inside whatever transaction the connection is
// in, under the same Cancel button and with the same error positions — and what
// lets a window show somebody the statements before they run.
//
// `edits` is one relation's worth of changes:
//
//   {"schema": …, "relation": …,
//    "updates": [{"key": [{"column": …, "value": …}], "set": [{…}]}],
//    "inserts": [{"set": [{…}]}],
//    "deletes": [{"key": [{…}]}]}
//
// A `value` of JSON null is SQL's NULL and a value of "" is an empty string. A
// grid has to be able to say both, and one string cannot.
//
// Values cross as text and reach the server as literals rather than as bound
// parameters, which is why a row is named by a declared key and nothing else: a
// key of that shape survives the round trip through text exactly. A relation
// with nothing to name a row by is refused, as are a partial key and text that
// is not the number its column says it is. Refusals are the point — the failure
// they prevent is not an error message, it is an UPDATE that silently changes
// the wrong row.
char* db_edit_sql_json(DbHandle* handle, const char* edits, char** err);

// Which columns name one row of a relation. Released with db_string_free:
//
//   {"columns": ["id"], "obstacle": null}
//   {"columns": [], "obstacle": "app.audit has no primary key or unique key, …"}
//
// The primary key where there is one; otherwise the narrowest UNIQUE constraint
// whose columns are all NOT NULL, and among equals the one whose name sorts
// first — an identity that changed between two runs against one schema would be
// an identity nobody could reason about.
//
// A UNIQUE constraint over a nullable column is refused by name: NULL != NULL,
// so a WHERE over it matches no row where the value is NULL and several where
// the constraint let several through.
//
// Asked here rather than worked out from db_columns_json, for the reason
// db_browse_statement exists: the rule has one home, and a window that had a
// copy of it would disagree with the core the day either was corrected.
//
// An empty `columns` is an ordinary answer and does not set err — it means the
// relation cannot be edited, and `obstacle` is the sentence to show. err is set
// only when the catalog could not be read.
char* db_row_identity_json(DbHandle* handle, const char* schema, const char* relation,
                           char** err);

// What this connection's transaction is doing. Released with db_string_free:
//
//   {"transactional":…, "autocommit":…, "open":…, "savepoints":[…]}
//
// `transactional` is the driver's answer and decides whether the rest is worth
// showing: a connection that cannot hold a transaction open is not in autocommit
// mode, it has no mode. Today that is PostgreSQL and the databases reached
// through its driver; the others run each statement on a connection from a pool,
// where a BEGIN would open a transaction the next statement never joins.
//
// `open` is what this side sent, not what the server was asked. PostgreSQL
// reports its transaction status in every ReadyForQuery and the client library
// keeps that to itself, so the core remembers instead — which means a BEGIN
// typed into the editor and run as an ordinary statement is a transaction the
// core does not know about, and db_tx_commit will refuse to end it.
//
// Pull this after anything that could have changed it. The window redraws at
// those moments anyway, and a push would be a second thing to keep in step.
char* db_tx_state_json(DbHandle* handle, char** err);

// Turns autocommit on (non-zero) or off (0). Returns 0 on success, -1 on
// failure.
//
// Sends nothing by itself: the mode decides what happens to the next statement,
// and there is nothing to tell the server until there is one. Refused while a
// transaction is open — the work in it is either wanted or not, and only the
// person who ran it knows which, so the window asks and then commits or rolls
// back. Refused too on a connection that cannot hold a transaction.
int db_tx_autocommit(DbHandle* handle, int on, char** err);

// Ends the open transaction, keeping (commit) or undoing (rollback) what it did.
// Returns 0 on success, -1 on failure — including when nothing was open, which
// is the window and the connection disagreeing rather than a harmless no-op.
int db_tx_commit(DbHandle* handle, char** err);
int db_tx_rollback(DbHandle* handle, char** err);

// Savepoints, within the open transaction. Return 0 on success, -1 on failure.
//
// db_tx_savepoint marks a point to come back to; db_tx_rollback_to undoes what
// happened after it and leaves the transaction open, which is the difference
// between a savepoint and a rollback; db_tx_release forgets the mark and the
// ones inside it, keeping the work.
//
// A name is a letter followed by letters, digits or underscores, and anything
// else is refused: the name reaches the server written into the statement as an
// identifier, where there is no placeholder to bind it to.
int db_tx_savepoint(DbHandle* handle, const char* name, char** err);
int db_tx_rollback_to(DbHandle* handle, const char* name, char** err);
int db_tx_release(DbHandle* handle, const char* name, char** err);

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

// Drains the cursor into the file at `path`, written as `format` — one of
// "csv", "tsv", "jsonl" or "parquet". Returns the rows written, -1 on error,
// -2 when the statement was cancelled.
//
// Batches are written and dropped as they arrive, so the result's size bounds
// the file and not the memory. Exporting from the front end instead means
// exporting only what it has already loaded.
//
// Stopping one goes through db_cursor_cancel, like any other fetch. A failure
// part way through removes the file: it was truncated on open, so there is no
// earlier version to keep, and a half-written result opens like a whole one.
//
// `row_limit` of 0 writes every row. A limit is how "only the rows already on
// screen" is offered without a second writer elsewhere to keep saying the same
// thing as this one — it is this call, stopping early.
int64_t db_export(DbCursor* cursor, const char* format, const char* path, int64_t row_limit,
                  char** err);

// Drains the cursor into the file at `path` as INSERT statements for `table`.
//
// Its own entry point rather than a fifth Format for db_export, because INSERT
// needs a table to name and a dialect to spell it in — four of the other five
// formats would get two arguments that mean nothing to them.
//
// Returns the rows written, -1 on error, -2 when the statement was cancelled.
// Fails if this build has no dialect for the database.
//
// Batches are written and dropped as they arrive, so the result's size bounds
// the file and not the memory. A failure part way through removes the file.
int64_t db_export_sql(DbHandle* handle, DbCursor* cursor, const char* table, const char* path,
                      int64_t row_limit, char** err);

#ifdef __cplusplus
}
#endif

#endif  // DBFFI_H

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

// All calls block. Do not call from the main thread.
// Any `err` out-parameter, when set, must be released with db_string_free.

DbHandle* db_connect(const char* conn_str, char** err);
void db_free(DbHandle* handle);

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

DbQuery* db_query(DbHandle* handle, const char* sql, size_t batch_rows, char** err);

// Fills `out` with the result schema (a struct type whose children are the
// columns). Returns 0 on success, -1 on error.
int db_query_schema(DbQuery* query, struct ArrowSchema* out, char** err);

// Returns 1 and fills `out` with the next batch, 0 when exhausted, -1 on error.
// The caller owns `out` and must invoke its release callback.
int db_query_next(DbQuery* query, struct ArrowArray* out, char** err);

void db_query_free(DbQuery* query);
void db_string_free(char* s);

#ifdef __cplusplus
}
#endif

#endif  // DBFFI_H

// What a front end that is not Swift finds when it uses this core.
//
// `dbffi.h` is written by hand, and nothing until now compared it against the
// library it describes. The Rust conformance harness calls the functions from
// Rust, so it never reads the header at all; the macOS app reads it through a
// modulemap, but only for the part Swift happens to use, and Swift's importer
// would forgive a struct whose fields drifted as long as the names still lined
// up. A C++ compiler is stricter and less forgiving in the useful direction:
// every field offset here is decided at compile time from this header, and a
// wrong one is not a diagnostic, it is a value read out of the wrong eight
// bytes. So this reads real data back and checks it against what was asked for.
//
// It also links. That was the original question and it still gets asked first:
// the staticlib has to come in, its Rust runtime has to start, and on Windows
// the C++ that DuckDB is written in has to find a runtime of its own.
//
// DuckDB in memory rather than a server, because a check that needs a container
// is a check that does not run.

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <string>

#include "dbffi.h"

namespace {

int failures = 0;

void check(bool ok, const std::string& what) {
    std::printf("%s  %s\n", ok ? "ok  " : "FAIL", what.c_str());
    if (!ok) {
        failures += 1;
    }
}

// Prints and releases whatever the core put in `err`, so a failure says what the
// core said rather than only where it happened.
bool failed(const char* what, char* err) {
    std::printf("FAIL  %s: %s\n", what, err ? err : "(no message)");
    if (err != nullptr) {
        db_string_free(err);
    }
    failures += 1;
    return false;
}

// The bit for row `i` of an Arrow validity bitmap, which is little-endian by
// bit as well as by byte. `buffers[0]` may be null, and that is not "no rows are
// valid" — it is the encoding for "every row is".
bool is_valid(const ArrowArray& array, int64_t i) {
    const auto* bitmap = static_cast<const uint8_t*>(array.buffers[0]);
    if (bitmap == nullptr) {
        return true;
    }
    const int64_t at = array.offset + i;
    return (bitmap[at / 8] & (1u << (at % 8))) != 0;
}

int64_t int64_at(const ArrowArray& array, int64_t i) {
    const auto* values = static_cast<const int64_t*>(array.buffers[1]);
    return values[array.offset + i];
}

// Arrow's `utf8` layout: offsets in `buffers[1]`, the bytes themselves packed
// end to end in `buffers[2]` with no terminators, so the length has to come from
// the offset pair rather than from strlen.
std::string string_at(const ArrowArray& array, int64_t i) {
    const auto* offsets = static_cast<const int32_t*>(array.buffers[1]);
    const auto* data = static_cast<const char*>(array.buffers[2]);
    const int32_t from = offsets[array.offset + i];
    const int32_t to = offsets[array.offset + i + 1];
    return std::string(data + from, static_cast<size_t>(to - from));
}

// The catalog this build was compiled with. No server, no handle, no file — so
// reaching it means the whole staticlib linked and the Rust runtime came up,
// rather than one leaf function happening to be reachable.
bool drivers_are_listed() {
    char* err = nullptr;
    char* drivers = db_drivers_json(&err);
    if (drivers == nullptr) {
        return failed("db_drivers_json", err);
    }
    const bool looks_right =
        drivers[0] == '[' && std::strstr(drivers, "duckdb") != nullptr;
    db_string_free(drivers);
    check(looks_right, "db_drivers_json returns a catalog with duckdb in it");
    return looks_right;
}

// One query, read the way a grid would read it, and checked against values the
// SQL fixes rather than against whatever came back.
//
// Three columns on purpose. `n` is fixed width, so it exercises the offset field
// and the values buffer. `label` is `utf8`, which is three buffers and an offset
// pair. `maybe` is null in the middle, which is the only one of the three that
// reads the validity bitmap — and a bitmap misread as absent still returns the
// right answer for a column with nothing missing, so without this column that
// path would be untested rather than passing.
bool a_result_arrives_with_its_values_intact() {
    char* err = nullptr;
    DbHandle* handle = db_connect("duckdb://:memory:", nullptr, 10, &err);
    if (handle == nullptr) {
        return failed("db_connect", err);
    }

    int err_position = 0;
    DbQuery* query = db_query(handle,
                              "SELECT i AS n,"
                              "       'row-' || i AS label,"
                              "       CASE WHEN i = 1 THEN NULL ELSE i * 10 END AS maybe "
                              "FROM range(3) t(i) ORDER BY i",
                              1000, &err, &err_position);
    if (query == nullptr) {
        db_free(handle);
        return failed("db_query", err);
    }

    ArrowSchema schema{};
    if (db_query_schema(query, &schema, &err) != 0) {
        db_query_free(query);
        db_free(handle);
        return failed("db_query_schema", err);
    }
    check(schema.n_children == 3, "the schema has three columns");
    if (schema.n_children == 3) {
        check(std::strcmp(schema.children[0]->name, "n") == 0, "column 0 is named n");
        check(std::strcmp(schema.children[1]->name, "label") == 0, "column 1 is named label");
        check(std::strcmp(schema.children[2]->name, "maybe") == 0, "column 2 is named maybe");
        // `u` is Arrow's format string for utf8. Checked because a header whose
        // ArrowSchema had drifted would still hand back a readable `name` while
        // `format` came from the wrong offset.
        check(std::strcmp(schema.children[1]->format, "u") == 0, "label declares itself as utf8");
    }

    ArrowArray batch{};
    const int got = db_query_next(query, &batch, &err);
    if (got != 1) {
        schema.release(&schema);
        db_query_free(query);
        db_free(handle);
        return failed("db_query_next did not produce a batch", err);
    }

    check(batch.length == 3, "the batch holds three rows");
    check(batch.n_children == 3, "the batch holds three columns");
    if (batch.length == 3 && batch.n_children == 3) {
        const ArrowArray& n = *batch.children[0];
        const ArrowArray& label = *batch.children[1];
        const ArrowArray& maybe = *batch.children[2];

        check(int64_at(n, 0) == 0 && int64_at(n, 1) == 1 && int64_at(n, 2) == 2,
              "n arrives as 0, 1, 2");
        check(string_at(label, 0) == "row-0" && string_at(label, 1) == "row-1"
                  && string_at(label, 2) == "row-2",
              "label arrives as row-0, row-1, row-2");
        check(is_valid(maybe, 0) && !is_valid(maybe, 1) && is_valid(maybe, 2),
              "maybe is null in the middle and present either side");
        check(maybe.null_count == 1, "maybe reports one null");
        if (is_valid(maybe, 0) && is_valid(maybe, 2)) {
            check(int64_at(maybe, 0) == 0 && int64_at(maybe, 2) == 20,
                  "the values around the null are 0 and 20");
        }
    }

    // The Arrow contract says a released structure marks itself by nulling its
    // own `release`, and this is the one part of the contract a caller can get
    // wrong without ever seeing a wrong value — until something releases twice.
    batch.release(&batch);
    check(batch.release == nullptr, "releasing a batch clears its release callback");
    schema.release(&schema);
    check(schema.release == nullptr, "releasing a schema clears its release callback");

    // A statement has to be pulled to exhaustion; the header says so, and it is
    // also where a fault during execution would arrive.
    ArrowArray tail{};
    const int end = db_query_next(query, &tail, &err);
    check(end == 0, "the result is exhausted after one batch");
    if (end == 1) {
        tail.release(&tail);
    } else if (end < 0 && err != nullptr) {
        db_string_free(err);
    }

    db_query_free(query);
    db_free(handle);
    return failures == 0;
}

}  // namespace

int main() {
    drivers_are_listed();
    a_result_arrives_with_its_values_intact();

    if (failures != 0) {
        std::printf("\n%d check(s) failed\n", failures);
        return 1;
    }
    std::printf("\nevery check passed\n");
    return 0;
}

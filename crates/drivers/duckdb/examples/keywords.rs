//! Prints DuckDB's reserved words, so the editor's keyword table is read out of
//! the database rather than remembered.
fn main() {
    let db = duckdb::Connection::open_in_memory().expect("in-memory database");
    let mut stmt = db
        .prepare(
            "SELECT DISTINCT lower(keyword_name) FROM duckdb_keywords() \
             WHERE keyword_category = 'reserved' ORDER BY 1",
        )
        .expect("duckdb_keywords()");
    let words: Vec<String> = stmt
        .query_map([], |r| r.get(0))
        .expect("query")
        .map(|r| r.expect("row"))
        .collect();
    println!("{}", words.join("\n"));
}

//! Prints SQLite's keywords, so the editor's table is read out of the library
//! rather than remembered.
//!
//! SQLite has no catalog view for this; it answers through the C API instead,
//! which is what `sqlite3_keyword_count` and `sqlite3_keyword_name` are for.
fn main() {
    let count = unsafe { rusqlite::ffi::sqlite3_keyword_count() };
    let mut words: Vec<String> = Vec::with_capacity(count as usize);
    for i in 0..count {
        let mut name: *const std::os::raw::c_char = std::ptr::null();
        let mut len: std::os::raw::c_int = 0;
        let rc = unsafe { rusqlite::ffi::sqlite3_keyword_name(i, &mut name, &mut len) };
        assert_eq!(rc, rusqlite::ffi::SQLITE_OK, "keyword {i}");
        let bytes = unsafe { std::slice::from_raw_parts(name as *const u8, len as usize) };
        words.push(String::from_utf8_lossy(bytes).to_lowercase());
    }
    words.sort();
    println!("{}", words.join("\n"));
}

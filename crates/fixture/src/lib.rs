//! Serialising test fixtures that live on a server outside the test process.
//!
//! Every integration suite in this workspace seeds a database it shares with
//! every other run of itself. Under `cargo test` one process ran a whole test
//! binary, so a `OnceCell` was enough to seed once and a `Mutex` was enough to
//! take turns. Under `cargo nextest` each test is its own process: both are
//! empty at every start, thirty-four processes seed the same database at once,
//! and the suite fails with whatever the losing seed happened to hit —
//! `Table bench.types_all already exists`, `Database 'dbeaver_test' is in
//! transition`, a reader invalidated mid-stream.
//!
//! A file lock is the smallest thing that spans processes, and it costs no
//! dependency: `std::fs::File::lock` has been stable since 1.89.
//!
//! Mutual exclusion is all this crate offers. Whether a fixture still *needs*
//! building is deliberately left to the fixture, because only the server can
//! answer it: a marker kept on this side would still say "seeded" after the
//! container was thrown away and replaced, and a suite that skips its seed
//! against an empty database fails as if the driver had lost the tables.
//! [`fingerprint`] is here so that every suite stamps its server the same way.

use std::fs::File;

/// Holds a named fixture until it is dropped. See [`exclusive`].
#[derive(Debug)]
pub struct Guard(File);

impl Drop for Guard {
    fn drop(&mut self) {
        // Closing the file would release the lock anyway; unlocking explicitly
        // only makes the release visible at the point the guard dies. Errors are
        // dropped because this runs on the panic path too, and a second failure
        // there would bury the one the test was reporting.
        let _ = self.0.unlock();
    }
}

/// Waits until no other process holds `name`, then holds it until the guard drops.
///
/// `name` is a fixture, not a test: everything that would collide over the same
/// server state passes the same name.
///
/// # Panics
///
/// If the lock file cannot be created or locked. A fixture that cannot take its
/// turn has no safe way to continue, and a test that reports the collision it
/// then hits would be reporting the wrong thing.
pub async fn exclusive(name: &str) -> Guard {
    let path = std::env::temp_dir().join(format!("dbeaver-fixture-{name}.lock"));
    // On a blocking thread because `lock` parks until the holder lets go, which
    // is as long as somebody else's seed takes — minutes, for the suites that
    // load a million rows. Parking a runtime worker for that would stall every
    // other task sharing it, including the timeouts some of these tests are
    // built out of.
    tokio::task::spawn_blocking(move || {
        let file = File::create(&path)
            .unwrap_or_else(|e| panic!("creating the fixture lock {}: {e}", path.display()));
        file.lock()
            .unwrap_or_else(|e| panic!("waiting for the fixture lock {}: {e}", path.display()));
        Guard(file)
    })
    .await
    .expect("the fixture lock task should not be cancelled")
}

/// What a fixture was built from, short enough to name a table after.
///
/// The point is that it changes when the DDL changes. A fixture stamped with
/// this and checked against it cannot be silently stale: the container outlives
/// every run, so "do the tables exist" answers yes for tables built by an older
/// version of the file beside them, and the test added alongside a new column
/// then fails naming the column — which reads exactly like the driver losing it.
///
/// FNV-1a rather than a hash crate: this is a cache key for a test fixture, so
/// the only property required of it is that different DDL gives a different
/// answer.
pub fn fingerprint<'a>(parts: impl IntoIterator<Item = &'a str>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in part.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn different_ddl_gets_a_different_fingerprint() {
        assert_ne!(
            fingerprint(["CREATE TABLE t (a int)"]),
            fingerprint(["CREATE TABLE t (b int)"])
        );
    }

    /// The parts are hashed as one stream, so where the boundaries fall must not
    /// matter — a suite that joins its statements and one that passes them
    /// separately have to agree, or a rename of the caller silently rebuilds
    /// every fixture.
    #[test]
    fn the_split_between_parts_does_not_change_the_answer() {
        assert_eq!(
            fingerprint(["CREATE ", "TABLE t"]),
            fingerprint(["CREATE TABLE t"])
        );
    }

    #[tokio::test]
    async fn a_guard_is_released_when_it_drops() {
        let name = "dbfixture-self-test";
        drop(exclusive(name).await);
        // Would deadlock against a lock this process still held.
        drop(exclusive(name).await);
    }
}

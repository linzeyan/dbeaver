//! Which driver a connection string names.
//!
//! One rule: a connection string is `<scheme>://<rest>`, the scheme names the
//! driver, and what the rest means is that driver's business. There is no
//! fallback for a string without one — a guess about which database a bare
//! `host=… port=…` refers to would be right until the day somebody pastes a
//! MySQL string in the same shape.
//!
//! The registry is static. There are fifteen drivers planned and all of them are
//! known at compile time, which is what lets this be a `match` rather than the
//! plugin system, extension registry and manifest parsing that upstream needs to
//! discover its own drivers at startup.

use dbconn::{DbError, Driver};
use driver_mongodb::MongoSource;
use driver_postgres::PgSource;
use driver_sqlite::SqliteSource;

/// Every scheme this build answers to, for the message a wrong one gets.
const KNOWN: &str = "postgres, postgresql, mongodb, mongodb+srv, sqlite";

/// Opens whichever database `url` names.
pub async fn connect(url: &str) -> Result<Box<dyn Driver>, DbError> {
    let Some((scheme, rest)) = url.split_once("://").filter(|(s, _)| !s.is_empty()) else {
        // Deliberately without the string that failed. A connection string holds
        // a password, and an error message is the one place it is certain to be
        // shown on screen and written to a log.
        return Err(DbError::new(
            "a connection string starts with the driver it names, \
             as in postgres://user@host/database or sqlite:///path/to/file.db",
        ));
    };

    match scheme {
        // Both spellings, because neither is ours to choose: a connection string
        // is pasted from whichever console handed it out, and cloud providers
        // print both. Refusing the vendor's own spelling is a defect dressed as
        // consistency.
        //
        // Passed on whole, scheme included, since libpq's URL form is what
        // tokio-postgres parses.
        "postgres" | "postgresql" => Ok(Box::new(PgSource::connect(url).await?)),
        // Everything after the scheme is the path, so three slashes give an
        // absolute one and two give a path relative to where the client was
        // started. That is the convention every other tool in this space uses,
        // and inventing a different one would only be a thing to look up.
        // Passed on whole for the same reason PostgreSQL is: the scheme is part
        // of what the MongoDB URI parser reads, and `mongodb+srv` in particular
        // means "look the hosts up in DNS" rather than naming one.
        "mongodb" | "mongodb+srv" => Ok(Box::new(MongoSource::connect(url).await?)),
        "sqlite" => Ok(Box::new(SqliteSource::connect(rest).await?)),
        other => Err(DbError::new(format!(
            "no driver for {other}://. This build has: {KNOWN}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure `url` produces, insisting there is one.
    ///
    /// A helper because `Box<dyn Driver>` is not `Debug` — the trait describes
    /// live connections, and a blanket `Debug` on one would be an invitation to
    /// print a struct holding a password.
    async fn refusal(url: &str) -> String {
        match connect(url).await {
            Err(e) => e.to_string(),
            Ok(_) => panic!("expected this to name no driver: {url}"),
        }
    }

    #[tokio::test]
    async fn a_string_that_names_no_driver_says_what_one_looks_like() {
        // The shape libpq accepts and this does not. Guessing PostgreSQL from it
        // would work until somebody pastes a MySQL string in the same shape.
        let message = refusal("host=127.0.0.1 port=5432 dbname=bench").await;
        assert!(message.contains("postgres://"), "got: {message}");
    }

    #[tokio::test]
    async fn a_driver_this_build_does_not_have_says_which_it_does() {
        let message = refusal("oracle://scott@localhost/orcl").await;
        assert!(message.contains("oracle"), "got: {message}");
        assert!(message.contains("sqlite"), "got: {message}");
    }

    #[tokio::test]
    async fn a_failure_never_repeats_the_string_it_was_given() {
        // Whatever else an error says, it must not put the password back on
        // screen — an error message is the one place it is certain to be shown
        // and logged.
        // A SQLite path is deliberately not covered: it is not a secret, and
        // naming the file that is not there is the whole value of that message.
        for url in [
            "host=127.0.0.1 password=hunter2",
            "oracle://scott:hunter2@localhost/orcl",
        ] {
            let message = refusal(url).await;
            assert!(
                !message.contains("hunter2"),
                "the string leaked into: {message}"
            );
        }
    }
}

//! Where the transaction on one connection has got to.
//!
//! The state is remembered here because it is the answer to a question the
//! server will not be asked again. PostgreSQL reports its transaction status in
//! every ReadyForQuery, and that field does not survive the client library's
//! API; every other database says even less. So the client that sent the `BEGIN`
//! is the one that knows a transaction is open, and it has to remember rather
//! than look.
//!
//! Which states the limitation exactly: a `BEGIN` typed into the editor and run
//! as an ordinary statement opens a transaction this does not know about, and
//! the Commit button will refuse to end it. Nothing here can fix that without
//! reading every statement the user runs, and a client that guessed wrong about
//! which of those were transaction control would be worse than one that admits
//! it only tracks its own.

use dbconn::{DbError, DbResult, Driver, TxStep};
use std::sync::Mutex;

/// What a front end needs to draw: whether control is possible at all, which
/// mode the connection is in, and what is open.
///
/// `transactional` is the driver's answer rather than this session's, and it is
/// what decides whether any of the rest is worth showing. A connection that
/// cannot hold a transaction open is not in autocommit mode — it has no mode.
#[derive(serde::Serialize)]
pub struct TxState {
    pub transactional: bool,
    pub autocommit: bool,
    pub open: bool,
    /// Innermost last, which is the order they can be rolled back to in.
    pub savepoints: Vec<String>,
}

struct Inner {
    autocommit: bool,
    open: bool,
    savepoints: Vec<String>,
}

/// The transaction state of one connection.
///
/// Locked for long enough to read or change a few booleans and never across a
/// call to the server: the front end asks for the state to draw a toolbar, and a
/// toolbar that waits out a running statement is a frozen window.
pub struct Session {
    inner: Mutex<Inner>,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// Autocommit, which is what a connection does before anybody says
    /// otherwise.
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                autocommit: true,
                open: false,
                savepoints: Vec::new(),
            }),
        }
    }

    pub fn state(&self, driver: &dyn Driver) -> TxState {
        let inner = self.lock();
        TxState {
            transactional: driver.transactional(),
            autocommit: inner.autocommit,
            open: inner.open,
            savepoints: inner.savepoints.clone(),
        }
    }

    /// Opens a transaction if the mode says the next statement belongs in one.
    ///
    /// Implicit rather than something the user starts, because that is what
    /// manual-commit mode means: statements accumulate until one of the two
    /// buttons is pressed, and a mode that also needed a Begin press would be
    /// asking the user to say the same thing twice.
    pub async fn before_statement(&self, driver: &dyn Driver) -> DbResult<()> {
        {
            let inner = self.lock();
            if inner.autocommit || inner.open {
                return Ok(());
            }
        }
        driver.transaction(&TxStep::Begin).await?;
        self.lock().open = true;
        Ok(())
    }

    /// Enters or leaves manual-commit mode.
    ///
    /// Sends nothing: the mode is a decision about the next statement, and a
    /// connection with no transaction open has nothing to tell the server yet.
    ///
    /// Refused while a transaction is open rather than deciding for the user.
    /// The work in it is either wanted or not, and that is a question with a
    /// person's answer — JDBC commits it, which is a surprise the first time it
    /// happens to somebody.
    pub fn set_autocommit(&self, driver: &dyn Driver, on: bool) -> DbResult<()> {
        let mut inner = self.lock();
        if inner.autocommit == on {
            return Ok(());
        }
        if inner.open {
            return Err(DbError::new(
                "commit or roll back the open transaction before changing mode",
            ));
        }
        if !on && !driver.transactional() {
            return Err(DbError::new(
                "this connection cannot hold a transaction open",
            ));
        }
        inner.autocommit = on;
        Ok(())
    }

    pub async fn commit(&self, driver: &dyn Driver) -> DbResult<()> {
        self.end(driver, TxStep::Commit).await
    }

    pub async fn rollback(&self, driver: &dyn Driver) -> DbResult<()> {
        self.end(driver, TxStep::Rollback).await
    }

    /// Marks a point in the open transaction to come back to.
    pub async fn savepoint(&self, driver: &dyn Driver, name: &str) -> DbResult<()> {
        let name = checked(name)?;
        if !self.lock().open {
            return Err(DbError::new("no transaction is open to mark"));
        }
        driver
            .transaction(&TxStep::Savepoint(name.to_string()))
            .await?;
        let mut inner = self.lock();
        // A name used twice makes a second savepoint that hides the first, and
        // rolling back goes to the newer one — so the list follows the server
        // and moves the name to the top rather than keeping both.
        inner.savepoints.retain(|s| s != name);
        inner.savepoints.push(name.to_string());
        Ok(())
    }

    /// Undoes the part of the transaction that came after `name`, leaving the
    /// transaction itself open.
    pub async fn rollback_to(&self, driver: &dyn Driver, name: &str) -> DbResult<()> {
        let name = checked(name)?;
        let at = self.find(name)?;
        driver
            .transaction(&TxStep::RollbackTo(name.to_string()))
            .await?;
        // The savepoints set after this one went with the statements they
        // marked; this one stays, and can be rolled back to again.
        self.lock().savepoints.truncate(at + 1);
        Ok(())
    }

    /// Forgets `name`, keeping everything done since it was set.
    pub async fn release(&self, driver: &dyn Driver, name: &str) -> DbResult<()> {
        let name = checked(name)?;
        let at = self.find(name)?;
        driver
            .transaction(&TxStep::Release(name.to_string()))
            .await?;
        // Releasing a savepoint releases the ones inside it too.
        self.lock().savepoints.truncate(at);
        Ok(())
    }

    async fn end(&self, driver: &dyn Driver, step: TxStep) -> DbResult<()> {
        if !self.lock().open {
            return Err(DbError::new("no transaction is open"));
        }
        driver.transaction(&step).await?;
        let mut inner = self.lock();
        inner.open = false;
        inner.savepoints.clear();
        Ok(())
    }

    fn find(&self, name: &str) -> DbResult<usize> {
        self.lock()
            .savepoints
            .iter()
            .position(|s| s == name)
            .ok_or_else(|| DbError::new(format!("no savepoint named {name} is open")))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("transaction state left locked by a panic")
    }
}

/// A savepoint name that can be written into a statement.
///
/// Checked rather than escaped. The name reaches the server inside `SAVEPOINT
/// {name}` — it is an identifier, not a value, so there is no placeholder to
/// bind it to — and a name that has to be quoted to be safe is a name nobody
/// meant to type.
fn checked(name: &str) -> DbResult<&str> {
    let plain = name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        Ok(name)
    } else {
        Err(DbError::new(format!(
            "{name:?} is not a name a savepoint can have: a letter, then letters, digits or underscores"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::checked;

    #[test]
    fn a_savepoint_name_is_a_plain_word() {
        assert!(checked("before_edit").is_ok());
        assert!(checked("s2").is_ok());
    }

    #[test]
    fn a_savepoint_name_that_carries_a_statement_is_refused() {
        // The reason this check exists, spelled out: the name is interpolated
        // into SQL, so anything that could end the statement has to be stopped
        // here rather than quoted and hoped about.
        assert!(checked("s; DROP TABLE users").is_err());
        assert!(checked("\"quoted\"").is_err());
        assert!(checked("").is_err());
        assert!(checked("2fast").is_err());
    }
}

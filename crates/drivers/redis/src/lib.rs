//! Redis, behind the same `Driver` trait as the SQL databases.
//!
//! MongoDB was in phase 2 to find out whether the trait describes databases or
//! merely describes SQL. It does describe databases. This driver asks a narrower
//! and harder question: whether it describes a database that has no rows, no
//! columns, no schema, no query language and no result sets. It does, but only
//! after one decision that is not obvious and is the whole of this driver's
//! design.
//!
//! **A relation is a type, not a key.** Redis's namespace is server → numbered
//! database → key, which looks like server → schema → relation until you count
//! the keys: a relation per key means a navigator with a million entries in it,
//! refreshed by scanning the whole keyspace. So `relations()` answers with the
//! six value types — `string`, `hash`, `list`, `set`, `zset`, `stream` — and the
//! rows of the `hash` relation are the keys that hold hashes, one row each, with
//! the key, its TTL, its size and its whole value. That is the join that makes
//! everything else fit: a relation has a fixed column set, so `columns()` has
//! something to answer; a browse is a `SCAN` filtered by type, so `browse()` has
//! a statement to write; and a keyspace of any size produces six navigator
//! entries.
//!
//! # The statement grammar
//!
//! A statement is **one Redis command per line**. Blank lines are skipped. The
//! commands run in order on the session's connection, and the reply to the last
//! line is the result; the lines before it are run for their effect and their
//! replies are discarded. Arguments are split on whitespace, and a `"` or `'`
//! quotes one that contains spaces, exactly as `redis-cli` does — `SET "my key"
//! "hello world"` is three arguments.
//!
//! ```text
//! SELECT 3
//! SCAN 0 MATCH user:* TYPE hash COUNT 100
//! ```
//!
//! That grammar exists for `SELECT`. Redis has no way to name a database in a
//! command — the database is a property of the connection — so a browse of `db3`
//! has to select it first, and a statement that could only hold one command could
//! never reach any database but the one the connection opened on. The cost is
//! that a statement is not one round trip, which nothing here promised it was.
//!
//! Two replies are not shown literally, and both are named here rather than
//! discovered:
//!
//! - **`SCAN`** is turned into a listing of keys — see `browse` — because its
//!   literal reply is a cursor and a nested array, and putting the iteration
//!   mechanism in two grid cells is showing somebody the plumbing instead of
//!   their data. `HSCAN`, `SSCAN` and `ZSCAN` are *not* covered: they iterate
//!   inside one key rather than over the keyspace, and their replies are shown as
//!   they arrive.
//! - **A map** — `HGETALL`, `CONFIG GET` — is two columns rather than one. That
//!   is only possible because this driver speaks RESP3; under RESP2 the same
//!   reply is a flat array and a client has to know, command by command, that the
//!   elements come in pairs. RESP3 is asked for unconditionally, which means this
//!   driver needs Redis 6 or later and says so at connect rather than mis-showing
//!   a hash.
//!
//! # What Redis does not have
//!
//! **No cursor with the trait's guarantee.** `SCAN` is Redis's own cursor and it
//! is weaker than `Driver::cursor` asks for; `RedisSource::cursor` states exactly
//! how, and the contract subject records that the guarantee is not claimed.
//!
//! **No transaction a session can hold open.** `transactional()` is false and
//! `transaction()` refuses every step by name — `MULTI`/`EXEC` is a batch, not a
//! session's transaction, and the reason is at `transaction`.
//!
//! **No secondary index, no view, no foreign key, no trigger, no constraint.**
//! Five of the nine metadata calls answer with nothing and send no command to
//! find out. `metadata.rs` says which and why.
//!
//! **No cancel that leaves the connection alone, in general.** `cancel` tries
//! `CLIENT UNBLOCK` first, which does, and falls back to `CLIENT KILL` for
//! everything that is not blocking. What that costs is at `Reach::stop`.

mod driver;
mod metadata;
mod shape;

use arrow::array::RecordBatch;
use arrow::datatypes::SchemaRef;
use redis::aio::MultiplexedConnection;
use redis::{Client, Cmd, ProtocolVersion, Value};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use shape::{KeyRow, Reply};
pub use shape::{KeyType, TYPES};

/// The number of databases a server that will not answer `CONFIG GET` is assumed
/// to have. See `metadata::schemas`, which is the only place it is used.
pub(crate) const DATABASES: i64 = 16;

/// A failure, as this driver reports it.
///
/// `redis::RedisError` is the crate's type and this one shadows its name, which
/// is a wart worth stating once: every mention of the wire type below is written
/// out in full.
#[derive(Debug, thiserror::Error)]
pub enum RedisError {
    /// Something that failed with no statement to place it in: connecting,
    /// reading metadata, delivering a cancel.
    #[error("{}", said(.0))]
    Server(#[from] redis::RedisError),
    /// A command the server refused, read against the statement that sent it so
    /// that the line it was on can be pointed at.
    #[error("{}", said(.error))]
    Command {
        error: redis::RedisError,
        position: Option<u32>,
    },
    /// A statement that stopped because somebody pressed Cancel.
    #[error("{}", said(.0))]
    Cancelled(redis::RedisError),
    /// A statement this driver refused before troubling the server.
    #[error("{message}")]
    Statement {
        message: String,
        position: Option<u32>,
    },
    #[error("arrow: {0}")]
    Arrow(#[from] arrow::error::ArrowError),
}

impl RedisError {
    /// Where in the statement the trouble is: 1-based, counted in characters.
    ///
    /// Redis answers nothing like PostgreSQL's error position — a rejected
    /// command names what it disliked and never where in the text it was. What
    /// this driver can say instead comes from its own grammar: it knows which
    /// line held the command that failed, so the position is the offset of that
    /// line's first character. That points at the right command and no closer,
    /// which is the same bargain the SQL Server driver strikes with a line
    /// number.
    ///
    /// Only for a statement of more than one line. In a single-line statement
    /// the answer is always the first character, which locates nothing and
    /// places a caret confidently at the front of the only thing there.
    pub fn statement_position(&self) -> Option<u32> {
        match self {
            RedisError::Command { position, .. } | RedisError::Statement { position, .. } => {
                *position
            }
            _ => None,
        }
    }

    /// Whether this is the Cancel button rather than a fault.
    pub fn is_cancelled(&self) -> bool {
        matches!(self, RedisError::Cancelled(_))
    }
}

/// What the server said, rather than the wrapper redis-rs puts round it.
///
/// `RedisError`'s own Display reads "ResponseError: unknown command 'WIBBLE'",
/// where `ResponseError` is the crate's category for the reply and not a word
/// Redis used. What the reader needs is the line `redis-cli` would have printed:
/// the code the server sent, then the sentence it sent with it.
fn said(e: &redis::RedisError) -> String {
    match (e.code(), e.detail()) {
        (Some(code), Some(detail)) => format!("{code} {detail}"),
        (Some(code), None) => code.to_string(),
        // An I/O failure, or anything else with no code of its own.
        (None, _) => e.to_string(),
    }
}

/// Whether a failure means the connection behind it is finished.
///
/// Asked because the session connection outlives the statement that failed on
/// it. A command Redis refused — a wrong type, an unknown command — left the
/// connection exactly as it was, selected database included, and throwing it
/// away for that would move the next statement to a different database. What
/// does end a connection is `CLIENT KILL`, which arrives here as a dropped
/// socket. An `UNBLOCK` cancellation deliberately does not: that is the whole
/// reason `stop` prefers it.
fn ends_the_connection(e: &RedisError) -> bool {
    match e {
        RedisError::Server(wire)
        | RedisError::Command { error: wire, .. }
        | RedisError::Cancelled(wire) => wire.is_connection_dropped() || wire.is_io_error(),
        _ => false,
    }
}

/// Turns a failure into one that knows whether it was asked for.
///
/// Part evidence and part memory, and both halves are needed. `UNBLOCKED` is the
/// server's own word — it is what `CLIENT UNBLOCK … ERROR` makes a blocked
/// command return — and is as good as PostgreSQL's SQLSTATE 57014. A kill has no
/// such marker: the connection simply stops answering, which is indistinguishable
/// from the network dropping, so it is only read as a cancellation when this
/// driver remembers having asked for one.
fn classify(e: RedisError, killed: &AtomicBool) -> RedisError {
    let asked_for = killed.swap(false, Ordering::AcqRel);
    match e {
        RedisError::Server(wire) | RedisError::Command { error: wire, .. }
            if wire.code() == Some("UNBLOCKED")
                || (asked_for && (wire.is_connection_dropped() || wire.is_io_error())) =>
        {
            RedisError::Cancelled(wire)
        }
        other => other,
    }
}

/// One command of a statement, and the line it was written on.
#[derive(Debug)]
struct Line {
    /// 1-based, counted in lines of the text as given — including the blank ones
    /// that carry no command, since a caret has to land where the user is
    /// looking.
    at: usize,
    args: Vec<String>,
}

/// The 1-based character offset at which `line` starts, or `None` when a line
/// number says nothing.
///
/// Characters and not bytes, as the trait requires: a statement naming a key in
/// Chinese puts three bytes under every character, and a caret placed at the byte
/// offset lands well past the line it belongs to.
fn caret(text: &str, line: usize) -> Option<u32> {
    // A statement of one line is entirely on line one, so the number adds
    // nothing the caller does not already have.
    if !text.contains('\n') {
        return None;
    }
    let mut offset: u32 = 1;
    for (n, current) in text.split('\n').enumerate() {
        if n + 1 == line {
            return Some(offset);
        }
        // The newline that ended the line is a character of the statement too.
        offset += current.chars().count() as u32 + 1;
    }
    None
}

/// One line split into a command and its arguments, or `None` for a quote that
/// is never closed.
///
/// The rules `redis-cli` uses, cut down to what somebody typing into an editor
/// needs: whitespace separates, `"` and `'` quote, and a backslash inside a quote
/// escapes the next character. Without this, a key with a space in it — which
/// Redis allows and applications produce — could not be typed at all.
///
/// The arguments come back as `String`, which is narrower than Redis: its
/// arguments are arbitrary bytes. Nothing is lost that could have been gained,
/// since the source here is text somebody typed.
fn split_args(line: &str) -> Option<Vec<String>> {
    let mut args: Vec<String> = Vec::new();
    let mut chars = line.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut arg = String::new();
        let quote = if c == '"' || c == '\'' {
            chars.next();
            Some(c)
        } else {
            None
        };
        let mut closed = quote.is_none();
        while let Some(c) = chars.next() {
            match quote {
                Some(q) if c == q => {
                    closed = true;
                    break;
                }
                Some(_) if c == '\\' => match chars.next() {
                    Some(escaped) => arg.push(escaped),
                    None => return None,
                },
                None if c.is_whitespace() => break,
                _ => arg.push(c),
            }
        }
        if !closed {
            return None;
        }
        args.push(arg);
    }
    Some(args)
}

/// A statement, as the commands it is made of.
fn parse_statement(text: &str) -> Result<Vec<Line>, RedisError> {
    let mut lines = Vec::new();
    for (n, current) in text.split('\n').enumerate() {
        if current.trim().is_empty() {
            continue;
        }
        let Some(args) = split_args(current) else {
            return Err(RedisError::Statement {
                message: "this line opens a quote it never closes".to_string(),
                position: caret(text, n + 1),
            });
        };
        if !args.is_empty() {
            lines.push(Line { at: n + 1, args });
        }
    }
    if lines.is_empty() {
        return Err(RedisError::Statement {
            message: "a statement is one Redis command per line, as in \
                      SELECT 3 on one line and SCAN 0 MATCH * TYPE hash on the next"
                .to_string(),
            position: None,
        });
    }
    Ok(lines)
}

fn command_of(line: &Line) -> Cmd {
    let mut cmd = redis::cmd(&line.args[0]);
    for arg in &line.args[1..] {
        cmd.arg(arg.as_str());
    }
    cmd
}

/// What a `SCAN` line asks for.
///
/// Parsed here rather than sent through, because this driver re-issues `SCAN`
/// itself to page the iteration forward — so it has to understand every option
/// on the line. An option it does not know is refused by name rather than passed
/// on, since passing it on would mean the second call to `SCAN` is not the one
/// the statement described.
#[derive(Debug)]
struct ScanArgs {
    cursor: String,
    pattern: Option<String>,
    of: Option<KeyType>,
    /// The statement's `COUNT`, which this driver reads as both the hint it
    /// sends and the number of rows it stops at. See `Scan` for why.
    count: Option<u64>,
    /// The type as written, when it is not one of the six. Kept so that a
    /// listing can still be produced — the rows will simply be the keys that
    /// match, which for a type Redis does not have is none of them.
    unknown: bool,
}

fn parse_scan(line: &Line, text: &str) -> Result<ScanArgs, RedisError> {
    let refuse = |message: String| RedisError::Statement {
        message,
        position: caret(text, line.at),
    };
    let mut args = line.args[1..].iter();
    let Some(cursor) = args.next() else {
        return Err(refuse(
            "SCAN takes the cursor to start from, as in SCAN 0".to_string(),
        ));
    };
    let mut scan = ScanArgs {
        cursor: cursor.clone(),
        pattern: None,
        of: None,
        count: None,
        unknown: false,
    };
    while let Some(option) = args.next() {
        let value = args.next().ok_or_else(|| {
            refuse(format!(
                "SCAN's {} takes a value after it",
                option.to_uppercase()
            ))
        })?;
        match option.to_ascii_uppercase().as_str() {
            "MATCH" => scan.pattern = Some(value.clone()),
            "COUNT" => {
                scan.count =
                    Some(value.parse().map_err(|_| {
                        refuse(format!("SCAN's COUNT takes a number, not {value:?}"))
                    })?)
            }
            "TYPE" => match KeyType::parse(value) {
                Some(of) => scan.of = Some(of),
                // Not an error: `SCAN 0 TYPE ReJSON-RL` is a legitimate question
                // about a type a module added, and Redis answers it. This driver
                // cannot read such a value, so the listing is the mixed one and
                // the value column stays empty.
                None => scan.unknown = true,
            },
            other => {
                return Err(refuse(format!(
                    "SCAN takes MATCH, COUNT and TYPE; this driver does not know {other}"
                )));
            }
        }
    }
    Ok(scan)
}

fn is_scan(line: &Line) -> bool {
    line.args[0].eq_ignore_ascii_case("SCAN")
}

/// The keyspace being iterated, one page at a time.
///
/// **What `COUNT` means here.** Redis documents `COUNT` as a hint about how much
/// work one `SCAN` call may do, not as a row limit: a call may return more
/// elements than asked for, and an iteration ends when the cursor comes back to
/// zero. This driver reads it as both — the hint it sends, and the number of rows
/// it stops at. That is a stronger reading than Redis's own and it is taken
/// deliberately: `Browse::limit` is a row ceiling, Redis has no `LIMIT` to put it
/// in, and a browse statement that a user can read has to say how many rows it
/// will produce. The cost is that `SCAN 0 COUNT 1000` typed into an editor stops
/// at a thousand keys where `redis-cli --scan` would keep going. A statement with
/// no `COUNT` has no ceiling and iterates to the end of the keyspace.
struct Scan {
    conn: MultiplexedConnection,
    of: Option<KeyType>,
    pattern: Option<String>,
    count: Option<u64>,
    /// Whether the type named is one this driver has no reader for, in which
    /// case no key can match and the iteration is over before it starts.
    unknown: bool,
    cursor: String,
    done: bool,
    buffered: VecDeque<KeyRow>,
    produced: u64,
}

impl Scan {
    fn start(conn: MultiplexedConnection, args: ScanArgs) -> Scan {
        Scan {
            conn,
            of: args.of,
            pattern: args.pattern,
            count: args.count,
            unknown: args.unknown,
            cursor: args.cursor,
            done: false,
            buffered: VecDeque::new(),
            produced: 0,
        }
    }

    /// The columns this listing produces, known before a single key is read.
    fn schema(&self) -> SchemaRef {
        shape::key_shape(self.of)
    }

    /// How many more rows the ceiling allows.
    fn room(&self) -> usize {
        match self.count {
            Some(ceiling) => {
                (ceiling.saturating_sub(self.produced) as usize).saturating_sub(self.buffered.len())
            }
            None => usize::MAX,
        }
    }

    /// Reads forward until at least `want` rows are buffered, or there are no
    /// more.
    async fn fill(&mut self, want: usize) -> Result<(), RedisError> {
        while !self.done && self.buffered.len() < want {
            let room = self.room();
            if room == 0 {
                self.done = true;
                break;
            }
            let mut cmd = redis::cmd("SCAN");
            cmd.arg(&self.cursor);
            if let Some(pattern) = &self.pattern {
                cmd.arg("MATCH").arg(pattern.as_str());
            }
            // The statement's own COUNT where it gave one. Where it did not, the
            // size of the page being filled — Redis's default of 10 would make a
            // hundred-row page ten round trips, and the statement said nothing
            // that this would be contradicting.
            cmd.arg("COUNT").arg(self.count.unwrap_or(want as u64));
            if let Some(of) = self.of {
                cmd.arg("TYPE").arg(of.name());
            }
            let reply: Value = cmd.query_async(&mut self.conn).await?;
            let (cursor, mut keys) = split_scan(reply)?;
            self.cursor = cursor;
            if self.cursor == "0" {
                self.done = true;
            }
            if keys.len() > room {
                keys.truncate(room);
                self.done = true;
            }
            let rows = read_keys(&mut self.conn, &keys, self.of).await?;
            self.buffered.extend(rows);
        }
        Ok(())
    }

    /// The next page, or an empty vector once the iteration is over.
    async fn next_page(&mut self, want: usize) -> Result<Vec<KeyRow>, RedisError> {
        // A type this driver cannot read matches no key it could show, so there
        // is nothing to ask the server.
        if self.unknown {
            return Ok(Vec::new());
        }
        self.fill(want).await?;
        let take = want.min(self.buffered.len());
        let page: Vec<KeyRow> = self.buffered.drain(..take).collect();
        self.produced += page.len() as u64;
        Ok(page)
    }
}

/// A `SCAN` reply, split into the cursor to continue from and the keys it found.
fn split_scan(reply: Value) -> Result<(String, Vec<Vec<u8>>), RedisError> {
    let Value::Array(mut parts) = reply else {
        return Err(RedisError::Statement {
            message: "SCAN answered with something that is not a cursor and a page".to_string(),
            position: None,
        });
    };
    if parts.len() != 2 {
        return Err(RedisError::Statement {
            message: "SCAN answered with something that is not a cursor and a page".to_string(),
            position: None,
        });
    }
    let keys = match parts.pop() {
        Some(Value::Array(items)) | Some(Value::Set(items)) => items
            .into_iter()
            .map(|item| match item {
                Value::BulkString(bytes) => bytes,
                other => shape::text(&other).into_bytes(),
            })
            .collect(),
        _ => Vec::new(),
    };
    // The cursor comes back as a bulk string even though it is a number, and it
    // is sent back the same way: it is a 64-bit value the client is not meant to
    // interpret, and parsing it into an integer here would be this driver
    // claiming to know what the server means by it.
    let cursor = parts.pop().map(|c| shape::text(&c)).unwrap_or_default();
    Ok((cursor, keys))
}

/// Reads the TTL, size and value of each key on a page.
///
/// Pipelined, and it has to be: a page of a hundred keys is three hundred
/// commands, and one round trip each would make a browse of a small keyspace
/// slower than a table scan of a large one.
///
/// `ignore_errors` is set, so a command that fails leaves an error in its own
/// cell instead of failing the page. That is not leniency about faults, it is
/// about a race that is ordinary here and impossible elsewhere: `SCAN` lists a
/// key, and by the time its value is read the key can have been deleted, expired,
/// or replaced by one of another type. A browse that failed outright because one
/// key changed under it would be unusable on a keyspace anybody is writing to.
async fn read_keys(
    conn: &mut MultiplexedConnection,
    keys: &[Vec<u8>],
    of: Option<KeyType>,
) -> Result<Vec<KeyRow>, RedisError> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }

    // What each key holds. Known already when the scan named a type; asked for
    // in a round trip of its own when it did not, since the command that reads a
    // value depends on the answer.
    let (kinds, named): (Vec<Option<KeyType>>, Vec<Option<String>>) = match of {
        Some(of) => (vec![Some(of); keys.len()], vec![None; keys.len()]),
        None => {
            let mut types = redis::pipe();
            types.ignore_errors();
            for key in keys {
                let mut cmd = redis::cmd("TYPE");
                cmd.arg(key);
                types.add_command(cmd);
            }
            let replies: Vec<Value> = types.query_async(conn).await?;
            let words: Vec<String> = replies.iter().map(shape::text).collect();
            (
                words.iter().map(|word| KeyType::parse(word)).collect(),
                words.into_iter().map(Some).collect(),
            )
        }
    };

    let mut reads = redis::pipe();
    reads.ignore_errors();
    // How many replies each key contributes, since a string has no size command
    // and a key of a type this driver cannot read has no value command either.
    let mut counts: Vec<usize> = Vec::with_capacity(keys.len());
    for (key, kind) in keys.iter().zip(&kinds) {
        let mut n = 1;
        let mut ttl = redis::cmd("TTL");
        ttl.arg(key);
        reads.add_command(ttl);
        if let Some(kind) = kind {
            reads.add_command(kind.read(key));
            n += 1;
            if let Some(size) = kind.size(key) {
                reads.add_command(size);
                n += 1;
            }
        }
        counts.push(n);
    }
    let replies: Vec<Value> = reads.query_async(conn).await?;

    let mut rows = Vec::with_capacity(keys.len());
    let mut at = 0usize;
    for ((key, kind), n) in keys.iter().zip(&kinds).zip(&counts) {
        let taken = &replies[at.min(replies.len())..(at + n).min(replies.len())];
        at += n;
        let mut row = KeyRow {
            key: String::from_utf8_lossy(key).into_owned(),
            ttl: None,
            kind: named.get(rows.len()).cloned().flatten(),
            size: None,
            value: None,
        };
        if let Some(Value::Int(seconds)) = taken.first() {
            // -1 is "no expiry" and -2 is "no such key"; neither is a duration.
            row.ttl = (*seconds >= 0).then_some(*seconds);
        }
        if let Some(kind) = kind {
            if let Some(value) = taken.get(1) {
                row.value = kind.render(value);
            }
            if let Some(Value::Int(size)) = taken.get(2) {
                row.size = Some(*size);
            }
        }
        rows.push(row);
    }
    Ok(rows)
}

/// What a cancel needs in order to reach one connection.
#[derive(Clone)]
struct Reach {
    client: Client,
    /// The connection's own `CLIENT ID`, read when it was opened. Kept out here
    /// rather than beside the connection because a cancel arrives while the
    /// connection is busy with the statement it is trying to stop, so anything
    /// that had to be read through it would queue behind that.
    id: Arc<AtomicI64>,
    busy: Arc<AtomicBool>,
    killed: Arc<AtomicBool>,
}

impl Reach {
    /// Asks the server to abandon whatever this connection is running.
    ///
    /// Two spellings, tried cheapest first, and the difference between them is
    /// worth the extra round trip.
    ///
    /// `CLIENT UNBLOCK <id> ERROR` stops a client that is blocked inside a
    /// blocking command — `BLPOP`, `XREAD BLOCK`, `WAIT` — and leaves the
    /// connection open and usable. The command returns an error whose code is
    /// `UNBLOCKED`, which is the server saying in its own words that this was
    /// asked for. It answers 0 when the client is not blocked, which costs one
    /// round trip and changes nothing.
    ///
    /// `CLIENT KILL ID <id>` is what is left for everything else, and it ends the
    /// connection rather than the command. Redis has no `pg_cancel_backend`:
    /// there is no way to interrupt a running command and keep the socket. What
    /// that costs here is smaller than the same choice costs the SQL Server
    /// driver, and for a reason worth stating — a killed Redis connection takes
    /// no transaction with it, because this driver holds none. It takes the
    /// selected database, and the next statement selects again.
    ///
    /// What a kill can and cannot reach is decided by something outside this
    /// driver: Redis runs commands on one thread, so a command the server is busy
    /// with keeps it busy, and the kill is not read until that command has
    /// finished. Between round trips is the only place a cancel lands, which for
    /// this driver means a `SCAN` iteration — and that is the statement a Cancel
    /// button is actually pressed during. A single slow command finishes first
    /// whatever anybody does, here or in `redis-cli`.
    ///
    /// Aimed only at a connection that is actually running something. A kill sent
    /// to an idle one would destroy a session nobody asked to end, and the
    /// contract requires cancelling an idle cursor to be a no-op that succeeds.
    /// The check is repeated immediately before the kill, which narrows but does
    /// not close the window in which a statement finishes between the two: losing
    /// that race costs one reconnection.
    async fn stop(&self) -> Result<(), RedisError> {
        if !self.busy.load(Ordering::Acquire) {
            return Ok(());
        }
        let id = self.id.load(Ordering::Acquire);
        if id == 0 {
            return Ok(());
        }
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let unblocked: i64 = redis::cmd("CLIENT")
            .arg("UNBLOCK")
            .arg(id)
            .arg("ERROR")
            .query_async(&mut conn)
            .await?;
        if unblocked == 1 {
            return Ok(());
        }
        if !self.busy.load(Ordering::Acquire) {
            return Ok(());
        }
        // Marked before the kill is sent, so the failure it causes cannot arrive
        // before the memory of having asked for it.
        self.killed.store(true, Ordering::Release);
        redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(id)
            .query_async::<Value>(&mut conn)
            .await?;
        Ok(())
    }
}

/// One session against one Redis server.
///
/// Two connections, and the split is the one the trait asks for in as many
/// words: a result being read must not make the navigator wait behind it.
/// Statements run on the session connection, one at a time, because the selected
/// database is a property of a connection and a `SELECT` on a shared one would
/// move somebody else's statement to another database. Metadata has a connection
/// of its own so that expanding the tree does not queue behind a browse of a
/// large keyspace. A cursor opens a third, for the length of the browse.
pub struct RedisSource {
    client: Client,
    /// The connection statements run on, empty when the last one left it
    /// unusable — which here means killed.
    session: Arc<AsyncMutex<Option<MultiplexedConnection>>>,
    meta: Arc<AsyncMutex<Option<MultiplexedConnection>>>,
    reach: Reach,
}

/// The session connection, held for the length of one statement.
struct Held {
    slot: OwnedMutexGuard<Option<MultiplexedConnection>>,
}

impl Held {
    /// Filled by `hold` before this value exists and only emptied by `discard`,
    /// which consumes it, so it is `Some` for the whole life of any reference
    /// taken through here.
    fn conn(&mut self) -> &mut MultiplexedConnection {
        self.slot.as_mut().unwrap()
    }

    /// Gives the connection up rather than back, for a statement that ended it.
    fn discard(mut self) {
        *self.slot = None;
    }
}

async fn client_id(conn: &mut MultiplexedConnection) -> Result<i64, RedisError> {
    Ok(redis::cmd("CLIENT").arg("ID").query_async(conn).await?)
}

impl RedisSource {
    /// Opens a session from a `redis://[:password@]host:port/db` URL.
    ///
    /// RESP3 is asked for whatever the URL says, and overriding the user there is
    /// deliberate: the difference between a hash arriving as a map and arriving
    /// as a flat array is the difference between this driver showing a hash and
    /// guessing at one. A server too old to speak it refuses the handshake, which
    /// is a failure to connect with a message rather than a driver that quietly
    /// shows the wrong thing.
    pub async fn connect(url: &str) -> Result<Self, RedisError> {
        let info: redis::ConnectionInfo = url.parse()?;
        let settings = info
            .redis_settings()
            .clone()
            .set_protocol(ProtocolVersion::RESP3);
        let client = Client::open(info.set_redis_settings(settings))?;
        // Opened eagerly, so a wrong password is a failure to connect rather
        // than a failure at the first metadata call.
        let mut conn = client.get_multiplexed_async_connection().await?;
        let id = client_id(&mut conn).await?;
        Ok(Self {
            reach: Reach {
                client: client.clone(),
                id: Arc::new(AtomicI64::new(id)),
                busy: Arc::new(AtomicBool::new(false)),
                killed: Arc::new(AtomicBool::new(false)),
            },
            client,
            session: Arc::new(AsyncMutex::new(Some(conn))),
            meta: Arc::new(AsyncMutex::new(None)),
        })
    }

    /// Takes the session connection, waiting for the statement before it.
    ///
    /// Opens one when the slot is empty, which is how a connection this driver
    /// killed is replaced. The replacement starts on the database the URL named
    /// — redis-rs selects it during the handshake — and not on the one the killed
    /// connection had selected, which is why a browse statement selects rather
    /// than assuming.
    async fn hold(&self) -> Result<Held, RedisError> {
        let mut slot = Arc::clone(&self.session).lock_owned().await;
        if slot.is_none() {
            let mut conn = self.client.get_multiplexed_async_connection().await?;
            let id = client_id(&mut conn).await?;
            self.reach.id.store(id, Ordering::Release);
            *slot = Some(conn);
        }
        Ok(Held { slot })
    }

    /// Runs `command` on the metadata connection, opening one if there is none.
    pub(crate) async fn ask(&self, command: Cmd) -> Result<Value, RedisError> {
        let mut slot = self.meta.lock().await;
        if slot.is_none() {
            *slot = Some(self.client.get_multiplexed_async_connection().await?);
        }
        let conn = slot.as_mut().unwrap();
        match command.query_async(conn).await {
            Ok(value) => Ok(value),
            Err(e) => {
                let e = RedisError::Server(e);
                if ends_the_connection(&e) {
                    *slot = None;
                }
                Err(e)
            }
        }
    }

    /// Runs a statement and reads its whole result.
    ///
    /// Whole, and that is a fact about Redis rather than a shortcut: the protocol
    /// has no streaming reply, so by the time a `Value` exists it is already in
    /// memory. The one statement that could have been read lazily is a `SCAN`,
    /// and it is not — a `query` over one iterates the keyspace to the end, or to
    /// the ceiling its `COUNT` set, before the first batch is handed out. The
    /// alternative would hold the session connection for as long as somebody was
    /// reading, which is what `cursor` is for and what the Content tab uses.
    pub async fn query(
        &self,
        statement: &str,
        batch_rows: usize,
    ) -> Result<ArrowStream, RedisError> {
        let lines = parse_statement(statement)?;
        let mut held = self.hold().await?;
        self.reach.killed.store(false, Ordering::Release);
        self.reach.busy.store(true, Ordering::Release);
        let outcome = read_whole(held.conn(), &lines, statement, batch_rows).await;
        self.reach.busy.store(false, Ordering::Release);

        match outcome {
            Ok((schema, batches)) => {
                let total = batches.iter().map(|b| b.num_rows() as u64).sum();
                Ok(ArrowStream {
                    schema,
                    batches,
                    total,
                })
            }
            Err(e) => {
                let e = classify(e, &self.reach.killed);
                if ends_the_connection(&e) {
                    held.discard();
                }
                Err(e)
            }
        }
    }

    /// Reads `statement` forward, a page at a time.
    ///
    /// **What this promises, exactly.** For a statement ending in `SCAN` the
    /// pages come from Redis's own cursor, and page *n* costs what page one
    /// costs — the first half of what `Driver::cursor` asks for holds
    /// completely. The second half does not. `SCAN` guarantees that every key
    /// present for the whole iteration is returned **at least once**, and a key
    /// **may be returned twice** if the hash table is resized underneath the
    /// iteration, which adding or removing enough keys causes. The trait asks
    /// that a write landing between two pages must not make a row appear twice.
    /// Redis does not give that, and this driver does not add it.
    ///
    /// Adding it would mean remembering every key already returned and skipping
    /// the repeats, which is what a client that needed the guarantee would do. It
    /// is not done here for two reasons. The memory is unbounded in the size of
    /// the browse — the one thing paging exists to avoid — and it would only fix
    /// half the gap: a key created after the iteration started may or may not
    /// appear, and no amount of bookkeeping on this side settles that. A partial
    /// fix bought with unbounded memory would leave the guarantee still unmet and
    /// the honesty harder to see.
    ///
    /// So the contract subject records `cursors: false`. The cursor is real, the
    /// Content tab uses it, and the driver's own suite pages through it; what is
    /// not claimed is a promise Redis has never made.
    ///
    /// For any other statement the reply is read whole and handed out in pages,
    /// which satisfies both properties for the reason ClickHouse's does: there is
    /// only ever one read.
    pub async fn cursor(&self, statement: &str, batch_rows: usize) -> Result<Cursor, RedisError> {
        let lines = parse_statement(statement)?;
        let batch_rows = batch_rows.max(1);
        // A connection of its own, held for as long as somebody is paging. On
        // the session connection a browse would keep every other statement
        // waiting behind a scrollbar.
        let mut conn = self.client.get_multiplexed_async_connection().await?;
        let id = client_id(&mut conn).await?;
        let reach = Reach {
            client: self.client.clone(),
            id: Arc::new(AtomicI64::new(id)),
            busy: Arc::new(AtomicBool::new(false)),
            killed: Arc::new(AtomicBool::new(false)),
        };

        reach.busy.store(true, Ordering::Release);
        let outcome = start_cursor(conn, &lines, statement, batch_rows).await;
        reach.busy.store(false, Ordering::Release);
        let pages = outcome.map_err(|e| classify(e, &reach.killed))?;

        Ok(Cursor {
            schema: match &pages {
                Pages::Ready { schema, .. } => Arc::clone(schema),
                Pages::Scan(scan) => scan.schema(),
            },
            pages,
            reach,
            batch_rows,
        })
    }

    /// Asks the server to abandon whatever this session is running.
    pub async fn cancel(&self) -> Result<(), RedisError> {
        self.reach.stop().await
    }
}

/// Runs every command and turns the last one's reply into batches.
async fn read_whole(
    conn: &mut MultiplexedConnection,
    lines: &[Line],
    text: &str,
    batch_rows: usize,
) -> Result<(SchemaRef, VecDeque<RecordBatch>), RedisError> {
    let (last, preamble) = lines.split_last().expect("a statement has a command");
    run_preamble(conn, preamble, text).await?;

    if is_scan(last) {
        let mut scan = Scan::start(conn.clone(), parse_scan(last, text)?);
        let schema = scan.schema();
        let mut batches = VecDeque::new();
        loop {
            let page = scan
                .next_page(batch_rows)
                .await
                .map_err(|e| at_line(e, text, last.at))?;
            if page.is_empty() {
                break;
            }
            batches.push_back(shape::key_batch(scan.of, &page)?);
        }
        return Ok((schema, batches));
    }

    let reply: Value =
        command_of(last)
            .query_async(conn)
            .await
            .map_err(|e| RedisError::Command {
                error: e,
                position: caret(text, last.at),
            })?;
    let reply = Reply::of(reply);
    let schema = reply.schema();
    Ok((schema, reply.into_batches(batch_rows)?))
}

/// The commands before the last one, run for their effect.
async fn run_preamble(
    conn: &mut MultiplexedConnection,
    lines: &[Line],
    text: &str,
) -> Result<(), RedisError> {
    for line in lines {
        command_of(line)
            .query_async::<Value>(conn)
            .await
            .map_err(|e| RedisError::Command {
                error: e,
                position: caret(text, line.at),
            })?;
    }
    Ok(())
}

/// Attaches a line to a failure that arose while reading one.
fn at_line(e: RedisError, text: &str, line: usize) -> RedisError {
    match e {
        RedisError::Server(error) => RedisError::Command {
            error,
            position: caret(text, line),
        },
        other => other,
    }
}

async fn start_cursor(
    mut conn: MultiplexedConnection,
    lines: &[Line],
    text: &str,
    batch_rows: usize,
) -> Result<Pages, RedisError> {
    let (last, preamble) = lines.split_last().expect("a statement has a command");
    run_preamble(&mut conn, preamble, text).await?;

    if is_scan(last) {
        return Ok(Pages::Scan(Box::new(Scan::start(
            conn,
            parse_scan(last, text)?,
        ))));
    }

    let reply: Value =
        command_of(last)
            .query_async(&mut conn)
            .await
            .map_err(|e| RedisError::Command {
                error: e,
                position: caret(text, last.at),
            })?;
    let reply = Reply::of(reply);
    let schema = reply.schema();
    Ok(Pages::Ready {
        schema,
        batches: reply.into_batches(batch_rows)?,
    })
}

/// A result being read forward in batches.
pub struct ArrowStream {
    schema: SchemaRef,
    batches: VecDeque<RecordBatch>,
    total: u64,
}

impl ArrowStream {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    /// Rows produced, once the result has been read to the end.
    ///
    /// Rows produced rather than rows changed, which the trait allows and which
    /// is the only meaning available: Redis has no statement that reports how
    /// many keys it touched. `DEL a b c` answers 3, and that 3 is itself this
    /// result's single row rather than a count beside it.
    pub fn rows_affected(&self) -> Option<u64> {
        self.batches.is_empty().then_some(self.total)
    }

    pub async fn next_batch(&mut self) -> Result<Option<RecordBatch>, RedisError> {
        Ok(self.batches.pop_front())
    }
}

enum Pages {
    /// A reply read whole, handed out a page at a time.
    Ready {
        schema: SchemaRef,
        batches: VecDeque<RecordBatch>,
    },
    /// The keyspace, iterated as the pages are asked for.
    Scan(Box<Scan>),
}

/// A result read a page at a time.
pub struct Cursor {
    schema: SchemaRef,
    pages: Pages,
    reach: Reach,
    batch_rows: usize,
}

impl Cursor {
    pub fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }

    pub async fn fetch(&mut self) -> Result<Option<RecordBatch>, RedisError> {
        // Busy for exactly as long as somebody is waiting on a page that has not
        // arrived, which is the window a Cancel button exists for.
        self.reach.busy.store(true, Ordering::Release);
        let outcome = self.page().await;
        self.reach.busy.store(false, Ordering::Release);
        outcome.map_err(|e| classify(e, &self.reach.killed))
    }

    async fn page(&mut self) -> Result<Option<RecordBatch>, RedisError> {
        match &mut self.pages {
            Pages::Ready { batches, .. } => Ok(batches.pop_front()),
            Pages::Scan(scan) => {
                let page = scan.next_page(self.batch_rows).await?;
                if page.is_empty() {
                    return Ok(None);
                }
                Ok(Some(shape::key_batch(scan.of, &page)?))
            }
        }
    }

    pub fn canceller(&self) -> CursorCancel {
        CursorCancel {
            reach: self.reach.clone(),
        }
    }

    /// Closes the cursor and releases the connection behind it.
    ///
    /// Optional: dropping it does the same. Replacing the pages is what releases
    /// the connection — a `MultiplexedConnection`'s socket lives exactly as long
    /// as the last clone of it.
    pub async fn close(&mut self) -> Result<(), RedisError> {
        self.pages = Pages::Ready {
            schema: Arc::clone(&self.schema),
            batches: VecDeque::new(),
        };
        Ok(())
    }
}

/// Stops the fetch one cursor is running.
pub struct CursorCancel {
    reach: Reach,
}

impl CursorCancel {
    /// Delivered is not interrupted, as with `RedisSource::cancel`: a fetch that
    /// had already finished leaves nothing to stop and this still succeeds.
    ///
    /// Using it ends the browse rather than pausing it, unless the fetch happened
    /// to be blocked — see `Reach::stop`. There is no way to interrupt a running
    /// Redis command and keep the connection, so the page being read is the last
    /// one.
    pub async fn cancel(&self) -> Result<(), RedisError> {
        self.reach.stop().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(text: &str) -> Vec<Vec<String>> {
        parse_statement(text)
            .expect("parses")
            .into_iter()
            .map(|line| line.args)
            .collect()
    }

    #[test]
    fn a_statement_is_one_command_per_line() {
        assert_eq!(
            commands("SELECT 3\nSCAN 0 MATCH * TYPE hash"),
            vec![
                vec!["SELECT", "3"],
                vec!["SCAN", "0", "MATCH", "*", "TYPE", "hash"]
            ]
        );
    }

    #[test]
    fn blank_lines_are_not_commands() {
        // An editor ends with a newline, and a statement that treated the empty
        // line after the last command as a command would send an empty one.
        assert_eq!(commands("SELECT 0\n\n  \nPING\n").len(), 2);
    }

    #[test]
    fn a_key_with_a_space_in_it_can_be_typed() {
        // The whole reason there is a splitter here rather than `split_whitespace`.
        assert_eq!(
            commands(r#"SET "my key" "hello world""#),
            vec![vec!["SET", "my key", "hello world"]]
        );
    }

    #[test]
    fn a_quote_inside_a_quoted_argument_is_escaped_rather_than_ending_it() {
        assert_eq!(
            commands(r#"SET k "a \"quoted\" word""#),
            vec![vec!["SET", "k", r#"a "quoted" word"#]]
        );
    }

    #[test]
    fn a_quote_that_is_never_closed_is_refused_rather_than_guessed_at() {
        let err = parse_statement("SELECT 0\nSET k \"unfinished").expect_err("an open quote");
        assert!(err.to_string().contains("quote"), "got: {err}");
        // The second line, and the caret says so.
        assert_eq!(err.statement_position(), Some(10));
    }

    #[test]
    fn an_empty_statement_says_what_one_looks_like() {
        let err = parse_statement("  \n\n").expect_err("nothing to run");
        assert!(err.to_string().contains("SCAN"), "got: {err}");
    }

    #[test]
    fn a_single_line_statement_has_no_position_to_give() {
        // The line number of a one-line statement is always 1, which locates
        // nothing; a caret placed at character one because of it points
        // confidently at the wrong character.
        assert_eq!(caret("SCAN 0 TYPE hash", 1), None);
    }

    #[test]
    fn a_line_number_becomes_the_offset_that_line_starts_at() {
        let text = "SELECT 3\nSCAN 0\nPING";
        assert_eq!(caret(text, 1), Some(1));
        assert_eq!(caret(text, 2), Some(10));
        assert_eq!(caret(text, 3), Some(17));
        assert_eq!(caret(text, 4), None);
    }

    #[test]
    fn an_offset_counts_characters_and_not_bytes() {
        // 訂單 is two characters and six bytes. Counting bytes would put the
        // caret four characters past the line it belongs to — invisible until
        // somebody names a key in a language that is not English.
        let text = "GET 訂單\nWIBBLE";
        assert_eq!(caret(text, 2), Some(8));
        assert_eq!(text.chars().count(), 6 + 1 + 6);
        assert_eq!(text.len(), 10 + 1 + 6);
    }

    fn scan_of(text: &str) -> ScanArgs {
        let lines = parse_statement(text).expect("parses");
        parse_scan(lines.last().expect("a command"), text).expect("a scan")
    }

    #[test]
    fn a_scan_names_the_type_it_reads() {
        let scan = scan_of("SCAN 0 MATCH user:* TYPE hash COUNT 50");
        assert_eq!(scan.cursor, "0");
        assert_eq!(scan.pattern.as_deref(), Some("user:*"));
        assert_eq!(scan.of, Some(KeyType::Hash));
        assert_eq!(scan.count, Some(50));
        assert!(!scan.unknown);
    }

    #[test]
    fn scans_options_are_read_whatever_case_they_are_typed_in() {
        // redis-cli accepts them in any case and so does the server, so a
        // statement that worked when pasted from a terminal has to work here.
        let scan = scan_of("scan 0 match user:* type list");
        assert_eq!(scan.pattern.as_deref(), Some("user:*"));
        assert_eq!(scan.of, Some(KeyType::List));
    }

    #[test]
    fn a_scan_with_no_cursor_is_refused_before_the_server_is_troubled() {
        // The server answers "ERR invalid cursor", which is right but says less
        // than the driver can: the cursor is the one argument SCAN requires.
        let lines = parse_statement("SCAN").expect("parses");
        let err = parse_scan(&lines[0], "SCAN").expect_err("no cursor");
        assert!(err.to_string().contains("SCAN 0"), "got: {err}");
    }

    #[test]
    fn an_option_scan_does_not_have_is_refused_by_name() {
        let lines = parse_statement("SCAN 0 NOVALUES").expect("parses");
        let err = parse_scan(&lines[0], "SCAN 0 NOVALUES").expect_err("not a SCAN option");
        assert!(err.to_string().contains("NOVALUES"), "got: {err}");
    }

    #[test]
    fn a_type_redis_has_that_this_driver_cannot_read_is_a_listing_of_no_rows() {
        // A module's type is a real answer from TYPE and there is no generic way
        // to read one, so the honest listing is an empty one rather than a
        // refusal to ask.
        let scan = scan_of("SCAN 0 TYPE ReJSON-RL");
        assert!(scan.unknown);
        assert_eq!(scan.of, None);
    }

    #[test]
    fn only_the_keyspace_scan_is_treated_as_a_cursor() {
        // HSCAN iterates inside one key. Reading its reply as a page of keys
        // would list a hash's fields as though they were keys of the database.
        let lines = parse_statement("HSCAN h 0").expect("parses");
        assert!(!is_scan(&lines[0]));
        let lines = parse_statement("scan 0").expect("parses");
        assert!(is_scan(&lines[0]));
    }

    #[test]
    fn a_scan_reply_is_a_cursor_and_a_page_of_keys() {
        let reply = Value::Array(vec![
            Value::BulkString(b"17".to_vec()),
            Value::Array(vec![
                Value::BulkString(b"a".to_vec()),
                Value::BulkString(b"b".to_vec()),
            ]),
        ]);
        let (cursor, keys) = split_scan(reply).expect("a scan reply");
        assert_eq!(cursor, "17");
        assert_eq!(keys, vec![b"a".to_vec(), b"b".to_vec()]);
    }
}

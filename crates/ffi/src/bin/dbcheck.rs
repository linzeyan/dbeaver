//! Whether a database this tree tests against can be reached from here.
//!
//! The Makefile's `db-check-*` targets used to ask this in one of two ways, and
//! both answered a different question than the one being asked.
//!
//! `docker exec … ping` runs a client *inside* the container. It says whether the
//! server has finished starting, which is worth knowing, but it never crosses the
//! port forward the tests go through — so a gate built on it reports a healthy
//! server while every test fails to reach it.
//!
//! `nc -z` connects from the host and says nothing much either: Docker's port
//! forwarder accepts the TCP connection itself, so `nc` succeeds whether or not
//! anything is behind it, and the connection it opens carries no bytes to the
//! database.
//!
//! This asks the question the tests ask, by the route the tests take: open the
//! connection through the same registry the application uses, then make a round
//! trip. Either it can be reached from this process on this machine or it cannot,
//! and the reason it cannot is whatever the driver says.
//!
//! Asked repeatedly until a deadline, because the question is being put to a
//! container that was started moments ago and "not yet" is not the same answer
//! as "no".

use std::ffi::{CStr, CString, c_char};
use std::process::ExitCode;
use std::ptr;
use std::time::{Duration, Instant};

/// Long enough for a server under load, short enough that an unreachable one is
/// a failed gate rather than a hung build. The tests behind this gate use their
/// drivers' own defaults; this number governs only the check.
const DEFAULT_TIMEOUT_SECS: u32 = 10;

/// How long to wait before asking a server that just said no again.
///
/// Short enough that a server which becomes ready is used almost at once, long
/// enough that a container mid-startup is not being dialled continuously while
/// it works.
const RETRY_PAUSE: Duration = Duration::from_millis(250);

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(url) = args.next() else {
        eprintln!("usage: dbcheck <url> [timeout-seconds]");
        return ExitCode::from(2);
    };
    let seconds = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    // A URL with a NUL in it is a typo in the Makefile rather than a database
    // that is down, and saying so is more use than reporting it unreachable.
    let Ok(c_url) = CString::new(url) else {
        eprintln!("dbcheck: the connection string contains a NUL byte");
        return ExitCode::from(2);
    };

    match within(Duration::from_secs(u64::from(seconds)), |left| {
        reach(&c_url, left)
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(said) => {
            eprintln!("dbcheck: {said} (still failing after {seconds}s)");
            ExitCode::FAILURE
        }
    }
}

/// Asks until the server answers or the budget runs out, and reports the last
/// refusal.
///
/// The budget covers the whole check rather than each attempt, which is what
/// keeps the promise the constant above makes: a server that is silent spends
/// it in one attempt and nothing is retried, so an unreachable database is
/// still a failed gate and not a hung build.
///
/// Retrying at all is the fix for a race this gate lost in CI. A container is
/// started and then asked immediately, and a server that is still coming up
/// answers a connection with a reset — which is an answer, so the timeout never
/// fires and the whole budget goes unspent. The gate reported an unreachable
/// database from a run whose every test would have passed a second later.
fn within<F>(budget: Duration, mut attempt: F) -> Result<(), String>
where
    F: FnMut(Duration) -> Result<(), String>,
{
    let deadline = Instant::now() + budget;
    loop {
        let said = match attempt(deadline.saturating_duration_since(Instant::now())) {
            Ok(()) => return Ok(()),
            Err(said) => said,
        };
        // Asked before sleeping, so the last thing this does is answer rather
        // than wait out a pause whose attempt would never be made.
        if Instant::now() + RETRY_PAUSE >= deadline {
            return Err(said);
        }
        std::thread::sleep(RETRY_PAUSE);
    }
}

/// One attempt at the question the tests ask, by the route the tests take.
///
/// `left` is what remains of the budget, and it is passed on as the driver's own
/// timeout so that a server which never replies cannot outlast the deadline.
fn reach(url: &CStr, left: Duration) -> Result<(), String> {
    let seconds = attempt_seconds(left);
    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { dbffi::db_connect(url.as_ptr(), std::ptr::null(), seconds, &mut err) };
    if handle.is_null() {
        return Err(format!("could not connect: {}", take(&mut err)));
    }

    // Asked as well as opened, because opening proves less than it looks. Several
    // of these drivers hand back a session before anything has been sent, so a
    // handle on its own says the socket was accepted and not that the database
    // behind it will answer.
    let mut ping_err: *mut c_char = ptr::null_mut();
    let answered = unsafe { dbffi::db_ping(handle, &mut ping_err) } == 0;
    unsafe { dbffi::db_free(handle) };

    if answered {
        Ok(())
    } else {
        Err(format!(
            "connected, but got no answer: {}",
            take(&mut ping_err)
        ))
    }
}

/// The timeout one attempt gives the driver, in the whole seconds `db_connect`
/// takes.
///
/// Never zero. `db_connect` reads zero as "no timeout at all", so a budget worn
/// down to under a second has to ask for one second rather than for none — the
/// deadline overshoots by that second, where asking for none would mean a
/// silent server hangs the build this deadline exists to bound.
fn attempt_seconds(left: Duration) -> u32 {
    u32::try_from(left.as_secs()).unwrap_or(u32::MAX).max(1)
}

/// The message behind `err`, released on the way out.
fn take(err: &mut *mut c_char) -> String {
    if err.is_null() {
        return "no reason given".to_owned();
    }
    let said = unsafe { CStr::from_ptr(*err) }
        .to_string_lossy()
        .into_owned();
    unsafe { dbffi::db_string_free(*err) };
    *err = ptr::null_mut();
    said
}

#[cfg(test)]
mod tests {
    use super::{RETRY_PAUSE, attempt_seconds, within};
    use std::cell::Cell;
    use std::time::Duration;

    /// The case this was written for: a container that refuses now and answers
    /// a moment later. Before the retry the first refusal was the verdict.
    #[test]
    fn a_server_that_answers_on_the_third_try_is_reachable() {
        let tries = Cell::new(0);
        let outcome = within(RETRY_PAUSE * 8, |_| {
            tries.set(tries.get() + 1);
            if tries.get() < 3 {
                Err("connection reset by peer".to_string())
            } else {
                Ok(())
            }
        });
        assert_eq!(outcome, Ok(()));
        assert_eq!(tries.get(), 3, "should have stopped at the first answer");
    }

    /// A database that is genuinely down still fails, and says why.
    ///
    /// The reason reported is the last one, not the first: a server that
    /// refuses the connection while starting and then reports a missing
    /// database has said two different things, and the second is the one
    /// somebody has to act on.
    #[test]
    fn the_reason_reported_is_the_last_one_given() {
        let tries = Cell::new(0);
        let outcome = within(RETRY_PAUSE * 3, |_| {
            tries.set(tries.get() + 1);
            Err(format!("refusal {}", tries.get()))
        });
        assert!(tries.get() > 1, "should have asked more than once");
        assert_eq!(outcome, Err(format!("refusal {}", tries.get())));
        // Paced, not spun. A server that refuses instantly would otherwise be
        // dialled thousands of times while it starts, which is a different way
        // to be in its way.
        assert!(
            tries.get() <= 4,
            "asked {} times in three pauses",
            tries.get()
        );
    }

    /// Under a second left is still a second asked for, because zero is how
    /// `db_connect` is told to wait forever.
    #[test]
    fn an_attempt_never_asks_for_no_timeout_at_all() {
        assert_eq!(attempt_seconds(Duration::ZERO), 1);
        assert_eq!(attempt_seconds(Duration::from_millis(900)), 1);
        assert_eq!(attempt_seconds(Duration::from_secs(7)), 7);
        // Wider than the seconds field: saturating rather than truncating,
        // which would wrap a very long budget round to none at all.
        assert_eq!(attempt_seconds(Duration::from_secs(1 << 40)), u32::MAX);
    }

    /// The budget covers the check and not each attempt, which is what keeps an
    /// unreachable database a failed gate rather than a hung build. A silent
    /// server spends the whole of it inside one attempt, and there is nothing
    /// left to retry with.
    #[test]
    fn an_attempt_that_spends_the_budget_is_not_followed_by_another() {
        let tries = Cell::new(0);
        let budget = RETRY_PAUSE * 2;
        let outcome = within(budget, |_| {
            tries.set(tries.get() + 1);
            std::thread::sleep(budget);
            Err("no answer".to_string())
        });
        assert_eq!(outcome, Err("no answer".to_string()));
        assert_eq!(tries.get(), 1);
    }

    /// And each attempt is told what is left of it, so the timeout it passes to
    /// the driver cannot outlast the deadline either.
    #[test]
    fn every_attempt_is_given_less_of_the_budget_than_the_last() {
        let seen: Cell<Option<Duration>> = Cell::new(None);
        let shrank = Cell::new(true);
        let _ = within(RETRY_PAUSE * 3, |left| {
            if let Some(before) = seen.get()
                && left >= before
            {
                shrank.set(false);
            }
            seen.set(Some(left));
            Err("not yet".to_string())
        });
        assert!(shrank.get(), "the remaining budget should only go down");
    }
}

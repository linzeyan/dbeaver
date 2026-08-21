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

use std::ffi::{CStr, CString, c_char};
use std::process::ExitCode;
use std::ptr;

/// Long enough for a server under load, short enough that an unreachable one is
/// a failed gate rather than a hung build. The tests behind this gate use their
/// drivers' own defaults; this number governs only the check.
const DEFAULT_TIMEOUT_SECS: u32 = 10;

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

    let mut err: *mut c_char = ptr::null_mut();
    let handle = unsafe { dbffi::db_connect(c_url.as_ptr(), std::ptr::null(), seconds, &mut err) };
    if handle.is_null() {
        eprintln!("dbcheck: could not connect: {}", take(&mut err));
        return ExitCode::FAILURE;
    }

    // Asked as well as opened, because opening proves less than it looks. Several
    // of these drivers hand back a session before anything has been sent, so a
    // handle on its own says the socket was accepted and not that the database
    // behind it will answer.
    let mut ping_err: *mut c_char = ptr::null_mut();
    let answered = unsafe { dbffi::db_ping(handle, &mut ping_err) } == 0;
    unsafe { dbffi::db_free(handle) };

    if answered {
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "dbcheck: connected, but got no answer: {}",
            take(&mut ping_err)
        );
        ExitCode::FAILURE
    }
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

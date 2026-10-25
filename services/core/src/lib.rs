//! Kapa core service: the platform's main process. It runs the Gmail module (watch every
//! configured account over IMAP, publish allowed messages, send from any account over SMTP)
//! and keeps the Kapa admin mailbox apart. Core exposes no network port: health and status
//! go through a status file read by `core --healthcheck` and `core --status`.
#![deny(missing_docs, clippy::missing_docs_in_private_items)]

pub mod config;
pub mod gmail;
pub mod imap;
pub mod mail;
pub mod smtp;
pub mod status;
pub mod store;

use std::fmt;

/// Every failure in core.
#[derive(Debug)]
pub enum Error {
    /// Network or file I/O.
    Io(std::io::Error),
    /// A mail server answered with an error.
    Server(String),
    /// A mail server answered something unexpected.
    Protocol(String),
    /// PostgreSQL.
    Db(String),
    /// Invalid configuration or request.
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "i/o: {e}"),
            Self::Server(m) => write!(f, "server: {m}"),
            Self::Protocol(m) => write!(f, "protocol: {m}"),
            Self::Db(m) => write!(f, "database: {m}"),
            Self::Invalid(m) => write!(f, "invalid: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.to_string())
    }
}

/// Result type of core.
pub type Result<T> = std::result::Result<T, Error>;

/// Current time in Unix milliseconds.
#[must_use]
pub fn now_ms() -> i64 {
    let since = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
}

/// Prints a timestamped log line to stderr (core's only logging).
pub fn log(message: &str) {
    eprintln!("{} {message}", mail::rfc3339(now_ms() / 1000));
}

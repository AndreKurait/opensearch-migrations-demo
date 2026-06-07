//! The crate's error type + result alias.
//!
//! Hand-rolled (no `thiserror`/`anyhow`) to match the migration-assistant CLI
//! this harness drives. An [`Error`] carries a human message and a process exit
//! code; the dispatcher prints the message and returns the code.

use std::fmt;

/// The crate result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// A harness error: a message plus the exit code to return from the process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub message: String,
    pub code: i32,
}

impl Error {
    /// An error with an explicit exit code.
    pub fn with_code(message: impl Into<String>, code: i32) -> Self {
        Self {
            message: message.into(),
            code,
        }
    }

    /// A fatal error (exit code 1) — the common "die with a message" case.
    pub fn die(message: impl Into<String>) -> Self {
        Self::with_code(message, 1)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {}

/// `?` on an `io::Error` becomes a fatal harness error.
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::die(e.to_string())
    }
}

/// `?` on a JSON (de)serialization error becomes a fatal harness error.
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::die(format!("json: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn die_is_code_1() {
        assert_eq!(Error::die("x").code, 1);
    }

    #[test]
    fn with_code_preserves_code() {
        let e = Error::with_code("unknown", 64);
        assert_eq!(e.code, 64);
        assert_eq!(e.message, "unknown");
    }

    #[test]
    fn display_is_the_message() {
        assert_eq!(format!("{}", Error::die("boom")), "boom");
    }

    #[test]
    fn io_error_maps_to_die() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "missing");
        let e: Error = io.into();
        assert_eq!(e.code, 1);
        assert!(e.message.contains("missing"));
    }
}

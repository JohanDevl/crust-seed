//! Error types.
//!
//! `CrossSeedError` in the original was an `Error` subclass with its stack
//! deleted, so that user-facing configuration problems print as a bare message
//! instead of a stack trace. [`CrustSeedError`] plays the same role: anything
//! wrapped in it is a problem the *user* can fix, and `main` prints it without
//! a backtrace.

use std::fmt;

#[derive(Debug, Clone)]
pub struct CrustSeedError(pub String);

impl CrustSeedError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for CrustSeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CrustSeedError {}

/// Shorthand for `Err(CrustSeedError::new(format!(...)))`.
#[macro_export]
macro_rules! user_error {
    ($($arg:tt)*) => {
        $crate::errors::CrustSeedError::new(format!($($arg)*))
    };
}

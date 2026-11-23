//! Why a pattern does not compile, or a search does not finish.

use std::fmt;

use thiserror::Error;

// ========================================================================
// Data Structures
// ========================================================================

/// Why a pattern does not compile, or a search does not finish.
#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum Error {
    /// A search backtracked more times than its limit allows: the pattern
    /// may still match, but finding out costs too much.
    #[error("backtrack limit exceeded")]
    BacktrackLimit,
    /// The pattern is not valid in Oniguruma's syntax.
    #[error("{0}")]
    Syntax(PatternError),
    /// The pattern compiles to a program larger than its limit allows.
    #[error("compiled pattern exceeds the size limit")]
    TooBig,
    /// The pattern uses a construct reox does not support yet.
    #[error("unsupported: {0}")]
    Unsupported(PatternError),
}

/// What is wrong in a pattern, and where.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternError {
    /// What is wrong, in Oniguruma's words where it has them.
    message: Box<str>,
    /// Where in the pattern, in bytes.
    offset: usize,
}

// ========================================================================
// PatternError Implementations
// ========================================================================

impl PatternError {
    /// The error `message` at byte `offset` of the pattern.
    pub(crate) fn new(message: impl Into<Box<str>>, offset: usize) -> Self {
        Self {
            message: message.into(),
            offset,
        }
    }

    /// What is wrong.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Where in the pattern, in bytes.
    pub fn offset(&self) -> usize {
        self.offset
    }
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at offset {}", self.message, self.offset)
    }
}

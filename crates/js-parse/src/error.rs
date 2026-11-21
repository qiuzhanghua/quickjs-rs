//! Lexical errors.

use crate::token::Span;
use std::fmt;

/// A syntax error produced while scanning.
///
/// The message vocabulary follows the upstream QuickJS parser wherever a test
/// observes it (`unexpected character`, `unterminated string literal`, …) so
/// that the differential harness can compare messages once the parser lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LexError {
    /// Human readable message (no position prefix).
    pub message: String,
    /// Where the error happened.
    pub span: Span,
}

impl LexError {
    /// Build an error at `span`.
    pub fn new(message: impl Into<String>, span: Span) -> LexError {
        LexError {
            message: message.into(),
            span,
        }
    }

    /// 1-based line of the error.
    pub fn line(&self) -> u32 {
        self.span.start.line
    }

    /// 1-based column of the error.
    pub fn column(&self) -> u32 {
        self.span.start.column
    }
}

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line(), self.column(), self.message)
    }
}

impl std::error::Error for LexError {}

/// Result type used by the lexer.
pub type Result<T> = std::result::Result<T, LexError>;

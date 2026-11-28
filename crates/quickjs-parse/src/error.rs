//! Lexical errors.

use crate::token::Span;
use std::fmt;

/// A syntax error produced while scanning or parsing.
///
/// The message vocabulary follows the upstream QuickJS parser wherever a test
/// observes it (`unexpected character`, `unterminated string literal`, …) so
/// that the differential harness can compare messages once the parser lands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyntaxError {
    /// Human readable message (no position prefix).
    pub message: String,
    /// Where the error happened.
    pub span: Span,
    /// Whether the reference reports this error *without* a position.
    ///
    /// Almost every syntax error is printed as `SyntaxError: <message>` followed by
    /// `    at <file>:<line>:<column>`. A handful are not, and the difference is not
    /// cosmetic: the reference raises them from `js_resolve_module`, after parsing has
    /// succeeded, so there is no position to give —
    /// `export {a}` with no `a` prints a bare `SyntaxError: exported variable 'a' does
    /// not exist`. Our parser makes that check itself (it has the declared names in
    /// hand), so it has to say "report me like a link error" explicitly.
    pub positionless: bool,
}

impl LexError {
    /// Build an error at `span`.
    pub fn new(message: impl Into<String>, span: Span) -> LexError {
        LexError {
            message: message.into(),
            span,
            positionless: false,
        }
    }

    /// Mark this error as one the reference reports without a position.
    ///
    /// See [`SyntaxError::positionless`].
    pub fn positionless(mut self) -> LexError {
        self.positionless = true;
        self
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

/// Lexical errors share the syntax error type: the engine reports both as
/// `SyntaxError`.
pub type LexError = SyntaxError;

/// Parser errors share the syntax error type.
pub type ParseError = SyntaxError;

impl fmt::Display for SyntaxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.line(), self.column(), self.message)
    }
}

impl std::error::Error for SyntaxError {}

/// Result type used by the lexer and the parser.
pub type Result<T> = std::result::Result<T, SyntaxError>;

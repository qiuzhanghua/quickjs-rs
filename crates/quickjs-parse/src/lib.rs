//! `quickjs-parse` — ECMAScript 2025 lexer, AST and parser.
//!
//! Milestone status: **the lexer is complete (M1a)**; the AST and parser are the
//! next step and do not exist yet.
//!
//! ```no_run
//! use js_parse::{Lexer, TokenKind};
//!
//! let mut lexer = Lexer::new("let answer = 0x2a + 1_000;");
//! let mut kinds = Vec::new();
//! loop {
//!     let token = lexer.next_token().expect("valid source");
//!     let done = matches!(token.kind, TokenKind::Eof);
//!     kinds.push(token.kind);
//!     if done {
//!         break;
//!     }
//! }
//! assert_eq!(kinds.len(), 8);
//! ```
//!
//! # Known limitation
//!
//! String values are Rust `String`s, so a lone surrogate (`"\ud800"`) cannot be
//! represented yet: it is decoded to U+FFFD. QuickJS keeps UTF-16 code units
//! and reports `"\ud800".length === 1`, so the string representation must gain
//! WTF-8 (or UTF-16) support in the `js-value` milestone. This is tracked as a
//! `TODO(js-value)` and only affects lone surrogates, never well-formed text.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub mod ast;
mod error;
mod lexer;
pub mod parser;
mod token;
mod unicode_id;

pub use error::{LexError, ParseError, Result, SyntaxError};
pub use lexer::Lexer;
pub use parser::{
    EvalContext, Parser, parse_eval_script, parse_eval_script_in, parse_expression, parse_module,
    parse_script, parse_script_strict,
};

/// Parser configuration constants and helpers.
pub mod parser_limits {
    pub use crate::parser::DEFAULT_MAX_DEPTH;
}
pub use token::{
    BigIntLiteral, Keyword, NumberLiteral, Position, Punct, Span, TemplateKind, TemplateToken,
    Token, TokenKind,
};

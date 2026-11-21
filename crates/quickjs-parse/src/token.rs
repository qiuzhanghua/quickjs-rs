//! Token definitions for ECMAScript 2025.
//!
//! The lexer keeps the token stream small and borrowed-free: identifiers and
//! strings own their decoded value, numeric literals keep enough information for
//! `js-value` to build a `Number`/`BigInt`, and template literals carry both the
//! cooked (escape-expanded) and raw source text, because tagged templates need
//! the raw form and `undefined`-cooked chunks when an escape is invalid.

use std::fmt;

/// Byte offset, line (1-based) and column (1-based) of a source position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Position {
    /// Byte offset from the start of the source.
    pub offset: u32,
    /// 1-based line number.
    pub line: u32,
    /// 1-based column, counted in UTF-16 code units like a JS engine's
    /// `SyntaxError` positions.
    pub column: u32,
}

impl Position {
    /// A position at the very start of a source.
    pub const START: Position = Position {
        offset: 0,
        line: 1,
        column: 1,
    };
}

/// Half-open source range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    /// First covered position.
    pub start: Position,
    /// One past the last covered position.
    pub end: Position,
}

impl Span {
    /// A zero-width span at the very start of a source.
    pub const fn start() -> Span {
        Span {
            start: Position::START,
            end: Position::START,
        }
    }

    /// Create a span from two positions.
    pub fn new(start: Position, end: Position) -> Span {
        Span { start, end }
    }
}

/// A reserved word that is a keyword in every context (ES2025 §12.7.2), plus
/// `enum` (always reserved).
///
/// Context-sensitive words (`let`, `static`, `yield`, `await`, `async`, `of`,
/// `get`, `set`, `from`, `as`, `target`, …) are *not* keywords here: they are
/// identifiers whose meaning the parser decides, exactly like an engine does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Keyword {
    /// `break`
    Break,
    /// `case`
    Case,
    /// `catch`
    Catch,
    /// `class`
    Class,
    /// `const`
    Const,
    /// `continue`
    Continue,
    /// `debugger`
    Debugger,
    /// `default`
    Default,
    /// `delete`
    Delete,
    /// `do`
    Do,
    /// `else`
    Else,
    /// `enum` (reserved in all modes)
    Enum,
    /// `export`
    Export,
    /// `extends`
    Extends,
    /// `false`
    False,
    /// `finally`
    Finally,
    /// `for`
    For,
    /// `function`
    Function,
    /// `if`
    If,
    /// `import`
    Import,
    /// `in`
    In,
    /// `instanceof`
    Instanceof,
    /// `new`
    New,
    /// `null`
    Null,
    /// `return`
    Return,
    /// `super`
    Super,
    /// `switch`
    Switch,
    /// `this`
    This,
    /// `throw`
    Throw,
    /// `true`
    True,
    /// `try`
    Try,
    /// `typeof`
    Typeof,
    /// `var`
    Var,
    /// `void`
    Void,
    /// `while`
    While,
    /// `with`
    With,
}

impl Keyword {
    /// Look up a reserved word.
    pub fn lookup(s: &str) -> Option<Keyword> {
        use Keyword::*;
        Some(match s {
            "break" => Break,
            "case" => Case,
            "catch" => Catch,
            "class" => Class,
            "const" => Const,
            "continue" => Continue,
            "debugger" => Debugger,
            "default" => Default,
            "delete" => Delete,
            "do" => Do,
            "else" => Else,
            "enum" => Enum,
            "export" => Export,
            "extends" => Extends,
            "false" => False,
            "finally" => Finally,
            "for" => For,
            "function" => Function,
            "if" => If,
            "import" => Import,
            "in" => In,
            "instanceof" => Instanceof,
            "new" => New,
            "null" => Null,
            "return" => Return,
            "super" => Super,
            "switch" => Switch,
            "this" => This,
            "throw" => Throw,
            "true" => True,
            "try" => Try,
            "typeof" => Typeof,
            "var" => Var,
            "void" => Void,
            "while" => While,
            "with" => With,
            _ => return None,
        })
    }

    /// The source text of the keyword.
    pub fn as_str(self) -> &'static str {
        use Keyword::*;
        match self {
            Break => "break",
            Case => "case",
            Catch => "catch",
            Class => "class",
            Const => "const",
            Continue => "continue",
            Debugger => "debugger",
            Default => "default",
            Delete => "delete",
            Do => "do",
            Else => "else",
            Enum => "enum",
            Export => "export",
            Extends => "extends",
            False => "false",
            Finally => "finally",
            For => "for",
            Function => "function",
            If => "if",
            Import => "import",
            In => "in",
            Instanceof => "instanceof",
            New => "new",
            Null => "null",
            Return => "return",
            Super => "super",
            Switch => "switch",
            This => "this",
            Throw => "throw",
            True => "true",
            Try => "try",
            Typeof => "typeof",
            Var => "var",
            Void => "void",
            While => "while",
            With => "with",
        }
    }

    /// Whether the word is a *future reserved word in strict mode* only
    /// (ES2025 §12.7.2): legal as an identifier in sloppy code.
    pub fn is_strict_reserved(word: &str) -> bool {
        matches!(
            word,
            "implements"
                | "interface"
                | "let"
                | "package"
                | "private"
                | "protected"
                | "public"
                | "static"
                | "yield"
        )
    }
}

impl fmt::Display for Keyword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Every punctuator in ECMAScript 2025, longest-first where it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(missing_docs)]
pub enum Punct {
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Semicolon,
    Comma,
    Lt,
    Gt,
    LtEq,
    GtEq,
    EqEq,
    NotEq,
    EqEqEq,
    NotEqEq,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    StarStar,
    PlusPlus,
    MinusMinus,
    Shl,
    Shr,
    UShr,
    Amp,
    Pipe,
    Caret,
    Bang,
    Tilde,
    AmpAmp,
    PipePipe,
    QuestionQuestion,
    Question,
    Colon,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    StarStarEq,
    ShlEq,
    ShrEq,
    UShrEq,
    AmpEq,
    PipeEq,
    CaretEq,
    AmpAmpEq,
    PipePipeEq,
    QuestionQuestionEq,
    Arrow,
    Ellipsis,
    Dot,
    QuestionDot,
    Hash,
}

impl Punct {
    /// The source text of the punctuator.
    pub fn as_str(self) -> &'static str {
        use Punct::*;
        match self {
            LBrace => "{",
            RBrace => "}",
            LParen => "(",
            RParen => ")",
            LBracket => "[",
            RBracket => "]",
            Semicolon => ";",
            Comma => ",",
            Lt => "<",
            Gt => ">",
            LtEq => "<=",
            GtEq => ">=",
            EqEq => "==",
            NotEq => "!=",
            EqEqEq => "===",
            NotEqEq => "!==",
            Plus => "+",
            Minus => "-",
            Star => "*",
            Slash => "/",
            Percent => "%",
            StarStar => "**",
            PlusPlus => "++",
            MinusMinus => "--",
            Shl => "<<",
            Shr => ">>",
            UShr => ">>>",
            Amp => "&",
            Pipe => "|",
            Caret => "^",
            Bang => "!",
            Tilde => "~",
            AmpAmp => "&&",
            PipePipe => "||",
            QuestionQuestion => "??",
            Question => "?",
            Colon => ":",
            Eq => "=",
            PlusEq => "+=",
            MinusEq => "-=",
            StarEq => "*=",
            SlashEq => "/=",
            PercentEq => "%=",
            StarStarEq => "**=",
            ShlEq => "<<=",
            ShrEq => ">>=",
            UShrEq => ">>>=",
            AmpEq => "&=",
            PipeEq => "|=",
            CaretEq => "^=",
            AmpAmpEq => "&&=",
            PipePipeEq => "||=",
            QuestionQuestionEq => "??=",
            Arrow => "=>",
            Ellipsis => "...",
            Dot => ".",
            QuestionDot => "?.",
            Hash => "#",
        }
    }

    /// Whether the punctuator ends a statement for Automatic Semicolon
    /// Insertion purposes (used by the parser; `}` is handled separately).
    pub fn is_asi_boundary(self) -> bool {
        matches!(self, Punct::Semicolon | Punct::RBrace)
    }
}

/// The value of a numeric literal.
#[derive(Debug, Clone, PartialEq)]
pub enum NumberLiteral {
    /// Integer that fits in `i32` (and in 53 bits of precision): kept exact.
    Int(i32),
    /// Any other double.
    Float(f64),
}

/// A BigInt literal: the source digits and their radix, kept raw so that
/// `js-value` can build the arbitrary-precision integer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BigIntLiteral {
    /// Digits without prefix, separators or the `n` suffix.
    pub digits: String,
    /// 2, 8, 10 or 16.
    pub radix: u32,
}

/// Which part of a template literal a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    /// `` `text` `` — no substitution.
    NoSubstitution,
    /// `` `text${ `` — the template continues after the substitution.
    Head,
    /// `}text${` — middle chunk.
    Middle,
    /// `` }text` `` — last chunk.
    Tail,
}

/// A template literal chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateToken {
    /// Which chunk this is.
    pub kind: TemplateKind,
    /// Escape-expanded text, `None` for an invalid escape sequence (only legal
    /// in tagged templates, where `cooked` is `undefined`).
    pub cooked: Option<String>,
    /// Source text between the delimiters, verbatim.
    pub raw: String,
}

/// A lexical token.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// End of input.
    Eof,
    /// Identifier (including context-sensitive words like `let`/`await`).
    Ident(String),
    /// Reserved word.
    Keyword(Keyword),
    /// Numeric literal.
    Number(NumberLiteral),
    /// BigInt literal.
    BigInt(BigIntLiteral),
    /// String literal with escapes already decoded.
    String(String),
    /// Regular expression literal (needs a re-scan from `/`).
    RegExp {
        /// Pattern source, without the slashes.
        pattern: String,
        /// Flag letters, without validation (the compiler validates them).
        flags: String,
    },
    /// Template literal chunk.
    Template(TemplateToken),
    /// Punctuator.
    Punct(Punct),
}

impl TokenKind {
    /// A short human readable description, used in parser error messages.
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Eof => "end of input".to_string(),
            TokenKind::Ident(name) => format!("identifier '{name}'"),
            TokenKind::Keyword(kw) => format!("'{kw}'"),
            TokenKind::Number(n) => match n {
                NumberLiteral::Int(v) => format!("number '{v}'"),
                NumberLiteral::Float(v) => format!("number '{v}'"),
            },
            TokenKind::BigInt(b) => format!("BigInt literal '{}n'", b.digits),
            TokenKind::String(_) => "string literal".to_string(),
            TokenKind::RegExp { .. } => "regular expression".to_string(),
            TokenKind::Template(_) => "template literal".to_string(),
            TokenKind::Punct(p) => format!("'{}'", p.as_str()),
        }
    }

    /// If this is a punctuator, return it.
    pub fn as_punct(&self) -> Option<Punct> {
        match self {
            TokenKind::Punct(p) => Some(*p),
            _ => None,
        }
    }

    /// If this is a keyword, return it.
    pub fn as_keyword(&self) -> Option<Keyword> {
        match self {
            TokenKind::Keyword(kw) => Some(*kw),
            _ => None,
        }
    }

    /// Whether the token starts an expression (used to decide `/` == division
    /// versus regex in error recovery paths).
    pub fn is_identifier(&self) -> bool {
        matches!(self, TokenKind::Ident(_))
    }
}

/// A token plus where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Token payload.
    pub kind: TokenKind,
    /// Source range of the token.
    pub span: Span,
    /// Whether a line terminator appeared between the previous token and this
    /// one (drives Automatic Semicolon Insertion and restricted productions).
    pub newline_before: bool,
}

impl Token {
    /// Whether this token ends a statement, for ASI.
    pub fn is_eof(&self) -> bool {
        matches!(self.kind, TokenKind::Eof)
    }
}

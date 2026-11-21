//! The lexer: ECMAScript 2025 source text to [`Token`]s.
//!
//! Two productions cannot be decided by a lexer alone and are therefore
//! re-scanned on demand, exactly like upstream `js_parse_regexp` /
//! `js_parse_template_part`:
//!
//! * a `/` is a division punctuator until the parser asks for a regular
//!   expression literal with [`Lexer::re_scan_regex`];
//! * `}` closes a block or resumes a template literal, decided by
//!   [`Lexer::re_scan_template_continue`].
//!
//! Positions are byte offsets plus 1-based line/UTF-16 column numbers, which is
//! what `SyntaxError` locations report.

use crate::error::{LexError, Result};
use crate::token::{
    BigIntLiteral, Keyword, NumberLiteral, Position, Punct, Span, TemplateKind, TemplateToken,
    Token, TokenKind,
};
use crate::unicode_id::{is_id_continue, is_id_start};

/// A streaming lexer over one source text.
pub struct Lexer<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    line: u32,
    column: u32,
    strict: bool,
}

/// Every punctuator, longest first so that a prefix test picks the longest form.
const PUNCTUATORS: &[(&str, Punct)] = &[
    (">>>=", Punct::UShrEq),
    ("...", Punct::Ellipsis),
    ("===", Punct::EqEqEq),
    ("!==", Punct::NotEqEq),
    ("**=", Punct::StarStarEq),
    ("<<=", Punct::ShlEq),
    (">>=", Punct::ShrEq),
    (">>>", Punct::UShr),
    ("&&=", Punct::AmpAmpEq),
    ("||=", Punct::PipePipeEq),
    ("??=", Punct::QuestionQuestionEq),
    ("=>", Punct::Arrow),
    ("==", Punct::EqEq),
    ("!=", Punct::NotEq),
    ("<=", Punct::LtEq),
    (">=", Punct::GtEq),
    ("+=", Punct::PlusEq),
    ("-=", Punct::MinusEq),
    ("*=", Punct::StarEq),
    ("/=", Punct::SlashEq),
    ("%=", Punct::PercentEq),
    ("**", Punct::StarStar),
    ("++", Punct::PlusPlus),
    ("--", Punct::MinusMinus),
    ("<<", Punct::Shl),
    (">>", Punct::Shr),
    ("&&", Punct::AmpAmp),
    ("||", Punct::PipePipe),
    ("??", Punct::QuestionQuestion),
    ("?.", Punct::QuestionDot),
    ("{", Punct::LBrace),
    ("}", Punct::RBrace),
    ("(", Punct::LParen),
    (")", Punct::RParen),
    ("[", Punct::LBracket),
    ("]", Punct::RBracket),
    (";", Punct::Semicolon),
    (",", Punct::Comma),
    ("<", Punct::Lt),
    (">", Punct::Gt),
    ("+", Punct::Plus),
    ("-", Punct::Minus),
    ("*", Punct::Star),
    ("/", Punct::Slash),
    ("%", Punct::Percent),
    ("&", Punct::Amp),
    ("|", Punct::Pipe),
    ("^", Punct::Caret),
    ("!", Punct::Bang),
    ("~", Punct::Tilde),
    ("?", Punct::Question),
    (":", Punct::Colon),
    ("=", Punct::Eq),
    (".", Punct::Dot),
    ("#", Punct::Hash),
];

impl<'a> Lexer<'a> {
    /// Create a lexer positioned at the beginning of `src`.
    pub fn new(src: &'a str) -> Lexer<'a> {
        Lexer {
            src,
            bytes: src.as_bytes(),
            pos: 0,
            line: 1,
            column: 1,
            strict: false,
        }
    }

    /// Enable strict-mode lexical rules (octal escapes become syntax errors).
    pub fn set_strict(&mut self, strict: bool) {
        self.strict = strict;
    }

    /// Whether strict-mode lexical rules are active.
    pub fn is_strict(&self) -> bool {
        self.strict
    }

    /// Current position of the cursor.
    pub fn position(&self) -> Position {
        Position {
            offset: self.pos as u32,
            line: self.line,
            column: self.column,
        }
    }

    /// The source text being lexed.
    pub fn source(&self) -> &'a str {
        self.src
    }

    /// Scan the next token.
    pub fn next_token(&mut self) -> Result<Token> {
        let newline_before = self.skip_trivia()?;
        let start = self.position();
        let Some(c) = self.peek_char() else {
            return Ok(Token {
                kind: TokenKind::Eof,
                span: Span::new(start, start),
                newline_before,
            });
        };
        let kind = self.scan_token(c)?;
        Ok(Token {
            kind,
            span: Span::new(start, self.position()),
            newline_before,
        })
    }

    /// Scan every remaining token (tests and tooling; the parser drives the
    /// lexer one token at a time).
    pub fn tokenize(mut self) -> Result<Vec<Token>> {
        let mut out = Vec::new();
        loop {
            let token = self.next_token()?;
            let done = token.is_eof();
            out.push(token);
            if done {
                return Ok(out);
            }
        }
    }

    // -- cursor ------------------------------------------------------------

    fn peek_char(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn peek_char_at(&self, offset: usize) -> Option<char> {
        self.src.get(self.pos + offset..)?.chars().next()
    }

    fn peek_byte(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek_char()?;
        self.pos += c.len_utf8();
        match c {
            '\n' | '\u{2028}' | '\u{2029}' => {
                self.line += 1;
                self.column = 1;
            }
            '\r' => {
                // `\r\n` counts as a single line terminator: the `\n` half is
                // absorbed here so callers never see a double increment.
                if self.bytes.get(self.pos) == Some(&b'\n') {
                    self.pos += 1;
                }
                self.line += 1;
                self.column = 1;
            }
            _ => self.column += c.len_utf16() as u32,
        }
        Some(c)
    }

    fn reset_to(&mut self, position: Position) {
        self.pos = position.offset as usize;
        self.line = position.line;
        self.column = position.column;
    }

    fn error(&self, message: impl Into<String>, start: Position) -> LexError {
        LexError::new(message, Span::new(start, self.position()))
    }

    // -- trivia ------------------------------------------------------------

    /// Skip whitespace, line terminators and comments; report whether a line
    /// terminator was crossed.
    fn skip_trivia(&mut self) -> Result<bool> {
        let mut newline = false;
        // A hashbang line is only allowed at offset 0 (Annex B / ES2023).
        if self.pos == 0 && self.src.starts_with("#!") {
            while let Some(c) = self.peek_char() {
                if is_line_terminator(c) {
                    break;
                }
                self.bump();
            }
        }
        loop {
            let Some(c) = self.peek_char() else {
                return Ok(newline);
            };
            match c {
                ' ' | '\t' | '\u{0b}' | '\u{0c}' => {
                    self.bump();
                }
                c if is_line_terminator(c) => {
                    newline = true;
                    self.bump();
                }
                c if c.is_whitespace() || c == '\u{feff}' => {
                    // Unicode space separators (`Zs`) and the BOM.
                    self.bump();
                }
                '/' => match self.peek_char_at(1) {
                    Some('/') => {
                        self.bump();
                        self.bump();
                        while let Some(c) = self.peek_char() {
                            if is_line_terminator(c) {
                                break;
                            }
                            self.bump();
                        }
                    }
                    Some('*') => {
                        let start = self.position();
                        self.bump();
                        self.bump();
                        loop {
                            match self.peek_char() {
                                None => return Err(self.error("unterminated comment", start)),
                                Some('*') if self.peek_char_at(1) == Some('/') => {
                                    self.bump();
                                    self.bump();
                                    break;
                                }
                                Some(c) => {
                                    if is_line_terminator(c) {
                                        newline = true;
                                    }
                                    self.bump();
                                }
                            }
                        }
                    }
                    _ => return Ok(newline),
                },
                _ => return Ok(newline),
            }
        }
    }

    // -- tokens ------------------------------------------------------------

    fn scan_token(&mut self, c: char) -> Result<TokenKind> {
        if is_id_start(c) || c == '\\' {
            return self.scan_identifier();
        }
        if c.is_ascii_digit() {
            return self.scan_number();
        }
        match c {
            '.' if self.peek_char_at(1).is_some_and(|d| d.is_ascii_digit()) => self.scan_number(),
            '"' | '\'' => self.scan_string(c),
            '`' => {
                let start = self.position();
                self.bump();
                let chunk = self.scan_template_chunk(start, true)?;
                Ok(TokenKind::Template(chunk))
            }
            _ => self.scan_punct(c),
        }
    }

    fn scan_punct(&mut self, c: char) -> Result<TokenKind> {
        let start = self.position();
        for (text, punct) in PUNCTUATORS {
            if !self.src[self.pos..].starts_with(text) {
                continue;
            }
            // `?.` is only a punctuator when the next character is not a
            // decimal digit (`a?.5:b` means `a ? .5 : b`).
            if *punct == Punct::QuestionDot
                && self.peek_char_at(2).is_some_and(|d| d.is_ascii_digit())
            {
                continue;
            }
            for _ in 0..text.len() {
                self.bump();
            }
            return Ok(TokenKind::Punct(*punct));
        }
        Err(self.error(
            format!("unexpected character '{}'", c.escape_debug()),
            start,
        ))
    }

    // -- identifiers -------------------------------------------------------

    fn scan_identifier(&mut self) -> Result<TokenKind> {
        let start = self.position();
        let mut name = String::new();
        let mut first = true;
        while let Some(c) = self.peek_char() {
            let ch = if c == '\\' {
                if self.peek_char_at(1) != Some('u') {
                    return Err(self.error("invalid escape sequence in identifier", start));
                }
                self.bump();
                self.bump();
                let escaped = self.scan_unicode_escape_body(start)?;
                let ok = if first {
                    is_id_start(escaped)
                } else {
                    is_id_continue(escaped)
                };
                if !ok {
                    return Err(self.error("invalid Unicode escape sequence", start));
                }
                escaped
            } else if (first && is_id_start(c)) || (!first && is_id_continue(c)) {
                self.bump();
                c
            } else {
                break;
            };
            name.push(ch);
            first = false;
        }
        if name.is_empty() {
            return Err(self.error("unexpected character", start));
        }
        if let Some(keyword) = Keyword::lookup(&name) {
            return Ok(TokenKind::Keyword(keyword));
        }
        Ok(TokenKind::Ident(name))
    }

    // -- numbers -----------------------------------------------------------

    fn scan_number(&mut self) -> Result<TokenKind> {
        let start = self.position();
        // Radix prefixes.
        if self.peek_byte() == Some(b'0') {
            match self.bytes.get(self.pos + 1).map(u8::to_ascii_lowercase) {
                Some(b'x') | Some(b'o') | Some(b'b') => {
                    let radix = match self.bytes[self.pos + 1].to_ascii_lowercase() {
                        b'x' => 16,
                        b'o' => 8,
                        _ => 2,
                    };
                    self.bump();
                    self.bump();
                    let mut digits = String::new();
                    let count = self.scan_digits(radix, &mut digits, start)?;
                    if count == 0 {
                        return Err(self.error("invalid number literal", start));
                    }
                    return self.finish_radix_literal(digits, radix, start);
                }
                Some(b'0'..=b'9') => return self.scan_legacy_number(start),
                _ => {}
            }
        }

        let mut digits = String::new();
        let mut has_fraction = false;
        let mut has_exponent = false;

        // Integer part (may be empty for `.5`).
        if self.peek_char() != Some('.') {
            let count = self.scan_digits(10, &mut digits, start)?;
            if count == 0 {
                return Err(self.error("invalid number literal", start));
            }
        }
        if self.peek_char() == Some('.') {
            has_fraction = true;
            digits.push('.');
            self.bump();
            let _ = self.scan_digits(10, &mut digits, start)?;
        }
        if matches!(self.peek_char(), Some('e') | Some('E')) {
            has_exponent = true;
            digits.push('e');
            self.bump();
            if matches!(self.peek_char(), Some('+') | Some('-')) {
                let sign = self.peek_char().expect("checked above");
                digits.push(sign);
                self.bump();
            }
            let mut exponent = String::new();
            let count = self.scan_digits(10, &mut exponent, start)?;
            if count == 0 {
                return Err(self.error("invalid number literal", start));
            }
            digits.push_str(&exponent);
        }
        if self.peek_char() == Some('n') {
            if has_fraction || has_exponent {
                return Err(self.error("invalid BigInt literal", start));
            }
            self.bump();
            self.ensure_literal_boundary(start)?;
            return Ok(TokenKind::BigInt(BigIntLiteral { digits, radix: 10 }));
        }
        self.ensure_literal_boundary(start)?;
        Ok(TokenKind::Number(number_from_decimal(&digits)))
    }

    /// `0` followed by more digits: legacy octal (`017`) or a
    /// NonOctalDecimalIntegerLiteral (`019`), both Annex B.
    fn scan_legacy_number(&mut self, start: Position) -> Result<TokenKind> {
        let mut text = String::new();
        let mut all_octal = true;
        while let Some(c) = self.peek_char() {
            if !c.is_ascii_digit() {
                break;
            }
            if c > '7' {
                all_octal = false;
            }
            text.push(c);
            self.bump();
        }
        if matches!(
            self.peek_char(),
            Some('.') | Some('e') | Some('E') | Some('_')
        ) {
            // `08.5`, `08e1`, `0_1` are ordinary decimal literals / errors; hand
            // back to the general scanner by re-reading the leading `0`.
            self.reset_to(start);
            self.bump();
            let mut digits = String::from("0");
            let mut rest = String::new();
            self.scan_digits(10, &mut rest, start)?;
            digits.push_str(&rest);
            let mut has_fraction = false;
            let mut has_exponent = false;
            if self.peek_char() == Some('.') {
                has_fraction = true;
                digits.push('.');
                self.bump();
                self.scan_digits(10, &mut digits, start)?;
            }
            if matches!(self.peek_char(), Some('e') | Some('E')) {
                has_exponent = true;
                digits.push('e');
                self.bump();
                if matches!(self.peek_char(), Some('+') | Some('-')) {
                    let sign = self.peek_char().expect("checked above");
                    digits.push(sign);
                    self.bump();
                }
                let mut exponent = String::new();
                let count = self.scan_digits(10, &mut exponent, start)?;
                if count == 0 {
                    return Err(self.error("invalid number literal", start));
                }
                digits.push_str(&exponent);
            }
            if self.peek_char() == Some('n') {
                if has_fraction || has_exponent || text.len() > 1 {
                    return Err(self.error("invalid BigInt literal", start));
                }
                self.bump();
                return Ok(TokenKind::BigInt(BigIntLiteral { digits, radix: 10 }));
            }
            self.ensure_literal_boundary(start)?;
            return Ok(TokenKind::Number(number_from_decimal(&digits)));
        }
        if self.peek_char() == Some('n') {
            // `0n` is legal, `01n` / `08n` are not.
            if text.len() > 1 {
                return Err(self.error("invalid BigInt literal", start));
            }
            self.bump();
            return Ok(TokenKind::BigInt(BigIntLiteral {
                digits: "0".to_string(),
                radix: 10,
            }));
        }
        self.ensure_literal_boundary(start)?;
        let value = if all_octal {
            radix_digits_to_f64(&text, 8)
        } else {
            radix_digits_to_f64(&text, 10)
        };
        Ok(TokenKind::Number(number_from_f64(value)))
    }

    fn finish_radix_literal(
        &mut self,
        digits: String,
        radix: u32,
        start: Position,
    ) -> Result<TokenKind> {
        if self.peek_char() == Some('n') {
            self.bump();
            self.ensure_literal_boundary(start)?;
            return Ok(TokenKind::BigInt(BigIntLiteral { digits, radix }));
        }
        self.ensure_literal_boundary(start)?;
        Ok(TokenKind::Number(number_from_f64(radix_digits_to_f64(
            &digits, radix,
        ))))
    }

    /// Scan digits of `radix`, accepting `_` separators. Returns the digit count.
    fn scan_digits(&mut self, radix: u32, out: &mut String, start: Position) -> Result<usize> {
        let mut count = 0;
        let mut last_was_separator = false;
        loop {
            match self.peek_char() {
                Some('_') => {
                    if count == 0 || last_was_separator {
                        return Err(self.error("invalid numeric separator", start));
                    }
                    last_was_separator = true;
                    self.bump();
                }
                Some(c) if c.is_digit(radix) && c.is_ascii() => {
                    out.push(c);
                    count += 1;
                    last_was_separator = false;
                    self.bump();
                }
                _ => break,
            }
        }
        if last_was_separator {
            return Err(self.error("invalid numeric separator", start));
        }
        Ok(count)
    }

    /// A numeric literal may not be directly followed by an identifier start or
    /// a digit (`1abc`, `3in`).
    fn ensure_literal_boundary(&self, start: Position) -> Result<()> {
        match self.peek_char() {
            Some(c) if is_id_start(c) || c.is_ascii_digit() => {
                Err(self.error("invalid number literal", start))
            }
            _ => Ok(()),
        }
    }

    // -- strings -----------------------------------------------------------

    fn scan_string(&mut self, quote: char) -> Result<TokenKind> {
        let start = self.position();
        self.bump(); // opening quote
        let mut out = String::new();
        loop {
            let Some(c) = self.peek_char() else {
                return Err(self.error("unterminated string literal", start));
            };
            if c == quote {
                self.bump();
                return Ok(TokenKind::String(out));
            }
            if is_line_terminator(c) {
                return Err(self.error("unterminated string literal", start));
            }
            if c == '\\' {
                self.bump();
                self.scan_escape(&mut out, start, EscapeContext::String)?;
            } else {
                out.push(c);
                self.bump();
            }
        }
    }

    /// Scan the body of a `\u` escape (the `\u` has already been consumed).
    fn scan_unicode_escape_body(&mut self, start: Position) -> Result<char> {
        if self.peek_char() == Some('{') {
            self.bump();
            let mut value: u32 = 0;
            let mut digits = 0;
            loop {
                match self.peek_char() {
                    Some('}') => {
                        self.bump();
                        break;
                    }
                    Some(c) if c.is_ascii_hexdigit() => {
                        value = value
                            .checked_mul(16)
                            .and_then(|v| v.checked_add(c.to_digit(16).expect("hex digit")))
                            .ok_or_else(|| self.error("malformed Unicode escape", start))?;
                        if value > 0x10FFFF {
                            return Err(self.error("malformed Unicode escape", start));
                        }
                        digits += 1;
                        self.bump();
                    }
                    _ => return Err(self.error("malformed Unicode escape", start)),
                }
            }
            if digits == 0 {
                return Err(self.error("malformed Unicode escape", start));
            }
            return char::from_u32(value)
                .ok_or_else(|| self.error("malformed Unicode escape", start));
        }
        let mut value: u32 = 0;
        for _ in 0..4 {
            match self.peek_char() {
                Some(c) if c.is_ascii_hexdigit() => {
                    value = value * 16 + c.to_digit(16).expect("hex digit");
                    self.bump();
                }
                _ => return Err(self.error("malformed Unicode escape", start)),
            }
        }
        // A lone surrogate is allowed in string literals (`"\ud800"` has length
        // 1); `char` cannot represent it, so it is encoded as U+FFFD only when
        // it actually needs to travel as a scalar. QuickJS keeps UTF-16 code
        // units, so represent surrogates explicitly.
        Ok(char::from_u32(value).unwrap_or('\u{fffd}'))
    }

    fn scan_escape(
        &mut self,
        out: &mut String,
        start: Position,
        context: EscapeContext,
    ) -> Result<()> {
        let Some(c) = self.bump() else {
            return Err(self.error("unterminated string literal", start));
        };
        match c {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek_char().is_some_and(|c| c.is_ascii_digit()) => out.push('\0'),
            'x' => {
                let mut value = 0u32;
                for _ in 0..2 {
                    match self.peek_char() {
                        Some(c) if c.is_ascii_hexdigit() => {
                            value = value * 16 + c.to_digit(16).expect("hex digit");
                            self.bump();
                        }
                        _ => {
                            return Err(self.invalid_escape(context, start));
                        }
                    }
                }
                out.push(char::from_u32(value).expect("value <= 0xff"));
            }
            'u' => {
                let ch = self.scan_unicode_escape_body(start)?;
                out.push(ch);
            }
            c if is_line_terminator(c) => {
                // Escaped line terminator: contributes nothing (line
                // continuation). `\r\n` was already folded by `bump`.
            }
            '1'..='7' => {
                if context == EscapeContext::Template {
                    // Templates never allow octal escapes: this is an invalid
                    // escape sequence, so `cooked` becomes `undefined`.
                    return Err(self.invalid_escape(context, start));
                }
                if self.strict {
                    return Err(self.error(
                        "octal escape sequences are not allowed in strict mode",
                        start,
                    ));
                }
                let mut value = c.to_digit(8).expect("octal digit");
                for _ in 0..2 {
                    match self.peek_char() {
                        Some(d @ '0'..='7') => {
                            let next = value * 8 + d.to_digit(8).expect("octal digit");
                            if next > 255 {
                                break;
                            }
                            value = next;
                            self.bump();
                        }
                        _ => break,
                    }
                }
                out.push(char::from_u32(value).expect("value <= 0xff"));
            }
            '8' | '9' => {
                if context == EscapeContext::Template {
                    return Err(self.invalid_escape(context, start));
                }
                if self.strict {
                    return Err(self.error(
                        "octal escape sequences are not allowed in strict mode",
                        start,
                    ));
                }
                out.push(c);
            }
            other => out.push(other),
        }
        Ok(())
    }

    fn invalid_escape(&self, context: EscapeContext, start: Position) -> LexError {
        match context {
            EscapeContext::String => self.error("invalid escape sequence in string literal", start),
            EscapeContext::Template => {
                self.error("invalid escape sequence in template literal", start)
            }
        }
    }

    // -- templates ---------------------------------------------------------

    /// Resume a template literal after the `}` closing a substitution.
    ///
    /// `brace` is the position of that `}` (the parser keeps the token span).
    pub fn re_scan_template_continue(&mut self, brace: Position) -> Result<Token> {
        let start = brace;
        self.reset_to(brace);
        debug_assert_eq!(self.peek_char(), Some('}'));
        self.bump(); // consume the `}` so the chunk starts after it
        let chunk = self.scan_template_chunk(start, false)?;
        Ok(Token {
            kind: TokenKind::Template(chunk),
            span: Span::new(start, self.position()),
            newline_before: false,
        })
    }

    /// Scan a template chunk; the opening backtick or `}` is already consumed and
    /// `chunk_start` is its position (used for the `raw` slice and errors).
    fn scan_template_chunk(
        &mut self,
        chunk_start: Position,
        opening: bool,
    ) -> Result<TemplateToken> {
        let content_start = self.pos;
        let mut cooked = String::new();
        let mut valid_escape = true;
        let kind;
        loop {
            let Some(c) = self.peek_char() else {
                return Err(self.error("unterminated template literal", chunk_start));
            };
            match c {
                '`' => {
                    self.bump();
                    kind = if opening {
                        TemplateKind::NoSubstitution
                    } else {
                        TemplateKind::Tail
                    };
                    break;
                }
                '$' if self.peek_char_at(1) == Some('{') => {
                    self.bump();
                    self.bump();
                    kind = if opening {
                        TemplateKind::Head
                    } else {
                        TemplateKind::Middle
                    };
                    break;
                }
                '\\' => {
                    let before = self.position();
                    self.bump();
                    let mut scratch = String::new();
                    match self.scan_escape(&mut scratch, chunk_start, EscapeContext::Template) {
                        Ok(()) => cooked.push_str(&scratch),
                        Err(_) => {
                            // Invalid escapes are legal in tagged templates,
                            // where `cooked` becomes `undefined`. The raw text
                            // still has to be recorded, so re-scan verbatim.
                            valid_escape = false;
                            // Replay the escape verbatim: `cooked` is undefined
                            // but `raw` must keep the original bytes, and the
                            // scan has to continue after it.
                            self.reset_to(before);
                            self.bump(); // the backslash
                            if let Some(next) = self.peek_char() {
                                if !is_line_terminator(next) {
                                    self.bump();
                                }
                            }
                        }
                    }
                }
                c if is_line_terminator(c) => {
                    // Line terminators are normalised to LF in templates.
                    self.bump();
                    cooked.push('\n');
                }
                other => {
                    cooked.push(other);
                    self.bump();
                }
            }
        }
        let content_end = match kind {
            TemplateKind::NoSubstitution | TemplateKind::Tail => self.pos - 1,
            TemplateKind::Head | TemplateKind::Middle => self.pos - 2,
        }
        .max(content_start);
        let raw = self.src[content_start..content_end].to_string();
        Ok(TemplateToken {
            kind,
            cooked: if valid_escape { Some(cooked) } else { None },
            raw,
        })
    }

    // -- regular expressions -----------------------------------------------

    /// Re-scan the token starting at `start` (the `/`) as a regular expression
    /// literal. Called by the parser when it expects an expression.
    pub fn re_scan_regex(&mut self, start: Position) -> Result<Token> {
        self.reset_to(start);
        debug_assert_eq!(self.peek_char(), Some('/'));
        self.bump(); // opening slash
        let mut pattern = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.peek_char() else {
                return Err(self.error("unterminated regexp literal", start));
            };
            if is_line_terminator(c) {
                return Err(self.error("unterminated regexp literal", start));
            }
            match c {
                '\\' => {
                    pattern.push(c);
                    self.bump();
                    match self.peek_char() {
                        Some(next) if !is_line_terminator(next) => {
                            pattern.push(next);
                            self.bump();
                        }
                        _ => return Err(self.error("unterminated regexp literal", start)),
                    }
                }
                '[' => {
                    in_class = true;
                    pattern.push(c);
                    self.bump();
                }
                ']' => {
                    in_class = false;
                    pattern.push(c);
                    self.bump();
                }
                '/' if !in_class => {
                    self.bump();
                    break;
                }
                _ => {
                    pattern.push(c);
                    self.bump();
                }
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek_char() {
            if is_id_continue(c) {
                flags.push(c);
                self.bump();
            } else {
                break;
            }
        }
        Ok(Token {
            kind: TokenKind::RegExp { pattern, flags },
            span: Span::new(start, self.position()),
            newline_before: false,
        })
    }
}

/// Which literal an escape sequence belongs to; the two differ in their error
/// handling (invalid escapes are fatal in strings, `cooked = undefined` in
/// tagged templates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeContext {
    String,
    Template,
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Build the value of a decimal literal text (digits with an optional fraction
/// and exponent, separators already removed).
fn number_from_decimal(text: &str) -> NumberLiteral {
    let is_integer = !text.contains(['.', 'e', 'E']);
    if is_integer {
        if let Ok(value) = text.parse::<i64>() {
            if let Ok(value) = i32::try_from(value) {
                return NumberLiteral::Int(value);
            }
        }
    }
    // Rust's `f64` parser is correctly rounded, which is exactly what
    // `StringNumericLiteral` requires for decimal literals.
    NumberLiteral::Float(text.parse::<f64>().unwrap_or(f64::NAN))
}

fn number_from_f64(value: f64) -> NumberLiteral {
    if value.fract() == 0.0 && value >= i32::MIN as f64 && value <= i32::MAX as f64 {
        NumberLiteral::Int(value as i32)
    } else {
        NumberLiteral::Float(value)
    }
}

/// Convert digits in `radix` to a double.
///
/// Exact for every literal that fits in 128 bits (which covers everything a
/// program realistically contains); longer literals accumulate in `f64`, which
/// can double-round in rare cases. The exact radix conversion lands with the
/// `js-dtoa` milestone (FR-6).
pub(crate) fn radix_digits_to_f64(digits: &str, radix: u32) -> f64 {
    let mut acc: u128 = 0;
    let mut overflow = false;
    for c in digits.chars() {
        let Some(digit) = c.to_digit(radix) else {
            continue;
        };
        match acc
            .checked_mul(radix as u128)
            .and_then(|v| v.checked_add(digit as u128))
        {
            Some(value) => acc = value,
            None => {
                overflow = true;
                break;
            }
        }
    }
    if !overflow {
        return acc as f64;
    }
    let mut value = 0.0f64;
    for c in digits.chars() {
        let Some(digit) = c.to_digit(radix) else {
            continue;
        };
        value = value * radix as f64 + digit as f64;
    }
    value
}

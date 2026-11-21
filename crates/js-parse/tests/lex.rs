//! Lexer conformance and unit tests (M1a).

use js_parse::{Lexer, NumberLiteral, Punct, TemplateKind, TokenKind};

fn tokens(src: &str) -> Vec<TokenKind> {
    Lexer::new(src)
        .tokenize()
        .expect("source should lex")
        .into_iter()
        .map(|t| t.kind)
        .collect()
}

fn kinds(src: &str) -> Vec<TokenKind> {
    tokens(src).pop_if_eof()
}

trait PopEof {
    fn pop_if_eof(self) -> Self;
}

impl PopEof for Vec<TokenKind> {
    fn pop_if_eof(mut self) -> Self {
        if matches!(self.last(), Some(TokenKind::Eof)) {
            self.pop();
        }
        self
    }
}

fn ident(name: &str) -> TokenKind {
    TokenKind::Ident(name.to_string())
}

fn int(value: i32) -> TokenKind {
    TokenKind::Number(NumberLiteral::Int(value))
}

fn float(value: f64) -> TokenKind {
    TokenKind::Number(NumberLiteral::Float(value))
}

fn punct(p: Punct) -> TokenKind {
    TokenKind::Punct(p)
}

fn error_message(src: &str) -> String {
    Lexer::new(src)
        .tokenize()
        .expect_err("source should not lex")
        .message
}

// -- identifiers and keywords ---------------------------------------------

#[test]
fn identifiers_and_keywords() {
    assert_eq!(
        kinds("let answer = null;"),
        vec![
            ident("let"),
            ident("answer"),
            punct(Punct::Eq),
            TokenKind::Keyword(js_parse::Keyword::Null),
            punct(Punct::Semicolon)
        ]
    );
    assert_eq!(
        kinds("$x _y a1 \u{3b1}\u{3b2}"),
        vec![
            ident("$x"),
            ident("_y"),
            ident("a1"),
            ident("\u{3b1}\u{3b2}")
        ]
    );
}

#[test]
fn context_sensitive_words_are_identifiers() {
    // The parser decides what these mean; the lexer must not swallow them.
    for word in [
        "let",
        "static",
        "yield",
        "await",
        "async",
        "of",
        "get",
        "set",
        "from",
        "as",
        "implements",
    ] {
        assert_eq!(
            kinds(word),
            vec![ident(word)],
            "{word} should lex as an identifier"
        );
    }
}

#[test]
fn identifiers_accept_unicode_escapes() {
    assert_eq!(kinds(r"\u0061bc"), vec![ident("abc")]);
    assert_eq!(kinds(r"\u{1d4d0}"), vec![ident("\u{1d4d0}")]);
    assert_eq!(kinds(r"a\u200Cb"), vec![ident("a\u{200c}b")]);
    assert_eq!(error_message(r"\u0020x"), "invalid Unicode escape sequence");
}

#[test]
fn identifier_escape_must_be_followed_by_u() {
    assert_eq!(
        error_message(r"\x41"),
        "invalid escape sequence in identifier"
    );
}

// -- numbers --------------------------------------------------------------

#[test]
fn integer_literals() {
    assert_eq!(
        kinds("0 42 2147483647"),
        vec![int(0), int(42), int(2_147_483_647)]
    );
    assert_eq!(kinds("2147483648"), vec![float(2_147_483_648.0)]);
    assert_eq!(kinds("0x2a 0o17 0b1010"), vec![int(42), int(15), int(10)]);
    assert_eq!(kinds("0X2A 0O17 0B1010"), vec![int(42), int(15), int(10)]);
    assert_eq!(kinds("1_000_000"), vec![int(1_000_000)]);
    assert_eq!(kinds("0x1_f"), vec![int(31)]);
}

#[test]
fn float_literals() {
    assert_eq!(kinds("1.5"), vec![float(1.5)]);
    assert_eq!(kinds(".5"), vec![float(0.5)]);
    assert_eq!(kinds("1."), vec![float(1.0)]);
    assert_eq!(kinds("1.e3"), vec![float(1000.0)]);
    assert_eq!(
        kinds("1e3 1E3 1e+3 1e-3"),
        vec![float(1000.0), float(1000.0), float(1000.0), float(0.001)]
    );
    assert_eq!(kinds("1.5e-2"), vec![float(0.015)]);
    assert_eq!(kinds("1.5_5"), vec![float(1.55)]);
    // `0.1 + 0.2` problems start here: the literal must be the nearest double.
    match kinds("0.1")[0].clone() {
        TokenKind::Number(NumberLiteral::Float(v)) => assert_eq!(v.to_bits(), 0.1f64.to_bits()),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn legacy_octal_and_non_octal_decimal() {
    assert_eq!(kinds("017"), vec![int(15)], "Annex B legacy octal");
    assert_eq!(
        kinds("08 09"),
        vec![int(8), int(9)],
        "NonOctalDecimalIntegerLiteral"
    );
    assert_eq!(kinds("0195"), vec![int(195)]);
    assert_eq!(kinds("08.5"), vec![float(8.5)]);
    assert_eq!(kinds("08e1"), vec![float(80.0)]);
}

#[test]
fn bigint_literals() {
    assert_eq!(
        kinds("1n"),
        vec![TokenKind::BigInt(js_parse::BigIntLiteral {
            digits: "1".into(),
            radix: 10
        })]
    );
    assert_eq!(
        kinds("0x2an"),
        vec![TokenKind::BigInt(js_parse::BigIntLiteral {
            digits: "2a".into(),
            radix: 16
        })]
    );
    assert_eq!(
        kinds("123_456n"),
        vec![TokenKind::BigInt(js_parse::BigIntLiteral {
            digits: "123456".into(),
            radix: 10
        })]
    );
    assert_eq!(error_message("1.5n"), "invalid BigInt literal");
    assert_eq!(error_message("1e2n"), "invalid BigInt literal");
    assert_eq!(error_message("01n"), "invalid BigInt literal");
}

#[test]
fn numeric_separator_errors() {
    assert_eq!(error_message("1__0"), "invalid numeric separator");
    assert_eq!(error_message("1_"), "invalid numeric separator");
    assert_eq!(error_message("0x_1"), "invalid numeric separator");
    assert_eq!(error_message("1._5"), "invalid numeric separator");
}

#[test]
fn number_must_not_run_into_an_identifier() {
    assert_eq!(error_message("1abc"), "invalid number literal");
    assert_eq!(error_message("3in"), "invalid number literal");
    assert_eq!(error_message("0x"), "invalid number literal");
    assert_eq!(error_message("1e"), "invalid number literal");
}

#[test]
fn huge_hex_literals_match_their_double_value() {
    // 40 hex digits: exercises the wider-than-128-bit path.
    let src = "0x123456789abcdef0123456789abcdef012345678";
    let TokenKind::Number(NumberLiteral::Float(value)) = kinds(src)[0].clone() else {
        panic!("expected a float");
    };
    assert!(value.is_finite());
    assert!(
        value > 1e47 && value < 1e49,
        "unexpected magnitude: {value}"
    );
}

// -- strings --------------------------------------------------------------

fn string_token(src: &str) -> String {
    match kinds(src).pop().expect("one token") {
        TokenKind::String(s) => s,
        other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn string_escapes() {
    assert_eq!(string_token(r#""a\nb""#), "a\nb");
    assert_eq!(string_token(r#""\t\r\b\f\v\0""#), "\t\r\u{8}\u{c}\u{b}\0");
    assert_eq!(string_token(r#""\x41""#), "A");
    assert_eq!(string_token(r#""\u0041""#), "A");
    assert_eq!(string_token(r#""\u{1f600}""#), "\u{1f600}");
    assert_eq!(string_token(r#""\q""#), "q", "identity escape");
    assert_eq!(string_token("\"a\\\nb\""), "ab", "line continuation");
    assert_eq!(string_token("'it\\'s'"), "it's");
    assert_eq!(string_token(r#""\101""#), "A", "Annex B octal escape");
    assert_eq!(string_token(r#""\8""#), "8");
}

#[test]
fn strict_mode_rejects_octal_escapes() {
    let mut lexer = Lexer::new(r#""\101""#);
    lexer.set_strict(true);
    let err = lexer
        .tokenize()
        .expect_err("octal escapes are illegal in strict mode");
    assert_eq!(
        err.message,
        "octal escape sequences are not allowed in strict mode"
    );
}

#[test]
fn unterminated_strings() {
    assert_eq!(error_message("\"abc"), "unterminated string literal");
    assert_eq!(error_message("\"a\nb\""), "unterminated string literal");
    assert_eq!(
        error_message("\"\\x4\""),
        "invalid escape sequence in string literal"
    );
    assert_eq!(error_message(r#""\u{110000}""#), "malformed Unicode escape");
    assert_eq!(error_message(r#""\u{}""#), "malformed Unicode escape");
}

// -- templates ------------------------------------------------------------

#[test]
fn template_without_substitution() {
    let TokenKind::Template(chunk) = kinds("`plain`").remove(0) else {
        panic!()
    };
    assert_eq!(chunk.kind, TemplateKind::NoSubstitution);
    assert_eq!(chunk.cooked.as_deref(), Some("plain"));
    assert_eq!(chunk.raw, "plain");
}

#[test]
fn template_with_substitution() {
    let mut lexer = Lexer::new("`a${1}b${2}c`");

    let TokenKind::Template(head) = lexer.next_token().expect("head").kind else {
        panic!()
    };
    assert_eq!(head.kind, TemplateKind::Head);
    assert_eq!(head.cooked.as_deref(), Some("a"));
    assert_eq!(head.raw, "a");

    assert_eq!(lexer.next_token().expect("1").kind, int(1));

    let brace = lexer.next_token().expect("}").span.start;
    let TokenKind::Template(middle) = lexer.re_scan_template_continue(brace).expect("middle").kind
    else {
        panic!()
    };
    assert_eq!(middle.kind, TemplateKind::Middle);
    assert_eq!(middle.cooked.as_deref(), Some("b"));

    assert_eq!(lexer.next_token().expect("2").kind, int(2));

    let brace = lexer.next_token().expect("}").span.start;
    let TokenKind::Template(tail) = lexer.re_scan_template_continue(brace).expect("tail").kind
    else {
        panic!()
    };
    assert_eq!(tail.kind, TemplateKind::Tail);
    assert_eq!(tail.cooked.as_deref(), Some("c"));

    assert_eq!(lexer.next_token().expect("eof").kind, TokenKind::Eof);
}

#[test]
fn template_escapes_and_raw_text() {
    let TokenKind::Template(chunk) = kinds(r"`a\nb\u{41}`").remove(0) else {
        panic!()
    };
    assert_eq!(chunk.cooked.as_deref(), Some("a\nbA"));
    assert_eq!(chunk.raw, r"a\nb\u{41}");
}

#[test]
fn invalid_template_escape_makes_cooked_undefined() {
    // Legal only in tagged templates; `cooked` is `undefined` there.
    let TokenKind::Template(chunk) = kinds(r"`a\1b`").remove(0) else {
        panic!()
    };
    assert_eq!(chunk.cooked, None);
    assert_eq!(chunk.raw, r"a\1b");
    let TokenKind::Template(ok) = kinds(r"`a\0b`").remove(0) else {
        panic!()
    };
    assert_eq!(ok.cooked.as_deref(), Some("a\0b"));
}

#[test]
fn template_crlf_is_normalised() {
    let TokenKind::Template(chunk) = kinds("`a\r\nb`").remove(0) else {
        panic!()
    };
    assert_eq!(chunk.cooked.as_deref(), Some("a\nb"));
    assert_eq!(chunk.raw, "a\r\nb");
}

#[test]
fn template_line_continuation() {
    let TokenKind::Template(chunk) = kinds("`a\\\nb`").remove(0) else {
        panic!()
    };
    assert_eq!(chunk.cooked.as_deref(), Some("ab"));
}

#[test]
fn unterminated_template() {
    assert_eq!(error_message("`abc"), "unterminated template literal");
}

// -- regular expressions --------------------------------------------------

#[test]
fn regex_is_rescanned_on_demand() {
    let mut lexer = Lexer::new("/a\\/b[c/]d/gi");
    let slash = lexer.next_token().expect("slash");
    assert_eq!(slash.kind, punct(Punct::Slash));
    let regex = lexer.re_scan_regex(slash.span.start).expect("regex");
    match regex.kind {
        TokenKind::RegExp { pattern, flags } => {
            assert_eq!(pattern, r"a\/b[c/]d");
            assert_eq!(flags, "gi");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(lexer.next_token().expect("eof").kind, TokenKind::Eof);
}

#[test]
fn regex_scanned_after_a_paren() {
    let mut lexer = Lexer::new("(/(a)/)");
    assert_eq!(lexer.next_token().expect("( ").kind, punct(Punct::LParen));
    let slash = lexer.next_token().expect("slash");
    let regex = lexer.re_scan_regex(slash.span.start).expect("regex");
    match regex.kind {
        TokenKind::RegExp { pattern, .. } => assert_eq!(pattern, "(a)"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn unterminated_regex() {
    let mut lexer = Lexer::new("/abc");
    let slash = lexer.next_token().expect("slash");
    assert_eq!(
        lexer.re_scan_regex(slash.span.start).unwrap_err().message,
        "unterminated regexp literal"
    );
}

// -- punctuators ----------------------------------------------------------

#[test]
fn longest_match_wins() {
    let src = ">>>= ... === !== **= <<= >>= >>> &&= ||= ??= => ?? ?. ? : = .";
    let expected = [
        Punct::UShrEq,
        Punct::Ellipsis,
        Punct::EqEqEq,
        Punct::NotEqEq,
        Punct::StarStarEq,
        Punct::ShlEq,
        Punct::ShrEq,
        Punct::UShr,
        Punct::AmpAmpEq,
        Punct::PipePipeEq,
        Punct::QuestionQuestionEq,
        Punct::Arrow,
        Punct::QuestionQuestion,
        Punct::QuestionDot,
        Punct::Question,
        Punct::Colon,
        Punct::Eq,
        Punct::Dot,
    ];
    assert_eq!(
        kinds(src),
        expected.iter().map(|p| punct(*p)).collect::<Vec<_>>()
    );
}

#[test]
fn question_dot_versus_conditional_number() {
    // `a?.5:b` is a conditional, not optional chaining.
    assert_eq!(
        kinds("a?.5:b"),
        vec![
            ident("a"),
            punct(Punct::Question),
            float(0.5),
            punct(Punct::Colon),
            ident("b")
        ]
    );
    assert_eq!(
        kinds("a?.b"),
        vec![ident("a"), punct(Punct::QuestionDot), ident("b")]
    );
}

/// Every variant of [`Punct`]; kept exhaustive so a missing lexer table entry
/// (a punctuator that can never be produced) fails the round-trip test.
const ALL_PUNCTUATORS: &[Punct] = &[
    Punct::LBrace,
    Punct::RBrace,
    Punct::LParen,
    Punct::RParen,
    Punct::LBracket,
    Punct::RBracket,
    Punct::Semicolon,
    Punct::Comma,
    Punct::Lt,
    Punct::Gt,
    Punct::LtEq,
    Punct::GtEq,
    Punct::EqEq,
    Punct::NotEq,
    Punct::EqEqEq,
    Punct::NotEqEq,
    Punct::Plus,
    Punct::Minus,
    Punct::Star,
    Punct::Slash,
    Punct::Percent,
    Punct::StarStar,
    Punct::PlusPlus,
    Punct::MinusMinus,
    Punct::Shl,
    Punct::Shr,
    Punct::UShr,
    Punct::Amp,
    Punct::Pipe,
    Punct::Caret,
    Punct::Bang,
    Punct::Tilde,
    Punct::AmpAmp,
    Punct::PipePipe,
    Punct::QuestionQuestion,
    Punct::Question,
    Punct::Colon,
    Punct::Eq,
    Punct::PlusEq,
    Punct::MinusEq,
    Punct::StarEq,
    Punct::SlashEq,
    Punct::PercentEq,
    Punct::StarStarEq,
    Punct::ShlEq,
    Punct::ShrEq,
    Punct::UShrEq,
    Punct::AmpEq,
    Punct::PipeEq,
    Punct::CaretEq,
    Punct::AmpAmpEq,
    Punct::PipePipeEq,
    Punct::QuestionQuestionEq,
    Punct::Arrow,
    Punct::Ellipsis,
    Punct::Dot,
    Punct::QuestionDot,
    Punct::Hash,
];

#[test]
fn every_punctuator_lexes_back_to_itself() {
    for &punct_value in ALL_PUNCTUATORS {
        assert_eq!(
            kinds(punct_value.as_str()),
            vec![punct(punct_value)],
            "{punct_value:?}"
        );
    }
    // `?.` is the one punctuator whose text is also a prefix of another token,
    // so check it in a context that cannot be mistaken for a conditional.
    assert_eq!(
        kinds("a?.b"),
        vec![ident("a"), punct(Punct::QuestionDot), ident("b")]
    );
}

#[test]
fn all_punctuators_round_trip() {
    for punct_value in [
        Punct::LBrace,
        Punct::RBrace,
        Punct::LParen,
        Punct::RParen,
        Punct::LBracket,
        Punct::RBracket,
        Punct::Semicolon,
        Punct::Comma,
        Punct::Lt,
        Punct::Gt,
        Punct::Plus,
        Punct::Minus,
        Punct::Star,
        Punct::Slash,
        Punct::Percent,
        Punct::Bang,
        Punct::Tilde,
        Punct::Amp,
        Punct::Pipe,
        Punct::Caret,
        Punct::Hash,
    ] {
        assert_eq!(
            kinds(punct_value.as_str()),
            vec![punct(punct_value)],
            "{punct_value:?}"
        );
    }
}

// -- trivia, ASI and positions -------------------------------------------

#[test]
fn comments_and_newline_tracking() {
    let mut lexer = Lexer::new("a /* x\n y */ b // tail\nc");
    let a = lexer.next_token().expect("a");
    assert!(!a.newline_before);
    let b = lexer.next_token().expect("b");
    assert!(b.newline_before, "block comment contains a newline");
    let c = lexer.next_token().expect("c");
    assert!(c.newline_before, "line comment ends with a newline");
}

#[test]
fn hashbang_is_only_special_at_offset_zero() {
    let tokens = kinds("#!/usr/bin/env qjs\n1");
    assert_eq!(tokens, vec![int(1)]);
    // Not at offset 0: `#` and `!` are ordinary punctuators (the parser is the
    // one that rejects a stray private name).
    assert_eq!(
        kinds(" #!x"),
        vec![punct(Punct::Hash), punct(Punct::Bang), ident("x")]
    );
}

#[test]
fn unterminated_block_comment() {
    assert_eq!(error_message("/* forever"), "unterminated comment");
}

#[test]
fn positions_are_line_and_utf16_column_based() {
    let tokens = Lexer::new("\"\u{1f600}\"+b\ncd").tokenize().expect("lex");
    // `"` is column 1, the emoji is two UTF-16 code units wide (columns 2-3),
    // the closing `"` is column 4, so `+` starts at column 5.
    assert_eq!(
        tokens[0].span.start,
        js_parse::Position {
            offset: 0,
            line: 1,
            column: 1
        }
    );
    assert_eq!(
        tokens[0].span.end,
        js_parse::Position {
            offset: 6,
            line: 1,
            column: 5
        }
    );
    assert_eq!(
        tokens[1].span.start,
        js_parse::Position {
            offset: 6,
            line: 1,
            column: 5
        }
    );
    assert_eq!(
        tokens[2].span.start,
        js_parse::Position {
            offset: 7,
            line: 1,
            column: 6
        }
    );
    // `cd` is a single identifier on line 2; the trailing token is `Eof`.
    assert_eq!(
        tokens[3].span.start,
        js_parse::Position {
            offset: 9,
            line: 2,
            column: 1
        }
    );
    assert_eq!(
        tokens[4].span.start,
        js_parse::Position {
            offset: 11,
            line: 2,
            column: 3
        }
    );
}

#[test]
fn crlf_counts_as_one_line_terminator() {
    let tokens = Lexer::new("a\r\nb\rc\nd").tokenize().expect("lex");
    assert_eq!(tokens[1].span.start.line, 2);
    assert_eq!(tokens[1].span.start.column, 1);
    assert_eq!(tokens[2].span.start.line, 3);
    assert_eq!(tokens[3].span.start.line, 4);
}

#[test]
fn unexpected_characters_are_reported() {
    assert_eq!(error_message("a @ b"), "unexpected character '@'");
}

//! Differential tests for literal values, using the C reference build as the
//! oracle.
//!
//! Numeric literals are compared **bit for bit**: the reference program stores
//! the value in a `DataView` and prints the two 32-bit halves, which is the only
//! way to see the exact double a literal denotes (dtoa is not implemented yet).
//!
//! Skipped when no reference build is available; see `scripts/setup-oracle.sh`.

use js_parse::{Lexer, NumberLiteral, TokenKind};
use quickjs_conformance::{Engine, locate_oracle};

fn oracle() -> Option<Engine> {
    locate_oracle().map(|path| Engine::new("reference", path))
}

macro_rules! require_oracle {
    () => {
        match oracle() {
            Some(oracle) => oracle,
            None => {
                eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
                return;
            }
        }
    };
}

fn our_number(literal: &str) -> f64 {
    // `-0` is a unary expression in the grammar, not part of the literal, so the
    // sign has to be applied by hand.
    let (sign, text) = match literal.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, literal),
    };
    let tokens = Lexer::new(text).tokenize().expect("literal should lex");
    let value = match tokens[0].kind {
        TokenKind::Number(NumberLiteral::Int(v)) => v as f64,
        TokenKind::Number(NumberLiteral::Float(v)) => v,
        ref other => panic!("expected a number, got {other:?}"),
    };
    sign * value
}

fn our_number_accepted(literal: &str) -> bool {
    match Lexer::new(literal).tokenize() {
        Ok(tokens) => matches!(tokens[0].kind, TokenKind::Number(_) | TokenKind::BigInt(_)),
        Err(_) => false,
    }
}

fn oracle_bits(oracle: &Engine, literal: &str) -> Option<f64> {
    let program = format!(
        "const b = new DataView(new ArrayBuffer(8));\
         b.setFloat64(0, {literal});\
         print(b.getUint32(0) + ':' + b.getUint32(4));"
    );
    let out = oracle.output(&["-e", &program]).ok()?;
    if out.code != 0 {
        return None;
    }
    let (hi, lo) = out.stdout.trim().split_once(':')?;
    let hi: u64 = hi.parse().ok()?;
    let lo: u64 = lo.parse().ok()?;
    Some(f64::from_bits((hi << 32) | lo))
}

fn oracle_string(oracle: &Engine, literal: &str) -> Option<String> {
    let program = format!("print(JSON.stringify({literal}));");
    let out = oracle.output(&["-e", &program]).ok()?;
    if out.code != 0 {
        return None;
    }
    // The oracle printed a JSON string literal; decode it back to text.
    let json = out.stdout.trim_end_matches('\n');
    decode_json_string(json)
}

fn decode_json_string(json: &str) -> Option<String> {
    let inner = json.strip_prefix('"')?.strip_suffix('"')?;
    let mut out = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'u' => {
                let hex: String = chars.by_ref().take(4).collect();
                let value = u32::from_str_radix(&hex, 16).ok()?;
                out.push(char::from_u32(value).unwrap_or('\u{fffd}'));
            }
            other => out.push(other),
        }
    }
    Some(out)
}

fn our_string(literal: &str) -> String {
    let tokens = Lexer::new(literal).tokenize().expect("literal should lex");
    match tokens[0].kind {
        TokenKind::String(ref s) => s.clone(),
        ref other => panic!("expected a string, got {other:?}"),
    }
}

#[test]
fn numeric_literals_denote_the_same_double() {
    let oracle = require_oracle!();
    let literals = [
        "0",
        "1",
        "42",
        "2147483647",
        "2147483648",
        "4294967296",
        "0.1",
        "0.2",
        "0.30000000000000004",
        "1.7976931348623157e308",
        "5e-324",
        "2.2250738585072014e-308",
        "1.0000000000000002",
        "123456789012345678901234567890",
        "9007199254740993",
        "0x123456789abcdef0123456789abcdef012345678",
        "0xffffffffffffffffffffffffffffffff",
        "0x1fffffffffffff",
        "0b1111111111111111111111111111111111111111111111111111111111111",
        "0o777777777777777777777777",
        "017",
        "08",
        "0195",
        "1_000_000",
        "0x1_f",
        "1.5_5",
        "1.",
        ".5",
        "1.e3",
        "1e21",
        "1e-7",
        "0.000001",
        "1e309",
        "-0",
        "1.1e308",
        "0.1e1",
    ];
    let mut compared = 0;
    for literal in literals {
        let Some(expected) = oracle_bits(&oracle, literal) else {
            // The reference rejected it (e.g. `-0` is not a literal on its own
            // in this position); our lexer must reject it too.
            assert!(
                !our_number_accepted(literal),
                "{literal} was rejected by the reference but accepted by our lexer"
            );
            continue;
        };
        let actual = our_number(literal);
        assert_eq!(
            actual.to_bits(),
            expected.to_bits(),
            "{literal}: our value {actual:?} ({:x}) != reference {expected:?} ({:x})",
            actual.to_bits(),
            expected.to_bits()
        );
        compared += 1;
    }
    assert!(
        compared >= 30,
        "expected to compare the whole corpus, only {compared} ran"
    );
}

#[test]
fn invalid_numeric_literals_are_rejected_by_both() {
    let oracle = require_oracle!();
    for literal in [
        "1_", "0x", "1__0", "1._5", "1abc", "0b2", "0o8", "1.5n", "1e2n", "01n",
    ] {
        let reference_ok = oracle_bits(&oracle, literal).is_some()
            || oracle
                .output(&["-e", literal])
                .map(|o| o.code == 0)
                .unwrap_or(false);
        let ours_ok = our_number_accepted(literal);
        assert_eq!(
            ours_ok, reference_ok,
            "{literal}: our lexer says {ours_ok}, the reference says {reference_ok}"
        );
    }
}

#[test]
fn bigint_literals_are_recognised_by_both() {
    let oracle = require_oracle!();
    for literal in [
        "1n",
        "0n",
        "0x2an",
        "123_456n",
        "0b101n",
        "0o17n",
        "9007199254740993n",
    ] {
        let out = oracle
            .output(&["-e", &format!("print(typeof {literal})")])
            .expect("run reference");
        assert_eq!(
            out.stdout.trim(),
            "bigint",
            "{literal} should be a BigInt in the reference"
        );
        let tokens = Lexer::new(literal).tokenize().expect("should lex");
        assert!(
            matches!(tokens[0].kind, TokenKind::BigInt(_)),
            "{literal} should lex as a BigInt, got {:?}",
            tokens[0].kind
        );
    }
}

#[test]
fn string_literals_decode_to_the_same_text() {
    let oracle = require_oracle!();
    let literals = [
        r#""abc""#,
        r#"'abc'"#,
        r#""a\nb""#,
        r#""\t\r\b\f\v\0""#,
        r#""\x41\u0042\u{43}""#,
        r#""\q""#,
        r#""it\'s""#,
        r#""\101""#,
        r#""\8""#,
        r#""\u{1f600}""#,
        r#""a\
b""#,
        r#""""#,
        r#""\u00e9\u00e9""#,
    ];
    for literal in literals {
        let Some(expected) = oracle_string(&oracle, literal) else {
            panic!("the reference rejected {literal}");
        };
        let actual = our_string(literal);
        assert_eq!(actual, expected, "literal {literal}");
    }
}

#[test]
fn syntax_errors_are_syntax_errors_in_both() {
    let oracle = require_oracle!();
    for source in ["\"abc", "`abc", "/* x", "a @ b"] {
        let out = oracle.output(&["-e", source]).expect("run reference");
        assert_ne!(
            out.code, 0,
            "{source} should be a syntax error in the reference"
        );
        assert!(
            Lexer::new(source).tokenize().is_err(),
            "{source} should be a lexical error"
        );
    }
}

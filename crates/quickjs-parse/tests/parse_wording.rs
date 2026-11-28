//! Differential test: does our `SyntaxError` wording match the reference's?
//!
//! `parse_oracle.rs` compares only the *verdict* — accepted or rejected. That left the
//! message text completely unmeasured, and it drifted freely: a survey over
//! `test262/test/language` found agreement on **2.3%** of 4055 rejections, in 408 distinct
//! shapes. Every string was our own invention, `expecting a variable name, got '='` where
//! the reference says `variable name expected`.
//!
//! This file makes the text a measured quantity. For each source in [`CORPUS`] it asks the
//! reference parser and this one for their message and compares them. Pairs listed in
//! [`KNOWN_DIFFERENCES`] are allowed to disagree; **anything else that disagrees fails**, so
//! the list cannot silently grow. The reverse also fails: a listed source that starts
//! agreeing must be removed, so the list cannot rot either.
//!
//! # Why the corpus is hand-written
//!
//! Deriving it from test262 at test time would mean a few thousand `qjsc` invocations per
//! run. The corpus below is one or two sources per reference message that this parser can
//! actually reach, which is what makes a failure point at a specific wording rather than at
//! a directory.

use js_parse::{parse_module, parse_script};
use quickjs_conformance::{Engine, locate_qjsc};
use std::path::Path;

fn oracle() -> Option<Engine> {
    locate_qjsc().map(|path| Engine::new("qjsc", path))
}

macro_rules! require_oracle {
    () => {
        match oracle() {
            Some(oracle) => oracle,
            None => {
                eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
                return;
            }
        }
    };
}

/// One source and the message the reference reports for it.
///
/// A `None` expectation means the reference **accepts** the source; the entry then only
/// checks that we accept it too, which is the one case where wording cannot be compared.
type Case = (&'static str, Option<&'static str>);

/// Sources whose wording is deliberately still different, with the reason.
///
/// Every entry here is a *diagnosis* difference rather than a vocabulary one: the two
/// parsers notice a different problem first, so matching the text would mean matching the
/// order of the checks. Fixing one is a parser change, not a string change.
const KNOWN_DIFFERENCES: &[(&str, &str)] = &[
    (
        "for (var i, j of k) ;",
        "we reject the second binding before looking for `of`; the reference reports the \
         missing `of`/`in` first",
    ),
    (
        "try {} catch",
        "at EOF the reference takes the `(binding)` path and asks for `(`; we take the \
         optional-binding path and ask for `{`. Both are correct for a truncated file",
    ),
    (
        "a ?? b || c",
        "the reference parses `a ?? b` as a complete expression and then finds `||` \
         unexpected; we detect the illegal mix. Its `expecting ';'` says less",
    ),
    (
        "var {",
        "we report the object pattern's property name; the reference reports the missing \
         binding name. Both describe the same truncated `var {`",
    ),
    (
        "async function f() { await = 1 }",
        "we reject the assignment to `await` as an early error; the reference reports the \
         `=` as an unexpected token",
    ),
    (
        "do {}",
        "the reference emits `expecting '<U+FFFD>'`: `js_parse_expect` formats the expected \
         token with `%c` and `TOK_WHILE` does not fit in a byte, so the message carries an \
         invalid UTF-8 byte. Its own source says `XXX: dump token correctly in all cases`. \
         Reproducing corrupted output is not worth a byte of compatibility",
    ),
];

/// Sources whose reference message cannot be pinned as text.
///
/// `do {}` (a `do` with no `while`) is the entire list: the reference formats the expected
/// token with `%c`, and `TOK_WHILE` does not fit in a byte, so it writes an invalid UTF-8
/// byte that arrives here as a replacement character. Whatever it decodes to is an artefact
/// of the transport, not a message, so it is compared but not pinned.
const UNPINNABLE: &[&str] = &["do {}"];

/// Sources where we and the reference disagree about whether to **accept** at all.
///
/// Not a wording problem: one side rejects a program the other accepts, so there is no
/// message to compare. Kept apart from [`KNOWN_DIFFERENCES`] because the two call for
/// different work — a wording gap is a string, an acceptance gap is a missing check.
const KNOWN_ACCEPTANCE_GAPS: &[(&str, &str)] = &[
    (
        "`\\u{ZZ}`",
        "we do not validate escapes in an untagged template literal, so this malformed \
         program is accepted. A tagged template must keep accepting it (`cooked` becomes \
         `undefined`), which is why the fix needs the tagged/untagged distinction rather \
         than a blanket check",
    ),
    (
        "class A { get constructor() {} }",
        "the reference rejects a getter named `constructor`; we accept it",
    ),
    (
        "class A extends B { constructor() { super; } }",
        "the reference rejects a bare `super` reference; we accept it",
    ),
    (
        "[...a=1] = c",
        "the reference rejects a default on a rest *element*; we accept it. The rest \
         *parameter* form is already rejected, so this is the assignment-target path \
         missing its own check",
    ),
];

/// The corpus: every reference syntax-error message this parser can be driven to.
///
/// A `Some` string is the reference's exact `SyntaxError` text, as `qjsc -c` prints it after
/// the `SyntaxError: ` prefix.
const CORPUS: &[Case] = &[
    // -- the generic token message -------------------------------------------
    ("1+", Some("unexpected token in expression: ''")),
    ("if (", Some("unexpected token in expression: ''")),
    ("a.b(", Some("unexpected token in expression: ''")),
    ("throw", Some("unexpected token in expression: ''")),
    ("var a = ;", Some("unexpected token in expression: ';'")),
    ("a ?? b || c", Some("expecting ';'")),
    // -- expected punctuator -------------------------------------------------
    ("({a:1", Some("expecting '}'")),
    ("a ? b", Some("expecting ':'")),
    ("switch (a) { case 1", Some("expecting ':'")),
    ("function f(...a=1){}", Some("expecting ')'")),
    ("function f(", Some("missing formal parameter")),
    ("function f(1)", Some("missing formal parameter")),
    ("function f(...)", Some("missing formal parameter")),
    ("function f(...a,)", Some("expecting ')'")),
    ("function f(...a){}", None),
    ("[...1] = 2", Some("invalid destructuring target")),
    // Recorded from the oracle: for this shape the reference reports the "last one"
    // check before noticing the default.
    ("[...a=1] = c", Some("rest element must be the last one")),
    // -- names ---------------------------------------------------------------
    ("var =", Some("variable name expected")),
    ("let [", Some("variable name expected")),
    ("var {", Some("variable name expected")),
    ("function (", Some("function name expected")),
    ("class {", Some("class statement requires a name")),
    ("a.+b", Some("expecting field name")),
    ("x = {,}", Some("invalid property name")),
    ("class A {", Some("invalid property name")),
    ("class A { x", Some("invalid property name")),
    ("class A { [", Some("unexpected token in expression: ''")),
    (
        "class A { m() {",
        Some("unexpected token in expression: ''"),
    ),
    ("function f(a=1){}", None),
    ("new.t", Some("expecting target")),
    // -- redefinition: the four messages -------------------------------------
    (
        "let x; let x;",
        Some("invalid redefinition of lexical identifier"),
    ),
    (
        "let x; var x;",
        Some("invalid redefinition of lexical identifier"),
    ),
    (
        "var x; let x;",
        Some("invalid redefinition of global identifier"),
    ),
    (
        "{ { var x; let x; } }",
        Some("invalid redefinition of global identifier"),
    ),
    (
        "function f(){ var x; let x; }",
        Some("invalid redefinition of a variable"),
    ),
    (
        "function f(){ function g(){} let g; }",
        Some("invalid redefinition of a variable"),
    ),
    (
        "function f(a){ let a; }",
        Some("invalid redefinition of parameter name"),
    ),
    (
        "try{}catch(e){ let e; }",
        Some("invalid redefinition of lexical identifier"),
    ),
    ("function f(a){ var a; }", None),
    ("try{}catch(e){ var e; }", None),
    // -- assignment and update targets ---------------------------------------
    ("1 = 2", Some("invalid assignment left-hand side")),
    ("a + b = 1", Some("invalid assignment left-hand side")),
    ("[1] = 2", Some("invalid destructuring target")),
    ("({a:1} = 2)", Some("invalid destructuring target")),
    ("++1", Some("invalid increment/decrement operand")),
    ("1++", Some("invalid increment/decrement operand")),
    (
        "-(1) ** 2",
        Some("unparenthesized unary expression can't appear on the left-hand side of '**'"),
    ),
    // -- statements ----------------------------------------------------------
    ("return 1", Some("return not in a function")),
    ("break", Some("break must be inside loop or switch")),
    ("continue", Some("continue must be inside loop")),
    ("super", Some("invalid use of 'super'")),
    ("'use strict'; with (a) {}", Some("invalid keyword: with")),
    (
        "'use strict'; delete x",
        Some("cannot delete a direct reference in strict mode"),
    ),
    ("const y;", Some("missing initializer for const variable")),
    (
        "for (let x = 1 of y) ;",
        Some("a declaration in the head of a for-of loop can't have an initializer"),
    ),
    // -- functions and classes -----------------------------------------------
    (
        "({ get x(a) {} })",
        Some("invalid number of arguments for getter or setter"),
    ),
    (
        "({ set x() {} })",
        Some("invalid number of arguments for getter or setter"),
    ),
    (
        "class A { constructor() {} constructor() {} }",
        Some("property constructor appears more than once"),
    ),
    (
        "class A { static prototype() {} }",
        Some("invalid method name"),
    ),
    (
        "class A { #a; #a; }",
        Some("private class field is already defined"),
    ),
    (
        "function f(a=1) { 'use strict'; }",
        Some("\"use strict\" not allowed in function with default or destructuring parameter"),
    ),
    (
        "'use strict'; var eval = 1",
        Some("invalid variable name in strict mode"),
    ),
    (
        "({ __proto__: 1, __proto__: 2 })",
        Some("duplicate __proto__ property name"),
    ),
    // -- literals ------------------------------------------------------------
    ("0x", Some("invalid number literal")),
    ("1e", Some("invalid number literal")),
    ("1_", Some("invalid number literal")),
    ("0x1_", Some("invalid number literal")),
    ("0b12", Some("invalid number literal")),
    ("\"abc", Some("unexpected end of string")),
    ("'abc", Some("unexpected end of string")),
    ("`abc", Some("unexpected end of string")),
    ("/a", Some("unexpected end of regexp")),
    (
        "\"\\u{ZZ}\"",
        Some("malformed escape sequence in string literal"),
    ),
    (
        "\"\\x\"",
        Some("malformed escape sequence in string literal"),
    ),
    (
        "\"\\u{110000}\"",
        Some("malformed escape sequence in string literal"),
    ),
    (
        "`\\u{ZZ}`",
        Some("malformed escape sequence in string literal"),
    ),
    ("tag`\\u{ZZ}`", None),
    // -- declared differences, kept in the corpus so the list cannot rot ---------
    (
        "for (var i, j of k) ;",
        Some("expected 'of' or 'in' in for control expression"),
    ),
    ("try {} catch", Some("expecting '('")),
    (
        "async function f() { await = 1 }",
        Some("unexpected token in expression: '='"),
    ),
    // `do` with no `while`: compared, but its reference text is unpinnable (see UNPINNABLE).
    ("do {}", None),
    (
        "class A { get constructor() {} }",
        Some("invalid method name"),
    ),
    (
        "class A extends B { constructor() { super; } }",
        Some("invalid use of 'super'"),
    ),
    // -- accepted, as controls ----------------------------------------------
    ("let x;", None),
    ("/a/g = 1", Some("invalid assignment left-hand side")),
];

/// What one source's message is, on each side.
struct Outcome {
    reference: Option<String>,
    ours: Option<String>,
}

fn outcome(oracle: &Engine, dir: &Path, source: &str) -> Outcome {
    let path = dir.join("case.js");
    std::fs::write(&path, source).expect("write the case");
    let out_path = dir.join("out.c");
    let args = [
        "-c",
        "-o",
        out_path.to_str().expect("utf-8 path"),
        path.to_str().expect("utf-8 path"),
    ];
    let reference = match oracle.output(&args) {
        Ok(out) if out.code == 0 => None,
        Ok(out) => {
            let text = if out.stderr.is_empty() {
                &out.stdout
            } else {
                &out.stderr
            };
            Some(
                text.lines()
                    .next()
                    .unwrap_or("")
                    .trim_start_matches("SyntaxError: ")
                    .to_string(),
            )
        }
        Err(error) => Some(format!("<oracle failed: {error}>")),
    };
    let ours = match parse_script(source) {
        Ok(_) => None,
        Err(error) => Some(error.message),
    };
    Outcome { reference, ours }
}

#[test]
fn syntax_error_wording_matches_the_reference() {
    let oracle = require_oracle!();
    let dir = std::env::temp_dir().join(format!("quickjs-rs-wording-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");

    let mut unexpected = Vec::new();
    let mut stale = Vec::new();
    let mut checked = 0usize;
    for (source, _) in CORPUS {
        checked += 1;
        let result = outcome(&oracle, &dir, source);
        let agrees = result.reference == result.ours;
        let listed = KNOWN_DIFFERENCES.iter().any(|(known, _)| known == source)
            || KNOWN_ACCEPTANCE_GAPS
                .iter()
                .any(|(known, _)| known == source);
        match (agrees, listed) {
            (false, false) => unexpected.push(format!(
                "  source: {source:?}\n    reference: {:?}\n    ours:      {:?}",
                result.reference, result.ours
            )),
            // A listed difference that now agrees: the list has rotted and must shrink.
            (true, true) => stale.push(*source),
            _ => {}
        }
    }
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        stale.is_empty(),
        "{} of {} listed differences now agree — remove them from the list:\n{}",
        stale.len(),
        KNOWN_DIFFERENCES.len() + KNOWN_ACCEPTANCE_GAPS.len(),
        stale
            .iter()
            .map(|source| format!("  {source:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        unexpected.is_empty(),
        "{} of {checked} cases disagree with the reference parser:\n{}",
        unexpected.len(),
        unexpected.join("\n")
    );
}

/// Every entry in [`KNOWN_DIFFERENCES`] must name a source that is in [`CORPUS`].
///
/// Otherwise the reason text would describe a case nobody is running, which is how a gap
/// list turns into a comment.
#[test]
fn every_declared_difference_is_actually_run() {
    let missing: Vec<_> = KNOWN_DIFFERENCES
        .iter()
        .chain(KNOWN_ACCEPTANCE_GAPS.iter())
        .map(|(source, _)| *source)
        .filter(|source| !CORPUS.iter().any(|(known, _)| known == source))
        .collect();
    assert!(
        missing.is_empty(),
        "declared differences with no corpus entry: {missing:?}"
    );
}

/// No source is listed twice, so a copy-paste cannot hide a case behind its twin.
#[test]
fn the_corpus_has_no_duplicates() {
    let mut seen = std::collections::BTreeSet::new();
    let duplicates: Vec<_> = CORPUS
        .iter()
        .map(|(source, _)| *source)
        .filter(|source| !seen.insert(*source))
        .collect();
    assert!(
        duplicates.is_empty(),
        "duplicate corpus entries: {duplicates:?}"
    );
}

/// The recorded expectations still describe what the reference does.
///
/// [`CORPUS`] carries a `Some(message)` per source so a failure reads as "we changed"
/// rather than "both moved". That only holds if the recorded text is still the reference's,
/// so it is re-checked here — a reference update that rewords a message shows up as this
/// test failing, not as the differential test silently passing.
#[test]
fn the_recorded_expectations_still_match_the_reference() {
    let oracle = require_oracle!();
    let dir = std::env::temp_dir().join(format!("quickjs-rs-wording-pin-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let mut drifted = Vec::new();
    for (source, expected) in CORPUS {
        if UNPINNABLE.contains(source) {
            continue;
        }
        let result = outcome(&oracle, &dir, source);
        // A `None` expectation means "accepted"; anything else must match verbatim.
        if result.reference.as_deref() != *expected {
            drifted.push(format!(
                "  source: {source:?}\n    recorded: {expected:?}\n    oracle:   {:?}",
                result.reference
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        drifted.is_empty(),
        "the reference no longer reports the recorded message for {} case(s):\n{}",
        drifted.len(),
        drifted.join("\n")
    );
}

/// Modules are parsed separately, and the two module-only messages are checked here.
#[test]
fn module_error_wording_matches_the_reference() {
    let oracle = require_oracle!();
    let dir = std::env::temp_dir().join(format!("quickjs-rs-wording-mod-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let cases = ["export {a};", "import a from;", "export default 1 +"];
    let mut disagreements = Vec::new();
    for source in cases {
        let path = dir.join("case.mjs");
        std::fs::write(&path, source).expect("write the case");
        let out_path = dir.join("out.c");
        let args = [
            "-c",
            "-m",
            "-o",
            out_path.to_str().expect("utf-8 path"),
            path.to_str().expect("utf-8 path"),
        ];
        let reference = match oracle.output(&args) {
            Ok(out) if out.code == 0 => None,
            Ok(out) => {
                let text = if out.stderr.is_empty() {
                    &out.stdout
                } else {
                    &out.stderr
                };
                Some(
                    text.lines()
                        .next()
                        .unwrap_or("")
                        .trim_start_matches("SyntaxError: ")
                        .to_string(),
                )
            }
            Err(error) => Some(format!("<oracle failed: {error}>")),
        };
        let ours = match parse_module(source) {
            Ok(_) => None,
            Err(error) => Some(error.message),
        };
        if reference != ours {
            disagreements.push(format!(
                "  source: {source:?}\n    reference: {reference:?}\n    ours:      {ours:?}"
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        disagreements.is_empty(),
        "module wording disagrees:\n{}",
        disagreements.join("\n")
    );
}

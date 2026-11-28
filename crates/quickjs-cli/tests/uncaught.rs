//! Differential test: what `qjs` prints for an error nothing caught.
//!
//! This is the one output a user sees without asking for it, and it has three separate
//! pieces, each of which was wrong in a different way:
//!
//! 1. **The value, printed not stringified.** The reference reports a pending exception
//!    with `JS_PrintValue` (`js_std_dump_error` is three lines ending in it), which is the
//!    engine's own dumper — the same one behind `std.__printObject`. Reporting through
//!    `ToString` instead printed `throw "s"` without its quotes, `throw {a:1}` as
//!    `[object Object]`, `throw [1,2]` as `1,2`, and an error with no stack at all.
//! 2. **A parse error's position on its own line.** The reference prints
//!    `SyntaxError: <message>` then `    at <file>:<line>:<col>`. Prefixing the file name
//!    instead — `f.js: SyntaxError: 1:5: …` — put the file name where the reference puts
//!    nothing and the position where it puts the file name.
//! 3. **The exception itself, not its `message`.** An earlier version read `name` and
//!    `message` off the object, which loses a user-defined `toString` and a subclass's
//!    inherited `name`.
//!
//! Every case here is compared byte for byte, stderr included. `-e` cases go through
//! `Case::eval` so both binaries name the source `<cmdline>`; script cases share one temp
//! file so both name it identically.

use quickjs_conformance::{Case, Engine, Harness, locate_oracle};
use std::path::PathBuf;

fn candidate() -> Engine {
    Engine::new("rust", env!("CARGO_BIN_EXE_qjs"))
}

/// A directory of script cases, removed when dropped.
struct Scripts(Vec<Case>, PathBuf);

impl Drop for Scripts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.1);
    }
}

/// Write one `.js` per source and return the cases.
fn scripts(tag: &str, sources: &[&str]) -> Scripts {
    let mut dir = std::env::temp_dir();
    dir.push(format!("quickjs-rs-uncaught-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let cases = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let path = dir.join(format!("case{index}.js"));
            std::fs::write(&path, source).expect("write the case");
            Case::script(path)
        })
        .collect();
    Scripts(cases, dir)
}

fn compare(cases: &[Case]) {
    let Some(oracle_path) = locate_oracle() else {
        eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
        return;
    };
    let harness = Harness::new(candidate(), Engine::new("reference", oracle_path));
    let report = harness.run(cases);
    assert!(!report.has_failures(), "{}", report.summary());
}

/// The printed form of a thrown value, for every kind the printer distinguishes.
#[test]
fn a_thrown_value_is_printed_not_stringified() {
    let cases = [
        // Primitives: a string keeps its quotes, which `ToString` drops.
        "throw 1",
        "throw \"s\"",
        "throw true",
        "throw null",
        "throw undefined",
        "throw 1n",
        // Objects and arrays, which `ToString` flattened to `[object Object]` and `1,2`.
        "throw {a: 1}",
        "throw {a: {b: [1]}}",
        "throw [1, 2]",
        "throw []",
        "throw {}",
        // Things that only a printer can render.
        "throw Symbol(\"y\")",
        "throw function named() {}",
        "throw new Map([[1, 2]])",
        "throw new Set([1])",
        "var cycle = {}; cycle.self = cycle; throw cycle",
    ];
    let scripts = scripts("values", &cases);
    compare(&scripts.0);
}

/// An error carries its stack, whether the engine or a constructor made it.
#[test]
fn a_thrown_error_carries_its_stack() {
    let cases = [
        "throw new Error(\"b\")",
        "throw new TypeError(\"t\")",
        "null.x",
        "undefined()",
        "function f(){ throw new TypeError(\"t\") } f()",
        "function f(){ null.x } function g(){ f() } g()",
        // A subclass inherits `name` from the prototype, which reading the instance misses.
        "class MyError extends Error {} throw new MyError(\"m\")",
        // A user-defined `toString` wins over the class name.
        "function E(m){this.message=m} E.prototype.toString=function(){return \"E!\"}\nthrow new E(\"x\")",
        // A rejection nothing handled is reported the same way, one line later.
        "Promise.reject(new Error(\"r\"))",
        "Promise.reject(42)",
    ];
    let scripts = scripts("errors", &cases);
    compare(&scripts.0);
}

/// A parse error prints its message, then where it happened.
#[test]
fn a_syntax_error_prints_its_position() {
    let cases = [
        "var =",
        "let x = 1;\nlet y = ;\n",
        "if (",
        "class A {",
        "'unterminated",
        "1 2",
        "for (let x = 1 of y) ;",
    ];
    let scripts = scripts("syntax", &cases);
    compare(&scripts.0);
}

/// The same, through `-e`, where the source is named `<cmdline>`.
#[test]
fn a_syntax_error_from_the_command_line_names_cmdline() {
    let cases = [
        Case::eval("var ="),
        Case::eval("1 2"),
        Case::eval("1n + 1"),
        Case::eval("null.x"),
    ];
    compare(&cases);
}

/// A module that parses but does not link reports no position at all.
///
/// `export {a}` with no `a` is caught in `js_resolve_module`, after parsing, so the
/// reference has nothing to point at and prints a bare message. Our parser makes the same
/// check itself, so it has to say "report me without a position" explicitly.
#[test]
fn a_module_link_error_prints_no_position() {
    let Some(oracle_path) = locate_oracle() else {
        eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
        return;
    };
    let mut dir = std::env::temp_dir();
    dir.push(format!("quickjs-rs-uncaught-mod-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("case.mjs");
    std::fs::write(&path, "export {a};").expect("write the case");

    let reference = Engine::new("reference", oracle_path);
    let expected = reference
        .output(&["-m", path.to_str().expect("utf-8 path")])
        .expect("run reference");
    let actual = candidate()
        .output(&["-m", path.to_str().expect("utf-8 path")])
        .expect("run candidate");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(actual.code, expected.code);
    assert_eq!(actual.stderr, expected.stderr);
}

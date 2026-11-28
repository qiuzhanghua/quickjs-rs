//! Differential test: redeclarations and duplicate parameter names.
//!
//! These are the two families of *early error* an engine gets wrong quietly, because
//! accepting a program the spec rejects is invisible until something else trips over the
//! duplicated binding. Both are also where a naive model goes wrong in opposite directions:
//!
//! * A function declaration in a **block** is a lexical binding of that block, so
//!   `{ function f(){} var f }` is an error — while the same two declarations in a function
//!   *body* are accepted, because there a function declaration behaves like a `var`. The
//!   reference reaches both through one `define_var` call whose branch depends on
//!   `fd->scope_level == fd->body_scope`, and adds an Annex B exception that applies only to
//!   *plain* functions in sloppy mode.
//! * A duplicate **parameter** has two different messages. Which one is used depends on how
//!   the duplicate was reached: a name inside a destructuring pattern, or any name once the
//!   list is known to contain an `=`, is checked while the parameters are parsed
//!   (`duplicate parameter names`); everything else waits for the final check
//!   (`duplicate argument names`).
//!
//! Verdicts come from the reference `qjsc -c` (parse and compile only, and its exit status
//! is a pure accept/reject verdict). Positions are compared for the redeclaration family
//! only: the reference's `js_parse_error` always points at the *current token*, which for a
//! redeclaration is whatever follows the offending name, and our checks now sit at the same
//! points. The duplicate-parameter family is compared on the message alone, because matching
//! the position there needs the reference's `SKIP_HAS_ASSIGNMENT` lookahead, which reports
//! the token after a *pattern's inner* name — a known gap, recorded in `docs/m9-plan.md`.

use js_parse::{parse_module, parse_script};
use quickjs_conformance::{Engine, locate_qjsc};
use std::path::PathBuf;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "quickjs-rs-redecl-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write the case");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the reference said about one source: `None` when it parsed, else the first line of
/// its `SyntaxError` with the prefix removed.
fn reference(qjsc: &Engine, dir: &TempDir, source: &str, module: bool) -> Option<String> {
    let name = if module { "case.mjs" } else { "case.js" };
    let path = dir.write(name, source);
    let out_path = dir.0.join("out.c");
    let mut args = vec!["-c", "-o", out_path.to_str().expect("utf-8 path")];
    if module {
        args.push("-m");
    }
    args.push(path.to_str().expect("utf-8 path"));
    let output = qjsc.output(&args).expect("run the reference");
    let text = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    text.lines()
        .next()
        .filter(|line| !line.is_empty())
        .map(|line| line.trim_start_matches("SyntaxError: ").to_string())
}

/// Our verdict with the position appended, the way the reference prints one.
fn ours_with_position(source: &str, module: bool) -> Option<String> {
    let result = if module {
        parse_module(source).err()
    } else {
        parse_script(source).err()
    };
    result.map(|error| {
        if error.positionless {
            error.message
        } else {
            format!(
                "{}:{}:{}",
                error.message, error.span.start.line, error.span.start.column
            )
        }
    })
}

/// The reference's message with its trailing `line:column` folded on, from the `at` line.
fn reference_with_position(
    qjsc: &Engine,
    dir: &TempDir,
    source: &str,
    module: bool,
) -> Option<String> {
    let name = if module { "case.mjs" } else { "case.js" };
    let path = dir.write(name, source);
    let out_path = dir.0.join("out.c");
    let mut args = vec!["-c", "-o", out_path.to_str().expect("utf-8 path")];
    if module {
        args.push("-m");
    }
    args.push(path.to_str().expect("utf-8 path"));
    let output = qjsc.output(&args).expect("run the reference");
    let text = if output.stderr.is_empty() {
        &output.stdout
    } else {
        &output.stderr
    };
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let message = lines.next()?.trim_start_matches("SyntaxError: ");
    // `    at case.js:1:15` — the position is the last two colon-separated fields.
    match lines.next().and_then(|line| {
        let mut fields = line.rsplitn(3, ':');
        let column = fields.next()?;
        let line_number = fields.next()?;
        Some((line_number.to_string(), column.to_string()))
    }) {
        Some((line_number, column)) => Some(format!("{message}:{line_number}:{column}")),
        None => Some(message.to_string()),
    }
}

fn qjsc() -> Option<Engine> {
    locate_qjsc().map(|path| Engine::new("reference", path))
}

/// Redeclarations, compared on message *and* position.
///
/// One entry per rule rather than per spelling: each line below was added because it
/// separates two branches of the reference's `define_var`.
const REDECLARATIONS: &[&str] = &[
    // Lexical versus lexical, in one block.
    "{ let a; let a; }",
    "{ const a = 1; let a; }",
    "{ class a{}; let a; }",
    "{ let a; class a{} }",
    "{ 'use strict'; { class a{}; class a{} } }",
    // A block-level function declaration is lexical, in both orders and both modes.
    "{ function a(){} let a; }",
    "{ let a; function a(){} }",
    "{ function a(){} var a; }",
    "{ var a; function a(){} }",
    "{ function a(){} const a = 1; }",
    "'use strict'; { function a(){} var a; }",
    // …and two function declarations obey Annex B.3.3.4, which requires both to be plain
    // and the mode to be sloppy.
    "{ function a(){} function a(){} }",
    "'use strict'; { function a(){} function a(){} }",
    "{ function a(){} function* a(){} }",
    "{ async function a(){} function a(){} }",
    "{ function* a(){} function* a(){} }",
    // A function declaration in a function *body* is var-like instead.
    "function f(){ function a(){} var a; }",
    "function f(){ var a; function a(){} }",
    "function f(){ let a; function a(){} }",
    "function f(){ function a(){} let a; }",
    "function f(){ function a(){} function a(){} }",
    "function f(){ let a; let a; }",
    "function f(){ var a; let a; }",
    "function f(){ let a; var a; }",
    // At the top level of a script the lexical check is skipped for a function
    // declaration — "avoid creating a lexical variable in the global scope".
    "function a(){} let a;",
    "let a; function a(){}",
    "function a(){} var a;",
    "var a; function a(){}",
    // Parameters and catch bindings clash differently from each other.
    "function f(a){ let a; }",
    "function f(a){ const a = 1; }",
    "function f(a){ var a; }",
    "function f(){ try{}catch(e){ let e; } }",
    "function f(){ try{}catch(e){ var e; } }",
    "function f(){ try{}catch(e){ function e(){} } }",
    // A `var` two scopes out is a global one, and the message says so.
    "{ let a; { var a; } }",
    "{ var a; { let a; } }",
    "{ function a(){} { function a(){} } }",
    // `using` is lexical too, and clashes the same way.
    "{ using a = null; let a }",
    "{ let a; using a = null }",
    "{ using a = null; using a = null }",
    "{ var a; using a = null }",
];

#[test]
fn redeclarations_match_the_reference() {
    let Some(qjsc) = qjsc() else {
        eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
        return;
    };
    let dir = TempDir::new("redecl");
    let mut disagreements = Vec::new();
    for source in REDECLARATIONS {
        for module in [false, true] {
            let want = reference_with_position(&qjsc, &dir, source, module);
            let got = ours_with_position(source, module);
            if want != got {
                let context = if module { "module" } else { "script" };
                disagreements.push(format!(
                    "  {source:?} ({context})\n    reference: {want:?}\n    ours:      {got:?}"
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} checks disagree:\n{}",
        disagreements.len(),
        REDECLARATIONS.len() * 2,
        disagreements.join("\n")
    );
}

/// Duplicate parameters, compared on the message only.
const DUPLICATE_PARAMETERS: &[(&str, &str)] = &[
    // A sloppy simple list accepts them; a strict one does not.
    ("function f(a, a){}", ""),
    ("'use strict'; function f(a, a){}", "argument"),
    ("function f(a, a){'use strict';}", "argument"),
    // A plain generator or `async function` behaves like a plain function here; only an
    // async *method* is covered by the reference's condition.
    ("function* f(a, a){}", ""),
    ("async function f(a, a){}", ""),
    ("async function* f(a, a){}", ""),
    // Methods are always checked, however sloppy.
    ("({m(a, a){}})", "argument"),
    ("class C { m(a, a){} }", "argument"),
    ("class C { static m(a, a){} }", "argument"),
    ("class C { constructor(a, a){} }", "argument"),
    // Arrows too.
    ("(a, a) => {};", "argument"),
    // An `=` anywhere in the list switches every duplicate to the early message...
    ("function f(a, a = 1){}", "parameter"),
    ("function f(a = 1, a){}", "parameter"),
    ("(a, a = 1) => 0;", "parameter"),
    ("({m(a, a = 1){}})", "parameter"),
    // ...and so does a name bound inside a pattern.
    ("function f([a], [a]){}", "parameter"),
    ("function f(a, [a]){}", "parameter"),
    ("class C { m(a, [a]){} }", "parameter"),
    ("([a], [a]) => 0;", "parameter"),
    // A rest parameter is not a pattern, so it leaves the message alone — but it does make
    // the list non-simple, which makes the duplicate illegal in the first place.
    ("function f(a, ...a){}", "argument"),
    ("(a, ...a) => 0;", "argument"),
    // Which duplicate is reported: the second plain one is not checked when the third is a
    // pattern, so the *third* is what the reference names.
    ("function f([a], a, [a]){}", "parameter"),
    ("function f([a], a, a){}", "argument"),
    ("function f(a, a, [b]){}", "argument"),
];

/// The two messages, spelled out so the expectation cannot drift.
fn expected_duplicate_message(kind: &str) -> String {
    match kind {
        "argument" => "duplicate argument names not allowed in this context".to_string(),
        "parameter" => "duplicate parameter names not allowed in this context".to_string(),
        other => panic!("unexpected kind {other:?}"),
    }
}

#[test]
fn duplicate_parameters_match_the_reference() {
    let Some(qjsc) = qjsc() else {
        eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
        return;
    };
    let dir = TempDir::new("params");
    let mut disagreements = Vec::new();
    for (source, kind) in DUPLICATE_PARAMETERS {
        let parsed = js_parse::parse_script(source);
        let got = match parsed {
            Ok(_) => None,
            Err(error) => Some(error.message),
        };
        // The reference's verdict is the oracle; the table's `kind` only pins which message
        // it must be, so a change in the reference is visible rather than absorbed.
        let want = reference(&qjsc, &dir, source, false);
        let pinned = if kind.is_empty() {
            None
        } else {
            Some(expected_duplicate_message(kind))
        };
        if want != got || pinned != want {
            disagreements.push(format!(
                "  {source:?}\n    reference: {want:?}\n    ours:      {got:?}\n    \
                 pinned:    {pinned:?}"
            ));
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} checks disagree:\n{}",
        disagreements.len(),
        DUPLICATE_PARAMETERS.len(),
        disagreements.join("\n")
    );
}

/// The reference's duplicate-parameter positions are a known gap, and this records exactly
/// what the gap is rather than leaving it to be rediscovered.
///
/// A redeclaration points at the token after the offending name, and we now match that. A
/// duplicate *parameter* points at the same token, but for a name inside a destructuring
/// pattern the reference reports the token after the *inner* name (`let [a] = []` → the
/// `]`) — and our check runs once the whole pattern has been parsed.
#[test]
fn a_redeclaration_position_is_the_token_after_the_name() {
    let parsed = parse_script("{ let a; let a = 1; }").expect_err("a redeclaration");
    // `{ let a; let a = 1; }` — the `=` follows the second `a`.
    assert_eq!((parsed.span.start.line, parsed.span.start.column), (1, 16));
}

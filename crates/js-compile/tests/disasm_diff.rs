//! Differential test: our disassembly against the reference compiler's.
//!
//! The oracle is a QuickJS build with `-DDUMP_BYTECODE=1`, which prints each
//! function's bytecode through `js_dump_function_bytecode`. Comparing that text
//! with ours is the strongest check available before a VM exists: it pins down
//! instruction choice, operand encoding, slot numbering, scope chains, stack
//! size and jump targets all at once.
//!
//! Build the oracle with `scripts/setup-oracle.sh` (it produces
//! `/tmp/quickjs-master/qjsc-dump`).

use js_compile::{compile_script, disassemble};
use js_parse::parse_script;
use quickjs_conformance::locate_disassembler;
use std::path::PathBuf;

fn oracle() -> Option<PathBuf> {
    locate_disassembler()
}

macro_rules! require_oracle {
    () => {
        match oracle() {
            Some(path) => path,
            None => {
                eprintln!("skipping: no disassembling qjsc found (run scripts/setup-oracle.sh)");
                return;
            }
        }
    };
}

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Scratch {
        // A per-call counter, because a thread id can be reused and two tests
        // started at once would otherwise share a directory.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "quickjs-rs-compile-{tag}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("scratch dir");
        Scratch(path)
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write case");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Run the reference compiler and return its disassembly, or `None` if the
/// program does not parse there.
fn reference_disassembly(oracle: &PathBuf, scratch: &Scratch, source: &str) -> Option<String> {
    let file = scratch.file("case.js", source);
    let output = std::process::Command::new(oracle)
        .arg("-c")
        .arg("-o")
        .arg(scratch.0.join("out.c"))
        .arg(&file)
        .output()
        .expect("run reference compiler");
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The disassembly with the file-name prefix stripped, so the comparison does
/// not depend on where the temporary file lives.
fn normalise(text: &str, path: &str) -> String {
    text.replace(path, "<file>")
}

/// Our disassembly, with the same file-name prefix the reference prints.
fn ours(source: &str, path: &str) -> Result<String, String> {
    let program = parse_script(source).map_err(|e| e.to_string())?;
    let mut function = compile_script(&program).map_err(|e| e.to_string())?;
    // Every function carries the file name in the reference's dump, nested ones
    // included.
    set_filename(&mut function, "<file>");
    let _ = path;
    Ok(disassemble(&function))
}

/// Stamp every function in the tree with a filename.
fn set_filename(function: &mut js_compile::Function, name: &str) {
    function.filename = Some(name.to_string());
    for constant in &mut function.cpool {
        if let js_compile::bytecode::Constant::Function(inner) = constant {
            set_filename(inner, name);
        }
    }
}

fn compare(cases: &[&str]) -> Vec<String> {
    let Some(oracle) = oracle() else {
        return Vec::new();
    };
    let scratch = Scratch::new("diff");
    let mut failures = Vec::new();
    for source in cases {
        let Some(reference) = reference_disassembly(&oracle, &scratch, source) else {
            continue;
        };
        let path = scratch.0.join("case.js");
        let path = path.to_str().expect("utf-8 path");
        let reference = normalise(&reference, path);
        match ours(source, path) {
            Err(err) => failures.push(format!("  {source:?}\n    we failed: {err}")),
            Ok(actual) => {
                if actual != reference {
                    failures.push(format!("  {source:?}\n{}", diff(&reference, &actual)));
                }
            }
        }
    }
    failures
}

/// A side-by-side diff, so a failure names the instruction that differs.
fn diff(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let mut out = String::from("    reference                              | ours\n");
    let mut shown = 0;
    for index in 0..expected_lines.len().max(actual_lines.len()) {
        let e = expected_lines.get(index).copied().unwrap_or("<missing>");
        let a = actual_lines.get(index).copied().unwrap_or("<missing>");
        if e == a {
            if shown > 0 && index + 1 < expected_lines.len().max(actual_lines.len()) {
                // Keep a little context after a difference.
                out.push_str(&format!("    {e:<38} | {a}\n"));
                shown += 1;
            }
            continue;
        }
        out.push_str(&format!("  > {e:<38} | {a}\n"));
        shown += 1;
        if shown > 12 {
            out.push_str("    …\n");
            break;
        }
    }
    out
}

#[test]
fn literals_match_the_reference() {
    require_oracle!();
    let cases = [
        "1;",
        "0;",
        "-1;",
        "7;",
        "8;",
        "100;",
        "-128;",
        "127;",
        "1000;",
        "1000000;",
        "1.5;",
        "0.1;",
        "true;",
        "false;",
        "null;",
        "\"s\";",
        "\"\";",
        "\"two words\";",
        "undefined;",
    ];
    assert_cases(&cases);
}

#[test]
fn operators_match_the_reference() {
    require_oracle!();
    let cases = [
        "a + b;",
        "a - b;",
        "a * b;",
        "a / b;",
        "a % b;",
        "a ** b;",
        "a << b;",
        "a >> b;",
        "a >>> b;",
        "a & b;",
        "a | b;",
        "a ^ b;",
        "a < b;",
        "a <= b;",
        "a > b;",
        "a >= b;",
        "a == b;",
        "a != b;",
        "a === b;",
        "a !== b;",
        "a in b;",
        "a instanceof b;",
        "-a;",
        "+a;",
        "!a;",
        "~a;",
        "typeof a;",
        "void a;",
        "a && b;",
        "a || b;",
        "a ? b : c;",
        "a, b;",
        "(a, b, c);",
    ];
    assert_cases(&cases);
}

#[test]
fn variables_match_the_reference() {
    require_oracle!();
    let cases = [
        "var x;",
        "var x = 1;",
        "var x = 1, y = 2;",
        "let x = 1;",
        "const x = 1;",
        "x = 1;",
        "x += 1;",
        "x -= 1;",
        "x *= 2;",
        "x++;",
        "x--;",
        "++x;",
        "--x;",
        "var x = 1; x;",
        "function f() { var x = 1; return x; }",
        "function f(a) { return a; }",
        "function f(a, b) { return a + b; }",
        "function f() { return 1; } f();",
    ];
    assert_cases(&cases);
}

/// Constructs the compiler rejects with a clear error rather than compiling them
/// wrongly. Each one is a known gap, listed here so the list cannot silently
/// grow without a test noticing.
#[test]
fn unsupported_constructs_fail_loudly() {
    for source in [
        "for (;;) break;",
        "for (;;) continue;",
        "l: a;",
        "switch (a) { }",
        "try { } catch (e) { }",
        "for (const x of xs) ;",
        "class A { }",
        "function* g() { }",
        "async function f() { }",
        "let [a] = x;",
        "with (a) b;",
    ] {
        let program = parse_script(source).expect("parses");
        let error =
            compile_script(&program).expect_err(&format!("{source:?} should not compile yet"));
        assert!(!error.message.is_empty(), "{source:?} needs a message");
    }
}

#[test]
fn statements_match_the_reference() {
    require_oracle!();
    let cases = [
        "if (a) b;",
        "if (a) b; else c;",
        "if (a) { b; } else { c; }",
        "while (a) b;",
        "return_ = 1;",
        "a.b;",
        "a[b];",
        "a.b = 1;",
        "a[0] = 1;",
        "f();",
        "f(1);",
        "f(1, 2);",
        "f(1, 2, 3);",
    ];
    assert_cases(&cases);
}

/// The temporal dead zone and the scope chain.
///
/// Every case here exercises `set_loc_uninitialized`, the `_check` opcodes, or
/// the `scope_next` links the runtime walks to instantiate a scope.
#[test]
fn tdz_and_scopes_match_the_reference() {
    require_oracle!();
    let cases = [
        // A `let` in a function body starts uninitialised.
        "function f(){ let x = 1; return x; }",
        "function f(){ let x; return x; }",
        "function f(){ let x; x = 1; return x; }",
        "function f(){ let x = 0; x = 1; return x; }",
        "function f(){ let x = 0; x += 1; }",
        "function f(){ let x = 0; x++; }",
        "function f(){ const c = 1; return c; }",
        // Reads before the declaration are still inside the dead zone.
        "function f(){ return x; let x = 1; }",
        "function f(){ g(); let x = 1; }",
        // Blocks: the binding is marked when the block is entered.
        "function f(){ { let x = 1; } return 0; }",
        "function f(){ { let x = 1; return x; } }",
        "function f(){ let a = 1; { let b = 2; return b; } }",
        "function f(){ let a; { let b = a; } }",
        "function f(){ { let a; let b; } }",
        "function f(){ let a; let b; }",
        "function f(){ let a; let b; let c; }",
        "function f(){ { let a; } { let b; } }",
        "function f(){ let a; { let b; { let c; } } }",
        "function f(){ { { let c; } let b; } let a; }",
        "function f(){ { let a = 1; } let b = 2; }",
        "function f(){ let b = 2; { let a = 1; } }",
        // `var` never enters the dead zone.
        "function f(){ return v; var v = 1; }",
        "function f(){ var v = 1; { let w = 2; } return v; }",
        // Closures capture the binding, which forces a reference cell.
        "function f(){ let x = 1; return function(){ return x; }; }",
        "function f(){ let x = 0; return function(){ x += 1; }; }",
        "function f(){ let x = 1; return function(){ x = 2; return x; }; }",
        "function f(a){ return function(){ return a; }; }",
        "function f(){ var v = 1; return function(){ return v; }; }",
        // Function declarations are instantiated when the scope is entered.
        "function f(){ function g(){} return g; }",
        "function f(){ a; function g(){} }",
        "g(); function f(){ return 1; }",
        "function f(){ function g(){} function h(){} return h; }",
        // A captured binding in a nested block is closed when the block ends.
        "function f(){ { let x = 1; g = function(){ return x; }; } }",
        "function f(){ let a = 1; { let b = 2; g = function(){ return a + b; }; } }",
        // Globals reached from a nested function go through the parent's entry.
        "function f(){ g(); }",
        "var g; function f(){ g(); }",
        "function f(){ function h(){ k(); } }",
        "function f(){ g(); h(); }",
        // Dead code after a `return` is not compiled.
        "function f(){ return 1; let x = 2; }",
        "function f(){ return 1; var x = 2; }",
        "function f(){ return; g(); }",
        "function f(){ throw 1; g(); }",
        // `const` is checked like `let`, but never written.
        "function f(){ const c = 1; return c; }",
        "function f(){ const c = 1; c; return c; }",
        // Source-order slot numbering must survive interleaved declarations.
        "function f(){ let a = 1; var b = 2; }",
        "function f(){ var b = 2; let a = 1; }",
        "function f(){ { let a = 1; } var b = 2; }",
        "function f(){ var b = 2; { let a = 1; } }",
        "function f(){ let a = 1; function g(){} }",
        // Nesting depth: the chain has to reach across several scopes.
        "function f(){ let a; { let b; { let c; } } }",
        "function f(){ { { let c; } let b; } let a; }",
        "function f(){ let a; { let b; } let c; }",
    ];
    assert_cases(&cases);
}

/// Function name inference and closure plumbing, which the TDZ work touches.
#[test]
fn named_functions_and_captures_match_the_reference() {
    require_oracle!();
    let cases = [
        "var f = function(){};",
        "var f = function named(){};",
        "g = function(){};",
        "function k(){ var f = function(){}; }",
        "function k(p){ return function(){ return p; }; }",
        "function k(p){ var q = p; return function(){ return q + p; }; }",
        "function k(){ var a = 1; var b = 2; return function(){ return a + b; }; }",
        "function k(){ return function(){ return function(){ return 1; }; }; }",
        "function k(){ var a = 1; return function(){ return function(){ return a; }; }; }",
        "function k(){ { let a = 1; } return 0; }",
        "function k(){ for (let i = 0; i < 1; i++); }",
        "function k(){ for (;;) { let a = 1; } }",
        "function k(){ { } }",
        "function k(){ {} {} }",
        "function k(){}",
        "function k(a, b, c, d){ return d; }",
        "function k(a, b, c, d, e){ return e; }",
    ];
    assert_cases(&cases);
}

fn assert_cases(cases: &[&str]) {
    let failures = compare(cases);
    assert!(
        failures.is_empty(),
        "{} of {} cases differ from the reference:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n")
    );
}

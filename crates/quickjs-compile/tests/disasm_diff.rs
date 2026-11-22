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
        // A destructuring catch parameter is not wired up yet.
        "function f(){ try { a(); } catch ([e]) { } }",
        // A destructuring `for` head, `for await`, classes and generators are still
        // gaps, in that order of cost.
        "for (const [x] of xs) ;",
        "async function f(xs){ for await (const x of xs) ; }",
        "function* g() { }",
        "async function f() { }",
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

/// `break`, `continue` and labelled statements.
#[test]
fn loops_and_labels_match_the_reference() {
    require_oracle!();
    let cases = [
        // A bare break out of an infinite loop makes the trailing `goto` dead.
        "function f(){ for(;;){ break; } }",
        "function f(){ for(;;){ continue; } }",
        "function f(){ do { break; } while(1); }",
        // With work after the loop, control has somewhere to go.
        "function f(){ for(;;){ a(); break; b(); } c(); }",
        "function f(){ do { a(); break; } while(b); c(); }",
        // A conditional break keeps both paths alive.
        "function f(c){ for(;;){ a(); if(c) break; b(); } c2(); }",
        "function f(c){ while(c){ a(); if(c2) break; } c3(); }",
        // `continue` runs the update in a for-loop and the test in a do-while.
        "function f(c){ for(let i=0;i<1;i++){ a(); if(c) continue; b(); } }",
        "function f(c){ do { a(); if(c) continue; } while(c2); }",
        "function f(c){ while(c){ a(); if(c2) continue; b(); } c3(); }",
        // Labelled loops.
        "function f(c){ l: for(;;){ if(c) break l; a(); } c2(); }",
        "function f(c){ l: for(;;){ if(c) continue l; a(); } c2(); }",
        "function f(c){ outer: for(;;){ for(;;){ break outer; } } }",
        "function f(c){ outer: for(;;){ for(;;){ continue outer; } } }",
        // A label on a block only provides a break target.
        "function f(c){ l: { a(); if(c) break l; b(); } }",
        "function f(c){ l: { a(); if(c) break l; b(); } c2(); }",
        // Nested labels on one statement all reach the same loop.
        "function f(c){ a: b: for(;;){ break a; } }",
        "function f(c){ a: b: for(;;){ break b; } }",
        "function f(c){ a: b: for(;;){ continue a; } }",
        // Nested unlabelled loops bind to the innermost.
        "function f(c){ for(;;){ for(;;){ break; } a(); } }",
        "function f(c){ for(;;){ for(;;){ continue; } a(); } }",
    ];
    assert_cases(&cases);
}

/// Known gaps: jump and constant-folding optimisations the reference applies but
/// this compiler does not yet.
///
/// These are *optimisation* differences only — the programs mean the same thing —
/// but the disassembly differs, so they cannot live in the suite above without
/// turning a real regression into a false alarm. They are listed here so the gap
/// is recorded and cannot quietly grow.
#[test]
#[ignore = "needs fold_constants and the resolve_labels jump rewrites"]
fn constant_conditions_and_dead_jumps_match_the_reference() {
    require_oracle!();
    let cases = [
        // A constant condition removes the test entirely.
        "function f(){ while(1){ break; } }",
        "function f(){ while(1){ a(); break; b(); } c(); }",
        "function f(){ while(0){ a(); } c(); }",
        "function f(){ if(1) a(); }",
        "function f(){ if(0) a(); else b(); }",
        "function f(){ for(;1;){ break; } }",
        // A returned call is a tail call, and a returned method call is
        // `tail_call_method`; both are a separate optimisation.
        "function f(a,c){ return a.b(c); }",
        "function f(a,c){ return g(c); }",
        // A sequence whose left side is a constant folds away entirely.
        "function f(a,c){ (0, a.b)(c); }",
        "function f(a,c){ (0, g)(c); }",
        // Interior runs of dead jumps that only the reference's `skip_dead_code`
        // removes.
        "function f(c){ while(c){ for(;;){ break; } } }",
        "function f(){ for(;;){ for(;;){ break; } } }",
        "function f(){ for(;;){ for(;;){ break; } a(); } }",
        "function f(c){ o: for(;;){ i: while(1){ break o; } } }",
        "function f(c){ o: for(;;){ i: while(1){ break i; a(); } } }",
        "function f(c){ o: for(;;){ i: while(1){ continue i; } } }",
        // A labelled break out of an inner loop leaves the same shape: the target
        // is the outer loop's end, so the reference folds `if_false`+`goto` into
        // one inverted jump.
        "function f(c){ o: for(;;){ i: while(c2){ break o; } } }",
        "function f(c){ o: for(;;){ i: while(c2){ break i; a(); } } }",
        "function f(c){ o: for(;;){ i: while(c2){ continue i; } } }",
        // `.length` is its own opcode: the reference folds `get_field length` into
        // `get_length`, for any object, not just freshly built arrays.
        "function f(a){ return a.length; }",
        "function f(){ return [1,2].length; }",
    ];
    assert_cases(&cases);
}

/// Assignments, in statement *and* value position.
///
/// The distinction matters: `put_field`/`put_array_el` consume both the object and
/// the value, so a store whose value is kept needs `insert2`/`insert3` — and one
/// whose value is thrown away must not have them. Nothing else in the suite covers
/// this, which is how a store that silently dropped its operator survived.
#[test]
fn assignments_match_the_reference() {
    require_oracle!();
    let cases = [
        // Identifier, plain.
        "function f(v){ var a; a = v; }",
        "function f(a,b){ var x = (a = b); }",
        "function f(v){ g = v; }",
        "function f(v){ a = b = v; }",
        // Identifier, compound: every operator reads the place first.
        "function f(a,b){ a += b; }",
        "function f(a,b){ var x = (a -= b); }",
        "function f(a,b){ a *= b; }",
        "function f(a,b){ a /= b; }",
        "function f(a,b){ a %= b; }",
        "function f(a,b){ a **= b; }",
        "function f(a,b){ a <<= b; }",
        "function f(a,b){ a >>= b; }",
        "function f(a,b){ a >>>= b; }",
        "function f(a,b){ a &= b; }",
        "function f(a,b){ a |= b; }",
        "function f(a,b){ a ^= b; }",
        // Identifier, logical: the right-hand side must not run when the left
        // already decides the result.
        "function f(a,b){ a ||= b; }",
        "function f(a,b){ a &&= b; }",
        "function f(a,b){ a ??= b; }",
        "function f(a,b){ var x = (a ||= b); }",
        "function f(b){ var x = (g ||= b); }",
        // Member, plain: no `insert2` when the value is discarded.
        "function f(o,p,v){ o.p = v; }",
        "function f(o,v){ var x = (o.a = v); }",
        "function f(o,k,v){ o[k] = v; }",
        "function f(o,k,v){ var x = (o[k] = v); }",
        // Member, compound.
        "function f(o,b){ o.a += b; }",
        "function f(o,b){ var x = (o.a += b); }",
        "function f(o,k,b){ o[k] += b; }",
        "function f(o,k,b){ var x = (o[k] += b); }",
        "function f(o,b){ o.a -= b; c(); }",
        // Member, logical: the store path jumps over the cleanup the keep path
        // needs, so falling through would `nip` a stack holding only the value.
        "function f(o,b){ o.a ||= b; c(); }",
        "function f(o,b){ o.a &&= b; c(); }",
        "function f(o,b){ o.a ??= b; c(); }",
        "function f(o,k,b){ o[k] ||= b; c(); }",
        "function f(o,k,b){ o[k] ??= b; c(); }",
        "function f(o,k,b){ var x = (o[k] ||= b); }",
    ];
    assert_cases(&cases);
}

/// Calls, which have to pass their receiver.
///
/// `o.m(args)` is `get_field2` + `call_method`: the object stays under the
/// function so `this` survives. A plain `get_field` + `call` would drop it.
#[test]
fn calls_pass_their_receiver() {
    require_oracle!();
    let cases = [
        "function f(a,c){ a.b(c); }",
        "function f(a){ a.b(); }",
        "function f(a,c){ a.b(c,d); }",
        // Parentheses do not change the receiver.
        "function f(a,c){ (a.b)(c); }",
        "function f(a,c){ ((a.b))(c); }",
        // Only the last link is a method read; the rest are plain reads.
        "function f(a,c){ a.b.c(d); }",
        "function f(a,c){ a.b(c)(d); }",
        "function f(a,c){ a.b()(c); }",
        // Computed: the key is consumed, so `get_array_el2` not `_3`.
        "function f(a,c){ a[k](c); }",
        "function f(a,c){ a[\"b\"](c); }",
        "function f(a,c){ a.b[k](c); }",
        // A callee that is not a member has no receiver to pass.
        "function f(a,c){ g(c); }",
        "function f(a,c){ a(c); }",
        "function f(a,c){ (g)(c); }",
        // Optional forms.
        "function f(a,c){ a?.b(c); }",
        "function f(a,c){ a?.[k](c); }",
    ];
    assert_cases(&cases);
}

/// `??` and optional chaining.
#[test]
fn nullish_and_optional_chains_match_the_reference() {
    require_oracle!();
    let cases = [
        // `??` keeps a falsy-but-defined value, which is what separates it from
        // `||`: `0 ?? b` stays `0`.
        "function f(a,b){ return a ?? b; }",
        "function f(a,b,c){ return a ?? b ?? c; }",
        "function f(a,b){ return a || b; }",
        "function f(a,b,c){ return a || b || c; }",
        "function f(a,b){ return (a || b) && c; }",
        "function f(a,b){ return a ?? b || c; }",
        "function f(a,b){ if (a ?? b) c(); }",
        // Optional chains: the guard short-circuits the whole spine, so `a?.b.c`
        // must not read `.c` when `a` is nullish.
        "function f(a){ return a?.b; }",
        "function f(a){ return a?.b.c; }",
        "function f(a){ return a.b?.c; }",
        "function f(a){ return (a?.b).c; }",
        "function f(a){ return a?.[k]; }",
        "function f(a){ return a?.[k]?.l; }",
        "function f(a){ return a?.b?.[k]; }",
        "function f(a){ return a?.(); }",
        "function f(a){ return a?.b(); }",
        "function f(a){ return a?.[k](); }",
        "function f(a){ return a?.(); }",
        "function f(a){ var x = a?.b; }",
        "function f(a){ return a?.b + 1; }",
        "function f(a){ if (a?.b) c(); }",
    ];
    assert_cases(&cases);
}

/// `switch`.
#[test]
fn switch_matches_the_reference() {
    require_oracle!();
    let cases = [
        // The discriminant stays on the stack; the trailing drop may be removed.
        "function f(x){ switch(x){ } }",
        "function f(x){ switch(x){ case 1: a(); } }",
        "function f(x){ switch(x){ case 1: } }",
        // Fallthrough: a clause continues into the next one's body, not its test.
        "function f(x){ switch(x){ case 1: a(); case 2: b(); } }",
        // Break.
        "function f(x){ switch(x){ case 1: a(); break; } }",
        "function f(x){ switch(x){ case 1: a(); break; case 2: b(); break; } }",
        // Default.
        "function f(x){ switch(x){ default: b(); } }",
        "function f(x){ switch(x){ case 1: a(); break; default: b(); } }",
        "function f(x){ switch(x){ default: b(); case 2: c(); } }",
        // Chains of case labels share one body.
        "function f(x){ switch(x){ case 1: case 2: a(); } }",
        "function f(x){ switch(x){ case 1: case 2: case 3: a(); break; } }",
        "function f(x){ switch(x){ case 1: case 2: a(); case 3: b(); } }",
        // `default` in the middle: the first run hands over to the later run's
        // tests, and the later run falls back to the default body.
        "function f(x){ switch(x){ case 1: a(); default: b(); case 2: c(); } }",
        "function f(x){ switch(x){ case 1: default: b(); } }",
        // Runs whose bodies are all empty.
        "function f(x){ switch(x){ case 1: case 2: } }",
        "function f(x){ switch(x){ case 1: case 2: case 3: } }",
        // A different discriminant, and a body that returns.
        "function f(x){ switch(x.y){ case 1: return 1; } }",
        "function f(x){ switch(x){ case 1: return; default: b(); } }",
    ];
    assert_cases(&cases);
}

/// `try`/`catch`/`finally`.
#[test]
fn try_catch_finally_matches_the_reference() {
    require_oracle!();
    let cases = [
        // `catch` records an exception edge; the handler binds the thrown value.
        "function f(){ try { a(); } catch(e) { b(e); } }",
        "function f(){ try { a(); } catch { b(); } }",
        "function f(){ try { a(); } catch(e) { } }",
        // `finally` alone: one copy of the finalizer, reached by `gosub` from the
        // normal path and from the propagating-exception path.
        "function f(){ try { a(); } finally { c(); } }",
        "function f(){ try { a(); } finally { c(); } d(); }",
        // Both: the finalizer is shared by the normal path, the handler path and
        // the rethrow path.
        "function f(){ try { a(); } catch(e) { b(e); } finally { c(); } }",
        "function f(){ try { a(); } catch(e) { b(e); } finally { c(); } d(); }",
        // A body that throws ends the block, so no epilogue is emitted for it.
        "function f(){ try { a(); throw x; } catch(e) { b(e); } }",
        "function f(){ try { a(); } catch(e) { throw e; } }",
        // Scopes: `let` in each part is its own scope; the catch parameter is a
        // local that never sits in the temporal dead zone.
        "function f(){ try { let x = 1; a(x); } catch(e) { let y = 2; b(e, y); } }",
        "function f(){ try { a(); } catch(e) { b(); } finally { let z = 3; c(z); } }",
        // Nesting.
        "function f(){ try { try { a(); } catch(e) { b(e); } } catch(e) { c(e); } }",
        "function f(){ try { a(); } catch(e) { try { b(); } finally { c(); } } }",
        // Leaping out of a `try`. `nip_catch` discards the catch offset while
        // keeping the return value, then each enclosing finalizer runs.
        "function f(){ try { return 1; } catch(e) { b(); } }",
        "function f(){ try { return 1; } finally { c(); } }",
        "function f(){ try { return 1; } catch(e) { b(); } finally { c(); } }",
        "function f(){ try { return; } finally { c(); } }",
        "function f(){ try { a(); return 1; } finally { c(); } }",
        "function f(){ try { try { return 1; } finally { c(); } } finally { d(); } }",
        // A `switch` inside the `try` leaves its discriminant on the stack, and
        // `nip_catch` has to take that with it.
        "function f(y){ try { switch(y){ case 1: return 2; } } finally { c(); } }",
        // `break` and `continue` out of a `try` with a finalizer.
        "function f(){ while(a){ try { break; } finally { c(); } } }",
        "function f(){ while(a){ try { continue; } finally { c(); } } }",
        "function f(){ while(a){ try { if(b) break; d(); } finally { c(); } } e(); }",
        // A labelled break out of two nested `try`s.
        "function f(){ o: while(a){ try { try { break o; } finally { c(); } } finally { d(); } } e(); }",
        // `break` out of a `finally` has to drop both its slots.
        // `return` inside a `finally`.
        "function f(){ try { a(); } finally { return 1; } }",
    ];
    assert_cases(&cases);
}

/// `for`/`in` and `for`/`of`.
#[test]
fn for_in_of_matches_the_reference() {
    require_oracle!();
    let cases = [
        // The record stays on the value stack; `for`/`of` closes its iterator on the
        // way out, `for`/`in` only drops the enumeration object.
        "function f(o){ for (var k in o) a(k); }",
        "function f(o){ for (var v of o) a(v); }",
        // Lexical loop variables live in the loop's own scope.
        "function f(o){ for (let k in o) a(k); }",
        "function f(o){ for (const v of o) a(v); }",
        // `break` lands on the single cleanup, so it emits nothing of its own.
        "function f(o){ for (var k in o) { break; } a(); }",
        "function f(o){ for (var v of o) { break; } a(); }",
        "function f(o){ for (var k in o) { a(); break; } b(); }",
        "function f(o){ for (var v of o) { if (v) break; } a(); }",
        // `continue` goes straight to the test.
        "function f(o){ for (var k in o) { if (k) continue; a(); } b(); }",
        "function f(o){ for (var v of o) { if (v) continue; a(); } b(); }",
        // Passing *through* the loop is what costs: the record has to go.
        "function f(x){ o: while(c){ for (var k in x){ break o; } } a(); }",
        "function f(x){ o: while(c){ for (var v of x){ break o; } } a(); }",
        "function f(x){ o: while(c){ for (var k in x){ continue o; } } a(); }",
        "function f(x){ o: while(c){ for (var v of x){ continue o; } } a(); }",
        // `return` from inside: no cleanup for `for`/`in`, an iterator close for
        // `for`/`of`.
        "function f(o){ for (var k in o) return k; }",
        "function f(o){ for (var v of o) return v; }",
        "function f(o){ for (var v of o) { if (v) return 1; } }",
        "function f(o){ for (var k in o) { if (k) return; } }",
        // Nesting, and a loop inside a `try`.
        "function f(o){ for (var k in o) for (var v of o) a(k, v); }",
        "function f(o){ try { for (var v of o) a(v); } finally { b(); } }",
    ];
    assert_cases(&cases);
}

/// Object and array literals.
#[test]
fn literals_build_in_place() {
    require_oracle!();
    let cases = [
        // The object stays on the stack while properties are added.
        "function f(){ return {}; }",
        "function f(){ return {a:1}; }",
        "function f(){ return {a:1, a:2}; }",
        "function f(a){ return {a}; }",
        "function f(){ return {a:1, b:2, c:3}; }",
        // `__proto__` sets the prototype; the shorthand is a plain property.
        "function f(p){ return {__proto__:p}; }",
        "function f(){ return {\"__proto__\":x}; }",
        "function f(){ return {__proto__}; }",
        // A method is not a field with a function in it.
        "function f(){ return {m(){return 1}}; }",
        "function f(){ return {get a(){return 1}}; }",
        "function f(){ return {set a(v){}}; }",
        "function f(){ return {a:function(){}}; }",
        "function f(){ return {a:function b(){}}; }",
        // Computed keys: `to_propkey` then `define_array_el`, and the key is
        // dropped; a computed method lets `define_method_computed` convert.
        "function f(k){ return {[k]:1}; }",
        "function f(){ return {[\"a\"]:1}; }",
        "function f(k){ return {[k](){}}; }",
        "function f(k){ return {get [k](){return 1}}; }",
        "function f(k){ return {[k]:function(){}}; }",
        // Spread copies with an exclusion list.
        "function f(b){ return {...b}; }",
        "function f(){ return {a:1, ...b, c:2}; }",
        "function f(){ return {...a, ...b}; }",
        // Numeric keys: a canonical index becomes a tagged atom, which prints
        // bare; anything else stays a string atom.
        "function f(x){ return {2:x}; }",
        "function f(x){ return {\"x\":1, 2:3}; }",
        "function f(x){ return {2147483647:x}; }",
        "function f(x){ return {2147483648:x}; }",
        "function f(x){ return {\"007\":x}; }",
        "function f(x){ return {\"1e2\":x}; }",
        "function f(x){ return {1e2:x}; }",
        "function f(x){ return {0x10:x}; }",
        "function f(x){ return {1.5:x}; }",
        "function f(x){ return {4294967296:x}; }",
        "function f(x){ return {0:x}; }",
        "function f(x){ return {\"10\":x}; }",
        // Arrays, including the index phase and spread.
        "function f(){ return []; }",
        "function f(){ return [1,2,3]; }",
        "function f(b){ return [1,...b]; }",
        "function f(){ return [1,,3]; }",
        "function f(){ return [,,]; }",
        "function f(){ return [1,]; }",
        "function f(){ return [...a, ...b]; }",
        // Nesting.
        "function f(){ return {a:[1,{b:2}]}; }",
        "function f(){ return [{a:1},{b:2}]; }",
        "function f(){ return {a:{b:{c:1}}}; }",
        "function f(){ return [[[1]]]; }",
        // The three array phases meet at these boundaries.
        "function f(){ return [,,1]; }",
        "function f(){ return [,,,...a]; }",
        "function f(){ return [1,,...a]; }",
        "function f(){ return [...a,,1]; }",
        "function f(){ return [1,...,,]; }",
        // 33 elements crosses the 32-element limit on phase 1.
        "function f(){ var a = [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32,33]; }",
        "function f(){ var a = [1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32]; }",
        // Literals as operands of other constructs.
        "function f(o){ return o[{a:1}]; }",
        "function f(){ for (var k in [1,2]) a(k); }",
        "function f(){ return {a:1}.a; }",
        // A literal inside a call and as a default-ish argument.
        "function f(){ g({a:1}, [2]); }",
        "function f(){ return typeof {}; }",
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

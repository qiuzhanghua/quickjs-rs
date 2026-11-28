//! Differential test: which source position a stack frame reports.
//!
//! A frame's `line:column` comes from a pc → position table, and both sides build that
//! table the same way in principle: an instruction records a position only where the
//! compiler *emits* one, and anything after it inherits that position until the next
//! record. The reference reaches this through `emit_source_pos`, which is called at a
//! handful of operators — a member selector, a call, an assignment, `return`, `throw`, a
//! binary operator, a member access — and nowhere else.
//!
//! Getting our table to agree therefore meant fixing three separate things, each of which
//! showed up as a wrong column in a trace:
//!
//! 1. **Recording too much.** Every expression used to record its own start, so a
//!    *literal* wrote a table entry the reference never writes. Nothing observable
//!    happened until an instruction with no position of its own inherited the wrong one —
//!    the `using` disposal epilogue, which the reference emits no position for at all.
//! 2. **Recording too little at the front.** Every function's table starts at pc 0 with
//!    the position of the function definition itself (`compute_pc2line_info` seeds it from
//!    `fd->source_pos`), which is what makes an inherited position resolve to `function`
//!    for a function whose body records nothing.
//! 3. **Synthetic functions' own positions.** A class with no written constructor still
//!    gets one, created *at* the class body's closing `}`, so that is what its first table
//!    entry says.
//!
//! The cases below are the ones where that arithmetic is visible. Every one is compared
//! through a whole `qjs` run, stderr included, so a wrong column fails here rather than
//! being noticed by a human reading a trace.

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

fn scripts(tag: &str, sources: &[&str]) -> Scripts {
    let mut dir = std::env::temp_dir();
    dir.push(format!("quickjs-rs-frames-{tag}-{}", std::process::id()));
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

/// The operator token, not the start of the enclosing expression.
#[test]
fn a_frame_reports_the_operator_that_failed() {
    let cases = [
        // The selector punctuator: `.` for a getter that throws, `[` for a computed one.
        "var o = { get x() { throw new Error('g') } }; o.x",
        "var o = { get x() { throw new Error('g') } }; o['x']",
        // A binary operator, including one reached through `ToPrimitive`.
        "Symbol() + 1",
        "1 + Symbol()",
        "var s = Symbol(); s += 1",
        "var s = Symbol(); Math.abs(s + 1)",
        // A call, and a call through a member.
        "var o = { f: function () { throw new Error('m') } }; o.f()",
        "function f() { throw new Error('c') } (0, f)()",
        // `new` with a constructor that throws.
        "function C() { throw new Error('n') } new C()",
        "class C { constructor() { throw new Error('k') } } new C()",
        // Arrows, methods and getters each get their own frame.
        "var f = () => new Error('a'); f().stack",
        "throw (() => { throw new Error('arrow') })()",
        "var o = { m() { throw new Error('method') } }; o.m()",
        "class C { m() { throw new Error('class method') } } new C().m()",
        // `super.x` in a derived method reads through the home object.
        "class A { get x() { throw new Error('super') } }\nclass B extends A { m() { return super.x } }\nnew B().m()",
        // A frame from an outer function, reached through a closure.
        "function outer() { var o = {}; Object.defineProperty(o, 'p', { get() { throw new Error('deep') } }); return o.p } outer()",
    ];
    let scripts = scripts("operators", &cases);
    compare(&scripts.0);
}

/// A position the reference never records is inherited, and the caller sees the earlier one.
///
/// The `using` epilogue is the case that made this visible: it records no position of its
/// own, so `using x = 1` reports the function's start while `using x = { [Symbol.dispose]: 1 }`
/// reports the `.` inside the initialiser — the last position the reference did record.
#[test]
fn a_position_the_reference_never_records_is_inherited() {
    let cases = [
        "try { (function () { using x = { [Symbol.dispose]: 1 } })() } catch (e) { throw e }",
        "try { (function () { using x = {[Symbol.dispose]: 1} })() } catch (e) { throw e }",
        "try { (function () { using x = 1 })() } catch (e) { throw e }",
        "try { (function () { using x = {} })() } catch (e) { throw e }",
        "try { (function () { using x = [1] })() } catch (e) { throw e }",
        "try { (function () { using x = Object.freeze({}) })() } catch (e) { throw e }",
        // A block, so the epilogue sits inside the function that declared it.
        "try { (function () { { using x = { [Symbol.dispose]: 1 } } })() } catch (e) { throw e }",
        // `await using` reaches the same epilogue through a different opcode.
        "try { (function () { using x = { [Symbol.asyncDispose]: 1 } })() } catch (e) { throw e }",
    ];
    let scripts = scripts("inherited", &cases);
    compare(&scripts.0);
}

/// Native frames: which built-ins enter a call before refusing.
///
/// `BigInt` and `Symbol` are constructors as far as `[[Construct]]` is concerned (the
/// reference registers both with `JS_CFUNC_constructor_or_func`), and the refusal happens
/// *inside* the native function. Refusing before the call produced the same message with
/// the `at BigInt (native)` frame missing.
#[test]
fn a_native_that_refuses_still_shows_its_frame() {
    let cases = [
        "new BigInt(1)",
        "new BigInt()",
        "new Symbol()",
        "new Symbol('s')",
        "Reflect.construct(BigInt, [1])",
        "Reflect.construct(Symbol, ['s'])",
        // The message names `new.target`, and its frame is the class body's `}`.
        "class B extends BigInt {}; new B(1)",
        "class B extends Symbol {}; new B()",
        "new (class extends BigInt {})(1)",
        // A second frame below the native one, from the subclass.
        "class B extends BigInt {}; class C extends B {}; new C(1)",
        // `Reflect.construct` with a third argument refuses the argument itself.
        "Reflect.construct(BigInt, [1], {})",
        "Reflect.construct(BigInt, [1], function NC(){})",
        "Reflect.construct(Symbol, [], ()=>{})",
    ];
    let scripts = scripts("native", &cases);
    compare(&scripts.0);
}

/// A parse error from `eval` is positioned at the token the checks read.
#[test]
fn a_syntax_error_frame_points_at_the_token_the_check_read() {
    let cases = [
        // `super` is consumed before the follower is examined, so the follower is blamed.
        "eval('super.x')",
        "eval('super')",
        "eval('super()')",
        "eval('super[0]')",
        "eval('super.x + super')",
        // A method body is the place `super` is legal, so the error moves inward.
        "eval('({ m() { super.x } })')",
        // The same shape for a redeclaration: the token *after* the name.
        "eval('let a; let a = 1;')",
        "eval('{ let a; let a; }')",
        "eval('function f(a, a){}')",
        "eval('(function f([a], a, a){})')",
    ];
    let scripts = scripts("syntax", &cases);
    compare(&scripts.0);
}

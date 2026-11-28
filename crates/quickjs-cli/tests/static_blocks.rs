//! Differential test: class static blocks, run against the reference.
//!
//! A `static { … }` is compiled as a function of its own and called with the class as
//! `this`, *inside* the class's static field initialiser — so the observable behaviour is
//! spread over three layers at once: the block's own scope, the `this` and `super` it
//! inherits from the initialiser, and the source order it shares with the static fields
//! around it.
//!
//! Every case is compared as a whole `qjs` run — stdout, stderr and exit status — so an
//! early error that turns into a runtime one, a thrown error with the wrong message, or a
//! frame pointing at the wrong column all fail here rather than being spotted by eye.

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
    dir.push(format!(
        "quickjs-rs-static-blocks-{tag}-{}",
        std::process::id()
    ));
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

fn compare(tag: &str, cases: &[&str]) {
    let Some(oracle_path) = locate_oracle() else {
        eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
        return;
    };
    let scripts = scripts(tag, cases);
    let harness = Harness::new(candidate(), Engine::new("reference", oracle_path));
    let report = harness.run(&scripts.0);
    assert!(!report.has_failures(), "{}", report.summary());
}

/// `this` is the class, and the block runs when the class definition reaches it.
#[test]
fn the_block_runs_with_the_class_as_this() {
    compare(
        "this",
        &[
            "class C { static { this.x = 1 } } print(C.x)",
            "class C { static { print(this === C) } }",
            "class C { static { this.f = () => this } } print(C.f() === C)",
            // `this` reaches a nested arrow but not a nested ordinary function.
            "class C { static { this.f = function () { return this } } } print(C.f() === undefined)",
            // A class expression, where there is no name to refer to the class by.
            "var C = class { static { this.x = 2 } }; print(C.x)",
            // The class binding is initialised before the block runs.
            "class C { static { this.x = typeof C } } print(C.x)",
        ],
    );
}

/// Static fields and static blocks share one initialiser, so their order is the source's.
#[test]
fn blocks_and_static_fields_run_in_source_order() {
    compare(
        "order",
        &[
            "class C { static a = 1; static { this.b = this.a } } print(JSON.stringify([C.a, C.b]))",
            "class C { static { this.b = this.a } static a = 1 } print(JSON.stringify([C.a, C.b]))",
            "class C { static { print(1) } static a = 2 } print(C.a)",
            "class C { static a = 2; static { print(1) } } print(C.a)",
            // Two blocks are two calls, and `print` here records both.
            "class C { static { print('a') } static { print('b') } }",
            // A block between two fields sees the earlier one and not the later one.
            "class C { static a = 1; static { print(this.a, this.b) } static b = 2 }",
            // An instance field is a different initialiser and must not run here.
            "class C { x = print('instance'); static { print('static') } }",
        ],
    );
}

/// The block's own lexical scope is separate from every other block and from the class.
#[test]
fn each_block_has_its_own_scope() {
    compare(
        "scope",
        &[
            "class C { static { let a = 1; } static { let a = 2; } } print('ok')",
            "var a = 9; class C { static { let a = 1; this.x = a } } print(a, C.x)",
            "class C { static { { let a = 1; } let b = 2; this.x = b } } print(C.x)",
            "class C { static { var a = 1; this.f = () => a } } print(C.f())",
            // `var` is a local of the block's function, not of the enclosing scope.
            "class C { static { var a = 1 } } print(typeof a)",
            "function f() { class C { static { var a = 1 } } return typeof a } print(f())",
            // A `let` in the block does not leak to a later static field.
            "class C { static { let a = 1 } static b = typeof a } print(C.b)",
            // Shadowing the class name inside the block is legal.
            "class C { static { let C = 1; this.x = C } } print(C.x)",
        ],
    );
}

/// `super.x` inside a block resolves against the class's home object.
#[test]
fn super_reads_the_parent_class() {
    compare(
        "super-read",
        &[
            "class B { static m() { return 5 } } class C extends B { static { this.x = super.m() } } print(C.x)",
            "class B {} B.p = 7; class C extends B { static { this.x = super.p } } print(C.x)",
            "class B { static get g() { return this } } class C extends B { static { this.x = super.g } } print(C.x === C)",
            "class B {} class C extends B { static { print(super.missing) } }",
            // A getter sees `this`, which is the derived class.
            "class B { static get g() { return this.name } } class C extends B { static { this.q = super.g } } print(C.q)",
            // Arithmetic on a missing parent property, which is where `undefined` shows.
            "class B {} class C extends B { static { this.x = String(super.nope) } } print(C.x)",
        ],
    );
}

/// `super.x = v` writes to the class, not to the parent, and reads still start at the
/// parent.
#[test]
fn super_writes_land_on_this() {
    compare(
        "super-write",
        &[
            "class B {} class C extends B { static { super.a = 1 } } print(C.a, B.a)",
            "class B {} class C extends B { static { super['a'] = 1 } } print(C.a, B.a)",
            // The assignment is an expression: it yields the value it stored.
            "class B {} class C extends B { static { this.q = (super.a = 4) } } print(C.q, C.a)",
            // A compound store reads through the parent, so a missing parent property
            // makes it `NaN` rather than starting from zero.
            "class B {} class C extends B { static { super.a += 1 } } print(String(C.a))",
            "class B { static a = 1 } class C extends B { static { super.a += 1 } } print(C.a, B.a)",
            // Logical stores only evaluate their operand when the read says so.
            "class B { static a = 1 } class C extends B { static { super.a ||= 9; super.b ||= 8 } } print(C.a, B.a, C.b)",
            "class B { static a = 1 } class C extends B { static { super.a ??= 9; super.b ??= 8 } } print(C.a, C.b)",
            // A setter on the parent runs with the derived class as `this`.
            "class B { static set s(v) { this.via = v } } class C extends B { static { super.s = 3 } } print(C.via)",
        ],
    );
}

/// The early errors a block adds, each with the message the reference gives.
#[test]
fn the_block_is_not_a_function() {
    compare(
        "early-errors",
        &[
            "class C { static { return } }",
            "class C { static { return 1 } }",
            "class C { static { arguments } }",
            "class C { static { var f = () => arguments } }",
            "class C { static { await } }",
            "class C { static { await 1 } }",
            "class C extends B { static { super() } }",
            "class C { static { super() } }",
            "class C { static { break } }",
            "class C { static { continue } }",
            "class C { static { break label } }",
            "class C { static { yield } }",
            "class C { static { let a; let a } }",
            "class C { static { let a; var a } }",
        ],
    );
}

/// What a nested function recovers: its own `return`, its own `arguments`, and `await` as
/// an ordinary name.
#[test]
fn nested_functions_get_their_own_bindings_back() {
    compare(
        "nested",
        &[
            "class C { static { function f() { return 7 } this.x = f() } } print(C.x)",
            "class C { static { this.x = (function () { return arguments.length })(1, 2) } } print(C.x)",
            "class C { static { var f = function () { return await }; this.x = typeof f() } } print(C.x)",
            "class C { static { this.x = (() => await)() } } print(C.x)",
            // `yield` stays reserved: it is a strict-mode word, and the block is strict.
            "class C { static { var yield = 1 } }",
            // `new.target` is allowed and is `undefined`, because the block is called, not
            // constructed.
            "class C { static { this.x = String(new.target) } } print(C.x)",
            "class C { static { this.f = () => new.target } } print(String(C.f()))",
        ],
    );
}

/// A block is a strict-mode context, and its `this` is never rewritten.
#[test]
fn the_block_is_strict() {
    compare(
        "strict",
        &[
            "class C { static { this.x = this } } print(C.x === C)",
            "class C { static { undeclared = 1 } }",
            "class C { static { var o = {}; Object.freeze(o); o.p = 1 } }",
            "class C { static { with ({}) {} } }",
            "class C { static { function f(a, a) {} } }",
            "class C { static { let eval = 1 } }",
        ],
    );
}

/// A throw from a block stops the class definition where it stands.
#[test]
fn a_throw_aborts_the_class_definition() {
    compare(
        "abrupt",
        &[
            "try { class C { static { throw new Error('in block') } } } catch (e) { print(e.message) }",
            "try { class C { static x = 1; static { throw new Error('after field') } static y = 2 } } catch (e) { print(e.message) }",
            // The class binding is never initialised, so a later reference is an error
            // rather than a half-built class.
            "class C { static { throw new Error('boom') } }",
            // The error's own stack names the block's frame.
            "class C { static { this.x = 1\n\n  throw new Error('trace') } }",
            // A block reached through `extends` ordering: the parent is evaluated first.
            "try { class C extends (class { static { throw new Error('parent') } }) {} } catch (e) { print(e.message) }",
        ],
    );
}

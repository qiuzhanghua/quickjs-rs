//! Acceptance differential: does our parser accept exactly what QuickJS's
//! parser accepts?
//!
//! The oracle is `qjsc -c`, which compiles a script (or module) to bytecode and
//! **does not execute it**, so its exit status is a pure parse verdict:
//!
//! * exit 0 — the reference parser accepted the program;
//! * exit 1 with `SyntaxError` on stderr — it rejected it.
//!
//! This lets the parser be validated long before a VM exists. Every mismatch is
//! printed with the reference message, so the failing construct is obvious.

use js_parse::{parse_module, parse_script};
use quickjs_conformance::{Engine, locate_qjsc};
use std::path::PathBuf;

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

/// A temp directory that is cleaned up when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        // The tests run in parallel, and a timestamp alone can collide: two
        // threads are quite capable of reading the same nanosecond.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        let unique = format!("quickjs-rs-{tag}-{}-{sequence}", std::process::id());
        path.push(unique);
        std::fs::create_dir_all(&path).expect("create temp dir");
        TempDir(path)
    }

    fn file(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).expect("write temp file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What the reference parser does with a source.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ReferenceVerdict {
    Accept,
    Reject(String),
    /// The reference could not be run (missing file, crash, …).
    Unknown(String),
}

fn reference_verdict(
    oracle: &Engine,
    dir: &TempDir,
    source: &str,
    module: bool,
) -> ReferenceVerdict {
    let extension = if module { "mjs" } else { "js" };
    let path = dir.file(&format!("case.{extension}"), source);
    let out_path = dir.0.join("out.c");
    let mut args = vec![
        "-c".to_string(),
        "-o".to_string(),
        out_path.display().to_string(),
    ];
    if module {
        args.push("-m".to_string());
    }
    args.push(path.display().to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    match oracle.output(&arg_refs) {
        Err(err) => ReferenceVerdict::Unknown(err.to_string()),
        Ok(out) => {
            if out.code == 0 {
                ReferenceVerdict::Accept
            } else if out.stderr.starts_with("SyntaxError") {
                ReferenceVerdict::Reject(out.stderr.lines().next().unwrap_or("").to_string())
            } else {
                // Module resolution and link errors are not parse verdicts.
                ReferenceVerdict::Unknown(out.stderr.lines().next().unwrap_or("").to_string())
            }
        }
    }
}

/// Check one program against the reference parser.
///
/// Returns `None` when they agree, or `Some(explanation)` when they do not.
fn check(source: &str, module: bool) -> Option<String> {
    let oracle = oracle()?;
    let dir = TempDir::new(if module { "mod" } else { "script" });
    let reference = reference_verdict(&oracle, &dir, source, module);
    let ours = if module {
        parse_module(source)
    } else {
        parse_script(source)
    };
    match (&reference, &ours) {
        (ReferenceVerdict::Accept, Ok(_)) => None,
        (ReferenceVerdict::Reject(_), Err(_)) => None,
        (ReferenceVerdict::Accept, Err(err)) => {
            Some(format!("we rejected, the reference accepted: {err}"))
        }
        (ReferenceVerdict::Reject(msg), Ok(_)) => {
            Some(format!("we accepted, the reference rejected: {msg}"))
        }
        (ReferenceVerdict::Unknown(_), _) => None,
    }
}

#[allow(dead_code)]
fn assert_corpus_agrees(corpus: &[&str], module: bool) {
    let mut mismatches = Vec::new();
    for source in corpus {
        if let Some(reason) = check(source, module) {
            mismatches.push(format!("  source: {source:?}\n    {reason}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} cases disagree with the reference parser:\n{}",
        mismatches.len(),
        corpus.len(),
        mismatches.join("\n")
    );
}

#[test]
fn valid_scripts_are_accepted_by_both() {
    require_oracle!();
    let corpus = [
        // literals and operators
        "1 + 2 * 3;",
        "0.1 + 0.2;",
        "1n << 64n;",
        "'a' + `b${1}c`;",
        "/a(b+)/gi.test('ab');",
        "({a: 1, b: 2});",
        "[1, , 3, ...[]];",
        "a ?? b;",
        "(a ?? b) || c;",
        "a?.b?.[c]?.(d);",
        "new a.b.c(1, 2);",
        "new (a())();",
        "typeof typeof x;",
        "void 0;",
        "delete a.b;",
        "++a; a++;",
        "(-a) ** 2;",
        "++a ** 2;",
        "2 ** 3 ** 2;",
        "a = b = c;",
        "a += 1; a **= 2; a &&= b; a ??= c;",
        "true ? 1 : 2;",
        "(a, b, c);",
        // statements
        "if (a) b; else c;",
        "for (var i = 0; i < 10; i++) ;",
        "for (let i = 0, j = 1; i < j; i++, j--) ;",
        "for (;;) break;",
        "for (const x of [1, 2]) ;",
        "for (const k in {a: 1}) ;",
        "for ([a, b] of pairs) ;",
        "for ({a} of xs) ;",
        "for (obj.x of xs) ;",
        "while (a) { break; }",
        "do { continue; } while (a);",
        "switch (a) { case 1: break; default: }",
        "l: for (;;) { break l; continue l; }",
        "a: b: while (1) { continue a; }",
        "try { } catch (e) { } finally { }",
        "try { } catch { }",
        "try { } finally { }",
        "throw new Error('x');",
        "debugger;",
        ";",
        "{}",
        "{ let x = 1; }",
        // functions
        "function f(a, b = 1, ...rest) { return a; }",
        "function* g() { yield 1; yield* [2]; }",
        "async function h() { await 1; }",
        "async function* i() { for await (const x of y) ; }",
        "var f = function named() {};",
        "var f = () => {};",
        "var f = (a, b) => a + b;",
        "var f = a => a;",
        "var f = async a => a;",
        "var f = async (a) => a;",
        "var f = () => ({a: 1});",
        "var f = ({a, b: [c = 1]}) => a;",
        "var f = (a = 1) => a;",
        "(function(){ 'use strict'; return 1; })();",
        "function f() { 'use strict'; return this; }",
        // classes
        "class A { }",
        "class A extends B { constructor() { super(); } }",
        "class A { m() {} get x() { return 1 } set x(v) {} static s() {} }",
        "class A { #p = 1; m() { return this.#p; } static #q; }",
        "class A { static { this.x = 1; } }",
        "class A { async m() {} *g() {} async *ag() {} }",
        "class A { [Symbol.iterator]() {} 'str'() {} 42() {} }",
        "var A = class Named extends B {};",
        "class A { accessor; }",
        "class A { static = 1; }",
        "class A { get = 1; set = 2; async = 3; }",
        // destructuring and defaults
        "var [a, , b = 2, ...c] = xs;",
        "var {a, b: c, d = 1, ...rest} = obj;",
        "var {[k]: v} = obj;",
        "[a, b] = [b, a];",
        "({a, b} = obj);",
        "[a.b, c[0]] = xs;",
        "for (let {length} of 'abc') ;",
        // misc
        "label: { break label; }",
        "if (1) function f() {}",
        "var {a: {b: {c}}} = x;",
        "function f({a: [b]}, [c, {d}]) {}",
        "a\n++b;",
        "var a = 1\nvar b = 2",
        "return_ = 1;",
        "let\nx = 1;",
        "var let_ = 1; let a = 2; const b = 3;",
        "0b1010 + 0o777 + 0xff + 1_0;",
        "`a\\n${b}c`;",
        "tag`a${b}c`;",
        "x = {__proto__: null, ['__proto__']: 1};",
        "new.target;",
        "function f() { new.target; }",
        "with (o) { a; }",
        "var arguments_; function g() { return arguments; }",
        "var yield_ = 1;",
        "async () => {};",
        "(async () => { await x; })();",
        "let {a = 1} = {};",
        "var f = function* () { yield; };",
        "0 .toString();",
        "1..toString();",
        "x?.();",
        "a ??= b ?? c;",
    ];
    assert_corpus_agrees(&corpus, false);
}

#[test]
fn invalid_scripts_are_rejected_by_both() {
    require_oracle!();
    let corpus = [
        "let x = ;",
        "var 1x;",
        "if (a) else b;",
        "function () {}",
        "class {}",
        "1 + ;",
        "({a: 1, a: 2, __proto__: 1, __proto__: 2});",
        "for (;;",
        "a ? b;",
        "a ?? b || c;",
        "a || b ?? c;",
        "function f() { return; } return 1;",
        "break;",
        "continue;",
        "for (;;) { continue l; }",
        "'use strict'; with (o) {};",
        "'use strict'; var eval = 1;",
        "'use strict'; delete x;",
        "'use strict'; 010;",
        "function f(a, a) { 'use strict'; }",
        "var f = (a, a) => 1;",
        "'use strict'; class A { static prototype() {} }",
        "class A { constructor() {} constructor() {} }",
        "class A { #x; #x; }",
        "class A { m() { return this.#y; } }",
        "new a?.b();",
        "a?.b = 1;",
        "a?.b`x`;",
        "yield = 1; function* g() { yield = 1; }",
        "async function f() { var await = 1; }",
        "let let = 1;",
        "'use strict'; let let = 1;",
        "var a = 1,;",
        "function f(...a, b) {}",
        "function f(...a = 1) {}",
        "({...a, b} = c);",
        "[...a, b] = c;",
        "1 = 2;",
        "a() = 1;",
        "(a, b) = 1;",
        "++a++;",
        "delete a?.b;",
        "var x = 1 / ;",
        "'unterminated",
        "/* unterminated",
        "`unterminated",
        "a\n=> b;",
        "async\n(a) => 1;",
        "super.x;",
        "class A { constructor() { super(); } }",
        "function f() { super(); }",
        "import x from 'y';",
        "({a = 1});",
        "1n ** 2n,",
        "-a ** 2;",
        "void a ** 2;",
    ];
    assert_corpus_agrees(&corpus, false);
}

#[test]
fn modules_agree_with_the_reference() {
    require_oracle!();
    let valid = [
        "export function f() {}",
        "export default function () {}",
        "export default class {}",
        "export default 1 + 2;",
        "import x from 'y';",
        "import * as ns from 'y';",
        "import {a, b as c} from 'y';",
        "import 'y';",
        "import x, {a} from 'y';",
        "import x, * as ns from 'y';",
        "const a = 1; export {a};",
        "const a = 1; export {a as b};",
        "export {a}; const a = 1;",
        "export {a} from 'y';",
        "export * from 'y';",
        "export * as ns from 'y';",
        "import json from './a.json' with { type: 'json' };",
        "const x = import.meta.url;",
        "const p = import('m');",
        "await Promise.resolve(1);",
        "for await (const x of xs) ;",
        "export default async function () {}",
        "export default (class {});",
        "let x = 1; export {x as y};",
    ];
    let invalid = [
        "import {a} from 1;",
        "export * from;",
        "export 1;",
        "export {missing};",
        "with (o) {}",
        "var eval = 1;",
        "return 1;",
        "new.target;",
        "arguments = 1;",
        "delete x;",
        "010;",
        "export default export default 1;",
        "import {a as} from 'y';",
        "import * from 'y';",
    ];
    let mut mismatches = Vec::new();
    for source in valid.iter().chain(invalid.iter()) {
        if let Some(reason) = check(source, true) {
            mismatches.push(format!("  source: {source:?}\n    {reason}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} of {} module cases disagree:\n{}",
        mismatches.len(),
        valid.len() + invalid.len(),
        mismatches.join("\n")
    );
}

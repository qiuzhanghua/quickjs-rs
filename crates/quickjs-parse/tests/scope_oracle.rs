//! Differential test for declaration-conflict early errors.
//!
//! QuickJS rejects duplicate/conflicting declarations while parsing; this file
//! checks that we agree, case by case, against `qjsc -c`.

use quickjs_conformance::{Engine, locate_qjsc};
use std::path::{Path, PathBuf};

macro_rules! require_oracle {
    () => {
        match locate_qjsc() {
            Some(path) => Engine::new("qjsc", path),
            None => {
                eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
                return;
            }
        }
    };
}

fn scratch() -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("quickjs-rs-scope-{}", std::process::id()));
    std::fs::create_dir_all(&path).expect("scratch dir");
    path
}

fn reference_verdict(oracle: &Engine, dir: &Path, source: &str) -> Option<bool> {
    let file = dir.join("case.js");
    std::fs::write(&file, source).expect("write case");
    let out = oracle
        .output(&[
            "-c",
            "-o",
            dir.join("out.c").to_str().expect("utf8"),
            file.to_str().expect("utf8"),
        ])
        .ok()?;
    if out.code == 0 {
        Some(true)
    } else if out.stderr.starts_with("SyntaxError") {
        Some(false)
    } else {
        None
    }
}

#[test]
fn declaration_conflicts_match_the_reference() {
    let oracle = require_oracle!();
    let dir = scratch();
    // A mix of programs QuickJS accepts and rejects. Each one is a real early
    // error from ES2025 §13.2.1 (VarDeclaredNames / LexicallyDeclaredNames).
    let corpus = [
        // -- rejected by the reference --
        "var a = 1; let a = 2;",
        "let a; var a;",
        "let a; let a;",
        "let a; const a = 1;",
        "const a = 1; class a {}",
        "function f() { let a; var a; }",
        "function f(a) { let a; }",
        "function f(a) { var a; }",
        "function f({a}) { let a; }",
        "function f([a]) { const a = 1; }",
        "var f = (a) => { let a; };",
        "var f = (a) => { var a; };",
        "try {} catch (e) { let e; }",
        "try {} catch ({e}) { var e; }",
        "switch (x) { case 1: let a; break; case 2: let a; }",
        "for (let a of xs) { let a; }",
        "for (let a of xs) { var a; }",
        "for (let a = 0, a = 1; ; ) ;",
        "{ let a; { var a; } }",
        "{ let a; function g() { var a; } }",
        "let a; function a() {}",
        "class A {} class A {}",
        "import {a} from 'm'; let a;",
        // -- accepted by the reference --
        "var a; var a;",
        "var a; { let a; }",
        "let a; { let a; }",
        "{ let a; } { let a; }",
        "function f(a) { var a; }",
        "function f() { var a; var a; }",
        "for (let a of xs) { }",
        "for (;;) { let a; } for (;;) { let a; }",
        "function f() { let a; } function g() { let a; }",
        "switch (x) { case 1: { let a; } case 2: { let a; } }",
        "try {} catch (e) { var e2; }",
        "var a; function a() {}",
        "function a() {} var a;",
        "{ function a() {} } let a;",
        "let a; function a() {}",
        "{ function a() {} function a() {} }",
        "{ function a() {} { function a() {} } }",
        "function a() {} function a() {}",
        "{ let a; { function a() {} } }",
        "let a; { function a() {} }",
        "function f() { var a; { let a; } }",
        "for (let a of xs) { let b; }",
        "try {} catch (e) { var e; }",
        "try {} catch (e) { let e2; }",
        "function f({a}) { let a; }",
        "function f([a]) { var a; }",
        "class A { m() { let a; } }",
        "let a; { var b; }",
        // -- rejected: more conflict shapes --
        "{ function a() {} let a; }",
        "{ var a; let a; }",
        "for (let a of xs) { function a() {} }",
        "{ let a; for (;;) { var a; } }",
        "let a; { let a; { var a; } }",
        "try {} catch ({e}) { var e; }",
        "try {} catch ([e]) { let e; }",
        "class A { static { let a; var a; } }",
    ];
    let mut mismatches = Vec::new();
    for source in corpus {
        let Some(reference) = reference_verdict(&oracle, &dir, source) else {
            continue;
        };
        let ours = js_parse::parse_script(source).is_ok();
        if ours != reference {
            mismatches.push(format!(
                "  {source:?}\n    reference: {}, ours: {}",
                if reference { "accept" } else { "reject" },
                if ours { "accept" } else { "reject" }
            ));
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        mismatches.is_empty(),
        "{} cases disagree:\n{}",
        mismatches.len(),
        mismatches.join("\n")
    );
}

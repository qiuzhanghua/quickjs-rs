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
    // Two lists, and the label is now a *checked claim* rather than a comment. It used to
    // be one array with `// -- rejected --` section comments, and four sources drifted into
    // both halves: three that the reference actually accepts were listed as rejected, and
    // one that it rejects was listed as accepted. Nothing noticed, because the loop only
    // compared "ours vs the reference" and never looked at the label.
    //
    // Every case is an early error from §13.2.1 (VarDeclaredNames /
    // LexicallyDeclaredNames) or a legal shape that looks like one.
    let rejected_by_reference = [
        "var a = 1; let a = 2;",
        "let a; var a;",
        "let a; let a;",
        "let a; const a = 1;",
        "const a = 1; class a {}",
        "function f() { let a; var a; }",
        "function f(a) { let a; }",
        "var f = (a) => { let a; };",
        "try {} catch (e) { let e; }",
        "try {} catch ({e}) { var e; }",
        "switch (x) { case 1: let a; break; case 2: let a; }",
        "for (let a of xs) { var a; }",
        "for (let a = 0, a = 1; ; ) ;",
        "{ let a; { var a; } }",
        "class A {} class A {}",
        "import {a} from 'm'; let a;",
        "{ function a() {} let a; }",
        "{ var a; let a; }",
        "{ let a; for (;;) { var a; } }",
        "let a; { let a; { var a; } }",
        "class A { static { let a; var a; } }",
    ];
    let accepted_by_reference = [
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
        "function f([a]) { const a = 1; }",
        "var f = (a) => { var a; };",
        "try {} catch ([e]) { let e; }",
        "for (let a of xs) { let a; }",
        "{ let a; function g() { var a; } }",
        "for (let a of xs) { function a() {} }",
    ];
    let mut mismatches = Vec::new();
    for (expected, cases) in [
        (false, &rejected_by_reference[..]),
        (true, &accepted_by_reference[..]),
    ] {
        for source in cases {
            let Some(reference) = reference_verdict(&oracle, &dir, source) else {
                continue;
            };
            // The label has to be right about the reference before our agreement with the
            // reference means anything.
            if reference != expected {
                mismatches.push(format!(
                    "  {source:?}\n    listed as {}, but the reference {}",
                    if expected { "accepted" } else { "rejected" },
                    if reference { "accepts" } else { "rejects" }
                ));
                continue;
            }
            let ours = js_parse::parse_script(source).is_ok();
            if ours != reference {
                mismatches.push(format!(
                    "  {source:?}\n    reference: {}, ours: {}",
                    if reference { "accept" } else { "reject" },
                    if ours { "accept" } else { "reject" }
                ));
            }
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

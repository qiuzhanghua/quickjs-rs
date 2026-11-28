//! Differential test: where `await` is an operator, and where it is a name.
//!
//! `await` is the most context-sensitive word in the grammar, and the three contexts it
//! can appear in disagree:
//!
//! | context | is `await` an operator? | may it be a binding? |
//! |---|---|---|
//! | an ordinary script | no | yes |
//! | a module, at any depth | only at the top level | no |
//! | a *suspensible* script (`JS_EVAL_FLAG_ASYNC`) | at the top level | no |
//!
//! Getting this wrong is invisible in the accept/reject sense for most sources — an
//! `await` read as an identifier simply resolves to nothing at run time — so the corpus
//! below pins the *message* each context produces, which is what distinguishes "parsed as
//! an operator with a missing operand" from "parsed as a name and rejected".
//!
//! The oracle has no way to compile a suspensible script directly: `qjsc` only knows
//! `--script` and `--module`. So that column goes through `qjs --std` running a one-line
//! probe that calls `std.evalScript(source, {async: true})` and prints the message of the
//! `SyntaxError` it throws, or `ok` if the parse succeeded.

use js_parse::{parse_module, parse_script, parse_suspensible_script};
use quickjs_conformance::{Engine, locate_oracle, locate_qjsc};
use std::path::{Path, PathBuf};

/// The probe the reference runs for a suspensible script.
///
/// `evalScript` reports a parse failure by throwing, before any promise exists, so a
/// `try`/`catch` sees it. The whole point is that `message` is the *parser's* text.
fn probe_for(case_path: &Path) -> String {
    format!(
        r#"var source_ = std.loadFile({case:?});
try {{
    std.evalScript(source_, {{ async: true }});
    std.puts("ok\n");
}} catch (e) {{
    std.puts(String(e.message).split("\n")[0] + "\n");
}}
"#,
        case = case_path.to_str().expect("utf-8 path")
    )
}

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> TempDir {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "quickjs-rs-await-{tag}-{}-{sequence}",
            std::process::id()
        ));
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

/// The reference's verdict for `source` in each of the three contexts.
///
/// `None` means the source parsed; `Some(message)` is the first line of the
/// `SyntaxError`, with the `SyntaxError: ` prefix removed.
struct Reference {
    script: Option<String>,
    module: Option<String>,
    suspensible: Option<String>,
}

fn message_of(out: &quickjs_conformance::Output) -> Option<String> {
    let text = if out.stderr.is_empty() {
        &out.stdout
    } else {
        &out.stderr
    };
    let first = text.lines().next().unwrap_or("");
    if first == "ok" {
        return None;
    }
    (!first.is_empty()).then(|| first.trim_start_matches("SyntaxError: ").to_string())
}

fn reference(qjsc: &Engine, qjs: &Engine, dir: &TempDir, source: &str) -> Reference {
    let script_path = dir.file("case.js", source);
    let module_path = dir.file("case.mjs", source);
    let out_path = dir.0.join("out.c");
    let out = out_path.to_str().expect("utf-8 path");
    let script = message_of(
        &qjsc
            .output(&["-c", "-o", out, script_path.to_str().expect("utf-8")])
            .expect("script verdict"),
    );
    let module = message_of(
        &qjsc
            .output(&["-c", "-m", "-o", out, module_path.to_str().expect("utf-8")])
            .expect("module verdict"),
    );
    // A suspensible script has no file-level entry point, so it is reached through a
    // probe that calls `evalScript`. The source goes in a file so its newlines survive.
    let case = dir.file("susp.js", source);
    let probe = dir.file("probe.js", &probe_for(&case));
    let suspensible = message_of(
        // `--no-unhandled-rejection` keeps a runtime rejection out of the transcript:
        // this only cares whether the *parse* failed, and a corpus entry that parses may
        // still throw when it runs.
        &qjs.output(&[
            "--std",
            "--no-unhandled-rejection",
            probe.to_str().expect("utf-8"),
        ])
        .expect("suspensible verdict"),
    );
    Reference {
        script,
        module,
        suspensible,
    }
}

fn ours(source: &str) -> (Option<String>, Option<String>, Option<String>) {
    let script = parse_script(source).err().map(|e| e.message);
    let module = parse_module(source).err().map(|e| e.message);
    let suspensible = parse_suspensible_script(source).err().map(|e| e.message);
    (script, module, suspensible)
}

/// Sources whose three verdicts are checked against the reference.
///
/// One entry per rule, not one per spelling: each line was added because it distinguishes
/// two of the contexts from each other.
const CORPUS: &[&str] = &[
    // An operator only where the enclosing function is async.
    "await 1",
    "await;",
    "await;",
    "f(await)",
    "[await]",
    "x ? await : 1",
    "await .x",
    "var x = await 1",
    "for await (const a of []) ;",
    // …and not inside a nested non-async function, in either flavour.
    "function f(){ await 1 }",
    "() => await 1",
    "class A { m(){ await 1 } }",
    "async function g(){ await 1 }",
    // A block does not change the enclosing function.
    "try { await 1 } catch {}",
    "label: await 1",
    "if (1) { await 1 }",
    // Names: free in a script, reserved in module code at any depth, reserved at the top
    // level of a suspensible body but free inside a function there.
    "var await = 1",
    "function f(await){}",
    "function f(){ var await = 1 }",
    "async function g(){ var await = 1 }",
    // A keyword is not a label.
    "await: 1",
    "function f(){ await: 1 }",
    // Property names are not identifiers.
    "({ await: 1 })",
    "x = { await: 1 }",
    "x.await",
    "function f(){ x.await }",
    // Neither is a declaration in a `using` head.
    "await using y = z",
    // A completion value still needs an operand.
    "await",
    "x = await",
];

#[test]
fn await_is_an_operator_in_exactly_the_async_contexts() {
    let Some(qjsc_path) = locate_qjsc() else {
        eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
        return;
    };
    let Some(qjs_path) = locate_oracle() else {
        eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
        return;
    };
    let qjsc = Engine::new("qjsc", qjsc_path);
    let qjs = Engine::new("qjs", qjs_path);
    let dir = TempDir::new("contexts");

    let mut disagreements = Vec::new();
    for source in CORPUS {
        let expected = reference(&qjsc, &qjs, &dir, source);
        let (script, module, suspensible) = ours(source);
        for (context, want, got) in [
            ("script", &expected.script, &script),
            ("module", &expected.module, &module),
            ("suspensible", &expected.suspensible, &suspensible),
        ] {
            if want != got {
                disagreements.push(format!(
                    "  source: {source:?} ({context})\n    reference: {want:?}\n    ours:      {got:?}"
                ));
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "{} of {} checks disagree:\n{}",
        disagreements.len(),
        CORPUS.len() * 3,
        disagreements.join("\n")
    );
}

/// A suspensible script is not a function body for the rules that ask about functions.
///
/// This is the pair of messages the flag must *not* change: `return` and `new.target` are
/// rejected exactly as in a plain script, because the top level is still a script.
#[test]
fn the_flag_does_not_make_the_body_a_function() {
    for source in ["return 1", "new.target", "super()"] {
        let plain = parse_script(source).err().map(|e| e.message);
        let suspensible = parse_suspensible_script(source).err().map(|e| e.message);
        assert_eq!(
            plain, suspensible,
            "{source:?} should be rejected identically with and without the async flag"
        );
    }
}

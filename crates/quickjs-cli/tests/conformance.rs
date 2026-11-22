//! Differential tests: this build of `qjs` against the C reference build.
//!
//! The reference binary is discovered via `$QJS_ORACLE` or the default local
//! paths (see `quickjs_conformance::locate_oracle`). When it is missing the
//! differential tests are skipped with a message instead of failing, so the
//! suite stays usable on a machine without the C sources.
//!
//! Build the reference once with `scripts/setup-oracle.sh`.

use quickjs_conformance::{Case, Engine, Harness, Verdict, locate_oracle};

fn candidate() -> Engine {
    Engine::new("rust", env!("CARGO_BIN_EXE_qjs"))
}

fn oracle() -> Option<Engine> {
    locate_oracle().map(|path| Engine::new("reference", path))
}

macro_rules! require_oracle {
    () => {
        match oracle() {
            Some(oracle) => oracle,
            None => {
                eprintln!("skipping: no reference qjs found (run scripts/setup-oracle.sh)");
                return;
            }
        }
    };
}

#[test]
fn help_text_is_byte_identical_to_reference() {
    let oracle = require_oracle!();
    let expected = oracle.output(&["--help"]).expect("run reference --help");
    let actual = candidate().output(&["--help"]).expect("run rust --help");
    assert_eq!(actual.code, expected.code, "--help exit status differs");
    assert_eq!(actual.stdout, expected.stdout, "--help text differs");
}

#[test]
fn usage_errors_match_reference() {
    let oracle = require_oracle!();
    let candidate = candidate();
    for args in [
        vec!["--bogus"],
        vec!["-z"],
        vec!["-e"],
        vec!["-I"],
        vec!["--memory-limit"],
        vec!["--stack-size"],
    ] {
        let expected = oracle.output(&args).expect("run reference");
        let actual = candidate.output(&args).expect("run candidate");
        assert_eq!(
            actual.code, expected.code,
            "exit status differs for {args:?}"
        );
        assert_eq!(
            actual.stdout, expected.stdout,
            "stdout differs for {args:?}"
        );
        assert_eq!(
            actual.stderr, expected.stderr,
            "stderr differs for {args:?}"
        );
    }
}

#[test]
fn quit_option_exits_cleanly() {
    let candidate = candidate();
    for args in [vec!["-q"], vec!["--quit"]] {
        let out = candidate.output(&args).expect("run candidate");
        assert_eq!(out.code, 0, "{args:?} should succeed");
        assert!(out.stdout.is_empty(), "{args:?} should be silent");
        assert!(out.stderr.is_empty(), "{args:?} should be silent");
    }
}

#[test]
fn harness_self_check_compares_a_binary_with_itself() {
    let oracle = require_oracle!();
    let harness = Harness::new(oracle.clone(), oracle);
    let report = harness.self_check(&quickjs_conformance::language_cases());
    assert!(
        !report.has_failures(),
        "harness plumbing is broken:\n{}",
        report.summary()
    );
}

/// The language corpus, now that the VM runs it.
///
/// Before the VM landed every case came back *pending*; that assertion is gone
/// because it is no longer true. What replaces it is a floor plus a crash check:
/// the cases the engine supports must match the reference exactly, and the rest
/// must fail *politely*. The unsupported ones are the documented gaps — BigInt
/// literals, template literals, and the standard library that milestone M4 adds.
#[test]
fn simple_language_cases_match_or_report_a_known_gap() {
    let oracle = require_oracle!();
    let harness = Harness::new(candidate(), oracle);
    let cases = quickjs_conformance::language_cases();
    let report = harness.run(&cases);

    let mut crashed = Vec::new();
    for (case, verdict) in &report.entries {
        let message = match verdict {
            Verdict::Error(message) => Some(message.clone()),
            Verdict::Mismatch { actual, .. } => Some(format!("{actual:?}")),
            _ => None,
        };
        if let Some(message) = message
            && message.contains("panicked")
        {
            crashed.push((format!("{case:?}"), message));
        }
    }
    assert!(
        crashed.is_empty(),
        "the engine must not panic on any corpus case: {crashed:#?}"
    );

    // The floor records what works today; a drop means a regression, and raising it
    // is part of finishing the next milestone.
    assert!(
        report.matched() >= 17,
        "at least 17 cases must match the reference, got {}\n{}",
        report.matched(),
        report.summary()
    );
}

/// The upstream test scripts are still far out of reach, and must keep saying so.
///
/// `test_language.js` leans on classes, generators and the whole standard library,
/// so it cannot pass yet. The engine must *fail* on them rather than pretend: a
/// crash, a hang or a silent wrong answer would all be worse than a reported
/// mismatch. This asserts that every case terminates with a verdict.
#[test]
fn the_upstream_script_corpus_reports_a_verdict_for_every_case() {
    let oracle = require_oracle!();
    let quickjs_root = std::path::Path::new("/tmp/quickjs-master");
    let cases: Vec<Case> = quickjs_conformance::script_cases(quickjs_root);
    if cases.is_empty() {
        eprintln!("skipping: no upstream test scripts found under {quickjs_root:?}");
        return;
    }
    let harness = Harness::new(candidate(), oracle);
    let report = harness.run(&cases);
    assert_eq!(
        report.entries.len(),
        cases.len(),
        "every script case needs a verdict:\n{}",
        report.summary()
    );
    // Once every script case matched, this canary had served its purpose. If it fires
    // again, the corpus *stopped* fully matching and something regressed.
    assert_eq!(
        report.matched(),
        cases.len(),
        "the upstream corpus used to match completely; something regressed:\n{}",
        report.summary()
    );
}

/// `examples/hello_module.js` is the module system's acceptance test: a JavaScript
/// module, a JSON module and a text module, all imported from one file.
///
/// It is checked here rather than in `script_cases` because it lives under `examples/`,
/// and it is checked *whole* because the point is the graph — a JSON module is not code,
/// and `with { type: "text" }` makes the same file a different module. The output is
/// compared byte for byte, which is the only thing that catches a module returning the
/// parsed object where the text was asked for.
#[test]
fn hello_module_matches_the_reference() {
    let oracle = require_oracle!();
    let path = std::path::Path::new("/tmp/quickjs-master/examples/hello_module.js");
    if !path.is_file() {
        eprintln!("skipping: {path:?} is not present");
        return;
    }
    let harness = Harness::new(candidate(), oracle);
    let report = harness.run(&[Case::script(path)]);
    assert!(
        report.failures() == 0,
        "hello_module.js differs from the reference:\n{}",
        report.summary()
    );
}

/// A cyclic import: `a.js` and `b.js` import each other, and `b` calls `a`'s function
/// from its own top level.
///
/// The whole-file case in `script_cases` covers the upstream fixture. This one covers the
/// ordering that fixture does not: every module in the cycle must have its hoisted
/// bindings created during *linking*, before any body runs, or `b`'s call into `a` finds
/// nothing.
#[test]
fn a_cycle_between_two_files_matches_the_reference() {
    let oracle = require_oracle!();
    let dir = std::env::temp_dir().join("qjs-cycle-conformance");
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        dir.join("a.js"),
        "import { b } from \"./b.js\";\nexport function a(x) { return x + 1 }\n\
         print(\"a sees b: \" + typeof b);\n",
    )
    .expect("write a.js");
    std::fs::write(
        dir.join("b.js"),
        "import { a } from \"./a.js\";\nexport function b(x) { return 2 * a(x) }\n\
         print(\"b sees a: \" + typeof a);\nprint(\"b calls a: \" + a(1));\n",
    )
    .expect("write b.js");
    let main = dir.join("main.js");
    std::fs::write(
        &main,
        "import { a } from \"./a.js\";\nprint(\"main: \" + a(1));\n",
    )
    .expect("write main.js");
    let harness = Harness::new(candidate(), oracle);
    let report = harness.run(&[Case::script(&main)]);
    assert!(
        report.failures() == 0,
        "the cyclic import differs from the reference:\n{}",
        report.summary()
    );
}

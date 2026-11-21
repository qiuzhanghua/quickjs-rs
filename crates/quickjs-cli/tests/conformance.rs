//! Differential tests: this build of `qjs` against the C reference build.
//!
//! The reference binary is discovered via `$QJS_ORACLE` or the default local
//! paths (see `quickjs_conformance::locate_oracle`). When it is missing the
//! differential tests are skipped with a message instead of failing, so the
//! suite stays usable on a machine without the C sources.
//!
//! Build the reference once with `scripts/setup-oracle.sh`.

use quickjs_conformance::{Case, Engine, Harness, locate_oracle};

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

#[test]
fn language_corpus_is_pending_not_failing() {
    let oracle = require_oracle!();
    let harness = Harness::new(candidate(), oracle);
    let cases = quickjs_conformance::language_cases();
    let report = harness.run(&cases);
    assert!(
        !report.has_failures(),
        "unsupported code must report as pending, not as a mismatch:\n{}",
        report.summary()
    );
    assert_eq!(
        report.pending(),
        cases.len(),
        "every language case should be pending until the VM lands:\n{}",
        report.summary()
    );
    assert_eq!(report.matched(), 0);
}

#[test]
fn script_corpus_is_pending_not_failing() {
    let oracle = require_oracle!();
    let quickjs_root = std::path::Path::new("/tmp/quickjs-master");
    let cases: Vec<Case> = quickjs_conformance::script_cases(quickjs_root);
    if cases.is_empty() {
        eprintln!("skipping: no upstream test scripts found under {quickjs_root:?}");
        return;
    }
    let harness = Harness::new(candidate(), oracle);
    let report = harness.run(&cases);
    assert!(
        !report.has_failures(),
        "script cases must be pending:\n{}",
        report.summary()
    );
    assert_eq!(report.pending(), cases.len());
}

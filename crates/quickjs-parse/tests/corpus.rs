//! Corpus differential: parse every JavaScript file in a QuickJS checkout and
//! compare our accept/reject verdict with the reference parser (`qjsc -c`).
//!
//! This is the broadest parser check available without test262: the upstream
//! test suite alone is ~20k lines of hand-written ES2015–ES2025 code, including
//! the corners that only show up in real programs (ASI in odd places, nested
//! destructuring, class fields, private methods, generators, `for await`, …).
//!
//! Skipped when no reference build is available; see `scripts/setup-oracle.sh`.

use quickjs_conformance::{Engine, locate_qjsc};
use std::path::{Path, PathBuf};

fn oracle() -> Option<Engine> {
    locate_qjsc().map(|path| Engine::new("qjsc", path))
}

/// Find a QuickJS checkout, if there is one.
fn quickjs_root() -> Option<PathBuf> {
    for candidate in [
        "/tmp/quickjs-master",
        "/tmp/quickjs",
        "quickjs",
        "vendor/quickjs",
    ] {
        let path = PathBuf::from(candidate);
        if path.join("VERSION").is_file() || path.join("quickjs.c").is_file() {
            return Some(path);
        }
    }
    None
}

/// Runs `qjsc -c` on one file. `true` = the reference parser accepted it.
fn reference_accepts(oracle: &Engine, file: &Path, module: bool, scratch: &Path) -> Option<bool> {
    let out_path = scratch.join("out.c");
    let mut args = vec![
        "-c".to_string(),
        "-o".to_string(),
        out_path.display().to_string(),
    ];
    if module {
        args.push("-m".to_string());
    }
    args.push(file.display().to_string());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = oracle.output(&arg_refs).ok()?;
    if out.code == 0 {
        Some(true)
    } else if out.stderr.starts_with("SyntaxError") {
        Some(false)
    } else {
        // Link/instantiation errors are not parse verdicts.
        None
    }
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Scratch {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "quickjs-rs-corpus-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn parse_file(path: &Path, module: bool) -> Result<(), js_parse::SyntaxError> {
    let source = std::fs::read_to_string(path).map_err(|e| {
        js_parse::SyntaxError::new(format!("read error: {e}"), js_parse::Span::start())
    })?;
    if module {
        js_parse::parse_module(&source).map(|_| ())
    } else {
        js_parse::parse_script(&source).map(|_| ())
    }
}

/// Compare every `*.js` file under `dir` (recursively).
fn check_directory(
    oracle: &Engine,
    dir: &Path,
    module: bool,
    scratch: &Scratch,
    report: &mut Report,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            check_directory(oracle, &path, module, scratch, report);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("js") {
            continue;
        }
        let Some(reference) = reference_accepts(oracle, &path, module, &scratch.0) else {
            report.skipped += 1;
            continue;
        };
        report.checked += 1;
        match (reference, parse_file(&path, module)) {
            (true, Ok(())) | (false, Err(_)) => {}
            (true, Err(err)) => report.mismatches.push(format!(
                "{}: we rejected a valid file: {err}",
                path.display()
            )),
            (false, Ok(())) => report
                .mismatches
                .push(format!("{}: we accepted an invalid file", path.display())),
        }
    }
}

#[derive(Default)]
struct Report {
    checked: usize,
    skipped: usize,
    mismatches: Vec<String>,
}

#[test]
fn upstream_test_suite_parses_exactly_like_the_reference() {
    let Some(oracle) = oracle() else {
        eprintln!("skipping: no reference qjsc found (run scripts/setup-oracle.sh)");
        return;
    };
    let Some(root) = quickjs_root() else {
        eprintln!("skipping: no QuickJS checkout found");
        return;
    };
    let scratch = Scratch::new();
    let mut report = Report::default();

    // `qjsc` autodetects script versus module, so the comparison has to use the
    // same grammar: everything here is strict-compatible, so module mode is the
    // closest match to the reference's autodetection.
    check_directory(&oracle, &root.join("tests"), true, &scratch, &mut report);
    check_directory(&oracle, &root.join("examples"), true, &scratch, &mut report);

    assert!(
        report.checked >= 10,
        "expected a meaningful corpus, only {} files checked ({} skipped)",
        report.checked,
        report.skipped
    );
    assert!(
        report.mismatches.is_empty(),
        "{} of {} corpus files disagree with the reference parser:\n{}",
        report.mismatches.len(),
        report.checked,
        report.mismatches.join("\n")
    );
}

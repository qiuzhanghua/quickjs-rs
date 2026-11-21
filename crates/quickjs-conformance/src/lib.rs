//! Differential test harness: pure-Rust `qjs` vs the C reference implementation.
//!
//! Every acceptance criterion in `docs/requirements.md` is ultimately "behaves
//! like `bellard/quickjs`". Rather than encoding expected output by hand, we run
//! the same command line through both binaries and compare exit status and
//! stdout.
//!
//! ```no_run
//! use quickjs_conformance::{Case, Harness};
//!
//! let Some(harness) = Harness::from_env_or_defaults() else {
//!     eprintln!("no reference qjs available, skipping");
//!     return;
//! };
//! let report = harness.run(&[Case::eval("1+1"), Case::eval("print(1n << 64n)")]);
//! assert!(!report.has_failures());
//! ```
//!
//! Until the engine can execute scripts, the candidate exits with code 2 and a
//! `not implemented yet` message on stderr; those cases are reported as
//! [`Verdict::Pending`] instead of failures, so the harness can be wired up
//! before the engine exists.

use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Exit code the candidate uses for "parsed, engine not ready".
pub const EXIT_PENDING: i32 = 2;

/// Marker on stderr that classifies a run as pending.
pub const PENDING_MARKER: &str = "not implemented yet";

/// One engine under test.
#[derive(Debug, Clone)]
pub struct Engine {
    /// Human readable name, used in reports.
    pub name: String,
    /// Path to the executable.
    pub program: PathBuf,
}

impl Engine {
    /// Create an engine handle.
    pub fn new(name: impl Into<String>, program: impl Into<PathBuf>) -> Engine {
        Engine {
            name: name.into(),
            program: program.into(),
        }
    }

    fn run(&self, args: &[OsString]) -> std::io::Result<Output> {
        let out = Command::new(&self.program).args(args).output()?;
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    /// Whether this engine is the "not implemented yet" build.
    fn is_pending(&self, out: &Output) -> bool {
        out.code == EXIT_PENDING && out.stderr.contains(PENDING_MARKER)
    }

    /// Run the executable with `args` and capture its result.
    ///
    /// Wrapper around the internal runner for tests that inspect a single
    /// invocation (help text, usage errors, ...).
    pub fn output(&self, args: &[&str]) -> std::io::Result<Output> {
        self.run(&args.iter().map(OsString::from).collect::<Vec<_>>())
    }
}

/// Captured result of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// Process exit status (`-1` when killed by a signal).
    pub code: i32,
    /// Captured stdout.
    pub stdout: String,
    /// Captured stderr.
    pub stderr: String,
}

/// One test input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Case {
    /// `qjs -e 'expr'`
    Eval(String),
    /// `qjs script.js`
    Script(PathBuf),
}

impl Case {
    /// Convenience constructor for `-e` cases.
    pub fn eval(expr: impl Into<String>) -> Case {
        Case::Eval(expr.into())
    }

    /// Convenience constructor for script cases.
    pub fn script(path: impl Into<PathBuf>) -> Case {
        Case::Script(path.into())
    }

    fn args(&self) -> Vec<OsString> {
        match self {
            Case::Eval(expr) => vec![OsString::from("-e"), OsString::from(expr)],
            Case::Script(path) => vec![OsString::from(path)],
        }
    }
}

impl fmt::Display for Case {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Case::Eval(expr) => write!(f, "-e {expr}"),
            Case::Script(path) => write!(f, "{}", path.display()),
        }
    }
}

/// Result of comparing one case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Exit status and stdout agree.
    Match,
    /// The candidate cannot execute JavaScript yet.
    Pending(String),
    /// Output differs.
    Mismatch {
        /// Reference result.
        expected: Output,
        /// Candidate result.
        actual: Output,
    },
    /// One of the two binaries failed to run.
    Error(String),
}

/// Outcome of a whole run.
#[derive(Debug, Default)]
pub struct Report {
    /// `(case, verdict)` pairs, in input order.
    pub entries: Vec<(Case, Verdict)>,
}

impl Report {
    /// Number of cases whose output matched the reference.
    pub fn matched(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Match))
            .count()
    }

    /// Number of cases that could not run yet because the engine is incomplete.
    pub fn pending(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Pending(_)))
            .count()
    }

    /// Number of genuine failures.
    pub fn failures(&self) -> usize {
        self.entries
            .iter()
            .filter(|(_, v)| matches!(v, Verdict::Mismatch { .. } | Verdict::Error(_)))
            .count()
    }

    /// Whether every case either matched or is legitimately pending.
    pub fn has_failures(&self) -> bool {
        self.failures() > 0
    }

    /// Markdown-ish summary suitable for CI logs.
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{} cases: {} matched, {} pending, {} failed\n",
            self.entries.len(),
            self.matched(),
            self.pending(),
            self.failures()
        );
        for (case, verdict) in &self.entries {
            match verdict {
                Verdict::Match => {}
                Verdict::Pending(reason) => s.push_str(&format!("  PENDING {case}  ({reason})\n")),
                Verdict::Mismatch { expected, actual } => {
                    s.push_str(&format!("  FAIL    {case}\n"));
                    s.push_str(&format!(
                        "    expected: code={} stdout={:?}\n",
                        expected.code, expected.stdout
                    ));
                    s.push_str(&format!(
                        "    actual:   code={} stdout={:?}\n",
                        actual.code, actual.stdout
                    ));
                    if expected.stderr != actual.stderr {
                        s.push_str(&format!("    stderr:   expected={:?}\n", expected.stderr));
                        s.push_str(&format!("              actual  ={:?}\n", actual.stderr));
                    }
                }
                Verdict::Error(err) => s.push_str(&format!("  ERROR   {case}  ({err})\n")),
            }
        }
        s
    }
}

/// Pairs a candidate engine with a reference engine.
#[derive(Debug, Clone)]
pub struct Harness {
    /// The pure-Rust build.
    pub candidate: Engine,
    /// The C reference build (`bellard/quickjs`).
    pub oracle: Engine,
}

impl Harness {
    /// Build a harness from two explicit executables.
    pub fn new(candidate: Engine, oracle: Engine) -> Harness {
        Harness { candidate, oracle }
    }

    /// Look for a reference `qjs` in `$QJS_ORACLE` and the usual local paths.
    ///
    /// Returns `None` when no reference build is available, in which case the
    /// differential tests should be skipped rather than failed.
    pub fn from_env_or_defaults() -> Option<Harness> {
        let oracle = locate_oracle()?;
        let candidate = std::env::var_os("QJS_CANDIDATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("qjs"));
        Some(Harness::new(
            Engine::new("rust", candidate),
            Engine::new("reference", oracle),
        ))
    }

    /// Compare the reference build against itself. Used as a plumbing self-check:
    /// if this fails, the harness is broken, not the engine.
    pub fn self_check(&self, cases: &[Case]) -> Report {
        let mut report = Report::default();
        for case in cases {
            let args = case.args();
            let verdict = match self.oracle.run(&args) {
                Err(err) => Verdict::Error(err.to_string()),
                Ok(expected) => match self.oracle.run(&args) {
                    Err(err) => Verdict::Error(err.to_string()),
                    Ok(actual) if actual == expected => Verdict::Match,
                    Ok(actual) => Verdict::Mismatch { expected, actual },
                },
            };
            report.entries.push((case.clone(), verdict));
        }
        report
    }

    /// Run every case through both engines and classify the result.
    pub fn run(&self, cases: &[Case]) -> Report {
        let mut report = Report::default();
        for case in cases {
            let args = case.args();
            let verdict = match self.oracle.run(&args) {
                Err(err) => Verdict::Error(format!("reference: {err}")),
                Ok(expected) => match self.candidate.run(&args) {
                    Err(err) => Verdict::Error(format!("candidate: {err}")),
                    Ok(actual) if self.candidate.is_pending(&actual) => {
                        Verdict::Pending(first_line(&actual.stderr))
                    }
                    Ok(actual)
                        if actual.code == expected.code && actual.stdout == expected.stdout =>
                    {
                        Verdict::Match
                    }
                    Ok(actual) => Verdict::Mismatch { expected, actual },
                },
            };
            report.entries.push((case.clone(), verdict));
        }
        report
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

/// Search for a reference `qjs` binary.
pub fn locate_oracle() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("QJS_ORACLE") {
        let path = PathBuf::from(path);
        if is_executable(&path) {
            return Some(path);
        }
    }
    let candidates = [
        "/tmp/quickjs-master/qjs",
        "/tmp/quickjs/qjs",
        "quickjs/qjs",
        "vendor/quickjs/qjs",
        "../quickjs/qjs",
        "../../quickjs/qjs",
    ];
    for candidate in candidates {
        let path = PathBuf::from(candidate);
        if is_executable(&path) {
            return Some(path);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match std::fs::metadata(path) {
            Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// The starter corpus: language corners that exercise the compiler and VM.
///
/// These become the M3–M8 acceptance set; add cases as milestones land.
pub fn language_cases() -> Vec<Case> {
    [
        // values and arithmetic
        "1+1",
        "0.1+0.2",
        "1/3",
        "2**53 === 2**53+1",
        "-0 === 0",
        "Object.is(-0, 0)",
        "1n << 64n",
        "(2n ** 64n).toString(16)",
        // strings / templates
        r#"'a'+'b'"#,
        "`x${1+1}y`",
        "String.fromCharCode(0x1f600).length",
        r#""\u{1f600}".codePointAt(0)"#,
        // objects / arrays
        "[1,2,3].map(x=>x*2).join(',')",
        "JSON.stringify({a:[1,{b:2}]})",
        "Object.keys({a:1,b:2}).join('|')",
        "({...{a:1},b:2}).b",
        "(()=>{const o={};o.self=o;return JSON.stringify(o)===undefined?'cycle':''})()",
        // functions / closures / classes
        "function f(n){return n<2?n:f(n-1)+f(n-2)} f(20)",
        "(()=>{let n=0;return ()=>++n})()()",
        "class A{constructor(){this.x=1} get y(){return this.x+1}} new A().y",
        "(()=>{class B{#p=42; get(){return this.#p}} return new B().get()})()",
        // generators / async
        "(()=>{function*g(){yield 1;yield 2}return [...g()].join('')})()",
        "typeof (async()=>{})",
        "Promise.resolve(1).constructor === Promise",
        // errors
        "try{null.x}catch(e){e.constructor.name}",
        "(()=>{try{throw new Error('x')}catch(e){return e.message}})()",
        "(()=>{try{return 1}finally{}})()",
        "typeof AggregateError",
        // regexp
        "/a(b+)/.exec('abbb')[1]",
        "'aXbXc'.split(/X/).length",
        "/(?<y>\\d{4})-(?<m>\\d{2})/.exec('2026-06').groups.m",
        "RegExp.escape('a.b')",
        // numbers <-> strings (dtoa)
        "(123.456).toString()",
        "(0.1).toFixed(20)",
        "(1e21).toString()",
        "Number.parseFloat('1.7976931348623157e308')",
        "(1/3).toPrecision(17)",
        // typed arrays / buffers
        "new Uint8Array([1,2,3]).buffer.byteLength",
        "Float16Array ? 'yes' : 'no'",
        "Array.from(new Uint8Array([1,2,255])).join('-')",
        // collections / weak refs
        "(()=>{const m=new Map([[1,'a']]);return m.get(1)})()",
        "typeof WeakRef",
        "new Set([1,2,2]).size",
        // iterator helpers (2026 upstream addition)
        "typeof Iterator.prototype.map",
        "[1,2,3].values().map(x=>x*2).toArray().join('')",
        // Date
        "typeof Date.prototype.toTemporalInstant",
        // misc spec corners
        "typeof globalThis",
        "Reflect.ownKeys({a:1}).join(',')",
        "(function(){return typeof this})()",
        "'use strict';(function(){return typeof this})()",
        "eval('1+1')",
        "new Function('a','b','return a+b')(1,2)",
        "typeof Proxy",
        "(()=>{const p=new Proxy({},{get:()=>42});return p.anything})()",
    ]
    .into_iter()
    .map(Case::eval)
    .collect()
}

/// Script cases from a `bellard/quickjs` checkout (`<root>/tests`).
pub fn script_cases(quickjs_root: &Path) -> Vec<Case> {
    let tests = quickjs_root.join("tests");
    [
        "test_language.js",
        "test_builtin.js",
        "test_closure.js",
        "test_bigint.js",
        "test_loop.js",
        "test_bjson.js",
        "test_std.js",
    ]
    .into_iter()
    .map(|name| tests.join(name))
    .filter(|p| p.is_file())
    .map(Case::script)
    .collect()
}

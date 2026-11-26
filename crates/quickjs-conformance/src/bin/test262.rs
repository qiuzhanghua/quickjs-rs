//! Runs the official test262 suite against this engine.
//!
//! # Why this exists
//!
//! The differential suite compares this engine against the C reference on **upstream's own
//! `tests/*.js`** — seven files, of which the sliced runner only looks at five. That is a
//! fine regression net and a terrible coverage measure: typed arrays appear about five
//! times across the whole corpus and private class fields not at all, so "80/85 pass, no
//! known gaps" stayed true while `[...new Uint8Array([1])]` threw and `#x` refused to
//! compile. A green number over a narrow sample says nothing about what the sample never
//! touches.
//!
//! test262 is the standard's own suite, and `requirements.md` names it as the exit
//! criterion for the RegExp and TypedArray milestones — so measuring against it is
//! required anyway, and measuring *first* is what makes any later "we fixed it" checkable.
//!
//! # Why one process per test
//!
//! §Realm Isolation requires a fresh realm per test, and a fresh `Runtime` per test in one
//! process would be fast (~100µs). It is not used, because a *hang* is one of the outcomes
//! being measured: test262 contains tests that only terminate if the implementation
//! validates something first, and this engine does not validate everything yet. In-process,
//! the first such test takes the whole run with it and the remaining thousands are never
//! measured — which is the same failure mode as a silently skipped test.
//!
//! A process per test costs ~3.6ms (measured), so 24k tests take about a minute and a half,
//! and a hang, a panic or a stack overflow is contained: it becomes one recorded result.
//!
//! # The three outcomes
//!
//! * **pass** — behaved as its metadata requires;
//! * **fail** — did not;
//! * **unsupported** — needs something the engine does not have *at all* (a harness file,
//!   the `async` flag, a feature such as `Atomics`). Counted and grouped by reason rather
//!   than dropped: a skipped test is indistinguishable from a passing one in a summary,
//!   which is exactly how three wrong answers survived a milestone called "complete".

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// What running one test produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Behaves as its metadata requires.
    Pass,
    /// It did not. The string is what to show a human.
    Fail(String),
    /// It cannot be run at all yet. The string is the *reason*, grouped for counting.
    Unsupported(String),
}

/// One test file, parsed.
pub struct Test {
    /// Path relative to the test262 root, for reporting.
    pub name: String,
    /// The code, metadata comment included.
    pub source: String,
    /// The absolute path, which module fixtures resolve against.
    pub path: PathBuf,
    /// `flags:` entries.
    pub flags: Vec<String>,
    /// Files from `harness/` to evaluate before the test.
    pub includes: Vec<String>,
    /// `negative:` — the phase and the constructor name expected.
    pub negative: Option<(String, String)>,
    /// `features:` — the proposal features it exercises.
    pub features: Vec<String>,
}

impl Test {
    /// Parse the `/*--- … ---*/` block at the top of a test262 file.
    ///
    /// Hand-rolled rather than through a YAML crate: the metadata is a handful of
    /// top-level keys with scalar or flow-sequence values, and a YAML dependency is a lot
    /// of parser for the subset actually used.
    pub fn parse(name: String, path: PathBuf, source: String) -> Test {
        let mut test = Test {
            name,
            path,
            source: source.clone(),
            flags: Vec::new(),
            includes: Vec::new(),
            negative: None,
            features: Vec::new(),
        };
        let Some(block) = metadata_block(&source) else {
            return test;
        };
        let mut lines = block.lines().peekable();
        while let Some(line) = lines.next() {
            let trimmed = line.trim();
            // `negative:` is the only nested mapping, so it is read from the lines that
            // follow the key instead of from its own value.
            if trimmed == "negative:" {
                let (mut phase, mut kind) = (String::new(), String::new());
                while let Some(inner) = lines.peek() {
                    if inner.trim().is_empty() || !inner.starts_with(' ') {
                        break;
                    }
                    let inner = lines
                        .next()
                        .map(|l| l.trim().to_string())
                        .unwrap_or_default();
                    if let Some(value) = inner.strip_prefix("phase:") {
                        phase = unquote(value);
                    } else if let Some(value) = inner.strip_prefix("type:") {
                        kind = unquote(value);
                    }
                }
                if !phase.is_empty() && !kind.is_empty() {
                    test.negative = Some((phase, kind));
                }
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                continue;
            };
            match key.trim() {
                "flags" => test.flags = flow_sequence(value),
                "includes" => test.includes = flow_sequence(value),
                "features" => test.features = flow_sequence(value),
                _ => {}
            }
        }
        test
    }

    /// Whether this test needs a host capability the runner does not provide.
    ///
    /// Kept apart from `unsupported_reason` (which is about the engine) because these
    /// are *our* gaps: counting them as engine failures would overstate the engine's
    /// defects, and hiding them would understate the run's coverage.
    fn runner_gap(&self) -> Option<String> {
        // A module test cannot see a `var` from a separately-evaluated script, and the
        // engine has no cross-script global hook to install one.
        if self.flags.iter().any(|flag| flag == "module") && self.source.contains("$262") {
            return Some("runner: $262 in module tests".to_string());
        }
        for needle in [
            "$262.createRealm",
            "$262.agent",
            "$262.IsHTMLDDA",
            "$262.AbstractModuleSource",
        ] {
            if self.source.contains(needle) {
                return Some(format!("runner: {needle} not implemented"));
            }
        }
        None
    }

    /// Whether this test uses a flag or feature the runner cannot honour yet.
    fn unsupported_reason(&self) -> Option<String> {
        if self.flags.iter().any(|flag| flag == "async") {
            return Some("flag:async ($DONE) not wired up".to_string());
        }
        // Features whose API the engine does not have. Named rather than guessed at, so
        // the count says which part of the engine is missing.
        const MISSING_FEATURES: &[&str] = &[
            "Atomics",
            "SharedArrayBuffer",
            "Temporal",
            "Intl",
            "tail-call-optimization",
            "decorators",
            "explicit-resource-management",
        ];
        for feature in &self.features {
            if MISSING_FEATURES.contains(&feature.as_str()) {
                return Some(format!("feature:{feature}"));
            }
        }
        None
    }
}

/// The text between `/*---` and `---*/`, if there is one.
fn metadata_block(source: &str) -> Option<&str> {
    let start = source.find("/*---")? + "/*---".len();
    let end = source[start..].find("---*/")? + start;
    Some(&source[start..end])
}

/// A `[a, b]` or `a` metadata value as a list.
fn flow_sequence(value: &str) -> Vec<String> {
    let value = value.trim();
    let inner = value
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(value);
    inner
        .split(',')
        .map(|item| unquote(item.trim()))
        .filter(|item| !item.is_empty())
        .collect()
}

/// Strip one layer of quotes.
fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if let Some(rest) = value.strip_prefix(quote)
            && let Some(inner) = rest.strip_suffix(quote)
        {
            return inner.to_string();
        }
    }
    value.to_string()
}

/// The harness files every test needs, per INTERPRETING.md.
const ALWAYS_INCLUDED: &[&str] = &["sta.js", "assert.js"];

/// The `$262` host object, per INTERPRETING.md §Test262-Defined Bindings.
///
/// Written in JS because every part of it that this engine can support is expressible in
/// JS. The parts it cannot are absent on purpose and detected by [`Test::runner_gap`], so
/// the tests that need them are reported as *runner* gaps rather than as engine failures —
/// attributing a missing host capability to the engine would be the same kind of
/// overclaim this whole exercise exists to prevent.
const HOST_OBJECT: &str = r#"
var $262 = {
  global: globalThis,
  evalScript: function (source) { return (0, eval)(source); },
  detachArrayBuffer: function (buffer) {
    // `ArrayBuffer.prototype.transfer` performs DetachArrayBuffer on its receiver as a
    // side effect, which is exactly the operation wanted here.
    buffer.transfer();
  },
  gc: function () { throw new TypeError("the host provides no garbage collection"); }
};
"#;

/// How long one test may take before it is called a hang.
///
/// Generous, because a few tests legitimately do heavy work (large string builds, deep
/// recursion). Anything slower than this is a loop the engine failed to break out of.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Address-space ceiling for one test's process, in KiB.
///
/// Enforced by the shell that spawns the engine (`ulimit -v`), because the engine's own
/// `--memory-limit` is accepted and ignored. Sized to be unreachable by a legitimate
/// test and still small enough that a runaway one cannot exhaust the host.
const ADDRESS_SPACE_LIMIT_KB: usize = 512 * 1024;

/// Upper bound on the default job count.
///
/// The harness runs one process per test, so the default used to scale with the machine:
/// on a 28-core host that meant 28 concurrent engines, and a single unbounded allocation
/// in one of them was enough to take the session down. A number this small is plenty,
/// since the work is dominated by the minority of hangs, which are wall clock.
const MAX_DEFAULT_JOBS: usize = 8;

/// How a test that was still running at the deadline is recorded.
///
/// Separate from `Fail` on purpose: a hang means the engine failed to break out of a loop
/// the spec bounds (usually an unchecked `length`), which is a different defect from a
/// wrong value — and it is the one that costs a run minutes of wall clock.
const HANG: &str = "hang (still running at the deadline)";

/// Runs test262 tests by invoking the built `qjs`.
pub struct Runner {
    /// The test262 checkout.
    root: PathBuf,
    /// The `qjs` binary under test.
    engine: PathBuf,
    /// Where prepared test sources are written.
    scratch: PathBuf,
    /// How long one test may run.
    timeout: Duration,
}

impl Runner {
    /// A runner over the checkout at `root` using `engine`.
    pub fn new(root: impl Into<PathBuf>, engine: impl Into<PathBuf>, timeout: Duration) -> Runner {
        let scratch = std::env::temp_dir().join("qjs-test262");
        let _ = std::fs::create_dir_all(&scratch);
        Runner {
            root: root.into(),
            engine: engine.into(),
            scratch,
            timeout,
        }
    }

    /// Every test file under `dir`, sorted, skipping `_FIXTURE` files.
    pub fn collect(&self, dir: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&path) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "js")
                    && !path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.contains("_FIXTURE"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// Read and parse one test.
    pub fn load(&self, path: &Path) -> Option<Test> {
        let source = std::fs::read_to_string(path).ok()?;
        let name = path
            .strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        Some(Test::parse(name, path.to_path_buf(), source))
    }

    /// Run one test, at every strictness its flags call for.
    pub fn run(&self, test: &Test) -> Outcome {
        if let Some(reason) = test.runner_gap() {
            return Outcome::Unsupported(reason);
        }
        if let Some(reason) = test.unsupported_reason() {
            return Outcome::Unsupported(reason);
        }
        let harness = match self.harness_for(test) {
            Ok(harness) => harness,
            Err(reason) => return Outcome::Unsupported(reason),
        };
        let is_module = test.flags.iter().any(|flag| flag == "module");
        // `onlyStrict` runs once with the directive, `noStrict` once without, and a test
        // with neither must pass *both* — that is what its absence means.
        let strictness: &[bool] = if test.flags.iter().any(|flag| flag == "onlyStrict") {
            &[true]
        } else if test.flags.iter().any(|flag| flag == "noStrict") {
            &[false]
        } else {
            &[false, true]
        };
        for &strict in strictness {
            let outcome = self.run_once(test, &harness, is_module, strict);
            if outcome != Outcome::Pass {
                return outcome;
            }
        }
        Outcome::Pass
    }

    /// The concatenated harness text for `test`, or the name of a missing file.
    fn harness_for(&self, test: &Test) -> Result<String, String> {
        let mut out = String::new();
        for name in ALWAYS_INCLUDED
            .iter()
            .map(|name| name.to_string())
            .chain(test.includes.iter().cloned())
        {
            match std::fs::read_to_string(self.root.join("harness").join(&name)) {
                Ok(text) => {
                    out.push_str(&text);
                    out.push('\n');
                }
                Err(_) => return Err(format!("harness file missing: {name}")),
            }
        }
        Ok(out)
    }

    /// One execution of one test at one strictness, as a child process.
    fn run_once(&self, test: &Test, harness: &str, is_module: bool, strict: bool) -> Outcome {
        // A module test is written *next to its fixtures*, because `./x_FIXTURE.js` is
        // resolved relative to the importing file. The name is prefixed so a stray file
        // cannot be mistaken for a test262 one.
        let path = if is_module {
            let file = test
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "test.js".to_string());
            test.path.with_file_name(format!("__qjs_runner_{file}"))
        } else {
            let safe = test.name.replace('/', "__");
            self.scratch.join(format!("{safe}.js"))
        };
        let mut source = String::new();
        if is_module {
            // A module is strict already; the directive would be a syntax error in a
            // module's import/export position anyway, so it is only added for scripts.
            if strict {
                // Modules are strict by definition; nothing to add.
            }
            source.push_str(&test.source);
        } else {
            // A `raw` test gets no harness at all.
            if !test.flags.iter().any(|flag| flag == "raw") {
                source.push_str(HOST_OBJECT);
                source.push_str(harness);
            }
            // §Strict Mode: the directive goes on the *test*, after the harness, so the
            // harness itself stays sloppy.
            if strict {
                source.push_str("\"use strict\";\n");
            }
            source.push_str(&test.source);
        }
        if let Err(error) = std::fs::write(&path, source) {
            return Outcome::Unsupported(format!("cannot write test source: {error}"));
        }

        let mut command = Command::new("sh");
        // Each test gets a hard address-space limit. Without it a single test that
        // allocates in a loop takes the whole machine down: test262 has tests that build
        // huge arrays on purpose, and this engine does not enforce `--memory-limit`
        // (`Runtime::set_memory_limit` stores the value and nothing reads it), so the
        // allocation goes to the real allocator. Measured consequence: running the suite
        // with the default job count logged the user out of the machine.
        //
        // The limit is applied by `sh` rather than by the engine, so it holds even while
        // the engine's own limit is decorative. 512 MiB is far above what any single
        // test262 test needs and far below what it takes to starve the host.
        command
            .arg("-c")
            .arg(format!(
                "ulimit -v {} 2>/dev/null || true; exec \"$0\" \"$@\"",
                ADDRESS_SPACE_LIMIT_KB
            ))
            .arg(&self.engine);
        if is_module {
            command.arg("-m");
        }
        command
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let result = run_with_timeout(command, self.timeout);
        if is_module {
            let _ = std::fs::remove_file(&path);
        }
        match result {
            RunResult::Timeout => Outcome::Fail(HANG.to_string()),
            RunResult::SpawnError(error) => {
                Outcome::Unsupported(format!("cannot spawn engine: {error}"))
            }
            RunResult::Done {
                code,
                stdout,
                stderr,
            } => judge(test, code, &stdout, &stderr, is_module),
        }
    }
}

/// What a child process did.
enum RunResult {
    /// It finished.
    Done {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// It was still running at the deadline and was killed.
    Timeout,
    /// It could not be started.
    SpawnError(String),
}

/// Run `command`, killing it if it outlives `limit`.
///
/// `std` has no `wait_timeout`, so this polls `try_wait`. A 5ms poll is far below the
/// per-test cost and keeps the timeout honest without a thread per test.
fn run_with_timeout(mut command: Command, limit: Duration) -> RunResult {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return RunResult::SpawnError(error.to_string()),
    };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut pipe, &mut stderr);
                }
                return RunResult::Done {
                    code: status.code().unwrap_or(-1),
                    stdout,
                    stderr,
                };
            }
            Ok(None) => {
                if started.elapsed() >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    return RunResult::Timeout;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return RunResult::SpawnError(error.to_string()),
        }
    }
}

/// Decide a test's outcome from how the child finished.
///
/// The three `negative` phases are different observations: `parse` is "it refused to
/// compile", `resolution` is "the module graph could not be linked", `runtime` is "it
/// threw while running". This engine reports a compile error with a non-zero exit code and
/// a message containing the constructor name, so the phase is inferred from that.
fn judge(test: &Test, code: i32, stdout: &str, stderr: &str, is_module: bool) -> Outcome {
    let _ = (stdout, is_module);
    match &test.negative {
        None => {
            if code == 0 {
                Outcome::Pass
            } else {
                let detail = stderr
                    .lines()
                    .chain(stdout.lines())
                    .next()
                    .unwrap_or("<no output>")
                    .to_string();
                Outcome::Fail(detail)
            }
        }
        Some((phase, kind)) => {
            if code == 0 {
                return Outcome::Fail(format!(
                    "expected a {phase}-time {kind}, but it finished normally"
                ));
            }
            let all = format!("{stderr}\n{stdout}");
            if all.contains(kind.as_str()) {
                Outcome::Pass
            } else {
                let detail = all
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or("<no output>")
                    .to_string();
                Outcome::Fail(format!("expected a {phase}-time {kind}, got: {detail}"))
            }
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut root = PathBuf::from("/tmp/tc39-test262");
    let mut dir: Option<String> = None;
    let mut show_failures = 15usize;
    let mut timeout = TIMEOUT;
    let mut engine_path: Option<PathBuf> = None;
    let mut outcomes_path: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().expect("--root needs a path")),
            "--dir" => dir = Some(args.next().expect("--dir needs a path")),
            "--engine" => {
                engine_path = Some(PathBuf::from(args.next().expect("--engine needs a path")))
            }
            "--timeout" => {
                let secs: f64 = args.next().and_then(|n| n.parse().ok()).unwrap_or(10.0);
                timeout = Duration::from_secs_f64(secs);
            }
            "--show-failures" => {
                show_failures = args
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(show_failures);
            }
            // One line per test, sorted by name: the machine-readable form of the run.
            // A summary alone cannot answer "did this change break anything", because
            // two runs can pass the same number of tests with different test names, and
            // the aggregate hides exactly the swap that matters.
            "--outcomes" => {
                outcomes_path = Some(PathBuf::from(args.next().expect("--outcomes needs a path")))
            }
            other => {
                eprintln!("unknown option: {other}");
                std::process::exit(2);
            }
        }
    }
    let engine = engine_path.unwrap_or_else(|| PathBuf::from("target/release/qjs"));
    if !root.join("harness").is_dir() {
        eprintln!("no test262 checkout at {root:?} (--root)");
        std::process::exit(2);
    }
    if !engine.is_file() {
        eprintln!("no engine at {engine:?} (--engine)");
        std::process::exit(2);
    }
    let search = root.join(dir.unwrap_or_else(|| "test/built-ins".to_string()));
    let runner = Runner::new(&root, &engine, timeout);
    let files = runner.collect(&search);
    if files.is_empty() {
        eprintln!("no tests under {search:?}");
        std::process::exit(2);
    }

    // Tests are independent and a fresh process each, so they parallelise trivially. The
    // work is dominated by a minority of hangs, which are wall clock, not CPU — threads
    // overlap them rather than waiting on them one at a time.
    let jobs: usize = std::env::var("TEST262_JOBS")
        .ok()
        .and_then(|n| n.parse().ok())
        .or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .map(|n| n.get().min(MAX_DEFAULT_JOBS))
        })
        .unwrap_or(4);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut pass = 0usize;
    let mut failures: Vec<(String, String)> = Vec::new();
    let mut hangs = 0usize;
    let mut unsupported: BTreeMap<String, usize> = BTreeMap::new();
    let total = files.len();
    let progress = std::env::var_os("TEST262_PROGRESS").is_some();
    // A per-test record is only worth its allocations when someone asked for it.
    let record = outcomes_path.is_some();
    let mut batches = Vec::new();
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..jobs {
            handles.push(scope.spawn(|| {
                let mut local_pass = 0usize;
                let mut local_failures: Vec<(String, String)> = Vec::new();
                let mut local_hangs = 0usize;
                let mut local_unsupported: BTreeMap<String, usize> = BTreeMap::new();
                let mut local_outcomes: Vec<(String, Outcome)> = Vec::new();
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(path) = files.get(index) else {
                        break;
                    };
                    if progress && index % 500 == 0 {
                        eprintln!("[{index}/{total}] {}", path.display());
                    }
                    let Some(test) = runner.load(path) else {
                        continue;
                    };
                    let outcome = runner.run(&test);
                    if record {
                        local_outcomes.push((test.name.clone(), outcome.clone()));
                    }
                    match outcome {
                        Outcome::Pass => local_pass += 1,
                        Outcome::Fail(detail) => {
                            if detail == HANG {
                                local_hangs += 1;
                            }
                            local_failures.push((test.name.clone(), detail));
                        }
                        Outcome::Unsupported(reason) => {
                            *local_unsupported.entry(reason).or_default() += 1;
                        }
                    }
                }
                (
                    local_pass,
                    local_failures,
                    local_hangs,
                    local_unsupported,
                    local_outcomes,
                )
            }));
        }
        for handle in handles {
            batches.push(handle.join().expect("test worker panicked"));
        }
    });
    let mut outcomes_all: Vec<(String, Outcome)> = Vec::new();
    for (local_pass, mut local_failures, local_hangs, local_unsupported, local_outcomes) in batches
    {
        pass += local_pass;
        hangs += local_hangs;
        failures.append(&mut local_failures);
        outcomes_all.extend(local_outcomes);
        for (reason, count) in local_unsupported {
            *unsupported.entry(reason).or_default() += count;
        }
    }
    failures.sort();
    outcomes_all.sort_by(|left, right| left.0.cmp(&right.0));
    if let Some(path) = outcomes_path.as_ref() {
        // Tab separated and sorted by test name, so two runs diff as sets and nothing
        // here depends on the order the workers happened to finish in.
        let mut file = match std::fs::File::create(path) {
            Ok(file) => std::io::BufWriter::new(file),
            Err(error) => {
                eprintln!("cannot write outcomes to {path:?}: {error}");
                std::process::exit(2);
            }
        };
        for (name, outcome) in &outcomes_all {
            let line = match outcome {
                Outcome::Pass => format!("PASS\t{name}"),
                Outcome::Fail(_) => format!("FAIL\t{name}"),
                Outcome::Unsupported(reason) => format!("SKIP\t{name}\t{reason}"),
            };
            let _ = writeln!(file, "{line}");
        }
    }
    let unsupported_total: usize = unsupported.values().sum();
    let runnable = pass + failures.len();
    println!("== test262 {}", search.display());
    println!(
        "{pass}/{runnable} runnable pass ({:.1}%), {} fail, {unsupported_total} unsupported of {total} total",
        percent(pass, runnable),
        failures.len(),
    );
    if hangs > 0 {
        println!(
            "  of the failures, {hangs} are hangs (the engine never terminated), {} are wrong answers",
            failures.len() - hangs
        );
    }
    if !unsupported.is_empty() {
        println!("\n-- unsupported, by reason:");
        for (reason, count) in &unsupported {
            println!("  {count:6}  {reason}");
        }
    }
    if std::env::var_os("TEST262_GROUP").is_some() {
        let mut groups: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for (name, _) in &failures {
            groups.entry(group_of(name)).or_default().0 += 1;
        }
        for (name, _) in files
            .iter()
            .filter_map(|path| runner.load(path))
            .map(|t| (t.name, ()))
        {
            groups.entry(group_of(&name)).or_default().1 += 1;
        }
        let mut rows: Vec<(String, usize, usize)> = groups
            .into_iter()
            .map(|(group, (failed, total))| (group, failed, total))
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        // The denominator is every test in the area, unsupported included, so the ratio
        // reads as "how much of this area is broken" rather than flattering it.
        println!("\n-- failures by built-in (worst first):");
        for (group, failed, total) in rows.iter().take(40) {
            if *failed == 0 {
                continue;
            }
            println!("  {failed:6} of {total:<6} failing  {group}");
        }
    }
    if std::env::var_os("TEST262_SIGNATURES").is_some() {
        let mut signatures: BTreeMap<String, usize> = BTreeMap::new();
        for (_, detail) in &failures {
            *signatures.entry(signature(detail)).or_default() += 1;
        }
        let mut rows: Vec<(String, usize)> = signatures.into_iter().collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.1));
        println!("\n-- most common failure shapes:");
        for (shape, count) in rows.iter().take(30) {
            println!("  {count:6}  {shape}");
        }
    }
    if !failures.is_empty() {
        println!(
            "\n-- first {} of {} failures:",
            show_failures.min(failures.len()),
            failures.len()
        );
        for (name, detail) in failures.iter().take(show_failures) {
            println!("  {name}\n      {detail}");
        }
    }
    let _ = std::io::stderr().flush();
    std::process::exit(if failures.is_empty() { 0 } else { 1 });
}

/// Which built-in a test belongs to: `test/built-ins/Array/prototype/map/x.js` -> `Array`.
///
/// The point of grouping is to turn "10338 failures" into a map of what is broken, so the
/// next milestone can be chosen from evidence rather than from a checklist.
fn group_of(name: &str) -> String {
    let parts: Vec<&str> = name.split('/').collect();
    match parts.iter().position(|part| *part == "built-ins") {
        Some(index) => parts
            .get(index + 1)
            .map(|part| part.to_string())
            .unwrap_or_else(|| "<root>".to_string()),
        None => parts
            .get(1)
            .map(|part| part.to_string())
            .unwrap_or_else(|| "<root>".to_string()),
    }
}

/// The shape of a failure, with the specific values elided.
///
/// `Expected SameValue(«false», «true»)` and `Expected SameValue(«1», «2»)` are the same
/// defect; grouping by the message without its payload is what makes a histogram useful.
fn signature(detail: &str) -> String {
    let head = detail.lines().next().unwrap_or("").trim();
    let head = match head.find('«') {
        Some(index) => &head[..index],
        None => head,
    };
    let head = head.split_whitespace().collect::<Vec<_>>().join(" ");
    if head.chars().count() > 90 {
        format!("{}…", head.chars().take(90).collect::<String>())
    } else {
        head
    }
}

fn percent(part: usize, whole: usize) -> f64 {
    if whole == 0 {
        100.0
    } else {
        part as f64 * 100.0 / whole as f64
    }
}

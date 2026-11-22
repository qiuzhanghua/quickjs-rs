//! Foundations of the public `quickjs` API.

#[test]
fn runtime_defaults_are_sane() {
    let rt = quickjs::Runtime::new();
    assert_eq!(rt.heap().live_count(), 0);
    assert_eq!(rt.memory_limit(), None);
    assert_eq!(rt.stack_size(), None);
    assert!(rt.heap().threshold() > 0);
}

#[test]
fn context_eval_runs_a_script() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let value = ctx.eval("1+1").expect("runs");
    assert_eq!(value.to_js_string(), "2");
}

#[test]
fn a_syntax_error_is_reported_before_running() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval("let = ;") {
        Err(quickjs::Error::Syntax(_)) => {}
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

#[test]
fn a_thrown_value_comes_back_as_an_exception() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval("throw 'boom'") {
        Err(quickjs::Error::Exception(text)) => assert!(text.contains("boom"), "{text}"),
        other => panic!("expected an exception, got {other:?}"),
    }
}

#[test]
fn compat_version_matches_upstream_release() {
    assert_eq!(quickjs::COMPAT_VERSION, "2026-06-04");
}

#[test]
fn runtime_can_run_the_collector() {
    let rt = quickjs::Runtime::new();
    let stats = rt.run_gc();
    assert_eq!((stats.before, stats.collected, stats.after), (0, 0, 0));
}

/// Run `source` and answer what it printed.
fn run_output(source: &str) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    ctx.eval(source)
        .unwrap_or_else(|error| panic!("{source:?} failed: {error}"));
    ctx.output().to_string()
}

#[test]
fn a_label_on_a_for_of_loop_can_be_broken_and_continued() {
    // The target a `break label` reaches is recorded on the loop's jump target, and the
    // `for…of`/`for…in` compiler did not pass the label names through — so a labelled
    // loop reported "break has no enclosing loop" about a loop that plainly enclosed it.
    assert_eq!(
        run_output(
            r#"
var log = [];
outer: for (const i of [1, 2, 3]) {
  for (const j of [1, 2]) {
    if (j === 2) continue outer;
    if (i === 3) break outer;
    log.push(i + ":" + j);
  }
}
a: for (const k in {x: 1, y: 2}) { break a }
var n = 0;
b: while (true) { n++; if (n > 2) break b }
print(log.join(" ") + " n=" + n);
"#
        ),
        "1:1 2:1 n=3\n"
    );
}

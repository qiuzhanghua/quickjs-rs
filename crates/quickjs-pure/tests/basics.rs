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

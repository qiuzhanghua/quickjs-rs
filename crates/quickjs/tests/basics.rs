//! Foundations of the public `quickjs` API, before any JavaScript exists.

#[test]
fn runtime_defaults_are_sane() {
    let rt = quickjs::Runtime::new();
    assert_eq!(rt.heap().live_count(), 0);
    assert_eq!(rt.memory_limit(), None);
    assert_eq!(rt.stack_size(), None);
    assert!(rt.heap().threshold() > 0);
}

#[test]
fn context_eval_reports_not_implemented() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval("1+1") {
        Err(quickjs::Error::NotImplemented(what)) => assert!(what.contains("eval")),
        other => panic!("expected NotImplemented, got {other:?}"),
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

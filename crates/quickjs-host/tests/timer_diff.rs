//! `os.setTimeout` against the reference, case by case.
//!
//! Each expectation here was produced by running the same script through `qjs --std`, not
//! by reasoning about what a timer "should" do. Four of them are places where the obvious
//! implementation differs from the reference, and all four were found by diffing rather
//! than by reading the source.

use quickjs::{Context, Runtime};

/// Run `source` with the timer functions installed as a global `os`, and return what it
/// printed plus any error the loop reported.
fn run(source: &str) -> (String, Option<String>) {
    let runtime = Runtime::new();
    let mut ctx = Context::new(&runtime);
    let os = ctx.new_object();
    quickjs_host::timers::install(&mut ctx, &os).expect("timers install");
    ctx.set_global("os", os).expect("os installed");
    let error = ctx.eval(source).err().map(|error| error.to_string());
    let loop_errors = quickjs_host::run_until_idle(&mut ctx).unwrap_or_default();
    let mut output = ctx.output().to_string();
    // The loop reports what timer callbacks threw; make it visible to the assertions.
    for text in loop_errors {
        output.push_str(&format!("LOOPERROR: {text}\n"));
    }
    (output, error)
}

#[test]
fn the_surface_matches_the_reference() {
    // `qjs --std` says: setTimeout "function", setInterval "undefined",
    // clearInterval "undefined", setTimeout.length 2, clearTimeout.length 1.
    let (output, error) = run(r#"
        print(typeof os.setTimeout, typeof os.setInterval, typeof os.clearInterval);
        print(os.setTimeout.length, os.clearTimeout.length);
        "#);
    assert_eq!(error, None);
    assert_eq!(output, "function undefined undefined\n2 1\n");
}

#[test]
fn a_non_callable_is_rejected_when_scheduled() {
    // The reference throws from `js_os_setTimeout` before allocating a timer. Rejecting at
    // fire time instead would make the error appear later and somewhere unrelated.
    let (_, error) = run("os.setTimeout(42, 1);");
    assert!(
        error
            .as_deref()
            .is_some_and(|text| text.contains("not a function")),
        "the reference's message is 'not a function': {error:?}"
    );
}

#[test]
fn extra_arguments_are_not_passed_through() {
    // `setTimeout(f, 0, "x", "y")` passes arguments in browsers; the reference stores only
    // the function and calls it bare, so both parameters arrive as `undefined`.
    let (output, error) = run(r#"
        os.setTimeout(function (a, b) {
            print("args:" + String(a) + "," + String(b));
        }, 1, "x", "y");
        "#);
    assert_eq!(error, None);
    assert_eq!(output, "args:undefined,undefined\n");
}

#[test]
fn the_id_is_an_int32() {
    // `JS_NewInt32`, so `JSON.stringify` prints a bare integer rather than a float form.
    let (output, error) = run(r#"
        var id = os.setTimeout(function () {}, 1);
        print(typeof id, JSON.stringify(id), id >= 1);
        "#);
    assert_eq!(error, None);
    assert_eq!(output, "number 1 true\n");
}

#[test]
fn a_nan_delay_schedules_immediately() {
    // `JS_ToInt64(NaN)` is 0, so the timer is due at once rather than rejected or never.
    let (output, error) = run("os.setTimeout(function () { print(\"ran\"); }, NaN);");
    assert_eq!(error, None);
    assert_eq!(output, "ran\n");
}

#[test]
fn clear_timeout_ignores_junk_ids() {
    // `find_timer_by_id` returns NULL for `id <= 0`, so these cancel nothing even when a
    // real timer holds a low id.
    let (output, error) = run(r#"
        os.clearTimeout(0);
        os.clearTimeout(NaN);
        os.clearTimeout(-1);
        os.setTimeout(function () { print("survived"); }, 1);
        "#);
    assert_eq!(error, None);
    assert_eq!(output, "survived\n");
}

#[test]
fn timers_run_in_deadline_order() {
    let (output, error) = run(r#"
        os.setTimeout(function () { print("late"); }, 40);
        os.setTimeout(function () { print("early"); }, 1);
        os.setTimeout(function () { print("middle"); }, 20);
        "#);
    assert_eq!(error, None);
    assert_eq!(output, "early\nmiddle\nlate\n");
}

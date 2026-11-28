//! `std.evalScript`, and the shapes the reference's REPL depends on.
//!
//! The REPL is a JavaScript program (`repl.js`) rather than part of the C host, so getting
//! it to run meant matching four separate things exactly: the file name an eval reports,
//! the promise-returning `async` form, the `backtrace_barrier` truncation, and the fact that
//! `std.in.fileno()` is **0**.
//!
//! Every expected string here was produced by running the *same source* through
//! `qjs --std` on the reference and replacing its script path with `<input>`. The sources
//! are written without leading indent or newline on purpose: a trace line names
//! `line:column`, so indentation inside a raw string would change the expectation. That is
//! exactly how the first version of this file was wrong.

#![cfg(unix)]

use quickjs::{Context, Runtime};

fn run(source: &str) -> String {
    let runtime = Runtime::new();
    let mut ctx = Context::new(&runtime);
    let fds = quickjs_host::fdtable::FdTable::shared();
    let os = ctx.new_object();
    quickjs_host::timers::install(&mut ctx, &os).expect("timers");
    quickjs_host::os::install(&mut ctx, &os, &fds).expect("os");
    ctx.set_global("os", os).expect("mount os");
    let std = ctx.new_object();
    quickjs_host::std::install(&mut ctx, &std, &fds).expect("std");
    ctx.set_global("std", std).expect("mount std");
    ctx.eval(source).expect("script runs");
    while ctx.has_pending_work() {
        match ctx.run_one_turn().expect("a turn") {
            quickjs::Turn::Ran => {}
            quickjs::Turn::Wait(delay) | quickjs::Turn::Poll(delay) => std::thread::sleep(delay),
            quickjs::Turn::Done => break,
        }
    }
    ctx.output().to_string()
}

/// `std.in` is descriptor 0, not a duplicate of it.
///
/// This is not cosmetic: `repl.js` installs its read handler on
/// `os.setReadHandler(std.in.fileno(), …)`. When this host held a `dup`, `fileno()` answered
/// 3 and the REPL waited on a descriptor the terminal never writes to — it printed a prompt
/// and then read nothing, forever.
#[test]
fn the_standard_streams_report_their_real_descriptors() {
    let output = run(r#"print([std.in.fileno(), std.out.fileno(), std.err.fileno()].join(","));"#);
    assert_eq!(output, "0,1,2\n");
}

/// `std.evalScript` and `eval` report different file names, and both appear in a trace.
#[test]
fn eval_and_eval_script_are_named_separately() {
    let output = run(
        r#"try { std.evalScript("null.x"); } catch (e) { print(e.stack); }
print("--");
try { eval("null.x"); } catch (e) { print(e.stack); }"#,
    );
    // The native frame sits *between* the evaluated code and its caller. Emitting all
    // natives first — which was correct while they never re-entered JavaScript — put
    // `evalScript (native)` above the frame it belongs under.
    //
    // `print` adds its own newline, and a `stack` already ends with one, hence the blank
    // line before `--`.
    assert_eq!(
        output,
        "    at <eval> (<evalScript>:1:5)\n    \
         at evalScript (native)\n    \
         at <eval> (<input>:1:21)\n\
         \n--\n    \
         at <eval> (<input>:1:5)\n    \
         at <eval> (<input>:3:11)\n\
         \n"
    );
}

/// `{ async: true }` returns a promise that resolves to `{ value: completion }`.
///
/// The completion value is **not** awaited: `Promise.resolve(7)` resolves to
/// `{ value: <the promise> }`, and only an explicit top-level `await` gives `{ value: 7 }`.
#[test]
fn eval_script_async_wraps_the_completion_value() {
    let output = run(
        r#"std.evalScript("1+1", { async: true }).then(function (v) {
    print("sync", JSON.stringify(v));
});
std.evalScript("Promise.resolve(7)", { async: true }).then(function (v) {
    print("promise", JSON.stringify(v), typeof v.value.then);
});"#,
    );
    assert_eq!(
        output,
        "sync {\"value\":2}\npromise {\"value\":{}} function\n"
    );
}

/// A runtime failure **rejects** in the async form, while a syntax failure still throws
/// synchronously — the reference can tell them apart because one happens before a promise
/// exists and the other after.
#[test]
fn async_separates_syntax_errors_from_runtime_errors() {
    let output = run(r#"try {
    std.evalScript("var =", { async: true });
    print("syntax: no throw");
} catch (e) {
    print("syntax threw:", e.name);
}
std.evalScript("null.x", { async: true }).then(
    function () { print("runtime: resolved"); },
    function (e) { print("runtime rejected:", e.name); }
);"#);
    assert_eq!(
        output,
        "syntax threw: SyntaxError\nruntime rejected: TypeError\n"
    );
}

/// `backtrace_barrier` truncates the trace at this evaluation, so a REPL reports the line it
/// ran without also reporting its own readline loop.
#[test]
fn a_backtrace_barrier_hides_the_caller() {
    let output = run(r#"function outer() {
    try { std.evalScript("null.x", { backtrace_barrier: true }); }
    catch (e) { print(e.stack); }
}
outer();"#);
    assert_eq!(output, "    at <eval> (<evalScript>:1:5)\n\n");
}

/// An error the engine throws prints like one the constructor makes.
///
/// A `TypeError` from `null.x` used to print as `{ stack: … }` because the internal class
/// that tells a printer "this is an error" was only set on the constructor path.
#[test]
fn an_engine_thrown_error_prints_as_an_error() {
    // No trailing newline: `std.__printObject` writes the value and nothing else.
    assert_eq!(
        run(r#"try { null.x } catch (e) { std.__printObject(e); }"#),
        "TypeError: cannot read property 'x' of null\n    at <eval> (<input>:1:11)"
    );
}

/// `{async: true}` makes the body an `async` function, so it may `await` at its top level.
///
/// The script is *not* a function: `return` and `new.target` stay out of reach. What the
/// flag buys is exactly one thing, which is the same thing a module body gets — its
/// enclosing function is `async`.
#[test]
fn the_async_form_allows_top_level_await() {
    assert_eq!(
        run(r#"std.evalScript("await 1", { async: true })
    .then(r => print("value:" + r.value));"#),
        "value:1\n"
    );
}

/// The completion value is wrapped in `{value}` so a promise is not *adopted*.
///
/// Without the wrapper, resolving the body's promise with a promise completion would chain
/// to it: `evalScript("Promise.resolve(7)")` would answer `7`. The reference answers a
/// promise resolving to an object holding that promise, and only an explicit `await`
/// unwraps it — the same rule that makes the REPL print a promise rather than its value.
#[test]
fn a_promise_completion_is_wrapped_rather_than_adopted() {
    assert_eq!(
        run(r#"std.evalScript("Promise.resolve(7)", { async: true })
    .then(r => print("adopted:" + (r.value instanceof Promise)));
std.evalScript("await Promise.resolve(7)", { async: true })
    .then(r => print("awaited:" + r.value));"#),
        "adopted:true\nawaited:7\n"
    );
}

/// The body runs with the global receiver, and its `var`s are globals.
///
/// It is an async *function*, whose `this` would normally be `undefined` — but a global
/// eval scripts `this` to the global object, and that does not change with the flag.
///
/// `sees:41` on the third line is the ordering, not a mistake: the third body never
/// suspends, so its promise settles first, and it reads `gv` before the second body has
/// resumed past `await 0` to add one.
#[test]
fn the_async_body_keeps_the_global_receiver_and_scope() {
    assert_eq!(
        run(r#"std.evalScript("this === globalThis", { async: true })
    .then(r => print("this:" + r.value));
std.evalScript("var gv = 41; await 0; gv + 1", { async: true })
    .then(r => print("gv:" + r.value));
std.evalScript("gv", { async: true }).then(r => print("sees:" + r.value));"#),
        "this:true\nsees:41\ngv:42\n"
    );
}

/// A suspending `await` really does suspend: the timer fires before the body resumes, and
/// the ordering against the other queued jobs is the reference's, line for line.
#[test]
fn awaiting_inside_the_body_yields_to_the_event_loop() {
    assert_eq!(
        run(
            r#"std.evalScript("await new Promise(r => os.setTimeout(() => r('late'), 1))", { async: true })
    .then(r => print("timer:" + r.value));
std.evalScript("throw new Error('boom')", { async: true })
    .then(() => print("bad"), e => print("rejected:" + e.message));
std.evalScript("await Promise.reject(new Error('nope'))", { async: true })
    .then(() => print("bad"), e => print("rejected:" + e.message));
print("sync-after");"#
        ),
        "sync-after\nrejected:boom\nrejected:nope\ntimer:late\n"
    );
}

/// Without the flag, top-level `await` is an ordinary identifier and the parse fails — and
/// a *parse* failure throws rather than rejecting, because there is no promise yet.
#[test]
fn the_flag_is_what_makes_await_legal() {
    assert_eq!(
        run(r#"try { std.evalScript("await 1"); }
catch (e) { print("no-flag:" + e.message); }
try { std.evalScript("return 1", { async: true }); }
catch (e) { print("return:" + e.message); }
try { std.evalScript("new.target", { async: true }); }
catch (e) { print("new.target:" + e.message); }"#),
        "no-flag:expecting ';'\nreturn:return not in a function\n\
         new.target:new.target only allowed within functions\n"
    );
}

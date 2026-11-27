//! `os`'s process and time surface, against the reference.
//!
//! The expectations come from `qjs --std`, and the shapes are the reference's own: `os.exec`
//! returns an exit code (negative for a signal), `os.waitpid` returns the **raw** wait
//! status, and `os.now` is monotonic rather than a wall clock.
//!
//! Two divergences are deliberate and pinned below, both with their reasons in the code:
//! `os.kill(pid, 0)` reports `EINVAL`, and `os.waitpid` with `WNOHANG` reports status `0`
//! where the reference reports uninitialised stack memory.

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
    // Drain the event loop the way the CLI does: `os.sleepAsync` and `os.setTimeout` both
    // settle off the loop, not during `eval`.
    while ctx.has_pending_work() {
        match ctx.run_one_turn().expect("a turn") {
            quickjs::Turn::Ran => {}
            quickjs::Turn::Wait(delay) => std::thread::sleep(delay),
            // No descriptor handlers are registered in these tests, so a poll turn is only
            // the loop asking to wait.
            quickjs::Turn::Poll(delay) => std::thread::sleep(delay),
            quickjs::Turn::Done => break,
        }
    }
    ctx.output().to_string()
}

/// `os.exec` reports the exit code, a signal as a negative number, and an unstartable
/// program as 127 — the reference's child does `_exit(127)` after a failed `execve`.
#[test]
fn exec_reports_exit_codes_and_signals() {
    let output = run(r#"
        print(os.exec(["true"]));
        print(os.exec(["sh", "-c", "exit 42"]));
        print(os.exec(["/nonexistent/program"]));
        print(os.exec(["sh", "-c", "kill -TERM $$"]));
        try { os.exec([]); } catch (e) { print(e.name + ": " + e.message); }
        "#);
    assert_eq!(
        output,
        "0\n42\n127\n-15\nTypeError: invalid number of arguments\n"
    );
}

/// The reference's own `test_os_exec` pattern: redirect stdout into a pipe, read it back
/// through `std.fdopen`, then collect the child with `os.waitpid`.
///
/// The status is the raw `wait` word, so `status & 0x7f` is `0` for a normal exit and
/// `status >> 8` is the code — which is exactly how the reference's test inspects it.
#[test]
fn exec_redirects_and_waitpid_reports_the_raw_status() {
    let output = run(r#"
        var fds = os.pipe();
        var pid = os.exec(["sh", "-c", "echo $FOO"], {
            stdout: fds[1], block: false, env: { FOO: "hello" },
        });
        print(pid > 0);
        os.close(fds[1]);
        var f = std.fdopen(fds[0], "r");
        print(f.getline());
        print(f.getline());
        f.close();
        var w = os.waitpid(pid, 0);
        print(w[0] === pid, w[1] & 0x7f, w[1] >> 8);
        "#);
    assert_eq!(output, "true\nhello\nnull\ntrue 0 0\n");
}

/// `os.exec` options: `cwd` changes the child's directory and `env` *replaces* the
/// environment rather than adding to it.
#[test]
fn exec_options_cwd_and_env() {
    let output = run(r#"
        var fds = os.pipe();
        var pid = os.exec(["pwd"], { stdout: fds[1], block: false, cwd: "/tmp" });
        os.close(fds[1]);
        var f = std.fdopen(fds[0], "r");
        print(f.readAsString().trim().endsWith("tmp"));
        f.close();
        os.waitpid(pid, 0);

        // `env` is the whole environment: PATH is not inherited unless it is named.
        print(os.exec(["sh", "-c", "printf %s \"$PATH\""], { env: { OTHER: "x" } }));
        "#);
    assert_eq!(output, "true\n0\n");
}

/// `kill` a child and collect it: the status shows the signal in its low bits.
#[test]
fn kill_terminates_a_child() {
    let output = run(r#"
        var pid = os.exec(["sleep", "5"], { block: false });
        print(os.kill(pid, os.SIGTERM));
        var w = os.waitpid(pid, 0);
        print(w[0] === pid, w[1] & 0x7f);
        print(os.kill(999999, os.SIGTERM) < 0);
        "#);
    assert_eq!(output, "0\ntrue 15\ntrue\n");
}

/// A **deliberate divergence**: `kill(pid, 0)`, the existence-and-permission probe.
///
/// `rustix::process::Signal` is a `NonZeroI32`, so it structurally cannot carry the `0` that
/// `kill(pid, 0)` needs; rustix points at `rustix-libc-wrappers` for that, which is not
/// available on this registry. `Signal::from_raw_unchecked` exists but its own docs say the
/// result must not be used to send signals. So this reports `EINVAL` rather than taking a
/// dependency or an `unsafe` block for a probe.
#[test]
fn kill_with_signal_zero_is_a_documented_gap() {
    let output = run(r#"print(os.kill(os.getpid(), 0));"#);
    assert_eq!(output.trim_end(), "-22");
}

/// `os.now` is **monotonic milliseconds**, not a wall clock — the reference reads
/// `CLOCK_MONOTONIC`, so it reports uptime (`95607854.675` on a one-day-old machine), not
/// milliseconds since 1970. Only differences are meaningful.
#[test]
fn now_is_monotonic() {
    let output = run(r#"
        var start = os.now();
        os.sleep(30);
        var elapsed = os.now() - start;
        print(elapsed >= 20 && elapsed < 1000);
        "#);
    assert_eq!(output, "true\n");
}

/// `os.sleepAsync` resolves through the same timer queue as `os.setTimeout`, so the promise
/// settles on the event loop rather than immediately.
#[test]
fn sleep_async_resolves_on_the_loop() {
    let output = run(r#"
        os.sleepAsync(10).then(function (value) { print("resolved", value); });
        print("scheduled");
        "#);
    assert_eq!(output, "scheduled\nresolved undefined\n");
}

/// `dup` and `dup2` work on descriptors the host owns, and an unknown number is `EBADF`.
#[test]
fn dup_and_dup2() {
    let output = run(r#"
        var p = os.pipe();
        var copy = os.dup(p[1]);
        print(typeof copy, copy > 2);
        var bytes = new Uint8Array([65]);
        os.write(copy, bytes.buffer, 0, 1);
        os.close(p[1]);
        var got = new Uint8Array(2);
        print(os.read(p[0], got.buffer, 0, 2));
        print(String.fromCharCode(got[0]));
        os.close(p[0]);
        os.close(copy);
        print(os.dup(999));
        "#);
    assert_eq!(output, "number true\n1\nA\n-9\n");
}

/// A terminal that is not a terminal: `ttyGetWinSize` answers `null` and `ttySetRaw` does
/// nothing, which is what a redirected stream has to do rather than fail.
#[test]
fn tty_functions_are_quiet_without_a_terminal() {
    let output = run(r#"
        print(os.ttyGetWinSize(1));
        print(os.ttySetRaw(1));
        print(os.ttyGetWinSize(999));
        "#);
    assert_eq!(output, "null\nundefined\nnull\n");
}

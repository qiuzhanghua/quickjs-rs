//! `std.setenv` / `std.unsetenv`, against the reference.
//!
//! This is the one module in the workspace with `unsafe` outside the engine (NFR-6). The
//! argument for it is in `quickjs_host::env`; what these tests pin down is the *behaviour*,
//! which has two easy-to-miss properties:
//!
//! * The variable really lands in the process environment, so a child process spawned by
//!   `popen` inherits it. A host-side overlay map would not, which is why the overlay was
//!   rejected.
//! * An unusable name is silently ignored rather than aborting the host. `setenv(3)` returns
//!   `EINVAL` and the reference ignores it; `std::env::set_var` would **panic**, which a
//!   script must not be able to trigger.

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
    ctx.output().to_string()
}

/// Set, read back, overwrite, and unset — and `getenviron` agrees at every step.
#[test]
fn set_read_overwrite_unset() {
    let output = run(r#"
        var N = "QJS_TEST_ENV_ROUNDTRIP";
        print(std.getenv(N));
        print(std.setenv(N, "v1"));
        print(std.getenv(N));
        std.setenv(N, "v2");
        print(std.getenv(N), std.getenviron()[N]);
        print(std.unsetenv(N));
        print(std.getenv(N));
        print(std.getenviron().hasOwnProperty(N));
        "#);
    assert_eq!(
        output,
        "undefined\nundefined\nv1\nv2 v2\nundefined\nundefined\nfalse\n"
    );
}

/// The variable reaches a child process, which is the property a host-side overlay would
/// lose. `popen` runs `sh -c` directly, so the child sees only what is really in the
/// environment.
#[test]
fn a_child_process_inherits_the_change() {
    let output = run(r#"
        var N = "QJS_TEST_ENV_CHILD";
        std.setenv(N, "inherited");
        var f = std.popen("sh -c 'printf %s \"$" + N + "\"'", "r");
        print(f.readAsString());
        f.close();
        std.unsetenv(N);
        var g = std.popen("sh -c 'printf %s \"$" + N + "\"'", "r");
        print(JSON.stringify(g.readAsString()));
        g.close();
        "#);
    assert_eq!(output, "inherited\n\"\"\n");
}

/// Names `setenv(3)` rejects are ignored, not fatal. Without the check in `env.rs`, the
/// underlying `std::env::set_var` would panic and take the host down mid-script.
#[test]
fn unusable_names_are_ignored() {
    let output = run(r#"
        std.setenv("BAD=NAME", "x");
        std.setenv("", "x");
        std.unsetenv("BAD=NAME");
        std.unsetenv("");
        // Still running, and the environment is untouched.
        print("survived", std.getenv("BAD=NAME"));
        "#);
    assert_eq!(output, "survived undefined\n");
}

/// The arguments go through `ToString`, so a non-string value is coerced and a missing one
/// becomes `"undefined"` — the reference does the same `JS_ToCString` on both.
#[test]
fn arguments_are_coerced() {
    let output = run(r#"
        var N = "QJS_TEST_ENV_COERCE";
        std.setenv(N, 42);
        print(std.getenv(N));
        std.setenv(N);
        print(std.getenv(N));
        std.unsetenv(N);
        print(std.getenv(N));
        "#);
    assert_eq!(output, "42\nundefined\nundefined\n");
}

/// An empty value is a set variable with no content, not an unset one.
#[test]
fn an_empty_value_is_not_unset() {
    let output = run(r#"
        var N = "QJS_TEST_ENV_EMPTY";
        std.setenv(N, "");
        var value = std.getenv(N);
        print(typeof value, value.length, std.getenviron().hasOwnProperty(N));
        std.unsetenv(N);
        "#);
    assert_eq!(output, "string 0 true\n");
}

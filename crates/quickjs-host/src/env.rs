//! The process environment: the one place in this workspace that needs `unsafe`.
//!
//! # Why `unsafe`, and why here
//!
//! `std.setenv`/`std.unsetenv` exist in the reference (`js_std_setenv`, `js_std_unsetenv`)
//! and call `setenv(3)`/`unsetenv(3)`. In Rust those are `std::env::set_var`/
//! `remove_var`, which edition 2024 marks `unsafe` for a real reason: modifying the
//! environment is not thread-safe, because `getenv` may hand back a pointer into storage
//! that `setenv` is about to reallocate.
//!
//! NFR-6 confines `unsafe` to the four engine modules. This module is the documented
//! exception, and it is one module with two calls so the exception stays auditable: a
//! reviewer reads these lines instead of hunting for `unsafe` across the host layer.
//!
//! # Why there is no safe version
//!
//! `rustix` deliberately does not wrap the environment — it is the one piece of POSIX
//! process state Rust's standard library already owns — so there is no crate to reach for.
//!
//! The alternative would be a host-side overlay map merged into `getenv`, `getenviron` and
//! every child's environment. That avoids `unsafe` but turns one call into an invariant
//! four call sites have to agree on, and it still stops being `setenv`: a child would be
//! told about the change explicitly rather than inheriting it. One `unsafe` with one honest
//! argument is the smaller risk.
//!
//! # Why it is sound here
//!
//! `set_var` is unsound only if another thread is inside `getenv`/`setenv` at the same
//! moment. Two facts rule that out:
//!
//! * The interpreter is single-threaded. Everything that reaches this module runs on the
//!   thread that called into `Context::eval`, and this engine never runs JavaScript on
//!   another thread.
//! * `quickjs-host` declares `tokio` for descriptor readiness but starts no runtime and
//!   spawns no worker thread, so there is no second thread to race with. If a future change
//!   starts one, this module is where the argument has to be revisited — which is the point
//!   of collecting it here.
//!
//! # The panic these wrappers exist to prevent
//!
//! `set_var` and `remove_var` **panic** — they do not return an error — when the name is
//! empty, contains `=`, or contains a NUL byte. A script can produce all three, so calling
//! them directly would let `std.setenv("A=B", "x")` abort the host.
//!
//! The reference does not panic: `setenv(3)` returns `-1` with `EINVAL` for exactly these
//! names and `js_std_setenv` ignores the return value. So the names are rejected up front
//! and the call is skipped, which is the same observable "silently did nothing".

/// Whether `setenv`/`unsetenv` would reject `name` with `EINVAL` — or, in Rust, panic.
///
/// POSIX rejects an empty name, one containing `=`, and (because the name has to become a
/// C string) one containing a NUL byte.
fn rejected(name: &str) -> bool {
    name.is_empty() || name.contains('=') || name.contains('\0')
}

/// Set `name` to `value`, replacing any current value (`setenv(3)`, `overwrite = 1`).
///
/// Returns nothing on purpose: the reference ignores `setenv`'s return value, so
/// `std.setenv("bad=name", "x")` is silently a no-op rather than an error.
pub fn set(name: &str, value: &str) {
    if rejected(name) {
        return;
    }
    // SAFETY: the interpreter is single-threaded and no other thread in this process reads
    // or writes the environment. `name` is known not to be empty, to contain `=`, or to
    // contain a NUL byte — the three inputs that make `set_var` panic rather than fail.
    // See the module comment for the rest of the argument.
    unsafe { std::env::set_var(name, value) };
}

/// Remove `name` from the environment (`unsetenv(3)`).
///
/// Also silent on rejection, for the same reason as [`set`].
pub fn unset(name: &str) {
    if rejected(name) {
        return;
    }
    // SAFETY: identical to `set` — one thread, no concurrent `getenv`/`setenv`, and the
    // name is one `remove_var` will not panic on.
    unsafe { std::env::remove_var(name) };
}

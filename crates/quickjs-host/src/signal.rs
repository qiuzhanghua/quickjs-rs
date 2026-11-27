//! `os.signal`: installing signal handlers. The second documented `unsafe` exception.
//!
//! # Why `unsafe`, and why this shape
//!
//! Installing a signal handler is FFI to a callback the operating system invokes later, out
//! of band: there is no safe wrapper because the contract is inherently unsafe. A handler
//! runs at an arbitrary point in the interrupted code and may only call
//! **async-signal-safe** functions — no allocation, no locks, no JavaScript.
//!
//! So the handler here does the one thing that is safe: it sets a bit in an `AtomicU64` and
//! returns. The event loop reads the bits between turns and calls the JavaScript handler from
//! there. That is exactly the reference's design — `os_signal_handler` in `quickjs-libc.c`
//! only sets `sh->ishandling = TRUE`, and `os_poll` notices.
//!
//! # Why `libc` and not `rustix`
//!
//! `rustix` 1.1.6 keeps its `runtime` module `pub(crate)`, so `kernel_sigaction` — which is
//! `pub unsafe fn` inside it — is not reachable from a dependent crate. `libc` is already a
//! declared dependency of this crate (it was carried for the tokio/`AsyncFd` plan that never
//! materialised, so until now nothing used it); this module is its first real use. `signal(3)`
//! is also literally what the reference calls, so the disposition semantics match without
//! translation.
//!
//! # Scope of the `unsafe`
//!
//! Two items: the `extern "C"` handler and the `libc::signal` call that installs it.
//! [`mark_pending`] is the only code that ever runs in signal context, and it touches nothing
//! but an `AtomicU64` with `Relaxed` ordering.
//!
//! # Why the exception is worth it
//!
//! `os.signal` is not decoration: the reference's REPL installs a `SIGINT` handler on startup
//! so Ctrl-C aborts the current line instead of killing the process. Without it `repl.js`
//! throws at its first call, so the REPL cannot run at all.
//!
//! Like [`crate::env`], everything is confined to one module so a reviewer reads one file.

use std::sync::atomic::{AtomicU64, Ordering};

/// Which signals have arrived since the loop last looked.
///
/// Bit `n` means signal `n`. A `u64` covers every number `os.signal` accepts — the reference
/// rejects `sig_num >= 64`.
static PENDING: AtomicU64 = AtomicU64::new(0);

/// The handler the kernel calls. Runs in signal context: atomics only.
extern "C" fn mark_pending(signal: i32) {
    // `Relaxed` is all this needs: the bit is a one-way flag and the reader swaps the whole
    // word. A stronger order would cost an instruction in the one place where the cost is
    // least affordable.
    if let Ok(bit) = u32::try_from(signal) {
        if bit < 64 {
            PENDING.fetch_or(1 << bit, Ordering::Relaxed);
        }
    }
}

/// Take the signals that arrived, clearing them.
///
/// Called from the event loop, never from signal context.
pub fn take_pending() -> Vec<i32> {
    let bits = PENDING.swap(0, Ordering::Relaxed);
    (0..64).filter(|bit| bits & (1 << bit) != 0).collect()
}

/// Whether any signal is waiting, without consuming it.
pub fn has_pending() -> bool {
    PENDING.load(Ordering::Relaxed) != 0
}

/// Install [`mark_pending`] for `signal` (`os.signal(sig, func)`).
pub fn install(signal: i32) -> Result<(), i32> {
    if rustix::process::Signal::from_named_raw(signal).is_none() {
        return Err(22); // EINVAL
    }
    // SAFETY: `mark_pending` has the `extern "C" fn(c_int)` shape the kernel calls, and it
    // only touches an `AtomicU64`. Registering it is what makes this `unsafe`: the kernel
    // will invoke that pointer later, out of band, which no safe API can express.
    unsafe {
        // Two steps on purpose: clippy rejects casting a function item straight to an
        // integer, and the intermediate `extern "C" fn` pointer is also the shape the
        // kernel's handler slot actually wants.
        let handler: extern "C" fn(i32) = mark_pending;
        libc::signal(signal, handler as libc::sighandler_t);
    }
    Ok(())
}

/// Restore the default disposition (`os.signal(sig, null)`).
pub fn reset(signal: i32) -> Result<(), i32> {
    if rustix::process::Signal::from_named_raw(signal).is_none() {
        return Err(22);
    }
    // SAFETY: `SIG_DFL` is a constant the kernel understands. No Rust code stays registered,
    // so nothing can later be called that this module does not expect.
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
    }
    Ok(())
}

/// Ignore the signal (`os.signal(sig, undefined)`).
pub fn ignore(signal: i32) -> Result<(), i32> {
    if rustix::process::Signal::from_named_raw(signal).is_none() {
        return Err(22);
    }
    // SAFETY: as with `reset` — `SIG_IGN` registers no Rust code.
    unsafe {
        libc::signal(signal, libc::SIG_IGN);
    }
    Ok(())
}

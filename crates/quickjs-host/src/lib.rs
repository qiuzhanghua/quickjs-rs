//! Host modules and the event loop for the pure-Rust QuickJS port.
//!
//! This crate is the moral equivalent of `quickjs-libc.c`: it sits *beside* the engine, not
//! inside it. `quickjs-pure` has an empty dependency closure on purpose (NFR-7, checked by
//! `scripts/check-no-engine-deps.sh`), because an embedder wants an interpreter and not an
//! async runtime. Everything platform-shaped lives here instead.
//!
//! # Why tokio, and where it stops
//!
//! The reference implementation's `js_std_loop` is a `poll(2)` over the registered file
//! descriptors plus the nearest timer. tokio buys the same thing in a portable-ish way:
//!
//! * **Unix** — `tokio::io::unix::AsyncFd` multiplexes arbitrary descriptors via epoll or
//!   kqueue. This is what `os.setReadHandler`/`setWriteHandler` need.
//! * **Windows** — tokio does *not* help here, and it is worth being precise about why,
//!   because it is easy to assume otherwise. `AsyncFd` is `cfg(unix)` only, and
//!   `tokio::net::windows::named_pipe` covers *named* pipes while `os.pipe()` makes
//!   anonymous ones. So the Windows path uses `windows-sys` and `WaitForMultipleObjects`,
//!   and only for `stdin` — which is exactly what the reference does: its `js_os_poll`
//!   Windows branch puts `fd == 0` in the handle array and nothing else, and its `Makefile`
//!   excludes `tests/test_rw_handler.js` from Windows builds altogether.
//!
//! So the target is parity with upstream, not parity-plus.

pub mod env;
pub mod eventloop;
pub mod fds;
pub mod fdtable;
// `os` is `cfg(unix)` while it is built on `rustix`: NFR-8 also wants Windows, and the
// reference's Windows `js_os_*` functions are a different implementation, not a shared
// one. Rather than half-support Windows with a module that fails at runtime, the module is
// absent there and the CLI reports `--std` as unsupported on that platform until the
// `windows-sys` side exists.
#[cfg(unix)]
pub mod os;
pub mod print;
pub mod printf;
pub mod signal;
pub mod std;
pub mod stdio;
pub mod timers;

pub use eventloop::{RunError, run_repl, run_until_idle};

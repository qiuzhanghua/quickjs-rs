//! File-descriptor readiness, as far as each platform allows.
//!
//! `os.setReadHandler`/`os.setWriteHandler` are the reason this module exists. They are
//! also the one place where matching the reference implementation means *not* matching it
//! on every platform, so the split is stated here rather than discovered later.
//!
//! # Unix
//!
//! `AsyncFd` multiplexes arbitrary descriptors through epoll/kqueue. One caveat shapes the
//! API: `AsyncFd` requires the descriptor to already be non-blocking, and `std::fs::File`
//! has no `set_nonblocking`. Setting the flag therefore needs `fcntl`, which lives in
//! `libc` — this is the *only* reason `libc` is a dependency of the host layer, and the
//! reason the engine layer cannot own this code at all.
//!
//! # Windows
//!
//! There is no `poll(2)`. `WaitForMultipleObjects` waits on handles, which is what the
//! reference uses — but only ever for `stdin`:
//!
//! ```c
//! if (rh->fd == 0 && !JS_IsNull(rh->rw_func[0])) {
//!     handles[count++] = (HANDLE)_get_osfhandle(rh->fd);   /* stdin */
//! }
//! ```
//!
//! An `os.pipe()` descriptor is simply not in that list, which is why upstream's `Makefile`
//! excludes `tests/test_rw_handler.js` from Windows builds. Matching that is the goal here;
//! going past it would need a Windows design the reference never had.

/// Whether this build can register handlers for descriptors the script created itself
/// (`os.pipe()` and friends).
///
/// `false` on Windows, matching the reference: its wait list only ever holds stdin. Callers
/// check this so a script gets "not supported" instead of a handler that silently never
/// fires — the failure mode this project treats as worse than an honest error.
pub const ARBITRARY_FDS: bool = cfg!(unix);

/// Whether handlers on `stdin` can be registered.
///
/// True everywhere: Unix includes fd 0 in its `poll` set, and Windows puts it in the
/// `WaitForMultipleObjects` array explicitly.
pub const STDIN: bool = true;

#[cfg(test)]
mod tests {
    use super::*;

    /// The platform contract, pinned so a `cfg` edit cannot change it silently.
    ///
    /// `#[allow]` because `clippy` is right that these are compile-time constants — that is
    /// the point. The assertion is here so that changing `ARBITRARY_FDS`'s `cfg!` makes a
    /// test fail and a reviewer look, rather than quietly promising a handler that never
    /// fires on Windows.
    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn the_platform_contract_holds() {
        if cfg!(windows) {
            assert!(
                !ARBITRARY_FDS,
                "the reference never waited on arbitrary descriptors on Windows, and                  tokio's AsyncFd does not exist there; claiming otherwise would be a                  silently-never-firing handler"
            );
        } else {
            assert!(ARBITRARY_FDS);
        }
        assert!(STDIN);
    }
}

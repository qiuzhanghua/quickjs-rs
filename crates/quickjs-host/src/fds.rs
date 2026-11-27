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

/// One descriptor to watch, and the directions a handler is registered for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watched {
    /// The descriptor.
    pub fd: i32,
    /// Whether `os.setReadHandler` registered a callback.
    pub read: bool,
    /// Whether `os.setWriteHandler` registered a callback.
    pub write: bool,
}

/// What `poll(2)` reported for one entry of a [`Watched`] list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Ready {
    /// Readable, or in an error/hangup state — the reference treats all of them as readable
    /// (`POLLERR | POLLHUP | POLLNVAL | POLLIN`), because a hangup is exactly when a
    /// blocking read returns 0 and the handler needs to see it.
    pub read: bool,
    /// Writable.
    pub write: bool,
}

/// Poll `watched` for at most `timeout`, returning one [`Ready`] per entry.
///
/// `table` is the source of truth for which descriptors exist: `poll(2)` needs a borrowed
/// descriptor per entry, and the table is what holds them (see [`FdTable::with_all`]). A
/// number it does not own comes back with neither flag set rather than failing the whole
/// call — one bad registration must not stop the others from firing.
#[cfg(unix)]
pub fn poll(
    table: &crate::fdtable::FdTable,
    watched: &[Watched],
    timeout: std::time::Duration,
) -> Vec<Ready> {
    use rustix::event::{PollFd, PollFlags};

    let numbers: Vec<i32> = watched.iter().map(|entry| entry.fd).collect();
    // `None` stands for a descriptor this host does not own; it is skipped from the poll
    // and reported as not ready.
    table.with_all(&numbers, |borrowed| {
        let mut poll_fds: Vec<PollFd<'_>> = Vec::with_capacity(watched.len());
        let mut slots: Vec<Option<usize>> = Vec::with_capacity(watched.len());
        for (entry, handle) in watched.iter().zip(borrowed) {
            match handle {
                Some(handle) => {
                    let mut flags = PollFlags::empty();
                    if entry.read {
                        flags |= PollFlags::IN;
                    }
                    if entry.write {
                        flags |= PollFlags::OUT;
                    }
                    slots.push(Some(poll_fds.len()));
                    poll_fds.push(PollFd::new(handle, flags));
                }
                None => slots.push(None),
            }
        }
        let mut ready = vec![Ready::default(); watched.len()];
        if poll_fds.is_empty() {
            // Nothing watchable: a timed sleep, which is what `poll(2)` with an empty set
            // does too.
            std::thread::sleep(timeout);
            return ready;
        }
        let seconds = timeout.as_secs();
        let nanos = timeout.subsec_nanos();
        let deadline = rustix::event::Timespec {
            tv_sec: seconds as i64,
            tv_nsec: nanos as i64,
        };
        if rustix::event::poll(&mut poll_fds, Some(&deadline)).is_err() {
            return ready;
        }
        for (index, slot) in slots.iter().enumerate() {
            let Some(position) = *slot else { continue };
            let events = poll_fds[position].revents();
            // `POLLERR`/`POLLHUP`/`POLLNVAL` count as readable, as the reference decides:
            // a hangup is exactly when a blocking read returns 0 and the handler has to
            // see it.
            ready[index] = Ready {
                read: events
                    .intersects(PollFlags::IN | PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL),
                write: events.intersects(PollFlags::OUT),
            };
        }
        ready
    })
}

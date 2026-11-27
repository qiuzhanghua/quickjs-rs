//! Descriptor ownership for the `os` module.
//!
//! # Why a table
//!
//! A script asks for `os.open(...)` and gets back a number. Turning that number back into a
//! descriptor needs `BorrowedFd::borrow_raw`, which is `unsafe` — and NFR-6 confines
//! `unsafe` to four engine modules. Rather than break that, the host keeps the `OwnedFd`
//! and hands out only the number:
//!
//! * `open`/`pipe` create an `OwnedFd` and record it here.
//! * `read`/`write`/`seek` look it up and pass `OwnedFd::as_fd()`, which is safe.
//! * `close` removes it, and dropping the `OwnedFd` closes the descriptor.
//!
//! A number not in the table and not one of 0/1/2 names a descriptor this process does not
//! own, and every operation on it reports `EBADF` — which is what the reference does, and
//! without ever asserting an invariant that is not true.
//!
//! # 0, 1 and 2
//!
//! `stdin`/`stdout`/`stderr` belong to the process, not to the script, so they are never in
//! the table. They are reached through `std::io::stdin()` and friends, whose `AsFd` impls
//! are safe — this is the second reason no `unsafe` is needed here.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

/// The descriptors the script owns, keyed by the number it was given.
#[derive(Default)]
pub struct FdTable {
    inner: RefCell<BTreeMap<i32, OwnedFd>>,
}

impl FdTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// A table ready to share between `os` and `std`.
    pub fn shared() -> Shared {
        Rc::new(Self::default())
    }

    /// Take ownership of `fd` and return the number the script should see.
    ///
    /// The number is the descriptor's own, so `os.read(0, …)` and `os.write(1, …)` keep
    /// working against the real standard streams alongside these.
    pub fn adopt(&self, fd: OwnedFd) -> i32 {
        let number = fd.as_raw_number();
        self.inner.borrow_mut().insert(number, fd);
        number
    }

    /// Close a descriptor, reporting the errno when it is not one this host owns.
    ///
    /// `EBADF` (9) for anything unknown, including 0/1/2: the standard streams belong to
    /// the process, and letting a script close them would take the host's own output with
    /// it.
    pub fn close(&self, fd: i32) -> i32 {
        match self.inner.borrow_mut().remove(&fd) {
            // Dropping the `OwnedFd` is what closes it.
            Some(_) => 0,
            None => 9,
        }
    }

    /// Run `f` with a borrowed descriptor.
    ///
    /// Returns `None` when the number is not one this host owns, which the caller turns into
    /// `EBADF`. For 0/1/2 the borrow is of the std handle, which lives as long as the
    /// process, so it outlives `f`.
    pub fn with<T>(&self, fd: i32, f: impl FnOnce(BorrowedFd<'_>) -> T) -> Option<T> {
        if let Some(owned) = self.inner.borrow().get(&fd) {
            return Some(f(owned.as_fd()));
        }
        // The standard streams, borrowed from their std handles rather than from a number.
        // A local keeps the handle alive for the duration of the call.
        match fd {
            0 => {
                let stdin = std::io::stdin();
                Some(f(stdin.as_fd()))
            }
            1 => {
                let stdout = std::io::stdout();
                Some(f(stdout.as_fd()))
            }
            2 => {
                let stderr = std::io::stderr();
                Some(f(stderr.as_fd()))
            }
            _ => None,
        }
    }

    /// Take a descriptor out of the table, transferring ownership to the caller.
    ///
    /// `std.fdopen(fd, mode)` is the caller: `fdopen(3)` hands the descriptor to the
    /// `FILE`, which owns it from then on and closes it at `fclose`. Keeping the table
    /// entry as well would close the same number twice.
    pub fn take(&self, fd: i32) -> Option<OwnedFd> {
        self.inner.borrow_mut().remove(&fd)
    }

    /// Replace the descriptor behind `fd` with `fd_new`, closing the old one.
    ///
    /// This is `dup2(3)`: the target is closed first, which is what dropping the stored
    /// `OwnedFd` does. Only a number this table owns can be replaced — the standard streams
    /// belong to the process, so a script redirecting fd 1 is refused rather than allowed
    /// to take the host's own output with it.
    pub fn replace(&self, fd: i32, replacement: OwnedFd) -> Result<(), i32> {
        let mut table = self.inner.borrow_mut();
        match table.get_mut(&fd) {
            Some(slot) => {
                *slot = replacement;
                Ok(())
            }
            None => Err(9), // EBADF
        }
    }

    /// Whether `fd` is one this host can operate on.
    pub fn contains(&self, fd: i32) -> bool {
        self.inner.borrow().contains_key(&fd) || matches!(fd, 0..=2)
    }
}

/// A table shared by `os` and `std`.
///
/// The two modules have to agree, because a script does `var p = os.pipe();
/// std.fdopen(p[0], "r")` — the reference's own `test_os_exec()` does exactly that. One
/// table also means one answer to "does the script own this descriptor".
pub type Shared = Rc<FdTable>;

/// The descriptor number of an `OwnedFd` without giving up ownership.
trait AsRawNumber {
    fn as_raw_number(&self) -> i32;
}

impl AsRawNumber for OwnedFd {
    fn as_raw_number(&self) -> i32 {
        self.as_fd().as_raw_fd()
    }
}

/// Give up ownership so a descriptor can be handed to a `Command`'s stdio.
///
/// Used by `os.exec`, which needs to pass a descriptor to a child. The table entry is
/// removed first, so the same number is never owned twice.
pub fn release(table: &FdTable, fd: i32) -> Option<OwnedFd> {
    table.inner.borrow_mut().remove(&fd)
}

/// The number of descriptors the script currently owns. For tests and diagnostics.
pub fn owned_count(table: &FdTable) -> usize {
    table.inner.borrow().len()
}

/// A fresh `OwnedFd` from a number, for callers that just took ownership.
pub fn into_raw(fd: OwnedFd) -> i32 {
    fd.into_raw_fd()
}

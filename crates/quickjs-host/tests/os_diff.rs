//! `os` against the reference, function by function.
//!
//! Every expectation was produced by running the same call through `qjs --std`, not by
//! reasoning about what an `os` module "should" do. That mattered: the first version of
//! this module used one consistent error convention and differed from the reference in
//! **13 of 31** cases. The two conventions it actually has are documented in
//! `quickjs_host::os`, and the cases below are the ones that pin them down.

#![cfg(unix)]

use quickjs::{Context, Runtime};

/// Run `source` with `std`/`os` installed as globals, returning what it printed.
fn run(source: &str) -> String {
    let runtime = Runtime::new();
    let mut ctx = Context::new(&runtime);
    let os = ctx.new_object();
    quickjs_host::timers::install(&mut ctx, &os).expect("timers");
    quickjs_host::os::install(&mut ctx, &os).expect("os");
    ctx.set_global("os", os).expect("mount os");
    let std = ctx.new_object();
    quickjs_host::std::install(&mut ctx, &std).expect("std");
    ctx.set_global("std", std).expect("mount std");
    ctx.eval(source).expect("script runs");
    ctx.output().to_string()
}

/// The bare convention reports failure as a **negative** errno.
///
/// `js_get_errno` in `quickjs-libc.c` is `if (ret == -1) ret = -errno;`, so `EEXIST` (17)
/// arrives as `-17`. Reporting `17` would look tidier and be wrong, and the reference's own
/// `tests/test_std.js` cannot catch it because it only ever calls these on success.
#[test]
fn bare_functions_report_negative_errno() {
    let output = run(r#"
        print(os.mkdir("/definitely/not/creatable", 0o755));
        print(os.rename("/nope-a", "/nope-b"));
        print(os.chdir("/nope"));
        print(os.remove("/nope"));
        "#);
    // -2 ENOENT, -20 ENOTDIR, -17 EEXIST depending on which check fails first; the *sign*
    // is what this test is about, so each line is checked for being negative.
    for line in output.lines() {
        let value: i32 = line.parse().expect("a bare integer");
        assert!(
            value < 0,
            "the bare convention reports a negative errno, got {value}"
        );
    }
    assert_eq!(output.lines().count(), 4);
}

/// The pair convention reports failure as a **positive** errno, inside a pair.
#[test]
fn pair_functions_report_positive_errno() {
    let output = run(r#"
        print(JSON.stringify(os.stat("/definitely/not/here")));
        print(JSON.stringify(os.getcwd())[0] === "[" ? "pair" : "not a pair");
        "#);
    assert_eq!(output, "[null,2]\npair\n");
}

/// `os.open` returns the descriptor itself, not a pair — and `os.close` returns a bare
/// errno, so closing the same descriptor twice reports EBADF.
#[test]
fn open_and_close_are_bare() {
    let output = run(r#"
        var fd = os.open("/tmp", os.O_RDONLY);
        print(typeof fd, fd >= 0);
        print(os.close(fd));
        print(os.close(fd));
        "#);
    assert_eq!(output, "number true\n0\n-9\n");
}

/// `os.remove` is `remove(3)`: a file *or* a directory.
///
/// `unlink` alone would report `EPERM` for the directory. The reference calls `remove`,
/// which falls back to `rmdir`.
#[test]
fn remove_handles_files_and_directories() {
    let output = run(r#"
        var dir = "/tmp/qjs-os-remove-" + os.getpid();
        print(os.mkdir(dir, 0o755));
        var fd = os.open(dir + "/f", os.O_WRONLY | os.O_CREAT, 0o644);
        os.close(fd);
        print(os.remove(dir + "/f"));
        print(os.remove(dir));
        print(os.remove(dir));
        "#);
    assert_eq!(output, "0\n0\n0\n-2\n");
}

/// `os.isatty` returns a bare Boolean and `os.pipe` a bare pair; neither follows the
/// errno conventions, which is the reference's inconsistency rather than an oversight here.
#[test]
fn isatty_and_pipe_are_their_own_shape() {
    let output = run(r#"
        print(typeof os.isatty(1), os.isatty(999));
        var p = os.pipe();
        print(Array.isArray(p), p.length);
        var b = new Uint8Array([65, 66]);
        print(os.write(p[1], b.buffer, 0, 2));
        var got = new Uint8Array(4);
        print(JSON.stringify(os.read(p[0], got.buffer, 0, 4)));
        print(String.fromCharCode(got[0], got[1]));
        os.close(p[0]);
        os.close(p[1]);
        "#);
    assert_eq!(output, "boolean false\ntrue 2\n2\n2\nAB\n");
}

/// The constants `os` exposes have to be the platform's own values, because scripts do
/// arithmetic with them (`mode & os.S_IFMT`) rather than comparing them by name.
#[test]
fn constants_come_from_the_platform() {
    let output = run(r#"
        print(os.S_IFMT, os.S_IFDIR, os.S_IFREG, os.S_IFLNK);
        print(os.O_RDONLY, os.O_WRONLY, os.O_RDWR, os.O_CREAT, os.O_TRUNC, os.O_APPEND);
        print(os.SEEK_SET, os.SEEK_CUR, os.SEEK_END);
        print(typeof os.platform, os.platform.length > 0);
        "#);
    // Values verified against `qjs --std` on macOS and Linux; the mask and the seek
    // constants are POSIX and identical on both.
    let markers: Vec<&str> = output.lines().collect();
    assert_eq!(markers[0], "61440 16384 32768 40960");
    assert_eq!(markers[2], "0 1 2");
    assert_eq!(markers[3], "string true");
    // The `O_*` values differ between platforms, so only their distinctness is asserted.
    let flags: Vec<i64> = markers[1]
        .split(' ')
        .map(|part| part.parse().expect("a number"))
        .collect();
    assert_eq!(flags.len(), 6);
    assert_ne!(
        flags[0], flags[1],
        "O_RDONLY and O_WRONLY are different flags"
    );
    assert_ne!(
        flags[1], flags[2],
        "O_WRONLY and O_RDWR are different flags"
    );
}

/// A descriptor the host does not own reports EBADF instead of guessing.
///
/// The reference would happily `read(999)`, get EBADF from the kernel and report `-9`; this
/// host keeps its own fd table, so the answer is the same without having to construct a
/// `BorrowedFd` from an untrusted number.
#[test]
fn an_unknown_descriptor_is_badf() {
    let output = run(r#"
        var b = new Uint8Array(4);
        print(os.read(999, b.buffer, 0, 4));
        print(os.write(999, b.buffer, 0, 4));
        print(os.seek(999, 0, os.SEEK_SET));
        print(os.isatty(999));
        "#);
    assert_eq!(output, "-9\n-9\n-9\nfalse\n");
}

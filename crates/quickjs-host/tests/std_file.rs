//! `std`'s file object against the reference.
//!
//! The expectations are the reference's own suite (`tests/test_std.js`: `test_file1`,
//! `test_file2`, `test_getline`, `test_popen`) plus the observable shape of a `FILE`, all
//! checked against `qjs --std`.

#![cfg(unix)]

use quickjs::{Context, Runtime};

/// Run `source` with `std`/`os` mounted, returning what it printed.
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

/// `test_file1` and `test_file2` from the reference: write, seek to the end, `tell` gives
/// the size, seek back, read the same bytes out — then the same through `putByte`/`getByte`.
///
/// This is the test that rules out wrapping `BufReader`: `tell()` has to agree with what has
/// been written *and* what has been read back, which means the position is the stream's, not
/// the descriptor's.
#[test]
fn buffer_position_end_and_back() {
    let output = run(r#"
        var f = std.tmpfile(), str = "hello world\n", size, buf, ret;
        f.puts(str);
        f.seek(0, std.SEEK_SET);
        print(f.readAsString() === str);
        f.seek(0, std.SEEK_END);
        size = f.tell();
        f.seek(0, std.SEEK_SET);
        buf = new Uint8Array(size);
        ret = f.read(buf.buffer, 0, size);
        var same = true;
        for (var i = 0; i < size; i++) if (buf[i] !== str.charCodeAt(i)) same = false;
        f.close();
        print(size, ret, same);

        var g = std.tmpfile();
        for (var i = 0; i < str.length; i++) g.putByte(str.charCodeAt(i));
        g.seek(0, std.SEEK_SET);
        var ok = true;
        for (var i = 0; i < str.length; i++) if (g.getByte() !== str.charCodeAt(i)) ok = false;
        print(ok, g.getByte());
        g.close();
        "#);
    assert_eq!(output, "true\n12 12 true\ntrue -1\n");
}

/// `test_getline`: lines come back without their `\n`, and `eof()` is false before the end
/// and true after the last `null`.
#[test]
fn getline_strips_only_the_newline() {
    let output = run(r#"
        var lines = ["hello world", "line 1", "line 2"], f = std.tmpfile(), got = [];
        for (var i = 0; i < lines.length; i++) f.puts(lines[i], "\n");
        // A carriage return is not a line ending for the reference's `fgetc` loop.
        f.puts("dos\r\n");
        f.seek(0, std.SEEK_SET);
        print(f.eof());
        for (;;) { var line = f.getline(); if (line === null) break; got.push(line); }
        print(f.eof(), JSON.stringify(got));
        f.close();
        "#);
    assert_eq!(
        output,
        "false\ntrue [\"hello world\",\"line 1\",\"line 2\",\"dos\\r\"]\n"
    );
}

/// `test_popen`, plus the two numbers only `popen` can produce: `tell()` on a pipe is `-1`,
/// and `close()` returns the child's raw wait status (`exit 3` → `768`, which is `3 << 8`).
#[test]
fn popen_reads_the_child_and_reports_its_status() {
    let output = run(r#"
        var f = std.popen("echo hello", "r");
        print(f.tell());
        print(f.readAsString().trim());
        print(f.close());

        var g = std.popen("exit 3", "r");
        print(g.close());
        "#);
    assert_eq!(output, "-1\nhello\n0\n768\n");
}

/// A `FILE`'s shape has to match: methods on a non-enumerable prototype, nothing of its own
/// enumerable, and `JSON.stringify` seeing `{}`.
///
/// One divergence is deliberate and checked here so it cannot drift: the instance carries a
/// hidden `"\u{0}std.file"` property naming its slot in the host's table, so
/// `Object.getOwnPropertyNames` lists it where the reference lists nothing.
#[test]
fn a_file_looks_like_the_reference() {
    let output = run(r#"
        var f = std.tmpfile();
        print(JSON.stringify(Object.keys(f)));
        print(JSON.stringify(f));
        print(f.hasOwnProperty("close"), typeof f.close);
        print(Object.getPrototypeOf(f) !== Object.prototype);
        print(Object.keys(Object.getPrototypeOf(f)).length);
        print("close" in f);
        print(JSON.stringify(Object.getOwnPropertyNames(f)));
        f.close();
        "#);
    assert_eq!(
        output,
        "[]\n{}\nfalse function\ntrue\n0\ntrue\n[\"\\u0000std.file\"]\n"
    );
}

/// A closed handle throws rather than reading freed state, and the error is the reference's.
#[test]
fn a_closed_handle_throws() {
    let output = run(r#"
        var f = std.tmpfile();
        f.close();
        try { f.close(); } catch (e) { print(e.name + ": " + e.message); }
        try { f.tell(); } catch (e) { print(e.name + ": " + e.message); }
        // A non-file receiver is refused too, rather than reaching into whatever object.
        try { f.close.call({}); } catch (e) { print(e.name + ": " + e.message); }
        "#);
    assert_eq!(
        output,
        "TypeError: invalid file handle\nTypeError: invalid file handle\n\
         TypeError: not a file object\n"
    );
}

/// `std.open` reports failure as `null` plus `.errno`, and validates the mode first.
#[test]
fn open_reports_through_the_error_object() {
    let output = run(r#"
        var err = {};
        var f = std.open("/definitely/not/here", "r", err);
        print(f, err.errno);
        try { std.open("/tmp/x", "z"); } catch (e) { print(e.name + ": " + e.message); }
        try { std.popen("true", "rx"); } catch (e) { print(e.name + ": " + e.message); }
        // "rw" passes the character check and then fails in `popen(3)` itself.
        var e2 = {};
        print(std.popen("true", "rw", e2), e2.errno);
        "#);
    assert_eq!(
        output,
        "null 2\nTypeError: invalid file mode\nTypeError: invalid file mode\nnull 22\n"
    );
}

/// The window check is against the whole buffer, not the view, and `read`/`write` report a
/// `RangeError` rather than a short count.
#[test]
fn a_window_past_the_buffer_is_a_range_error() {
    let output = run(r#"
        var f = std.tmpfile(), b = new Uint8Array(2);
        try { f.read(b.buffer, 1, 5); } catch (e) { print(e.name + ": " + e.message); }
        try { f.write(b.buffer, 2, 1); } catch (e) { print(e.name + ": " + e.message); }
        f.close();
        "#);
    assert_eq!(
        output,
        "RangeError: read/write array buffer overflow\n\
         RangeError: read/write array buffer overflow\n"
    );
}

/// `std.out` writes to the host's sink, so it interleaves with `print` in order instead of
/// being a second buffer on the same stream.
#[test]
fn std_out_shares_the_sink() {
    let output = run(r#"
        std.out.puts("A");
        print("B");
        std.out.printf("%d", 7);
        print("C");
        "#);
    assert_eq!(output, "AB\n7C\n");
}

/// `std.fdopen` takes the descriptor out of the table `os` handed it, which is the
/// reference's `test_os_exec()` pattern — and nobody closes the same number twice.
#[test]
fn fdopen_adopts_a_descriptor_from_os_pipe() {
    let output = run(r#"
        var p = os.pipe();
        var w = new Uint8Array([104, 105, 10]);
        print(os.write(p[1], w.buffer, 0, 3));
        // The write end has to go before the reader can see end of file.
        os.close(p[1]);
        var f = std.fdopen(p[0], "r");
        print(f.getline());
        print(f.getline());
        print(f.close());
        // The descriptor now belongs to the file object, not to `os`.
        print(os.close(p[0]));
        "#);
    assert_eq!(output, "3\nhi\nnull\n0\n-9\n");
}

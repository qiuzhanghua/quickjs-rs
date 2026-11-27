//! `os.setReadHandler` / `os.setWriteHandler`, and the descriptor plumbing under them.
//!
//! These are the host half of the event loop: the engine knows which descriptors have
//! handlers (`Interp::pending_fds`) but cannot call `poll(2)`, so `Turn::Poll` asks the host
//! to wait and report back. The tests below drive that whole path.
//!
//! The reference's own `tests/test_rw_handler.js` is the strongest check — a 100-round
//! request/response handshake over a pipe — and it is run separately by `scripts/check.sh`'s
//! differential suite. What is here pins the smaller behaviours.

#![cfg(unix)]

use quickjs::{Context, Runtime};

/// Evaluate `source` with `std`/`os` mounted and drain the event loop, as the CLI does.
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
    // Bounded: a handler that never unregisters would otherwise spin here forever.
    let mut turns = 0;
    while ctx.has_pending_work() && turns < 10_000 {
        turns += 1;
        match ctx.run_one_turn().expect("a turn") {
            quickjs::Turn::Ran => {}
            quickjs::Turn::Wait(delay) | quickjs::Turn::Poll(delay) => {
                let mut watched: Vec<quickjs_host::fds::Watched> = ctx
                    .pending_fds()
                    .into_iter()
                    .map(|entry| quickjs_host::fds::Watched {
                        fd: entry.fd,
                        read: entry.read,
                        write: entry.write,
                    })
                    .collect();
                if !watched.is_empty() {
                    let ready = quickjs_host::fdtable::current(|table| match table {
                        Some(table) => quickjs_host::fds::poll(table, &watched, delay),
                        None => vec![quickjs_host::fds::Ready::default(); watched.len()],
                    });
                    for (index, event) in ready.iter().enumerate() {
                        if event.read || event.write {
                            let fd = watched[index].fd;
                            ctx.fire_fd(fd, event.read, event.write).expect("handler");
                            break;
                        }
                    }
                    watched.clear();
                } else {
                    std::thread::sleep(delay);
                }
            }
            quickjs::Turn::Done => break,
        }
    }
    ctx.output().to_string()
}

/// A read handler fires when the pipe has data, then again at end of file once the write end
/// closes — the reference reports a hangup as readable for exactly this reason.
#[test]
fn a_read_handler_fires_on_data_then_on_eof() {
    let output = run(r#"
        var fds = os.pipe();
        os.setReadHandler(fds[0], function () {
            var buffer = new Uint8Array(16);
            var read = os.read(fds[0], buffer.buffer, 0, 16);
            if (read > 0) {
                print("read", read);
            } else {
                os.setReadHandler(fds[0], null);
                os.close(fds[0]);
                print("eof");
            }
        });
        var bytes = new Uint8Array([104, 105]);
        os.write(fds[1], bytes.buffer, 0, 2);
        os.setTimeout(function () { os.close(fds[1]); }, 5);
        print("registered");
        "#);
    assert_eq!(output, "registered\nread 2\neof\n");
}

/// `setReadHandler(fd, null)` unregisters, and once both directions are gone the loop has
/// nothing left to wait for — otherwise it would poll a descriptor forever.
#[test]
fn clearing_a_handler_lets_the_loop_finish() {
    let output = run(r#"
        var fds = os.pipe();
        var fired = 0;
        os.setReadHandler(fds[0], function () { fired++; });
        os.setReadHandler(fds[0], null);
        os.close(fds[0]);
        os.close(fds[1]);
        print("cleared", fired);
        "#);
    assert_eq!(output, "cleared 0\n");
}

/// A write handler fires when the pipe has room, which it does immediately. The write end
/// stays open until the handler has run: closing it first would make `poll` report
/// `POLLNVAL` forever and the loop would never finish.
#[test]
fn a_write_handler_fires_when_writable() {
    let output = run(r#"
        var fds = os.pipe();
        os.setWriteHandler(fds[1], function () {
            os.setWriteHandler(fds[1], null);
            os.close(fds[0]);
            print("writable");
        });
        "#);
    assert_eq!(output, "writable\n");
}

/// `undefined` is not `null`: the reference tests `JS_IsNull` only, so passing `undefined`
/// falls through to the function check and throws.
#[test]
fn only_null_unregisters() {
    let output = run(r#"
        var fds = os.pipe();
        try {
            os.setReadHandler(fds[0], undefined);
        } catch (error) {
            print(error.name + ": " + error.message);
        }
        try {
            os.setReadHandler(fds[0], 42);
        } catch (error) {
            print(error.name + ": " + error.message);
        }
        os.close(fds[0]);
        os.close(fds[1]);
        "#);
    assert_eq!(
        output,
        "TypeError: not a function\nTypeError: not a function\n"
    );
}

/// `os.read(std.in.fileno(), …)` is what the reference's REPL does, so a descriptor owned by
/// a `std` file object has to be reachable from `os`. Without the bridge in `FdTable` this
/// read `EBADF` forever.
#[test]
fn os_reaches_a_descriptor_std_owns() {
    let output = run(r#"
        var fds = os.pipe();
        var file = std.fdopen(fds[0], "r");
        var bytes = new Uint8Array([65, 66]);
        os.write(fds[1], bytes.buffer, 0, 2);
        os.close(fds[1]);
        // The descriptor now belongs to the file object; `os` still has to see it.
        var got = new Uint8Array(4);
        print(os.read(file.fileno(), got.buffer, 0, 4));
        print(String.fromCharCode(got[0], got[1]));
        file.close();
        "#);
    assert_eq!(output, "2\nAB\n");
}

//! The `std` module: what the reference's `quickjs-libc.c` installs.
//!
//! # Order of arrival
//!
//! M9b is being built in pieces, each one verified against `qjs --std` before the next:
//!
//! 1. `printf`/`sprintf` — pure formatting, and the piece with the most surprising details
//! 2. the file object (`open`/`tmpfile`/`popen`/`fdopen` and its 17 methods)
//! 3. environment, `loadFile`/`loadScript`/`evalScript`, `strerror`, `urlGet`, `exit`, `gc`
//! 4. `parseExtJSON`
//! 5. `__sprintObject`/`__printObject`
//!
//! A function that has not arrived yet is **absent** rather than present and broken, so a
//! script that needs it fails at `std.whatever is not a function` — the failure mode this
//! project prefers over a stub that silently does nothing.

use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::rc::Rc;

use quickjs::object::PropertyKey;
use quickjs::value::Wtf8;
use quickjs::{Context, Gc, Interp, JsObject, JsResult, Value};
use rustix::fs::{Mode, OFlags};

use crate::stdio::{self, FileState, Slab};

/// Where the `FILE` prototype is parked. See [`prototype_of`].
const PROTO_KEY: &str = "\u{0}std.FILE";

/// Install the `std` members into `object`.
pub fn install(
    ctx: &mut Context<'_>,
    object: &Value,
    fds: &crate::fdtable::Shared,
) -> quickjs::Result<()> {
    if object.as_object().is_none() {
        return Err(quickjs::Error::Exception(
            "std: target is not an object".to_string(),
        ));
    }
    let slab: Slab = Rc::new(RefCell::new(Vec::new()));
    let fds = fds.clone();
    // So `os.read(std.in.fileno(), …)` works: `repl.js` does exactly that.
    fds.attach_slab(slab.clone());

    // The prototype lives on the global object rather than in the closures below: a
    // `HostFn` may not hold a `Gc`, because the collector cannot see into the closure.
    let proto = stdio::build_prototype(ctx, &slab)?;
    // Parked on the global so the host functions can reach it without capturing a `Gc`.
    let global = ctx.global();
    ctx.define_hidden(&global, PROTO_KEY, proto.clone())?;

    // `printf` writes to stdout and returns the byte count; `sprintf` returns the string.
    // The two share every rule about the format string, which is why only the sink differs.
    let sprintf = ctx.new_host_function_value("sprintf", 1, Box::new(js_sprintf))?;
    ctx.set_property(object, "sprintf", sprintf)?;
    let printf = ctx.new_host_function_value("printf", 1, Box::new(js_printf))?;
    ctx.set_property(object, "printf", printf)?;
    let parse_ext = ctx.new_host_function_value("parseExtJSON", 1, Box::new(js_parse_ext_json))?;
    ctx.set_property(object, "parseExtJSON", parse_ext)?;

    // `std.puts(...)` is `printf("%s")` per argument with no separator — it writes to
    // stdout, which is the sink here.
    let puts = ctx.new_host_function_value("puts", 1, Box::new(js_puts))?;
    ctx.set_property(object, "puts", puts)?;

    // The three seek origins, as plain numbers. `test_std.js` uses `std.SEEK_SET`.
    for (name, value) in [("SEEK_SET", 0), ("SEEK_CUR", 1), ("SEEK_END", 2)] {
        ctx.set_property(object, name, Value::Int(value))?;
    }

    // The file functions. Each takes the prototype from the global at call time, which is
    // why they are closures over the slab only.
    let file_functions: [(&str, u32, HostBody); 4] = [
        ("open", 3, Box::new(js_open)),
        ("popen", 3, Box::new(js_popen)),
        ("tmpfile", 1, Box::new(js_tmpfile)),
        ("fdopen", 3, Box::new(js_fdopen)),
    ];
    for (name, length, body) in file_functions {
        let slab = slab.clone();
        let fds = fds.clone();
        let mut body = body;
        let function = ctx.new_host_function_value(
            name,
            length,
            Box::new(move |interp: &mut Interp<'_>, _this, args: &[Value]| {
                body(interp, args, &slab, &fds)
            }),
        )?;
        ctx.set_property(object, name, function)?;
    }

    let plain_functions: [(&str, u32, Body); 12] = [
        ("loadFile", 1, Box::new(js_load_file)),
        ("loadScript", 1, Box::new(js_load_script)),
        ("evalScript", 1, Box::new(js_eval_script)),
        ("strerror", 1, Box::new(js_strerror)),
        ("getenv", 1, Box::new(js_getenv)),
        ("getenviron", 0, Box::new(js_getenviron)),
        ("exit", 1, Box::new(js_exit)),
        ("gc", 0, Box::new(js_gc)),
        ("setenv", 2, Box::new(js_setenv)),
        ("unsetenv", 1, Box::new(js_unsetenv)),
        ("__sprintObject", 1, Box::new(js_sprint_object)),
        ("__printObject", 1, Box::new(js_print_object)),
    ];
    for (name, length, body) in plain_functions {
        let function = ctx.new_host_function_value(name, length, body)?;
        ctx.set_property(object, name, function)?;
    }

    // `std.out`, `std.in` and `std.err` are file objects in the reference.
    let out = new_file(ctx, &proto, FileState::sink(), &slab)?;
    ctx.define_hidden(object, "out", out)?;
    let err = new_file(ctx, &proto, FileState::stderr(), &slab)?;
    ctx.define_hidden(object, "err", err)?;
    // `std.in` is descriptor 0 itself, borrowed rather than duplicated: `fileno()` has to
    // answer 0, because `repl.js` reads stdin through `os.read(std.in.fileno(), …)` and a
    // `dup` would report 3 and wait on the wrong descriptor forever.
    let input = new_file(ctx, &proto, FileState::stdin(), &slab)?;
    ctx.define_hidden(object, "in", input)?;
    Ok(())
}

type Body = Box<dyn for<'i> FnMut(&mut Interp<'i>, Value, &[Value]) -> JsResult<Value>>;

type HostBody = Box<
    dyn for<'i> FnMut(&mut Interp<'i>, &[Value], &Slab, &crate::fdtable::Shared) -> JsResult<Value>,
>;

/// The `FILE` prototype, fetched from where [`install`] parked it.
///
/// Reaching it through the global is what keeps `Gc` out of the closures. A missing
/// prototype means `std`'s install did not finish, which is a host bug rather than
/// something a script can cause, so it surfaces as a `TypeError` naming that.
fn prototype_of(interp: &mut Interp<'_>) -> JsResult<Gc<JsObject>> {
    let global = interp.realm.global.clone();
    let key = PropertyKey::String(Wtf8::new(PROTO_KEY));
    match interp.get_property_key(&global, &key)? {
        Some(Value::Object(object)) => Ok(object),
        _ => Err(interp.type_error("std: FILE prototype is missing")),
    }
}

/// A new file instance naming `slot`, with the shared prototype.
///
/// Two versions because there are two contexts to build from: `install` runs before any
/// script does and holds a [`Context`], while the file *functions* run inside host
/// functions and hold an [`Interp`].
fn new_file(
    ctx: &mut Context<'_>,
    proto: &Value,
    state: FileState,
    slab: &Slab,
) -> quickjs::Result<Value> {
    let object = ctx.new_object_with_prototype(proto)?;
    let slot = stdio::adopt(slab, state);
    // Non-enumerable, so `Object.keys(f)` stays `[]` and `JSON.stringify(f)` stays `"{}"`.
    let key = ctx.new_string(stdio::SLOT_KEY);
    ctx.define_hidden(&object, &key.to_js_string(), Value::Int(slot))?;
    Ok(object)
}

/// [`new_file`] from inside a host function.
fn new_file_in(interp: &mut Interp<'_>, state: FileState, slab: &Slab) -> JsResult<Value> {
    let proto = prototype_of(interp)?;
    let object = interp.new_object(Some(proto));
    let slot = stdio::adopt(slab, state);
    object.define_key(
        &PropertyKey::String(Wtf8::new(stdio::SLOT_KEY)),
        quickjs::Property::with_attributes(Value::Int(slot), true, false),
    );
    Ok(Value::Object(object))
}

/// `std.parseExtJSON(text)`.
///
/// The extended dialect lives in the engine's JSON parser behind a flag, exactly as the
/// reference implements it (`JS_ParseJSON2(..., JS_PARSE_JSON_EXT)`), rather than as a
/// pre-processing pass or a second parser. That keeps one grammar with one set of error
/// positions — and keeps `JSON.parse` strict, because it simply does not pass the flag.
fn js_parse_ext_json(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `JS_ToCStringLen` on the argument: a non-string is converted, and a missing one is
    // `undefined` → `"undefined"`, which then fails to parse. Same order as the reference.
    let text = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    interp.parse_ext_json(&text)
}

/// `std.sprintf(format, ...args)`.
fn js_sprintf(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::printf::sprintf(interp, args)?;
    Ok(interp.string(text))
}

/// `std.printf(format, ...args)`.
///
/// C writes the bytes to `stdout` and returns how many it wrote. The host's sink is where
/// script output belongs, so the text goes there, and the count is the byte length.
fn js_printf(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::printf::sprintf(interp, args)?;
    let written = text.len();
    stdio::emit(interp, text.as_bytes());
    Ok(Value::Float(written as f64))
}

/// `std.puts(...texts)` — each argument as a string, concatenated, no newline.
fn js_puts(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut text = String::new();
    for value in args {
        text.push_str(&interp.to_js_string(value)?);
    }
    stdio::emit(interp, text.as_bytes());
    Ok(Value::Undefined)
}

/// The flags a `mode` string asks for, or `None` when it is not a mode.
///
/// The reference validates with `mode[strspn(mode, allowed)] != '\0'`, so leading repeats
/// are fine (`"rr"` passes) and anything outside the allowed set is a `TypeError`:
/// `"rwa+b"` for `open`, `"rw"` for `popen`, `"rwa+"` for `fdopen`. `b` is accepted and
/// ignored, since there is no text mode here.
fn parse_mode(mode: &str, allowed: &str) -> Option<OFlags> {
    if mode.is_empty() || mode.contains(|c: char| !allowed.contains(c)) {
        return None;
    }
    let plus = mode.contains('+');
    let base = if mode.contains('a') {
        OFlags::CREATE | OFlags::APPEND
    } else if mode.contains('w') {
        OFlags::CREATE | OFlags::TRUNC
    } else {
        OFlags::empty()
    };
    let access = if plus {
        OFlags::RDWR
    } else if mode.contains('w') || mode.contains('a') {
        OFlags::WRONLY
    } else {
        OFlags::RDONLY
    };
    Some(base | access | OFlags::CLOEXEC)
}

/// Set `.errno` on the optional error argument, which is how `std.open` reports a failure
/// without throwing.
fn set_errno(interp: &mut Interp<'_>, args: &[Value], index: usize, errno: i32) {
    let Some(target) = args.get(index) else {
        return;
    };
    if let Some(object) = target.as_object() {
        object.define_data("errno", Value::Int(errno));
    }
    let _ = interp;
}

/// `std.open(filename, mode, error?)` → a file, or `null` with `.errno` set on `error`.
fn js_open(
    interp: &mut Interp<'_>,
    args: &[Value],
    slab: &Slab,
    _fds: &crate::fdtable::Shared,
) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    let mode_text = interp.to_js_string(args.get(1).unwrap_or(&Value::Undefined))?;
    let Some(flags) = parse_mode(&mode_text, "rwa+b") else {
        return Err(interp.type_error("invalid file mode"));
    };
    match rustix::fs::open(&name, flags, Mode::from_bits_truncate(0o666)) {
        Ok(fd) => {
            set_errno(interp, args, 2, 0);
            new_file_in(interp, FileState::from_fd(fd, 0), slab)
        }
        Err(error) => {
            set_errno(interp, args, 2, error.raw_os_error());
            Ok(Value::Null)
        }
    }
}

/// `std.popen(command, mode, error?)` — the command runs under `sh -c`, as `popen(3)` does.
fn js_popen(
    interp: &mut Interp<'_>,
    args: &[Value],
    slab: &Slab,
    _fds: &crate::fdtable::Shared,
) -> JsResult<Value> {
    let command_text = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    let mode_text = interp.to_js_string(args.get(1).unwrap_or(&Value::Undefined))?;
    // Two gates, in this order, because the reference has two. The character check is
    // `js_std_popen`'s own `strspn` test and throws; `popen(3)` itself then accepts only
    // exactly `"r"` or `"w"`, so `"rw"` and `"rr"` pass the first gate and come back as
    // `null` with `EINVAL` — which is what `qjs --std` does.
    if mode_text.is_empty() || mode_text.contains(|c: char| c != 'r' && c != 'w') {
        return Err(interp.type_error("invalid file mode"));
    }
    if mode_text != "r" && mode_text != "w" {
        set_errno(interp, args, 2, 22);
        return Ok(Value::Null);
    }
    let reading = mode_text == "r";
    let mut command = std::process::Command::new("sh");
    command.arg("-c").arg(&command_text);
    if reading {
        command.stdout(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::piped());
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            set_errno(interp, args, 2, error.raw_os_error().unwrap_or(0));
            return Ok(Value::Null);
        }
    };
    // The two ends are different types, so they cannot meet in one `if`; both become an
    // `OwnedFd`, which is what the file state wants anyway.
    let pipe: Option<OwnedFd> = if reading {
        child.stdout.take().map(OwnedFd::from)
    } else {
        child.stdin.take().map(OwnedFd::from)
    };
    let Some(pipe) = pipe else {
        set_errno(interp, args, 2, 0);
        return Ok(Value::Null);
    };
    set_errno(interp, args, 2, 0);
    new_file_in(interp, FileState::from_child(child, pipe), slab)
}

/// `std.tmpfile(error?)` — an anonymous seekable file.
///
/// `tmpfile(3)` unlinks the file as it creates it, so nothing is left behind even when the
/// script exits without closing. That is done here with an explicit unlink rather than
/// `memfd_create`, which is Linux-only.
fn js_tmpfile(
    interp: &mut Interp<'_>,
    args: &[Value],
    slab: &Slab,
    _fds: &crate::fdtable::Shared,
) -> JsResult<Value> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("qjs-tmpfile-{}-{nonce}", std::process::id()));
    let opened = rustix::fs::open(
        &path,
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    );
    let fd = match opened {
        Ok(fd) => fd,
        Err(error) => {
            set_errno(interp, args, 0, error.raw_os_error());
            return Ok(Value::Null);
        }
    };
    // From here the file has a descriptor and no name; a failure to unlink leaves it
    // behind, so it is reported rather than ignored.
    if let Err(error) = rustix::fs::unlink(&path) {
        set_errno(interp, args, 0, error.raw_os_error());
        return Ok(Value::Null);
    }
    set_errno(interp, args, 0, 0);
    new_file_in(interp, FileState::from_fd(fd, 0), slab)
}

/// `std.fdopen(fd, mode, error?)` — wrap a descriptor the script already has.
///
/// The descriptor is taken **out** of the shared table, because `fdopen(3)` gives it to the
/// `FILE`: from then on the file object owns it and `close()` closes it. Keeping the table
/// entry as well would close the same number twice.
///
/// The standard descriptors are not in the table — they belong to the process — so 0, 1 and
/// 2 are duplicated through their `std` handles instead. That is the case a script reaches
/// with `std.fdopen(1, "w")`.
fn js_fdopen(
    interp: &mut Interp<'_>,
    args: &[Value],
    slab: &Slab,
    fds: &crate::fdtable::Shared,
) -> JsResult<Value> {
    let number = args.first().map_or(-1, |value| value.to_number() as i32);
    let mode_text = interp.to_js_string(args.get(1).unwrap_or(&Value::Undefined))?;
    // `fdopen` accepts only what makes sense on an existing descriptor: no create, no trunc.
    if mode_text.is_empty() || mode_text.contains(|c: char| !"rwa+".contains(c)) {
        return Err(interp.type_error("invalid file mode"));
    }
    let duplicated = match number {
        0 => rustix::io::dup(std::io::stdin()),
        1 => rustix::io::dup(std::io::stdout()),
        2 => rustix::io::dup(std::io::stderr()),
        // Any other number has to be one this host handed out; a number from nowhere is
        // EBADF rather than an assertion about a descriptor the process may not own.
        _ => match fds.take(number) {
            Some(fd) => Ok(fd),
            None => {
                set_errno(interp, args, 2, 9);
                return Ok(Value::Null);
            }
        },
    };
    match duplicated {
        Ok(fd) => {
            set_errno(interp, args, 2, 0);
            new_file_in(interp, FileState::from_fd(fd, 0), slab)
        }
        Err(error) => {
            set_errno(interp, args, 2, error.raw_os_error());
            Ok(Value::Null)
        }
    }
}

// ---------------------------------------------------------------------------
// The rest of `std`: files by name, the environment, errors, exit and `gc`.
// ---------------------------------------------------------------------------

/// `std.loadFile(filename)` — the whole file as a string, or `null` when it cannot be read.
///
/// `null`, not an exception: the reference calls `js_load_file` and returns `JS_NULL` when
/// it comes back empty. The bytes are decoded as UTF-8 with replacement, which is what
/// `JS_NewStringLen` does — a file that is not UTF-8 still loads, with `U+FFFD` where the
/// bytes were not text.
fn js_load_file(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    match std::fs::read(&name) {
        Ok(bytes) => Ok(interp.string(String::from_utf8_lossy(&bytes).into_owned())),
        Err(_) => Ok(Value::Null),
    }
}

/// `std.strerror(errno)` — the platform's message for an error number.
///
/// The text has to be the platform's, not a table here: `qjs --std` reports
/// `std.strerror(0)` as `"Undefined error: 0"` on macOS, which is `strerror(0)` and not
/// anything a portable table would say. `std::io::Error`'s message *is* `strerror` — Rust
/// gets it from `strerror_r` — with `" (os error N)"` appended, so the suffix is removed.
fn js_strerror(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let errno = args.first().map_or(0, |value| value.to_number() as i32);
    let text = std::io::Error::from_raw_os_error(errno).to_string();
    let message = match text.find(" (os error") {
        Some(at) => text[..at].to_string(),
        None => text,
    };
    Ok(interp.string(message))
}

/// `std.getenv(name)` — the value, or `undefined` when the name is not set.
fn js_getenv(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    Ok(match std::env::var(&name) {
        Ok(value) => interp.string(value),
        // `var` fails for a non-UTF-8 value as well as for a missing name. The reference
        // would hand back the bytes; `undefined` is closer than a mangled string.
        Err(_) => Value::Undefined,
    })
}

/// `std.getenviron()` — every variable as one plain object.
fn js_getenviron(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    // A plain object with `Object.prototype`, as `JS_NewObject` makes: `std.getenviron()
    // .hasOwnProperty(...)` is a thing scripts do.
    let proto = interp.realm.object_proto.clone();
    let object = interp.new_object(Some(proto));
    for (name, value) in std::env::vars() {
        object.define_data(&name, interp.string(value));
    }
    Ok(Value::Object(object))
}

/// `std.exit(status)` — terminate the process now.
///
/// The sink is flushed first: `exit(3)` flushes stdio buffers, and a script that ends this
/// way has to leave its output behind or a test that compares output would see nothing.
fn js_exit(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let status = args.first().map_or(0, |value| value.to_number() as i32);
    if let Some(sink) = interp.output_sink.as_mut() {
        let _ = sink.flush();
    }
    std::process::exit(status);
}

/// `std.gc()` — collect now.
fn js_gc(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    interp.heap.collect();
    Ok(Value::Undefined)
}

/// `std.loadScript(filename)` — run a file as a global script and return its value.
fn js_load_script(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    let source = match std::fs::read_to_string(&name) {
        Ok(source) => source,
        Err(_) => return Ok(Value::Null),
    };
    // Module syntax would need the loader; `loadScript` is the global-script form, as
    // `JS_EVAL_TYPE_GLOBAL` says in the reference.
    let value = interp.eval_in_global(&source)?;
    Ok(value)
}

/// `std.evalScript(source, options)` — evaluate source as a global script.
///
/// `options.backtrace_barrier` is accepted and ignored: this engine has no `js_backtrace`
/// flag to hide frames behind, and a script that passes it is asking for a stack trace
/// shape, not for different evaluation.
/// `std.evalScript(source, options?)`.
///
/// `options.async` switches to the promise-returning form, which is what the reference's
/// REPL uses so that one line's failure cannot escape the read handler. `options
/// .backtrace_barrier` truncates an error's trace at this evaluation, so the REPL reports
/// the line it evaluated without also reporting its own readline loop.
fn js_eval_script(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let source = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    let (asynchronous, barrier) = match args.get(1).and_then(|value| value.as_object()) {
        Some(options) => {
            let flag = |interp: &mut Interp<'_>, name: &str| -> JsResult<bool> {
                Ok(match interp.get_property(options, name)? {
                    Some(value) => value.to_boolean(),
                    None => false,
                })
            };
            (flag(interp, "async")?, flag(interp, "backtrace_barrier")?)
        }
        None => (false, false),
    };
    if asynchronous {
        interp.eval_in_global_async(&source, barrier)
    } else {
        interp.eval_script_in_global(&source, barrier)
    }
}

/// `std.__sprintObject(value)` — the reference's `JS_PrintValue` as a string.
fn js_sprint_object(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::print::sprint(interp, args.first().unwrap_or(&Value::Undefined))?;
    Ok(interp.string(text))
}

/// `std.__printObject(value)` — the same text written to stdout.
///
/// No trailing newline, matching the reference: `js_std_file_printObject` is
/// `JS_PrintValue(ctx, js_print_value_write, stdout, …)` and nothing else. The callers that
/// want a line (`repl.js` prints every result this way) add the newline themselves, so
/// adding one here produced a blank line after each of them.
fn js_print_object(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::print::sprint(interp, args.first().unwrap_or(&Value::Undefined))?;
    stdio::emit(interp, text.as_bytes());
    Ok(Value::Undefined)
}

/// `std.setenv(name, value)` — set an environment variable.
///
/// Returns `undefined` whatever happens, because the reference ignores `setenv`'s return
/// value. The `unsafe` behind it lives in [`crate::env`], with its argument.
fn js_setenv(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    let value = interp.to_js_string(args.get(1).unwrap_or(&Value::Undefined))?;
    crate::env::set(&name, &value);
    Ok(Value::Undefined)
}

/// `std.unsetenv(name)` — remove an environment variable.
fn js_unsetenv(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let name = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    crate::env::unset(&name);
    Ok(Value::Undefined)
}

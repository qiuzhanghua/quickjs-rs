//! `std`'s file object: C `FILE *`, re-implemented in Rust.
//!
//! # Why not wrap an existing reader
//!
//! The reference's file object is a `FILE *`, and `FILE` is a *buffered* stream with a
//! position, an end-of-file indicator and an error indicator that all survive across
//! calls. `std::fs::File` is unbuffered and has none of them; `BufReader` has a buffer but
//! hides the position. The methods here need all of it at once — `test_file1()` in the
//! reference's own suite writes, seeks to the end, reads `tell()` for the size, seeks back
//! and reads the bytes out — so this module keeps its own read buffer, write buffer and
//! position, and implements `seek` by dropping the buffers.
//!
//! # One divergence, on purpose
//!
//! Methods live on a shared prototype, as in the reference, so `Object.keys(f)`,
//! `JSON.stringify(f)` and `f.hasOwnProperty("close")` all answer exactly as `qjs --std`
//! does. An instance still has to name its slot in the host's table, and the only way a
//! prototype method can do that is a property on the instance. That property is the one
//! difference: `Object.getOwnPropertyNames(f)` lists `"\u{0}std.file"` where the reference
//! lists nothing. A symbol key would hide it from `getOwnPropertyNames` and show it in
//! `getOwnPropertySymbols` instead, so the divergence does not go away — it moves — and
//! costs an indirection on every call. A string is the cheaper of two leaks.
//!
//! # Descriptors
//!
//! `std.out` writes to the host's **output sink**, not to file descriptor 1. The host owns
//! stdout here: `print` and `std.printf` already go to the sink, so a `std.out` writing to
//! the real descriptor would be a second buffer on the same stream and
//! `std.out.puts("a"); print("b")` could come out as `ba`. `std.err` does go to the real
//! stderr, which is unbuffered in the reference too.

use std::cell::RefCell;
use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::rc::Rc;

use quickjs::object::PropertyKey;
use quickjs::value::Wtf8;
use quickjs::{Context, Interp, JsResult, Value};

/// The property an instance uses to name its slot. See the module comment.
pub const SLOT_KEY: &str = "\u{0}std.file";

/// How much is read from a descriptor at a time.
const CHUNK: usize = 8192;

const EBADF: i32 = 9;
const EIO: i32 = 5;
const EINVAL: i32 = 22;
const ESPIPE: i32 = 29;

/// One open stream.
pub struct FileState {
    kind: Kind,
    /// Bytes read ahead of the position. `read_buf[read_pos..]` is what is left.
    read_buf: Vec<u8>,
    read_pos: usize,
    /// Bytes written but not yet handed to the descriptor.
    write_buf: Vec<u8>,
    /// The logical position: what `tell()` reports and what `SEEK_CUR` counts from.
    ///
    /// Tracked rather than asked of the kernel, because stdio's position runs ahead of the
    /// descriptor's — buffered writes are already "written" and buffered reads already
    /// "read" as far as the script is concerned. `-1` for a stream that is not seekable.
    pos: i64,
    seekable: bool,
    /// `feof`: a read reached the end.
    eof: bool,
    /// `ferror`: a read or write failed.
    error: bool,
    closed: bool,
}

thread_local! {
    /// Whether a read of the real stdin has reached end of file.
    static STDIN_EOF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether the real stdin has hit end of file.
///
/// The REPL reads stdin through `os.setReadHandler`, and `repl.js` has no end-of-file path:
/// it reads zero bytes and waits for more, which never comes. Letting the host notice keeps
/// the process from spinning — the reference hangs here, and matching a hang is not a
/// behaviour worth reproducing.
pub fn stdin_eof() -> bool {
    STDIN_EOF.with(std::cell::Cell::get)
}

/// Record that the real stdin has reached end of file.
///
/// Called from both read paths — a `std.in` file object and `os.read(0, …)` — because the
/// REPL uses the second and a script may use the first.
pub fn note_stdin_eof() {
    STDIN_EOF.with(|flag| flag.set(true));
}

enum Kind {
    /// A descriptor this host owns.
    Fd(OwnedFd),
    /// A child process, with this side's end of its pipe.
    Child {
        child: std::process::Child,
        pipe: OwnedFd,
    },
    /// The host's output sink — what `std.out` is here.
    Sink,
    /// The real stderr.
    Stderr,
    /// The real stdin, borrowed rather than owned.
    ///
    /// Borrowing is what makes `fileno()` answer **0**: the reference's `std.in` wraps
    /// descriptor 0 itself, and holding a `dup` here would report 3 instead — which is a
    /// silent break, because `repl.js` installs its read handler on
    /// `os.setReadHandler(std.in.fileno(), …)` and would then wait on a descriptor the
    /// terminal never writes to. Owning nothing also keeps the existing promise that a
    /// script closing `std.in` cannot take the process's stdin with it.
    Stdin,
}

impl FileState {
    /// Wrap an owned descriptor. `start` is the position to report before any I/O.
    pub fn from_fd(fd: OwnedFd, start: i64) -> Self {
        // `SEEK_CUR` with offset zero asks whether the descriptor is seekable without
        // moving it; a pipe answers `ESPIPE`.
        let seekable = rustix::fs::seek(&fd, rustix::fs::SeekFrom::Current(0)).is_ok();
        FileState {
            kind: Kind::Fd(fd),
            read_buf: Vec::new(),
            read_pos: 0,
            write_buf: Vec::new(),
            pos: if seekable { start } else { -1 },
            seekable,
            eof: false,
            error: false,
            closed: false,
        }
    }

    /// Wrap a child's pipe. `close` waits for the child and reports its raw wait status.
    pub fn from_child(child: std::process::Child, pipe: OwnedFd) -> Self {
        FileState {
            kind: Kind::Child { child, pipe },
            read_buf: Vec::new(),
            read_pos: 0,
            write_buf: Vec::new(),
            // `ftell` on a pipe is `-1`, and `popen`'s stream is a pipe.
            pos: -1,
            seekable: false,
            eof: false,
            error: false,
            closed: false,
        }
    }

    pub fn sink() -> Self {
        FileState {
            kind: Kind::Sink,
            read_buf: Vec::new(),
            read_pos: 0,
            write_buf: Vec::new(),
            pos: -1,
            seekable: false,
            eof: false,
            error: false,
            closed: false,
        }
    }

    /// `std.in`: descriptor 0, borrowed.
    pub fn stdin() -> Self {
        FileState {
            kind: Kind::Stdin,
            ..FileState::sink()
        }
    }

    pub fn stderr() -> Self {
        FileState {
            kind: Kind::Stderr,
            ..FileState::sink()
        }
    }

    /// Whether this stream has been closed, so a stale number is not mistaken for a live
    /// descriptor.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// The descriptor behind this stream, when it has one.
    pub fn fd(&self) -> Option<BorrowedFd<'_>> {
        match &self.kind {
            Kind::Fd(fd) => Some(fd.as_fd()),
            Kind::Child { pipe, .. } => Some(pipe.as_fd()),
            // `rustix::stdio::stdin` is a `BorrowedFd<'static>`, which is what lets
            // this outlive the temporary `std::io::stdin()` handle returns.
            Kind::Stdin => Some(rustix::stdio::stdin()),
            Kind::Sink | Kind::Stderr => None,
        }
    }

    /// The number `fileno()` reports.
    pub fn file_number(&self) -> i32 {
        match &self.kind {
            Kind::Fd(fd) => fd.as_raw_fd(),
            Kind::Child { pipe, .. } => pipe.as_raw_fd(),
            Kind::Stdin => 0,
            Kind::Sink => 1,
            Kind::Stderr => 2,
        }
    }

    /// Read more bytes into the buffer. `false` means end of file or error, both flagged.
    fn refill(&mut self) -> bool {
        self.read_buf.clear();
        self.read_pos = 0;
        let mut chunk = [0u8; CHUNK];
        let Some(fd) = self.fd() else {
            return false;
        };
        match rustix::io::read(fd, chunk.as_mut_slice()) {
            Ok(0) => {
                self.eof = true;
                // Stdin reaching end of file is how a piped REPL says "no more lines".
                // `repl.js` never unregisters its read handler, so without this the loop
                // would poll a descriptor that is permanently readable-at-EOF forever.
                if matches!(self.kind, Kind::Stdin) {
                    note_stdin_eof();
                }
                false
            }
            Ok(count) => {
                self.read_buf.extend_from_slice(&chunk[..count]);
                true
            }
            Err(error) if error == rustix::io::Errno::INTR => self.refill(),
            Err(_) => {
                self.error = true;
                false
            }
        }
    }

    /// One byte, `fgetc` style. `None` is `EOF`.
    fn next_byte(&mut self) -> Option<u8> {
        if self.read_pos == self.read_buf.len() && !self.refill() {
            return None;
        }
        let byte = self.read_buf[self.read_pos];
        self.read_pos += 1;
        if self.pos >= 0 {
            self.pos += 1;
        }
        Some(byte)
    }

    /// Fill `dest` from the stream, stopping early only at end of file. Returns the count.
    fn read_into(&mut self, dest: &mut [u8]) -> usize {
        let mut done = 0;
        while done < dest.len() {
            if self.read_pos == self.read_buf.len() && !self.refill() {
                break;
            }
            let available = self.read_buf.len() - self.read_pos;
            let take = available.min(dest.len() - done);
            dest[done..done + take]
                .copy_from_slice(&self.read_buf[self.read_pos..self.read_pos + take]);
            self.read_pos += take;
            done += take;
        }
        if self.pos >= 0 {
            self.pos += done as i64;
        }
        done
    }

    /// Hand the write buffer to its destination. Returns an errno, `0` for success.
    fn flush(&mut self, interp: &mut Interp<'_>) -> i32 {
        if self.write_buf.is_empty() {
            return 0;
        }
        let buffered = std::mem::take(&mut self.write_buf);
        match &self.kind {
            Kind::Sink => {
                emit(interp, &buffered);
                0
            }
            Kind::Stdin => {
                self.error = true;
                EBADF
            }
            Kind::Stderr => match std::io::stderr().write_all(&buffered) {
                Ok(()) => 0,
                Err(_) => {
                    self.error = true;
                    EIO
                }
            },
            _ => {
                let Some(fd) = self.fd() else {
                    self.error = true;
                    return EBADF;
                };
                match write_all(fd, &buffered) {
                    Ok(()) => 0,
                    Err(errno) => {
                        self.error = true;
                        errno
                    }
                }
            }
        }
    }

    /// Write bytes, buffering like `fwrite`. Returns how many were accepted.
    fn write(&mut self, interp: &mut Interp<'_>, bytes: &[u8]) -> usize {
        // A sink or stderr has no position and no reason to buffer, so it goes straight
        // out — which is also what keeps the sink's ordering honest against `print`.
        if matches!(self.kind, Kind::Sink | Kind::Stderr | Kind::Stdin) {
            self.write_buf.extend_from_slice(bytes);
            return match self.flush(interp) {
                0 => bytes.len(),
                _ => 0,
            };
        }
        self.write_buf.extend_from_slice(bytes);
        if self.pos >= 0 {
            self.pos += bytes.len() as i64;
        }
        if self.write_buf.len() >= CHUNK && self.flush(interp) != 0 {
            return 0;
        }
        bytes.len()
    }

    /// `fseek`: flush what is pending, drop what is buffered, move the position.
    fn seek(&mut self, interp: &mut Interp<'_>, offset: i64, whence: i32) -> i32 {
        match self.flush(interp) {
            0 => {}
            errno => return errno,
        }
        let target = match whence {
            0 => offset,
            1 => self.pos.saturating_add(offset),
            _ => match self
                .fd()
                .and_then(|fd| rustix::fs::seek(fd, rustix::fs::SeekFrom::End(0)).ok())
            {
                Some(size) => size as i64 + offset,
                None => return ESPIPE,
            },
        };
        if target < 0 {
            return EINVAL;
        }
        let Some(fd) = self.fd() else {
            return ESPIPE;
        };
        match rustix::fs::seek(fd, rustix::fs::SeekFrom::Start(target as u64)) {
            Ok(_) => {
                // Only on success: `fseek` clears the end-of-file indicator, a failed one
                // leaves it. Dropping read-ahead is what makes the clearing meaningful —
                // the next read refills from the new position instead of replaying bytes
                // the caller moved away from.
                self.read_buf.clear();
                self.read_pos = 0;
                self.pos = target;
                self.eof = false;
                self.error = false;
                0
            }
            Err(error) => error.raw_os_error(),
        }
    }
}

/// Write every byte, which is the `fwrite` contract: a short write is one to finish, not
/// one to report.
fn write_all(fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<(), i32> {
    let mut rest = bytes;
    while !rest.is_empty() {
        match rustix::io::write(fd, rest) {
            Ok(0) => return Err(EIO),
            Ok(count) => rest = &rest[count..],
            Err(error) if error == rustix::io::Errno::INTR => {}
            Err(error) => return Err(error.raw_os_error()),
        }
    }
    Ok(())
}

/// Put text on the host's output: the sink, plus the buffer `Context::output` reads.
pub fn emit(interp: &mut Interp<'_>, bytes: &[u8]) {
    if let Some(sink) = interp.output_sink.as_mut() {
        let _ = sink.write_all(bytes);
    }
    interp.output.push_str(&String::from_utf8_lossy(bytes));
}

/// The host's table of open streams, indexed by the slot an instance names.
pub type Slab = Rc<RefCell<Vec<Option<FileState>>>>;

/// Put `state` in `slab` and return the slot number to stamp on the instance.
pub fn adopt(slab: &Slab, state: FileState) -> i32 {
    let mut files = slab.borrow_mut();
    files.push(Some(state));
    (files.len() - 1) as i32
}

/// Build the prototype holding the seventeen methods.
///
/// The prototype is a plain object whose own prototype is `Object.prototype`, which is what
/// `qjs --std` shows: `Object.getPrototypeOf(std.out) !== Object.prototype` while
/// `Object.getPrototypeOf(std.out).constructor === Object`.
pub fn build_prototype(ctx: &mut Context<'_>, slab: &Slab) -> quickjs::Result<Value> {
    let proto = ctx.new_object();
    let mut out: Vec<(&'static str, u32, Body)> = Vec::new();
    macro_rules! method {
        ($name:literal, $length:expr, $body:path) => {{
            let slab = slab.clone();
            out.push((
                $name,
                $length,
                Box::new(move |interp: &mut Interp<'_>, this, args: &[Value]| {
                    $body(interp, this, args, &slab)
                }),
            ));
        }};
    }
    method!("close", 0, method_close);
    method!("puts", 1, method_puts);
    method!("printf", 1, method_printf);
    method!("flush", 0, method_flush);
    method!("tell", 0, method_tell);
    method!("tello", 0, method_tello);
    method!("seek", 2, method_seek);
    method!("eof", 0, method_eof);
    method!("fileno", 0, method_fileno);
    method!("error", 0, method_error);
    method!("clearerr", 0, method_clearerr);
    method!("read", 3, method_read);
    method!("write", 3, method_write);
    method!("getline", 0, method_getline);
    method!("readAsString", 0, method_read_as_string);
    method!("getByte", 0, method_get_byte);
    method!("putByte", 1, method_put_byte);

    for (name, length, body) in out {
        let function = ctx.new_host_function_value(name, length, body)?;
        ctx.define_hidden(&proto, name, function)?;
    }
    Ok(proto)
}

type Body = Box<dyn for<'i> FnMut(&mut Interp<'i>, Value, &[Value]) -> JsResult<Value>>;

/// Run `f` on the stream `this` names, or throw the reference's `invalid file handle`.
fn with_file<R>(
    interp: &mut Interp<'_>,
    this: &Value,
    slab: &Slab,
    f: impl FnOnce(&mut FileState, &mut Interp<'_>) -> R,
) -> JsResult<R> {
    let slot = slot_of(interp, this)?;
    let mut files = slab.borrow_mut();
    let Some(state) = files.get_mut(slot).and_then(Option::as_mut) else {
        return Err(interp.type_error("invalid file handle"));
    };
    if state.closed {
        return Err(interp.type_error("invalid file handle"));
    }
    Ok(f(state, interp))
}

/// The slot number `this` names.
fn slot_of(interp: &mut Interp<'_>, this: &Value) -> JsResult<usize> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("not a file object"));
    };
    let key = PropertyKey::String(Wtf8::new(SLOT_KEY));
    match interp.get_property_key(object, &key)? {
        Some(Value::Int(slot)) => Ok(slot as usize),
        _ => Err(interp.type_error("not a file object")),
    }
}

/// `close()`.
///
/// `fclose`/`pclose` through `js_get_errno`, so a failure is a **negative** errno. For a
/// `popen` stream the value is the child's raw wait status — `exit 3` reports `768`, which
/// is `3 << 8`, because that is what `pclose` returns.
fn method_close(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let slot = slot_of(interp, &this)?;
    let mut files = slab.borrow_mut();
    let Some(entry) = files.get_mut(slot) else {
        return Err(interp.type_error("invalid file handle"));
    };
    let Some(state) = entry.as_mut() else {
        return Err(interp.type_error("invalid file handle"));
    };
    if state.closed {
        return Err(interp.type_error("invalid file handle"));
    }
    let flush_errno = state.flush(interp);
    state.closed = true;
    let kind = std::mem::replace(&mut state.kind, Kind::Sink);
    // Taking the state out of the table closes its descriptors by dropping them.
    *entry = None;
    let errno = match kind {
        Kind::Child { mut child, pipe } => {
            drop(pipe);
            match child.wait() {
                Ok(status) => status_to_raw(status),
                Err(error) => -error.raw_os_error().unwrap_or(EIO),
            }
        }
        _ => {
            if flush_errno == 0 {
                0
            } else {
                -flush_errno
            }
        }
    };
    Ok(Value::Int(errno))
}

/// A `wait` status in the raw form `pclose` returns.
#[cfg(unix)]
fn status_to_raw(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match status.code() {
        Some(code) => code << 8,
        // Killed by a signal: the low seven bits carry the signal number.
        None => status.signal().unwrap_or(0),
    }
}

/// `puts(...texts)` — concatenated, no separator and no newline.
fn method_puts(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let mut text = String::new();
    for value in args {
        text.push_str(&interp.to_js_string(value)?);
    }
    with_file(interp, &this, slab, |state, interp| {
        state.write(interp, text.as_bytes());
        Value::Undefined
    })
}

/// `printf(format, ...args)` — returns the byte count, like `std.printf`.
fn method_printf(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let text = crate::printf::sprintf(interp, args)?;
    let written = text.len();
    with_file(interp, &this, slab, |state, interp| {
        state.write(interp, text.as_bytes());
        Value::Float(written as f64)
    })
}

/// `flush()`.
fn method_flush(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, interp| {
        state.flush(interp);
        Value::Undefined
    })
}

/// `tell()` — the position, or `-1` for a stream that is not seekable (`ftello` on a pipe).
fn method_tell(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, _| {
        Value::Float(if state.seekable { state.pos } else { -1 } as f64)
    })
}

/// `tello()` — the same number as a `BigInt`, for files past 2^53.
fn method_tello(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let position = with_file(interp, &this, slab, |state, _| {
        if state.seekable { state.pos } else { -1 }
    })?;
    Ok(interp.bigint(quickjs::bigint::BigInt::from_i64(position)))
}

/// `seek(offset, whence)` — `0` on success, a negative errno otherwise.
fn method_seek(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let offset = args.first().map_or(0, |value| value.to_number() as i64);
    let whence = args.get(1).map_or(0, |value| value.to_number() as i32);
    with_file(interp, &this, slab, |state, interp| {
        match state.seek(interp, offset, whence) {
            0 => Value::Int(0),
            errno => Value::Int(-errno),
        }
    })
}

/// `eof()` — whether a read has reached the end.
fn method_eof(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, _| Value::Bool(state.eof))
}

/// `error()` — whether a read or write has failed.
fn method_error(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, _| Value::Bool(state.error))
}

/// `clearerr()` — clear both indicators.
fn method_clearerr(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, _| {
        state.eof = false;
        state.error = false;
        Value::Undefined
    })
}

/// `fileno()` — the descriptor behind the stream.
fn method_fileno(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    with_file(interp, &this, slab, |state, _| {
        Value::Int(state.file_number())
    })
}

/// The `(buffer, offset, length)` a `read`/`write` covers.
fn buffer_window(interp: &mut Interp<'_>, args: &[Value]) -> JsResult<(Value, usize, usize)> {
    let Some(buffer) = args.first().cloned() else {
        return Err(interp.type_error("missing buffer"));
    };
    let offset = args.get(1).map_or(0, |value| value.to_number() as usize);
    let length = args.get(2).map_or(0, |value| value.to_number() as usize);
    Ok((buffer, offset, length))
}

/// `read(buffer, offset, length)` — bytes read, or a `RangeError` when the window does not
/// fit the buffer. The reference checks the window against the *whole* buffer, not the view.
fn method_read(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let (buffer, offset, length) = buffer_window(interp, args)?;
    let Some(storage) = crate::os::buffer_storage(&buffer) else {
        return Err(interp.type_error("not an ArrayBuffer"));
    };
    let size = storage.borrow().bytes.len();
    if offset + length > size {
        return Err(interp.range_error("read/write array buffer overflow"));
    }
    let count = with_file(interp, &this, slab, |state, _| {
        let mut window = storage.borrow_mut();
        state.read_into(&mut window.bytes[offset..offset + length])
    })?;
    Ok(Value::Float(count as f64))
}

/// `write(buffer, offset, length)` — bytes written.
fn method_write(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let (buffer, offset, length) = buffer_window(interp, args)?;
    let Some(storage) = crate::os::buffer_storage(&buffer) else {
        return Err(interp.type_error("not an ArrayBuffer"));
    };
    let size = storage.borrow().bytes.len();
    if offset + length > size {
        return Err(interp.range_error("read/write array buffer overflow"));
    }
    let bytes = storage.borrow().bytes[offset..offset + length].to_vec();
    let count = with_file(interp, &this, slab, |state, interp| {
        state.write(interp, &bytes)
    })?;
    Ok(Value::Float(count as f64))
}

/// `getline()` — one line without its newline, or `null` at end of file.
///
/// Only `\n` ends a line, and only `\n` is dropped: a `\r` stays, which is what the
/// reference's `fgetc` loop does.
fn method_getline(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let line = with_file(interp, &this, slab, |state, _| {
        let mut bytes = Vec::new();
        loop {
            match state.next_byte() {
                None => {
                    if bytes.is_empty() {
                        return None;
                    }
                    break;
                }
                Some(b'\n') => break,
                Some(byte) => bytes.push(byte),
            }
        }
        Some(bytes)
    })?;
    Ok(match line {
        Some(bytes) => interp.string(String::from_utf8_lossy(&bytes).into_owned()),
        None => Value::Null,
    })
}

/// `readAsString(max_size)` — the rest of the stream, or `max_size` bytes of it.
fn method_read_as_string(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let max = args.first().and_then(|value| match value {
        Value::Undefined => None,
        other => Some(other.to_number().max(0.0) as usize),
    });
    let bytes = with_file(interp, &this, slab, |state, _| {
        let mut bytes = Vec::new();
        while max.is_none_or(|max| bytes.len() < max) {
            match state.next_byte() {
                Some(byte) => bytes.push(byte),
                None => break,
            }
        }
        bytes
    })?;
    Ok(interp.string(String::from_utf8_lossy(&bytes).into_owned()))
}

/// `getByte()` — one byte as a number, or `-1` at end of file (`fgetc`'s `EOF`).
fn method_get_byte(
    interp: &mut Interp<'_>,
    this: Value,
    _args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let byte = with_file(interp, &this, slab, |state, _| state.next_byte())?;
    Ok(Value::Int(byte.map_or(-1, i32::from)))
}

/// `putByte(value)` — writes the low byte and returns what `fputc` returns: that byte.
fn method_put_byte(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    slab: &Slab,
) -> JsResult<Value> {
    let byte = args.first().map_or(0, |value| value.to_number() as i32) as u8;
    with_file(interp, &this, slab, |state, interp| {
        let written = state.write(interp, &[byte]);
        Value::Int(if written == 1 { i32::from(byte) } else { -1 })
    })
}

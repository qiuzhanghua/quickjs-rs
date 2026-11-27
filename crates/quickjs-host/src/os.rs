//! The `os` module: the file, directory and process surface.
//!
//! # The error protocol is not exceptions
//!
//! The reference has **two** failure conventions and both are load-bearing. It is worth
//! stating both plainly, because a single consistent one is the obvious "improvement" and
//! it is wrong.
//!
//! **Pair functions** return `[value, errno]` with a **positive** errno, built by
//! `make_obj_error`/`make_string_error` in `quickjs-libc.c`:
//!
//! ```text
//! os.stat os.lstat os.readlink os.realpath os.readdir os.getcwd os.utimes
//! ```
//!
//! **Bare functions** return the value itself, and a **negative** errno on failure — that
//! is `js_get_errno`, which is `if (ret == -1) ret = -errno;`:
//!
//! ```text
//! os.open os.close os.seek os.read os.write os.mkdir os.remove os.rename os.chdir
//! ```
//!
//! ```text
//! > os.stat("/nonexistent")     [null, 2]     // positive errno, in a pair
//! > os.mkdir("/tmp", 0o755)     -17           // negative errno, bare
//! > os.open("/tmp", os.O_RDONLY) 3            // the fd itself, bare
//! ```
//!
//! Two more asymmetries in the same spirit: `os.isatty` returns a bare Boolean, and
//! `os.pipe` returns a bare `[read_fd, write_fd]` (a `null` on failure, not a pair). The
//! per-function expectations live in `crates/quickjs-host/tests/os_diff.rs`, generated from
//! `qjs --std` rather than from this comment.
//!
//! # Why `rustix`, and where `unsafe` would otherwise have gone
//!
//! NFR-6 confines `unsafe` to four engine modules. Two things here would have broken that:
//! calling `libc` directly, and turning a script-supplied fd number back into a descriptor
//! (`BorrowedFd::borrow_raw` is `unsafe`). `rustix` covers the first; [`crate::fdtable`]
//! covers the second by keeping the `OwnedFd` on the host side. So this module has no
//! `unsafe` at all.
//!
//! # Descriptors the host does not own
//!
//! `os.read(0, …)` and `os.write(1, …)` reach the real standard streams through
//! `std::io::stdin()`/`stdout()`, which are safe. Anything else the script did not open
//! through `os.open`/`os.pipe` reports `EBADF`, because this process genuinely does not own
//! it — and saying so is better than guessing.

use quickjs::{Context, Interp, JsResult, Value};
use rustix::fs::{FileType, Mode, OFlags};

use crate::fdtable::{FdTable, Shared};

/// How often a path argument is read.
fn path(interp: &mut Interp<'_>, args: &[Value], index: usize) -> JsResult<String> {
    match args.get(index) {
        Some(value) => interp.to_js_string(value),
        None => Ok(String::new()),
    }
}

/// `[value, errno]`.
fn pair(interp: &mut Interp<'_>, value: Value, errno: i32) -> Value {
    Value::Object(interp.new_array(vec![value, Value::Int(errno)]))
}

fn ok_pair(interp: &mut Interp<'_>, value: Value) -> Value {
    pair(interp, value, 0)
}

fn errno_of(error: &rustix::io::Errno) -> i32 {
    error.raw_os_error()
}

/// `js_get_errno`: the bare convention, where failure is reported as a *negative* errno.
///
/// `ret == -1` in C means "look at errno"; the Rust equivalent is the `Err` arm.
fn signed(interp: &mut Interp<'_>, result: rustix::io::Result<i64>) -> Value {
    let _ = interp;
    match result {
        Ok(value) => Value::Float(value as f64),
        Err(error) => Value::Int(-errno_of(&error)),
    }
}

/// The bare convention for functions whose success value is itself an errno-shaped zero.
fn signed_zero(result: rustix::io::Result<()>) -> Value {
    match result {
        Ok(()) => Value::Int(0),
        Err(error) => Value::Int(-errno_of(&error)),
    }
}

/// `EBADF`, reported for a descriptor this process does not own.
const EBADF: i32 = 9;
/// `EINVAL`, reported for an argument of the wrong shape.
const EINVAL: i32 = 22;

/// Install `os` into `object`, sharing `table` with `std`.
///
/// The table is the caller's because `std.fdopen` has to see the descriptors `os.open` and
/// `os.pipe` produced. See [`crate::fdtable::Shared`].
pub fn install(ctx: &mut Context<'_>, object: &Value, table: &Shared) -> quickjs::Result<()> {
    if object.as_object().is_none() {
        return Err(quickjs::Error::Exception(
            "os: target is not an object".to_string(),
        ));
    }
    for (name, length, body) in functions(table) {
        let function = ctx.new_host_function_value(name, length, body)?;
        ctx.set_property(object, name, function)?;
    }
    for (name, value) in constants() {
        ctx.set_property(object, name, Value::Int(value))?;
    }
    // `os.platform` is a string property in the reference, not a function.
    let platform = ctx.new_string(PLATFORM);
    ctx.set_property(object, "platform", platform)?;
    Ok(())
}

type Body = Box<dyn for<'i> FnMut(&mut Interp<'i>, Value, &[Value]) -> JsResult<Value>>;

fn functions(table: &Shared) -> Vec<(&'static str, u32, Body)> {
    let mut out: Vec<(&'static str, u32, Body)> = Vec::new();
    macro_rules! add {
        ($name:literal, $length:expr, $body:expr) => {{
            let table = table.clone();
            out.push((
                $name,
                $length,
                Box::new(move |interp, this, args| $body(interp, this, args, &table)),
            ));
        }};
    }
    add!("open", 3, os_open);
    add!("close", 1, os_close);
    add!("seek", 3, os_seek);
    add!("read", 4, os_read);
    add!("write", 4, os_write);
    add!("pipe", 0, os_pipe);
    add!("remove", 1, os_remove);
    add!("rename", 2, os_rename);
    add!("stat", 1, os_stat);
    add!("lstat", 1, os_lstat);
    add!("utimes", 3, os_utimes);
    add!("realpath", 1, os_realpath);
    add!("symlink", 2, os_symlink);
    add!("readlink", 1, os_readlink);
    add!("mkdir", 2, os_mkdir);
    add!("readdir", 1, os_readdir);
    add!("chdir", 1, os_chdir);
    add!("getcwd", 0, os_getcwd);
    add!("isatty", 1, os_isatty);
    add!("getpid", 0, os_getpid);
    add!("exec", 1, os_exec);
    add!("waitpid", 2, os_waitpid);
    add!("kill", 2, os_kill);
    add!("dup", 1, os_dup);
    add!("dup2", 2, os_dup2);
    add!("now", 0, os_now);
    add!("sleep", 1, os_sleep);
    add!("sleepAsync", 1, os_sleep_async);
    add!("ttyGetWinSize", 1, os_tty_get_win_size);
    add!("ttySetRaw", 1, os_tty_set_raw);
    out
}

/// The `O_*`, `S_IF*` and `SIG*` constants, projected from `rustix`'s flag types.
///
/// They land as plain `Int32` properties (`JS_PROP_INT32_DEF` in the reference), so
/// `os.O_RDWR` is `2` rather than an opaque flag object.
fn constants() -> Vec<(&'static str, i32)> {
    use rustix::process::Signal as S;
    let o = |flag: OFlags| flag.bits() as i32;
    // `rustix` models the `S_IF*` values as `FileType` discriminants rather than `Mode`
    // bits: the same numbers in a different shape.
    let f = |kind: FileType| kind as isize as i32;
    vec![
        ("O_RDONLY", o(OFlags::RDONLY)),
        ("O_WRONLY", o(OFlags::WRONLY)),
        ("O_RDWR", o(OFlags::RDWR)),
        ("O_APPEND", o(OFlags::APPEND)),
        ("O_CREAT", o(OFlags::CREATE)),
        ("O_EXCL", o(OFlags::EXCL)),
        ("O_TRUNC", o(OFlags::TRUNC)),
        ("SEEK_SET", 0),
        ("SEEK_CUR", 1),
        ("SEEK_END", 2),
        // POSIX fixes the file-type mask at 0170000; Linux and macOS agree.
        ("S_IFMT", 0o170000),
        ("S_IFIFO", f(FileType::Fifo)),
        ("S_IFCHR", f(FileType::CharacterDevice)),
        ("S_IFDIR", f(FileType::Directory)),
        ("S_IFBLK", f(FileType::BlockDevice)),
        ("S_IFREG", f(FileType::RegularFile)),
        ("S_IFLNK", f(FileType::Symlink)),
        ("S_IFSOCK", f(FileType::Socket)),
        ("S_ISUID", Mode::SUID.bits() as i32),
        ("S_ISGID", Mode::SGID.bits() as i32),
        ("SIGINT", S::INT.as_raw()),
        ("SIGABRT", S::ABORT.as_raw()),
        ("SIGALRM", S::ALARM.as_raw()),
        ("SIGCHLD", S::CHILD.as_raw()),
        ("SIGCONT", S::CONT.as_raw()),
        ("SIGFPE", S::FPE.as_raw()),
        ("SIGILL", S::ILL.as_raw()),
        ("SIGPIPE", S::PIPE.as_raw()),
        ("SIGQUIT", S::QUIT.as_raw()),
        ("SIGSEGV", S::SEGV.as_raw()),
        ("SIGSTOP", S::STOP.as_raw()),
        ("SIGTERM", S::TERM.as_raw()),
        ("SIGTSTP", S::TSTP.as_raw()),
        ("SIGTTIN", S::TTIN.as_raw()),
        ("SIGTTOU", S::TTOU.as_raw()),
        // `waitpid`'s "do not block" flag, which `test_os_exec()` passes directly.
        (
            "WNOHANG",
            rustix::process::WaitOptions::NOHANG.bits() as i32,
        ),
        ("SIGUSR1", S::USR1.as_raw()),
        ("SIGUSR2", S::USR2.as_raw()),
    ]
}

/// `os.platform`, matching the reference's `OS_PLATFORM` macro.
pub const PLATFORM: &str = if cfg!(target_os = "macos") {
    "darwin"
} else if cfg!(target_os = "linux") {
    "linux"
} else {
    "unknown"
};

/// `os.open(path, flags, mode)` → `[fd, errno]`.
fn os_open(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    let flags = OFlags::from_bits_truncate(args.get(1).map_or(0, |v| v.to_number() as u32));
    let mode = Mode::from_bits_truncate(args.get(2).map_or(0o666, |v| v.to_number() as u16));
    // Bare, not a pair: `js_os_open` ends with `return JS_NewInt32(ctx, ret)`.
    match rustix::fs::open(&name, flags | OFlags::CLOEXEC, mode) {
        Ok(fd) => Ok(Value::Int(table.adopt(fd))),
        Err(error) => Ok(Value::Int(-errno_of(&error))),
    }
}

/// `os.close(fd)` → `errno`.
fn os_close(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    let _ = interp;
    // The table reports a positive errno internally (it is not a syscall); the script sees
    // the negative form, like every other bare function.
    let fd = args.first().map_or(-1, |v| v.to_number() as i32);
    let errno = table.close(fd);
    Ok(Value::Int(if errno == 0 { 0 } else { -errno }))
}

/// `os.seek(fd, offset, whence)` → `[position, errno]`.
fn os_seek(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    use rustix::fs::{SeekFrom, seek};
    let fd = args.first().map_or(-1, |v| v.to_number() as i32);
    let offset = args.get(1).map_or(0, |v| v.to_number() as i64);
    let whence = args.get(2).map_or(0, |v| v.to_number() as i32);
    let from = match whence {
        0 => SeekFrom::Start(offset.max(0) as u64),
        1 => SeekFrom::Current(offset),
        _ => SeekFrom::End(offset),
    };
    // Bare: `js_os_seek` ends with `JS_NewInt64`.
    let Some(result) = table.with(fd, |handle| seek(handle, from)) else {
        return Ok(Value::Int(-EBADF));
    };
    Ok(signed(interp, result.map(|position| position as i64)))
}

/// The byte storage behind an `ArrayBuffer`, or behind a typed-array view of one.
///
/// The reference reads through `JS_GetArrayBuffer`, which accepts the buffer a typed array
/// is a view of — so `os.read(fd, buf.buffer, 0, len)` and `os.read(fd, buf, 0, len)` both
/// work there. Following `typed_array_parts` gives the same tolerance here.
pub(crate) fn buffer_storage(
    buffer: &Value,
) -> Option<std::rc::Rc<std::cell::RefCell<quickjs::object::BufferStorage>>> {
    let object = buffer.as_object()?;
    if let Some(storage) = object.array_buffer_storage() {
        return Some(storage);
    }
    let (backing, _, _, _) = object.typed_array_parts()?;
    backing.array_buffer_storage()
}

/// The byte window `(offset, end)` a read or write covers, clamped to the buffer.
fn window(buffer: &Value, args: &[Value]) -> Option<(usize, usize)> {
    let offset = args.get(2).map_or(0, |v| v.to_number() as usize);
    let length = args.get(3).map_or(0, |v| v.to_number() as usize);
    let size = buffer_storage(buffer)?.borrow().bytes.len();
    let end = offset.saturating_add(length).min(size);
    (offset < end).then_some((offset, end))
}

/// `os.read(fd, buffer, offset, length)` → `[bytes_read, errno]`.
///
/// The buffer is a `Uint8Array` (or any typed array) and the offsets are bytes, which is
/// what makes `os.read(fd, buf.buffer, 0, buf.length)` the idiom it is.
fn os_read(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    let fd = args.first().map_or(-1, |v| v.to_number() as i32);
    let Some(buffer) = args.get(1).cloned() else {
        return Ok(pair(interp, Value::Null, EINVAL));
    };
    let Some(bytes) = buffer_storage(&buffer) else {
        return Ok(pair(interp, Value::Null, EINVAL));
    };
    let Some((offset, end)) = window(&buffer, args) else {
        return Ok(Value::Int(0));
    };
    let Some(result) = table.with(fd, |handle| {
        let mut chunk = vec![0u8; end - offset];
        let read = rustix::io::read(handle, chunk.as_mut_slice());
        (chunk, read)
    }) else {
        return Ok(Value::Int(-EBADF));
    };
    let (chunk, read) = result;
    // Bare: `js_os_read_write` ends with `JS_NewInt64(ctx, ret)`.
    match read {
        Ok(count) => {
            bytes.borrow_mut().bytes[offset..offset + count].copy_from_slice(&chunk[..count]);
            Ok(Value::Float(count as f64))
        }
        Err(error) => Ok(Value::Int(-errno_of(&error))),
    }
}

/// `os.write(fd, buffer, offset, length)` → `[bytes_written, errno]`.
fn os_write(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    let fd = args.first().map_or(-1, |v| v.to_number() as i32);
    let Some(buffer) = args.get(1).cloned() else {
        return Ok(pair(interp, Value::Null, EINVAL));
    };
    let Some(bytes) = buffer_storage(&buffer) else {
        return Ok(pair(interp, Value::Null, EINVAL));
    };
    let Some((offset, end)) = window(&buffer, args) else {
        return Ok(Value::Int(0));
    };
    let chunk = bytes.borrow().bytes[offset..end].to_vec();
    let Some(result) = table.with(fd, |handle| rustix::io::write(handle, &chunk)) else {
        return Ok(Value::Int(-EBADF));
    };
    Ok(signed(interp, result.map(|count| count as i64)))
}

/// `os.pipe()` → `[read_fd, write_fd]`.
///
/// Not a `[value, errno]` pair: the reference returns the two descriptors directly, and a
/// failure is a `TypeError` rather than an errno.
fn os_pipe(
    interp: &mut Interp<'_>,
    _this: Value,
    _args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    match rustix::pipe::pipe() {
        Ok((reader, writer)) => {
            let read = table.adopt(reader);
            let write = table.adopt(writer);
            Ok(Value::Object(
                interp.new_array(vec![Value::Int(read), Value::Int(write)]),
            ))
        }
        // `js_os_pipe` returns `JS_NULL` on failure, not a pair and not an exception.
        Err(_) => Ok(Value::Null),
    }
}

/// `os.remove(path)` → `errno`.
fn os_remove(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    // `unlink`, not `rmdir`: the reference reports EPERM/EISDIR for a directory rather than
    // falling back, so a script that passes one gets the same answer as upstream.
    // `remove(3)`, which unlinks a file *or* rmdirs a directory — the reference calls
    // `remove`, not `unlink`, so `os.remove("/tmp/somedir")` succeeds there.
    let removed = match std::fs::remove_file(&name) {
        Ok(()) => Ok(()),
        // `remove(3)` falls back to `rmdir` when `unlink` refuses a directory.
        Err(_) => std::fs::remove_dir(&name),
    };
    Ok(match removed {
        Ok(()) => Value::Int(0),
        Err(error) => Value::Int(-error.raw_os_error().unwrap_or(EINVAL)),
    })
}

/// `os.rename(from, to)` → `errno`.
fn os_rename(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let from = path(interp, args, 0)?;
    let to = path(interp, args, 1)?;
    Ok(signed_zero(rustix::fs::rename(&from, &to)))
}

/// `os.stat(path)` → `[stat, errno]`.
fn os_stat(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    stat_into(interp, rustix::fs::stat(&name))
}

/// `os.lstat(path)` → `[stat, errno]`, not following a final symlink.
fn os_lstat(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    stat_into(interp, rustix::fs::lstat(&name))
}

/// Build the JS object for a `struct stat`, in the reference's field order.
///
/// `mode` is an `Int32` while the rest are numbers, and `atime`/`mtime`/`ctime` are
/// **milliseconds** because scripts compare them against `Date.now()`.
fn stat_into(
    interp: &mut Interp<'_>,
    result: rustix::io::Result<rustix::fs::Stat>,
) -> JsResult<Value> {
    let stat = match result {
        Ok(stat) => stat,
        Err(error) => return Ok(pair(interp, Value::Null, errno_of(&error))),
    };
    let object = interp.new_object(None);
    let ms = |seconds: i64, nanos: i64| Value::Float(seconds as f64 * 1000.0 + nanos as f64 / 1e6);
    let fields: [(&str, Value); 12] = [
        ("dev", Value::Float(stat.st_dev as f64)),
        ("ino", Value::Float(stat.st_ino as f64)),
        ("mode", Value::Int(stat.st_mode as i32)),
        ("nlink", Value::Float(stat.st_nlink as f64)),
        ("uid", Value::Float(stat.st_uid as f64)),
        ("gid", Value::Float(stat.st_gid as f64)),
        ("rdev", Value::Float(stat.st_rdev as f64)),
        ("size", Value::Float(stat.st_size as f64)),
        ("blocks", Value::Float(stat.st_blocks as f64)),
        ("atime", ms(stat.st_atime, stat.st_atime_nsec)),
        ("mtime", ms(stat.st_mtime, stat.st_mtime_nsec)),
        ("ctime", ms(stat.st_ctime, stat.st_ctime_nsec)),
    ];
    let _ = interp;
    for (name, value) in fields {
        object.define_data(name, value);
    }
    Ok(ok_pair(interp, Value::Object(object)))
}

/// `os.utimes(path, atime_ms, mtime_ms)` → `errno`.
fn os_utimes(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    let to_timespec = |value: Option<&Value>| {
        let ms = value.map_or(0.0, Value::to_number);
        rustix::fs::Timespec {
            tv_sec: (ms / 1000.0) as i64,
            tv_nsec: ((ms % 1000.0) * 1e6) as i64,
        }
    };
    let times = rustix::fs::Timestamps {
        last_access: to_timespec(args.get(1)),
        last_modification: to_timespec(args.get(2)),
    };
    Ok(signed_zero(rustix::fs::utimensat(
        rustix::fs::CWD,
        &name,
        &times,
        rustix::fs::AtFlags::empty(),
    )))
}

/// `os.realpath(path)` → `[path, errno]`.
fn os_realpath(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    // `realpath` is not in `rustix`; `std`'s is the same call behind a safe signature.
    match std::fs::canonicalize(&name) {
        Ok(resolved) => {
            let text = resolved.to_string_lossy().into_owned();
            Ok(ok_pair(interp, interp.string(text)))
        }
        // An empty string, for the same reason as `readlink`.
        Err(error) => Ok(pair(
            interp,
            interp.string(""),
            error.raw_os_error().unwrap_or(EINVAL),
        )),
    }
}

/// `os.symlink(target, path)` → `errno`.
fn os_symlink(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let target = path(interp, args, 0)?;
    let name = path(interp, args, 1)?;
    Ok(Value::Int(match rustix::fs::symlink(&target, &name) {
        Ok(()) => 0,
        Err(error) => errno_of(&error),
    }))
}

/// `os.readlink(path)` → `[target, errno]`.
fn os_readlink(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    match rustix::fs::readlink(&name, Vec::new()) {
        Ok(target) => {
            let text = target.to_string_lossy().into_owned();
            Ok(ok_pair(interp, interp.string(text)))
        }
        // `make_string_error` puts an *empty string* in the value slot, not `null`.
        Err(error) => Ok(pair(interp, interp.string(""), errno_of(&error))),
    }
}

/// `os.mkdir(path, mode)` → `errno`.
fn os_mkdir(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    let mode = Mode::from_bits_truncate(args.get(1).map_or(0o777, |v| v.to_number() as u16));
    Ok(signed_zero(rustix::fs::mkdir(&name, mode)))
}

/// `os.readdir(path)` → `[names, errno]`, sorted.
///
/// Sorted because two listings of the same directory have to compare equal in a script;
/// the reference sorts for the same reason.
fn os_readdir(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    // `Dir` takes a directory *descriptor*, so the path is opened first. `DIRECTORY` makes
    // the kernel refuse a regular file with ENOTDIR instead of an empty listing.
    let opened = match rustix::fs::open(
        &name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) => return Ok(pair(interp, Value::Null, errno_of(&error))),
    };
    let mut directory = match rustix::fs::Dir::read_from(opened) {
        Ok(directory) => directory,
        Err(error) => return Ok(pair(interp, Value::Null, errno_of(&error))),
    };
    let mut names: Vec<String> = Vec::new();
    // `Dir` yields `Result` per entry; a damaged one ends the listing rather than failing
    // the whole call, which is what the reference's loop does.
    while let Some(entry) = directory.read() {
        let Ok(entry) = entry else { break };
        let name = entry.file_name().to_string_lossy().into_owned();
        // `.` and `..` come from the directory stream but not from the reference's list.
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    names.sort();
    let elements = names.into_iter().map(|name| interp.string(name)).collect();
    let array = interp.new_array(elements);
    Ok(ok_pair(interp, Value::Object(array)))
}

/// `os.chdir(path)` → `errno`.
fn os_chdir(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let name = path(interp, args, 0)?;
    Ok(signed_zero(rustix::process::chdir(&name)))
}

/// `os.getcwd()` → `[path, errno]`.
fn os_getcwd(
    interp: &mut Interp<'_>,
    _this: Value,
    _args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    match rustix::process::getcwd(Vec::new()) {
        Ok(directory) => {
            let text = directory.to_string_lossy().into_owned();
            Ok(ok_pair(interp, interp.string(text)))
        }
        Err(error) => Ok(pair(interp, interp.string(""), errno_of(&error))),
    }
}

/// `os.isatty(fd)` → `bool`.
///
/// A bare Boolean, not a pair — one of the asymmetries that makes this module look
/// inconsistent until it is read against `quickjs-libc.c`.
fn os_isatty(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &FdTable,
) -> JsResult<Value> {
    let _ = interp;
    let fd = args.first().map_or(-1, |v| v.to_number() as i32);
    // The closure is spelled out rather than passed as `rustix::termios::isatty`: a
    // function item is not general enough over the borrow lifetime `with` hands it.
    Ok(Value::Bool(
        table
            .with(fd, |handle| rustix::termios::isatty(handle))
            .unwrap_or(false),
    ))
}

/// `os.getpid()` → `number`.
fn os_getpid(
    interp: &mut Interp<'_>,
    _this: Value,
    _args: &[Value],
    _table: &FdTable,
) -> JsResult<Value> {
    let _ = interp;
    Ok(Value::Float(
        rustix::process::getpid().as_raw_nonzero().get() as f64,
    ))
}

// ---------------------------------------------------------------------------
// Processes: `exec`, `waitpid`, `kill`, `dup`, `dup2`.
// ---------------------------------------------------------------------------

/// The `stdin`/`stdout`/`stderr` option names, in the order the descriptors are numbered.
const STD_NAMES: [&str; 3] = ["stdin", "stdout", "stderr"];

/// Read `options[name]` as a Boolean, defaulting when the option is absent.
///
/// The reference's `get_bool_option` runs `JS_ToBool` on whatever is there, so `0`, `""`
/// and `null` all turn an option *off* — `{ block: 0 }` is asynchronous, not blocking.
fn bool_option(interp: &mut Interp<'_>, options: &Value, name: &str) -> JsResult<Option<bool>> {
    let Some(object) = options.as_object() else {
        return Ok(None);
    };
    let key = quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new(name));
    match interp.get_property_key(object, &key)? {
        None | Some(Value::Undefined) => Ok(None),
        Some(value) => Ok(Some(value.to_boolean())),
    }
}

/// Read `options[name]` as a number when present.
fn number_option(interp: &mut Interp<'_>, options: &Value, name: &str) -> JsResult<Option<f64>> {
    let Some(object) = options.as_object() else {
        return Ok(None);
    };
    let key = quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new(name));
    match interp.get_property_key(object, &key)? {
        None | Some(Value::Undefined) => Ok(None),
        Some(value) => Ok(Some(value.to_number())),
    }
}

/// Read `options[name]` as a string, distinguishing absent from empty.
fn string_option(interp: &mut Interp<'_>, options: &Value, name: &str) -> JsResult<Option<String>> {
    let Some(object) = options.as_object() else {
        return Ok(None);
    };
    let key = quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new(name));
    match interp.get_property_key(object, &key)? {
        None | Some(Value::Undefined) => Ok(None),
        Some(value) => Ok(Some(interp.to_js_string(&value)?)),
    }
}

/// `os.exec(args, options?)` — run a program.
///
/// Returns the exit code, or a **negative** signal number when the child was killed, or the
/// pid when `options.block` is false. A program that cannot be started exits 127, which is
/// what the reference's child does after a failed `execve` — so the caller sees 127 rather
/// than an exception.
///
/// # The options
///
/// `stdin`/`stdout`/`stderr` name descriptors this host knows about, which is more
/// restrictive than the reference: it does `dup2(fd, i)` on whatever number it is given, so
/// a script could redirect to a descriptor it opened some other way. Here the number has to
/// be in the shared descriptor table — the same rule `os.read` follows — and an unknown one
/// makes the child fail to start (exit 127) instead of silently redirecting nowhere.
fn os_exec(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &Shared,
) -> JsResult<Value> {
    // `args` is array-like: the reference reads `length` and then each index, so anything
    // with those properties works and a plain string is `length`-less (undefined → 0 → the
    // "invalid number of arguments" error below).
    let Some(array) = args.first() else {
        return Err(interp.type_error("invalid number of arguments"));
    };
    let count = match array.as_object() {
        Some(object) => {
            let key = quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new("length"));
            match interp.get_property_key(object, &key)? {
                Some(value) => value.to_number() as i64,
                None => 0,
            }
        }
        None => 0,
    };
    if !(1..=65535).contains(&count) {
        return Err(interp.type_error("invalid number of arguments"));
    }
    let Some(object) = array.as_object() else {
        return Err(interp.type_error("invalid number of arguments"));
    };
    let mut argv: Vec<String> = Vec::with_capacity(count as usize);
    for index in 0..count {
        let key =
            quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new(index.to_string()));
        let value = interp
            .get_property_key(object, &key)?
            .unwrap_or(Value::Undefined);
        argv.push(interp.to_js_string(&value)?);
    }

    let options = args.get(1).cloned().unwrap_or(Value::Undefined);
    let block = bool_option(interp, &options, "block")?.unwrap_or(true);
    let use_path = bool_option(interp, &options, "usePath")?.unwrap_or(true);
    let file = string_option(interp, &options, "file")?;
    let cwd = string_option(interp, &options, "cwd")?;
    // `uid`/`gid` need `setuid`/`setgid`, which are process-wide and unsafe to attempt on a
    // running interpreter. They are read so an unknown value cannot slip through as "unset",
    // and refused explicitly rather than silently ignored.
    if number_option(interp, &options, "uid")?.is_some()
        || number_option(interp, &options, "gid")?.is_some()
    {
        return Err(interp.type_error("os.exec: uid/gid are not supported"));
    }

    let mut command = std::process::Command::new(file.as_deref().unwrap_or(&argv[0]));
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    if !use_path {
        // `execve` does not search `PATH`; `Command` does. Naming the program by the exact
        // path the script gave, with an empty `PATH`, reproduces the no-search behaviour
        // without a second exec implementation.
        command.env("PATH", "");
    }
    if let Some(directory) = &cwd {
        command.current_dir(directory);
    }
    // `env` replaces the whole environment rather than adding to it, which is what
    // `build_envp` does in the reference.
    if let Some(env) = options.as_object() {
        let key = quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new("env"));
        if let Some(value) = interp.get_property_key(env, &key)? {
            if !matches!(value, Value::Undefined) {
                let Some(entries) = value.as_object() else {
                    return Err(interp.type_error("env is not an object"));
                };
                command.env_clear();
                for key in entries.own_keys() {
                    let name = key.to_string_lossy().into_owned();
                    let property =
                        quickjs::object::PropertyKey::String(quickjs::value::Wtf8::new(&name));
                    let Some(field) = interp.get_property_key(entries, &property)? else {
                        continue;
                    };
                    command.env(&name, interp.to_js_string(&field)?);
                }
            }
        }
    }

    // The three standard descriptors, defaulting to the real ones.
    let std_fds: [Option<i32>; 3] = [
        number_option(interp, &options, STD_NAMES[0])?.map(|n| n as i32),
        number_option(interp, &options, STD_NAMES[1])?.map(|n| n as i32),
        number_option(interp, &options, STD_NAMES[2])?.map(|n| n as i32),
    ];
    // Each redirect is resolved to an owned duplicate before the spawn, where the table is
    // still borrowable; `Stdio::from` then hands it to the child.
    let mut redirects: [Option<std::os::fd::OwnedFd>; 3] = [None, None, None];
    for (index, wanted) in std_fds.iter().enumerate() {
        let Some(wanted) = *wanted else { continue };
        let duplicated = table.with(wanted, |fd| rustix::io::dup(fd));
        let Some(Ok(copy)) = duplicated else {
            // An unknown descriptor is a child that cannot start, which the reference also
            // reports as 127 (its `_exit(127)` after a failed `dup2`).
            return Ok(Value::Int(127));
        };
        redirects[index] = Some(copy);
    }
    if let Some(input) = redirects[0].take() {
        command.stdin(std::process::Stdio::from(input));
    }
    if let Some(output) = redirects[1].take() {
        command.stdout(std::process::Stdio::from(output));
    }
    if let Some(error) = redirects[2].take() {
        command.stderr(std::process::Stdio::from(error));
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        // `fork`/`exec` failing is not an exception in the reference: the child `_exit(127)`s
        // and the parent reports that as the status.
        Err(_) => return Ok(Value::Int(127)),
    };
    if !block {
        let pid = child.id() as i32;
        // The child has to outlive this call for a later `waitpid` to find it. Leaking the
        // handle leaves the process running and unreaped by Rust, which is exactly what the
        // script expects: `os.waitpid(pid, …)` is how it is collected.
        std::mem::forget(child);
        return Ok(Value::Int(pid));
    }
    match child.wait() {
        Ok(status) => Ok(Value::Int(exit_status_of(status))),
        Err(_) => Ok(Value::Int(127)),
    }
}

/// An exit status as `os.exec` reports it: the code, or `-signal` when killed.
fn exit_status_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => code,
        (None, Some(signal)) => -signal,
        (None, None) => 127,
    }
}

/// `os.waitpid(pid, options)` → `[pid, status]`, or `[-errno, 0]` when it fails.
///
/// The status is the **raw** `wait` status, so a script tests it with `status & 0x7f` and
/// `status >> 8` — that is what the reference's own `test_os_exec()` does.
fn os_waitpid(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &Shared,
) -> JsResult<Value> {
    let pid = args.first().map_or(-1, |value| value.to_number() as i32);
    let flags = args.get(1).map_or(0, |value| value.to_number() as u32);
    let target = rustix::process::Pid::from_raw(pid);
    let options = rustix::process::WaitOptions::from_bits_truncate(flags);
    let result = rustix::process::waitpid(target, options);
    let (reported, status) = match result {
        Ok(Some((child, status))) => (child.as_raw_nonzero().get(), status.as_raw()),
        // `WNOHANG` with nothing to report: the reference reports pid 0 and status 0.
        Ok(None) => (0, 0),
        Err(error) => (-errno_of(&error), 0),
    };
    let elements = vec![Value::Int(reported), Value::Int(status)];
    Ok(Value::Object(interp.new_array(elements)))
}

/// `os.kill(pid, signal)` → a bare negative errno on failure.
fn os_kill(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &Shared,
) -> JsResult<Value> {
    let _ = interp;
    let pid = args.first().map_or(0, |value| value.to_number() as i32);
    let signal = args.get(1).map_or(0, |value| value.to_number() as i32);
    // `rustix::process::Signal` is a `NonZeroI32` newtype, so it cannot represent the `0`
    // that `kill(pid, 0)` uses as an existence-and-permission probe; rustix points at
    // `rustix-libc-wrappers` for that. Rather than take a second dependency (or reach for
    // `from_raw_unchecked`, whose own docs say it must not be used to send signals), signal
    // 0 reports EINVAL. Recorded as a gap in `docs/m9-plan.md`; every named signal that
    // `os.SIG*` exposes works.
    if signal == 0 {
        return Ok(Value::Int(-EINVAL));
    }
    let result = match rustix::process::Pid::from_raw(pid) {
        Some(pid) => match rustix::process::Signal::from_named_raw(signal) {
            Some(signal) => rustix::process::kill_process(pid, signal),
            // A number outside the named set (a real-time signal, or nonsense) is refused
            // rather than passed through unchecked.
            None => return Ok(Value::Int(-EINVAL)),
        },
        // A pid of 0 or a negative one means "a process group" to `kill(2)`. `Pid` cannot
        // hold those, and this host cannot deliver to a group it does not track.
        None => return Ok(Value::Int(-EINVAL)),
    };
    match result {
        Ok(()) => Ok(Value::Int(0)),
        Err(error) => Ok(Value::Int(-errno_of(&error))),
    }
}

/// `os.dup(fd)` → the new descriptor number, or a negative errno.
fn os_dup(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &Shared,
) -> JsResult<Value> {
    let _ = interp;
    let fd = args.first().map_or(-1, |value| value.to_number() as i32);
    let result = table.with(fd, |handle| rustix::io::dup(handle));
    match result {
        Some(Ok(copy)) => Ok(Value::Int(table.adopt(copy))),
        Some(Err(error)) => Ok(Value::Int(-errno_of(&error))),
        None => Ok(Value::Int(-EBADF)),
    }
}

/// `os.dup2(fd, fd2)` → `fd2` on success, or a negative errno.
///
/// The target is a descriptor the script already owns, so the table's entry is replaced:
/// `dup2` closes the old target first, which is what dropping the stored `OwnedFd` does.
fn os_dup2(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &Shared,
) -> JsResult<Value> {
    let _ = interp;
    let from = args.first().map_or(-1, |value| value.to_number() as i32);
    let to = args.get(1).map_or(-1, |value| value.to_number() as i32);
    if !table.contains(to) {
        return Ok(Value::Int(-EBADF));
    }
    let copied = table.with(from, |handle| rustix::io::dup(handle));
    let copy = match copied {
        Some(Ok(copy)) => copy,
        Some(Err(error)) => return Ok(Value::Int(-errno_of(&error))),
        None => return Ok(Value::Int(-EBADF)),
    };
    // `dup2` into a number this table does not own (0/1/2) cannot be honoured, because
    // closing and replacing the real standard stream is the host's business, not a
    // script's. `replace` reports that as EBADF rather than pretending.
    match table.replace(to, copy) {
        Ok(()) => Ok(Value::Int(to)),
        Err(errno) => Ok(Value::Int(-errno)),
    }
}

// ---------------------------------------------------------------------------
// Time and terminals: `now`, `sleep`, `sleepAsync`, `ttyGetWinSize`, `ttySetRaw`.
// ---------------------------------------------------------------------------

/// `os.now()` — **monotonic** milliseconds, not a wall-clock timestamp.
///
/// This is the reference's `get_time_ns()` on `CLOCK_MONOTONIC`, so the number counts from
/// an unspecified origin (boot, usually) — `qjs --std` reports values like `95607854.675`,
/// roughly a day of uptime, not milliseconds since 1970. Scripts use it for intervals:
/// `var t = os.now(); … ; os.now() - t`. A monotonic clock is what makes that correct
/// across a system clock adjustment.
fn os_now(
    interp: &mut Interp<'_>,
    _this: Value,
    _args: &[Value],
    _table: &Shared,
) -> JsResult<Value> {
    let _ = interp;
    Ok(Value::Float(monotonic_ms()))
}

/// Milliseconds on the process's monotonic clock.
fn monotonic_ms() -> f64 {
    // `Instant` is monotonic and has no epoch, which is exactly the contract `os.now()` has.
    // An `Instant` cannot be built from a fixed origin portably, so the elapsed time since
    // the first call is what is reported — the *differences* are what scripts measure.
    use std::sync::OnceLock;
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(std::time::Instant::now);
    origin.elapsed().as_secs_f64() * 1000.0
}

/// `os.sleep(ms)` — block for that long.
fn os_sleep(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &Shared,
) -> JsResult<Value> {
    let _ = interp;
    let delay = args.first().map_or(0.0, |value| value.to_number());
    if delay.is_finite() && delay > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(delay / 1000.0));
    }
    Ok(Value::Undefined)
}

/// `os.sleepAsync(ms)` — a promise that resolves after that long.
///
/// Built on the engine's own timer queue rather than a second mechanism, so the promise is
/// resolved by the same event loop that runs `os.setTimeout` and ordering between the two
/// stays consistent.
fn os_sleep_async(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    _table: &Shared,
) -> JsResult<Value> {
    let delay = args.first().map_or(0.0, |value| value.to_number());
    interp.sleep_async(delay)
}

/// `os.ttyGetWinSize(fd)` → `[width, height]`, or `null` when the descriptor is not a tty.
///
/// `null` rather than an error, matching the reference: it calls `ioctl(TIOCGWINSZ)` and
/// returns `JS_NULL` when that fails, which is also what a redirected stream gives.
fn os_tty_get_win_size(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &Shared,
) -> JsResult<Value> {
    let fd = args.first().map_or(-1, |value| value.to_number() as i32);
    // `tcgetwinsize` only works on a terminal; on anything else the call fails and the
    // answer is `null`, which is the case a script hits when its output is redirected.
    let size = table.with(fd, |handle| rustix::termios::tcgetwinsize(handle));
    match size {
        Some(Ok(size)) => {
            let elements = vec![
                Value::Int(i32::from(size.ws_col)),
                Value::Int(i32::from(size.ws_row)),
            ];
            Ok(Value::Object(interp.new_array(elements)))
        }
        _ => Ok(Value::Null),
    }
}

/// `os.ttySetRaw(fd)` — put the terminal into raw mode.
///
/// The reference passes the descriptor to `tty_set_raw`, which clears `ICANON`, `ECHO` and
/// the signal characters. There is deliberately no way to restore the mode: the reference
/// does not offer one either (a REPL restores it on exit by holding the old `termios`).
fn os_tty_set_raw(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    table: &Shared,
) -> JsResult<Value> {
    let fd = args.first().map_or(-1, |value| value.to_number() as i32);
    let result = table.with(fd, |handle| {
        let mut termios = rustix::termios::tcgetattr(handle)?;
        termios.make_raw();
        rustix::termios::tcsetattr(handle, rustix::termios::OptionalActions::Now, &termios)
    });
    let _ = interp;
    Ok(match result {
        Some(Ok(())) => Value::Undefined,
        // The reference ignores the return value, so a non-tty is silently a no-op.
        _ => Value::Undefined,
    })
}

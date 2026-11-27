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

use std::rc::Rc;

use quickjs::{Context, Interp, JsResult, Value};
use rustix::fs::{FileType, Mode, OFlags};

use crate::fdtable::FdTable;

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

/// Install `os` into `object`.
pub fn install(ctx: &mut Context<'_>, object: &Value) -> quickjs::Result<()> {
    if object.as_object().is_none() {
        return Err(quickjs::Error::Exception(
            "os: target is not an object".to_string(),
        ));
    }
    let table = Rc::new(FdTable::new());

    for (name, length, body) in functions(&table) {
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

fn functions(table: &Rc<FdTable>) -> Vec<(&'static str, u32, Body)> {
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
fn buffer_storage(
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

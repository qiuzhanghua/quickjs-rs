//! `qjs` — command line interpreter for the pure-Rust QuickJS port.
//!
//! The argument surface mirrors upstream `qjs.c`: same option names, same help
//! text (byte-for-byte, verified by `tests/conformance.rs`), same exit codes for
//! usage errors, and the same combined-short-option syntax (`-iq`, `-e'1+1'`).
//!
//! Until the parser/VM milestones land, anything that needs to execute
//! JavaScript exits with [`EXIT_PENDING`] and a `not implemented yet` message on
//! stderr. `quickjs-conformance` recognises that marker and reports such cases
//! as *pending* instead of as mismatches, so the harness stays usable while the
//! engine is being built.

use std::io::Write;
use std::process::ExitCode;

mod modules;

/// Exit code used "option parsed, engine not ready yet".
///
/// Note: upstream also exits with 2 for a missing `-e` expression, so the code
/// alone is not enough to classify a run; the stderr marker is.
const EXIT_PENDING: u8 = 2;

#[derive(Default, Debug)]
struct Options {
    eval: Option<String>,
    includes: Vec<String>,
    file: Option<String>,
    script_args: Vec<String>,
    interactive: bool,
    quit_only: bool,
    module: Option<bool>,
    strict: bool,
    load_std: bool,
    dump_memory: bool,
    trace_memory: bool,
    track_unhandled_rejection: bool,
    strip_source: bool,
    experimental: bool,
    memory_limit: Option<usize>,
    stack_size: Option<usize>,
}

fn help() -> ExitCode {
    // Upstream `help()` prints the usage and exits with 1, so scripts probing
    // `--help` see the same status.
    println!(
        "QuickJS version {}\n\
         usage: qjs [options] [file [args]]\n\
         -h  --help         list options\n\
         -e  --eval EXPR    evaluate EXPR\n\
         -i  --interactive  go to interactive mode\n\
         -m  --module       load as ES6 module (default=autodetect)\n\
         \x20   --script       load as ES6 script (default=autodetect)\n\
         \x20   --strict       force strict mode\n\
         -I  --include file include an additional file\n\
         \x20   --std          make 'std' and 'os' available to the loaded script\n\
         -T  --trace        trace memory allocation\n\
         -d  --dump         dump the memory usage stats\n\
         \x20   --memory-limit n  limit the memory usage to 'n' bytes (SI suffixes allowed)\n\
         \x20   --stack-size n    limit the stack size to 'n' bytes (SI suffixes allowed)\n\
         \x20   --no-unhandled-rejection  ignore unhandled promise rejections\n\
         -s                    strip all the debug info\n\
         \x20   --strip-source    strip the source code\n\
         \x20   --experimental    enable experimental Temporal and Intl support\n\
         -q  --quit         just instantiate the interpreter and quit",
        quickjs::COMPAT_VERSION
    );
    ExitCode::from(1)
}

fn die(message: &str, code: u8) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(code)
}

/// Same as [`die`] but without a trailing newline: upstream uses `fprintf`
/// without `\n` for the "expecting ..." messages, and `qjs` is compared
/// byte-for-byte by the conformance tests.
fn die_raw(message: &str, code: u8) -> ExitCode {
    eprint!("{message}");
    ExitCode::from(code)
}

fn unknown_option(display: &str) -> ExitCode {
    eprintln!("qjs: unknown option '{display}'");
    help()
}

/// Run `source`, print whatever the script printed, and report a pending
/// exception the way `qjs.c` does.
///
/// Upstream prints the exception on stderr and exits with 1; the script's own
/// output stays on stdout, so a run that throws halfway still shows what it got
/// through. With `-e` the completion value is printed when it is not `undefined`,
/// which is what makes `qjs -e '1+1'` print `2`.
fn run(ctx: &mut quickjs::Context<'_>, source: &str, filename: &str) -> ExitCode {
    // Stack traces name the script, so the context has to know it before compiling.
    ctx.set_script_name(filename);
    let result = ctx.eval(source);
    // The sink already wrote every line as it was printed; the buffer is only a copy and
    // would grow without bound in a long-running script, so it is drained here.
    ctx.take_output();
    let _ = std::io::stdout().flush();
    // A clean script may still have left timers pending. `js_std_loop` runs them after the
    // script returns, which is what makes `setTimeout` mean anything at all.
    let loop_result = match &result {
        Ok(_) => quickjs_host::run_until_idle(ctx).map_err(|error| error.to_string()),
        Err(_) => Ok(Vec::new()),
    };
    ctx.take_output();
    let _ = std::io::stdout().flush();
    match result {
        // Upstream `qjs -e '1+1'` prints nothing: `-e` reports only what the script
        // itself printed. The completion value is still returned through the API.
        Ok(_) => match loop_result {
            Ok(errors) if errors.is_empty() => ExitCode::SUCCESS,
            Ok(errors) => {
                // Errors a timer callback threw are reported and then the process still
                // fails: the reference prints them through `js_std_dump_error` and returns
                // a non-zero status.
                for text in errors {
                    let _ = writeln!(std::io::stderr(), "{text}");
                }
                ExitCode::from(1)
            }
            Err(text) => {
                let _ = writeln!(std::io::stderr(), "{text}");
                ExitCode::from(1)
            }
        },
        Err(error @ quickjs::Error::Syntax(_)) => {
            // Compile errors are reported before anything runs, with the constructor name
            // in front: the reference prints `SyntaxError: …`, and the conformance runner
            // relies on that word to tell a parse-time rejection from a runtime throw.
            let _ = writeln!(std::io::stderr(), "{filename}: {error}");
            ExitCode::from(1)
        }
        Err(error) => {
            // Upstream reports a pending exception through `JS_PrintValue`, which
            // prints just the value — no file prefix, and no position either (the
            // position lives in `error.stack`, which is printed when it exists).
            let _ = writeln!(std::io::stderr(), "{error}");
            ExitCode::from(1)
        }
    }
}

#[allow(dead_code)]
fn pending(what: &str, detail: &str) -> ExitCode {
    let stderr = std::io::stderr();
    let mut stderr = stderr.lock();
    let _ = writeln!(stderr, "qjs: not implemented yet: {what} ({detail})");
    ExitCode::from(EXIT_PENDING)
}

/// Parse the command line the way `qjs.c` does.
///
/// `Err` carries the exit status to return immediately (`--help` and usage
/// errors both terminate the process upstream).
fn parse_args(args: &[String]) -> Result<Options, ExitCode> {
    let mut opts = Options {
        track_unhandled_rejection: true,
        ..Options::default()
    };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            i += 1;
            break;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            if let Some(long) = arg.strip_prefix("--") {
                match long {
                    "help" => return Err(help()),
                    "eval" => {
                        opts.eval = Some(next_value(args, &mut i, || {
                            die("qjs: missing expression for -e", 2)
                        })?);
                    }
                    "include" => {
                        opts.includes.push(next_value(args, &mut i, || {
                            die_raw("expecting filename", 1)
                        })?);
                    }
                    "interactive" => opts.interactive = true,
                    "module" => opts.module = Some(true),
                    "script" => opts.module = Some(false),
                    "strict" => opts.strict = true,
                    "std" => opts.load_std = true,
                    "dump" => opts.dump_memory = true,
                    "trace" => opts.trace_memory = true,
                    "no-unhandled-rejection" => opts.track_unhandled_rejection = false,
                    "experimental" => opts.experimental = true,
                    "quit" => opts.quit_only = true,
                    "strip-source" => opts.strip_source = true,
                    "memory-limit" => {
                        let value =
                            next_value(args, &mut i, || die_raw("expecting memory limit", 1))?;
                        opts.memory_limit =
                            Some(parse_size(&value).ok_or_else(|| {
                                die(&format!("invalid memory limit '{value}'"), 1)
                            })?);
                    }
                    "stack-size" => {
                        let value =
                            next_value(args, &mut i, || die_raw("expecting stack size", 1))?;
                        opts.stack_size = Some(
                            parse_size(&value)
                                .ok_or_else(|| die(&format!("invalid stack size '{value}'"), 1))?,
                        );
                    }
                    _ => return Err(unknown_option(&format!("--{long}"))),
                }
            } else {
                // Combined short options, e.g. `-iq`, `-e'1+1'`, `-Td`.
                let chars: Vec<char> = arg[1..].chars().collect();
                let mut k = 0;
                while k < chars.len() {
                    let c = chars[k];
                    match c {
                        'h' | '?' => return Err(help()),
                        'e' => {
                            let rest: String = chars[k + 1..].iter().collect();
                            if !rest.is_empty() {
                                opts.eval = Some(rest);
                            } else {
                                opts.eval = Some(next_value(args, &mut i, || {
                                    die("qjs: missing expression for -e", 2)
                                })?);
                            }
                            break;
                        }
                        'I' => {
                            let rest: String = chars[k + 1..].iter().collect();
                            if !rest.is_empty() {
                                opts.includes.push(rest);
                            } else {
                                opts.includes.push(next_value(args, &mut i, || {
                                    die_raw("expecting filename", 1)
                                })?);
                            }
                            break;
                        }
                        'i' => opts.interactive = true,
                        'm' => opts.module = Some(true),
                        'd' => opts.dump_memory = true,
                        'T' => opts.trace_memory = true,
                        'q' => opts.quit_only = true,
                        's' => opts.strip_source = true,
                        other => return Err(unknown_option(&format!("-{other}"))),
                    }
                    k += 1;
                }
            }
            i += 1;
            continue;
        }
        // First non-option argument: the script. Everything from here on is positional,
        // which is exactly what `scriptArgs` holds — element 0 is the script itself when
        // there is one, and with `-e` the whole list is just the trailing arguments.
        opts.file = Some(arg.to_string());
        opts.script_args = args[i..].to_vec();
        i = args.len();
    }
    if opts.file.is_none() && i < args.len() {
        opts.file = Some(args[i].clone());
        opts.script_args = args[i..].to_vec();
    }
    Ok(opts)
}

/// Consume the next command line argument as an option value.
fn next_value(
    args: &[String],
    index: &mut usize,
    on_missing: impl FnOnce() -> ExitCode,
) -> Result<String, ExitCode> {
    *index += 1;
    match args.get(*index) {
        Some(value) => Ok(value.clone()),
        None => Err(on_missing()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opts = match parse_args(&args) {
        Ok(opts) => opts,
        Err(code) => return code,
    };

    let mut rt = quickjs::Runtime::new();
    rt.set_memory_limit(opts.memory_limit);
    rt.set_stack_size(opts.stack_size);

    // Three options are parsed, advertised in `--help`, and then do nothing. Accepting
    // them silently is the one thing this project does not do on purpose: a user who
    // passes `--memory-limit 64M` believes a runaway script will be stopped, and a user
    // who passes `--std` believes `os`/`std` are importable. Both beliefs are false, and
    // the second one is how `--memory-limit` stopped being decorative and started being
    // dangerous — an unbounded test run took the whole session down.
    //
    // So they fail loudly instead, the way an unimplemented feature is meant to.

    if opts.memory_limit.is_some() {
        return pending(
            "--memory-limit",
            "milestone M9; the limit is not enforced yet",
        );
    }
    if opts.stack_size.is_some() {
        return pending(
            "--stack-size",
            "milestone M9; the limit is not enforced yet",
        );
    }

    if opts.quit_only {
        return ExitCode::SUCCESS;
    }

    let mut ctx = quickjs::Context::new(&rt);
    ctx.set_module_loader(Box::new(modules::FileModuleLoader::new()));
    // `--std` mounts `std`/`os` on the global object, which is what the reference's
    // `js_std_add_helpers` + `--std` do. Only the timer half of `os` exists so far (M9c);
    // the rest arrives with M9d. Scripts that use the missing parts fail where they touch
    // them rather than silently getting `undefined`.
    if opts.load_std {
        // One descriptor table for both modules: `std.fdopen(os.pipe()[0], "r")` has to
        // work, and that is the reference's own `test_os_exec()`.
        let fds = quickjs_host::fdtable::FdTable::shared();
        let os = ctx.new_object();
        if let Err(error) = quickjs_host::timers::install(&mut ctx, &os) {
            return die(&format!("qjs: cannot install os: {error}"), 1);
        }
        #[cfg(unix)]
        if let Err(error) = quickjs_host::os::install(&mut ctx, &os, &fds) {
            return die(&format!("qjs: cannot install os: {error}"), 1);
        }
        if let Err(error) = ctx.set_global("os", os) {
            return die(&format!("qjs: cannot mount os: {error}"), 1);
        }
        let std = ctx.new_object();
        if let Err(error) = quickjs_host::std::install(&mut ctx, &std, &fds) {
            return die(&format!("qjs: cannot install std: {error}"), 1);
        }
        if let Err(error) = ctx.set_global("std", std) {
            return die(&format!("qjs: cannot mount std: {error}"), 1);
        }
    }
    // `print` writes straight to stdout, flushed per line, the way the reference does with
    // `fputs` + `fflush`. Without it a script's output would only appear once it finished,
    // which makes a timer callback's output indistinguishable from the script body's.
    ctx.set_output_sink(Box::new(std::io::stdout()));
    // `scriptArgs` is the positional list as `qjs` defines it: element 0 is the script
    // when a script was named, and the trailing arguments when `-e` supplied the source.
    ctx.set_script_args(opts.script_args.clone());
    if let Some(expr) = opts.eval {
        return run(&mut ctx, &expr, "<cmdline>");
    }
    if let Some(path) = opts.file {
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) => return die(&format!("qjs: cannot read {path}: {error}"), 1),
        };
        // `-m` forces module mode, `--script` forces script mode, and the default
        // autodetects: the reference compiles as a script and falls back to a module only
        // when the script parse rejects it, which is why a file with no `import`/`export`
        // runs either way.
        let as_module = match opts.module {
            Some(explicit) => explicit,
            None => quickjs::is_module_source(&source),
        };
        if as_module {
            return run_module(&mut ctx, &path);
        }
        return run(&mut ctx, &source, &path);
    }
    if opts.interactive || args.is_empty() {
        return pending("interactive REPL", "milestone M9");
    }
    ExitCode::SUCCESS
}

/// Run `path` as an ES module.
///
/// The module is loaded through the same loader the script path uses, so `import`ing it
/// again from somewhere else finds one record rather than two.
fn run_module(ctx: &mut quickjs::Context<'_>, path: &str) -> ExitCode {
    let result = ctx.eval_module(path);
    // Written live by the sink; drained rather than reprinted.
    ctx.take_output();
    let _ = std::io::stdout().flush();
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            // The reference reports a module failure through the same "print the value"
            // path a script uses, with no file prefix.
            let _ = writeln!(std::io::stderr(), "{error}");
            ExitCode::from(1)
        }
    }
}

/// Parse the SI-suffix sizes accepted by upstream (`k`, `m`, `g`, `t` with an
/// optional `b`, case insensitive).
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    let (digits, multiplier) = match s.as_bytes().last()?.to_ascii_lowercase() {
        b'k' => (&s[..s.len() - 1], 1024usize),
        b'm' => (&s[..s.len() - 1], 1024usize * 1024),
        b'g' => (&s[..s.len() - 1], 1024usize * 1024 * 1024),
        b't' => (&s[..s.len() - 1], 1024usize * 1024 * 1024 * 1024),
        b'b' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    digits.parse::<usize>().ok()?.checked_mul(multiplier)
}

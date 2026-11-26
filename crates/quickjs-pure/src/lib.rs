//! `quickjs` — an embeddable JavaScript engine written in pure Rust.
//!
//! The crate is being built as a drop-in replacement for
//! [bellard/quickjs](https://github.com/bellard/quickjs); the language level
//! tracked is the upstream release whose version string is
//! [`COMPAT_VERSION`].
//!
//! # Current state (v0.4.0 — milestones M0–M6)
//!
//! Source is parsed, compiled to bytecode and executed: [`Context::eval`] runs a
//! script and returns its completion value, and [`Context::output`] holds whatever
//! `print` wrote.
//!
//! ```
//! let rt = quickjs::Runtime::new();
//! let mut ctx = quickjs::Context::new(&rt);
//! ctx.eval("print(1 + 1)").expect("runs");
//! assert_eq!(ctx.output(), "2\n");
//! ```
//!
//! The standard library covers `Object`, `Function`, `Array`, `String`, `Number`,
//! `Math`, `JSON`, `Reflect`, `Date`, `RegExp`, `Symbol`, `Map`/`Set`, `BigInt`,
//! TypedArray, `DataView` and the error classes; the ES6+ runtime is in place
//! (class, destructuring, generators, `Proxy`, `Promise`/async, `for await`); and
//! ES modules run through [`Context::eval_module`].
//!
//! # Not there yet
//!
//! Every gap below is a loud failure rather than a wrong answer:
//!
//! * private class fields (`#x`) are rejected by the compiler with a
//!   `SyntaxError`;
//! * the `std`/`os` host modules and the event loop are absent, so `setTimeout`
//!   is undefined and a script cannot touch the filesystem;
//! * `Iterator` helpers and `RegExp.escape` are missing, and `RegExp` lookbehind
//!   does not parse;
//! * the host API is still small: a script can be evaluated, but Rust cannot yet
//!   register a native function on the global object or call a JavaScript
//!   function back.
//!
//! See `docs/requirements.md` in the repository for the milestone plan and
//! `docs/test262-status.md` for the measured language coverage.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub use js_compile;
pub use js_gc::{CollectStats, Gc, Heap, Trace, Tracer, WeakGc};
pub use js_parse;

mod bignum;

pub mod bigint;
pub mod builtins;
pub mod interp;
pub mod module;
pub mod object;
pub mod regexp;
mod tzif;
pub mod value;

pub use interp::{Interp, Signal};
pub use object::{Callable, JsObject, ObjectKind, Property};
pub use value::{Cell, JsString, Value};

/// Version string of the upstream QuickJS release this port tracks.
pub const COMPAT_VERSION: &str = "2026-06-04";

/// Error raised by the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The source did not parse, or did not compile.
    Syntax(String),
    /// The script threw, or the engine raised while running it.
    Exception(String),
    /// The feature/entry point exists in the API but is not implemented yet.
    NotImplemented(&'static str),
    /// The runtime exceeded its memory limit (`--memory-limit`).
    OutOfMemory,
    /// The runtime exceeded its stack limit (`--stack-size`).
    StackOverflow,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotImplemented(what) => write!(f, "not implemented yet: {what}"),
            Error::Syntax(what) => write!(f, "{what}"),
            Error::Exception(what) => write!(f, "{what}"),
            Error::OutOfMemory => write!(f, "out of memory"),
            Error::StackOverflow => write!(f, "stack overflow"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Whether `source` should be evaluated as a module, decided the way `qjs` decides.
///
/// The rule is deliberately shallow: the **first token** must be `import` or `export`
/// (an `import` followed by `.` or `(` is `import.meta` or `import()`, both of which are
/// expressions, so a script may use them). Nothing else makes a file a module.
///
/// It has to be this shallow to match the reference, and the difference is observable:
/// a file that merely *mentions* `import.meta` further down is a script, so it fails with
/// "import.meta only valid in module code" rather than silently becoming a module.
pub fn is_module_source(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    // A shebang line is not code.
    if bytes.starts_with(b"#!") {
        while index < bytes.len() && bytes[index] != b'\n' {
            index += 1;
        }
    }
    loop {
        // Whitespace.
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        if bytes[index..].starts_with(b"//") {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if bytes[index..].starts_with(b"/*") {
            index += 2;
            while index < bytes.len() && !bytes[index..].starts_with(b"*/") {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        break;
    }
    let rest = &source[index.min(source.len())..];
    let word = |w: &str| -> Option<&str> { rest.strip_prefix(w) };
    if let Some(tail) = word("export") {
        // `exportedThing` is an identifier, not the keyword.
        return !tail.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$');
    }
    if let Some(tail) = word("import") {
        if tail.starts_with(|c: char| c.is_alphanumeric() || c == '_' || c == '$') {
            return false;
        }
        let tail = tail.trim_start();
        // `import.meta` and `import(...)` are expressions.
        if tail.starts_with('.') || tail.starts_with('(') {
            return false;
        }
        return true;
    }
    false
}

/// A JavaScript runtime: one heap, one memory limit, one set of host hooks.
///
/// Mirrors the role of `JSRuntime` in the QuickJS C API.
pub struct Runtime {
    heap: Heap,
    memory_limit: Option<usize>,
    stack_size: Option<usize>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime {
    /// Create a runtime with default limits.
    pub fn new() -> Runtime {
        Runtime {
            heap: Heap::new(),
            memory_limit: None,
            stack_size: None,
        }
    }

    /// The runtime's object heap.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Equivalent of `JS_SetMemoryLimit`.
    pub fn set_memory_limit(&mut self, limit: Option<usize>) {
        self.memory_limit = limit;
    }

    /// Configured memory limit, if any.
    pub fn memory_limit(&self) -> Option<usize> {
        self.memory_limit
    }

    /// Equivalent of `JS_SetMaxStackSize`.
    pub fn set_stack_size(&mut self, limit: Option<usize>) {
        self.stack_size = limit;
    }

    /// Configured stack size limit, if any.
    pub fn stack_size(&self) -> Option<usize> {
        self.stack_size
    }

    /// Equivalent of `JS_RunGC`.
    pub fn run_gc(&self) -> CollectStats {
        self.heap.collect()
    }
}

/// An execution context: a global object plus the builtins installed on it.
///
/// Mirrors the role of `JSContext` in the QuickJS C API.
pub struct Context<'rt> {
    runtime: &'rt Runtime,
    /// The interpreter, created on first use.
    interp: Option<Interp<'rt>>,
    /// A loader the host installed, handed to the interpreter when it is built.
    module_loader: Option<Box<dyn module::ModuleLoader>>,
    /// The value of the last script, as `qjs` reports it.
    last_value: Value,
    /// The name a top-level script is compiled under, which is what `error.stack`
    /// prints: `<cmdline>` for `-e`, a path for a file, `None` for `<input>`.
    script_name: Option<String>,
    /// The script arguments, as `qjs` exposes them through `scriptArgs`.
    ///
    /// Kept here and installed into the realm on first use, because the interpreter (and
    /// so the global object) does not exist until something runs.
    script_args: Option<Vec<String>>,
}

impl<'rt> Context<'rt> {
    /// Create a context on `runtime`.
    pub fn new(runtime: &'rt Runtime) -> Context<'rt> {
        Context {
            runtime,
            interp: None,
            module_loader: None,
            last_value: Value::Undefined,
            script_name: None,
            script_args: None,
        }
    }

    /// Install how module specifiers are resolved and read.
    ///
    /// Without one, importing is a clear error rather than a silent failure: the engine
    /// cannot guess whether `./x.js` means a file, and the host is what knows.
    pub fn set_module_loader(&mut self, loader: Box<dyn module::ModuleLoader>) {
        // A context that has already run something owns a live interpreter, so the loader
        // goes to whichever exists.
        if let Some(interp) = self.interp.as_mut() {
            interp.module_loader = Some(loader);
        } else {
            self.module_loader = Some(loader);
        }
    }

    /// Name the scripts this context evaluates, for stack traces.
    ///
    /// Mirrors what `qjs` passes to `JS_Eval`: `-e` code is `<cmdline>`, a script file
    /// is its own path. Without a name, frames print `<input>`, which is what the
    /// reference uses for code with no file behind it.
    pub fn set_script_name(&mut self, name: impl Into<String>) {
        self.script_name = Some(name.into());
    }

    /// Set the arguments `scriptArgs` should hold.
    ///
    /// Element 0 is the script itself, matching `qjs`: `qjs f.js a b` gives
    /// `["f.js", "a", "b"]`. With `-e` there is no script, so the array is just the
    /// trailing arguments and `-e`'s own text never appears in it.
    ///
    /// This was parsed by the CLI and then dropped on the floor: `opts.script_args` was
    /// assigned in two places and read in none, so `scriptArgs` was simply undeclared.
    /// An ordinary script's `if (typeof scriptArgs != "undefined")` branch therefore never
    /// ran, and `examples/pi_bigint.js` printed a thousand digits of pi where the
    /// reference printed `usage: pi n_digits`.
    pub fn set_script_args(&mut self, args: Vec<String>) {
        self.script_args = Some(args);
    }

    /// The runtime this context belongs to.
    pub fn runtime(&self) -> &'rt Runtime {
        self.runtime
    }

    /// Everything `print` has written.
    pub fn output(&self) -> &str {
        self.interp.as_ref().map_or("", |interp| &interp.output)
    }

    /// The interpreter, creating it and its realm on first use.
    fn interp(&mut self) -> &mut Interp<'rt> {
        let heap: &'rt Heap = &self.runtime.heap;
        let loader = self.module_loader.take();
        let script_args = self.script_args.take();
        self.interp.get_or_insert_with(|| {
            let mut interp = Interp::new(heap);
            interp.module_loader = loader;
            // `scriptArgs` is a host addition, like `print`, so it is installed here
            // rather than in the realm's builtins. `qjs` defines it only when there is
            // something to put in it, which is why `-e` with no trailing arguments still
            // leaves it as an empty array rather than absent.
            if let Some(args) = script_args {
                let elements = args
                    .iter()
                    .map(|arg| interp.string(arg.clone()))
                    .collect::<Vec<_>>();
                let array = interp.new_array(elements);
                interp
                    .realm
                    .global
                    .clone()
                    .define_data("scriptArgs", Value::Object(array));
            }
            interp
        })
    }
    /// Evaluate `specifier` as the entry module, the way `qjs -m` does.
    ///
    /// The microtask queue is drained afterwards, so a module whose top level launched an
    /// async task has finished it by the time this returns.
    pub fn eval_module(&mut self, specifier: &str) -> Result<Value> {
        let loader = self.module_loader.take();
        let interp = self.interp();
        if interp.module_loader.is_none() {
            interp.module_loader = loader;
        }
        match interp.run_main_module(specifier) {
            Ok(value) => {
                let drained = interp.drain_jobs();
                let unhandled = interp.take_unhandled_rejections();
                if let Err(Signal::Throw(reason)) = drained {
                    let text = interp.describe_exception(&reason);
                    return Err(Error::Exception(text));
                }
                if !unhandled.is_empty() {
                    return Err(Error::Exception(unhandled.join("\n")));
                }
                self.last_value = value.clone();
                Ok(value)
            }
            Err(Signal::Throw(value)) => {
                let text = interp.describe_exception(&value);
                Err(Error::Exception(text))
            }
            Err(Signal::OutOfMemory) => Err(Error::OutOfMemory),
        }
    }

    /// Evaluate `source` as a global script and return its completion value.
    ///
    /// The value is reported the way `qjs -e` reports it: a string for anything the
    /// script leaves on the stack, and nothing at all for `undefined`.
    pub fn eval(&mut self, source: &str) -> Result<Value> {
        let program =
            js_parse::parse_script(source).map_err(|error| Error::Syntax(error.to_string()))?;
        let mut code = js_compile::compile_script(&program)
            .map_err(|error| Error::Syntax(error.message.clone()))?;
        // Position information travels with the code, but the file name is the host's
        // to supply — the parser never sees it.
        code.set_filename(self.script_name.as_deref().unwrap_or("<input>"));
        let interp = self.interp();
        match interp.run_script(code) {
            Ok(value) => {
                // An unhandled rejection is fatal *after* the script finishes, which is
                // when the reference decides nothing else will claim it. The script's own
                // output has already been buffered, so it still prints first.
                let unhandled = interp.take_unhandled_rejections();
                if !unhandled.is_empty() {
                    return Err(Error::Exception(unhandled.join("\n")));
                }
                self.last_value = value.clone();
                Ok(value)
            }
            Err(Signal::Throw(value)) => {
                let text = interp.describe_exception(&value);
                Err(Error::Exception(text))
            }
            Err(Signal::OutOfMemory) => Err(Error::OutOfMemory),
        }
    }

    /// The value of the script that ran last.
    pub fn last_value(&self) -> &Value {
        &self.last_value
    }
}

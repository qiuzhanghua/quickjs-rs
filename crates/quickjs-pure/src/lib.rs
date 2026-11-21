//! `quickjs` — an embeddable JavaScript engine written in pure Rust.
//!
//! The crate is being built as a drop-in replacement for
//! [bellard/quickjs](https://github.com/bellard/quickjs); the language level
//! tracked is the upstream release whose version string is
//! [`COMPAT_VERSION`].
//!
//! # Current state (M0)
//!
//! Only the foundations exist: [`Runtime`] owns a [`js_gc::Heap`] and the
//! memory-limit bookkeeping. Parsing, bytecode, the VM and the standard library
//! land in later milestones (see `docs/requirements.md`), so [`Context::eval`]
//! returns [`Error::NotImplemented`] for now.
//!
//! ```
//! let rt = quickjs::Runtime::new();
//! assert_eq!(rt.heap().live_count(), 0);
//! ```

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

pub use js_gc::{CollectStats, Gc, Heap, Trace, Tracer, WeakGc};

/// Version string of the upstream QuickJS release this port tracks.
pub const COMPAT_VERSION: &str = "2026-06-04";

/// Error raised by the engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The feature/entry point exists in the API but is not implemented yet.
    NotImplemented(&'static str),
    /// A JavaScript exception is pending on the context.
    Exception,
    /// The runtime exceeded its memory limit (`--memory-limit`).
    OutOfMemory,
    /// The runtime exceeded its stack limit (`--stack-size`).
    StackOverflow,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotImplemented(what) => write!(f, "not implemented yet: {what}"),
            Error::Exception => write!(f, "JavaScript exception"),
            Error::OutOfMemory => write!(f, "out of memory"),
            Error::StackOverflow => write!(f, "stack overflow"),
        }
    }
}

impl std::error::Error for Error {}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

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
}

impl<'rt> Context<'rt> {
    /// Create a context on `runtime`.
    pub fn new(runtime: &'rt Runtime) -> Context<'rt> {
        Context { runtime }
    }

    /// The runtime this context belongs to.
    pub fn runtime(&self) -> &'rt Runtime {
        self.runtime
    }

    /// Evaluate `source` as a global script.
    ///
    /// Not available yet: the parser/compiler/VM milestones have not landed.
    pub fn eval(&mut self, _source: &str) -> Result<()> {
        Err(Error::NotImplemented("Context::eval (M1-M3)"))
    }
}

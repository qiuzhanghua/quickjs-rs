//! The bytecode interpreter.
//!
//! The machine is the same shape as the reference's: a value stack per frame, a
//! program counter into `js_compile`'s resolved bytecode, and a frame chain. Two
//! properties of that design are worth stating up front because they are visible
//! everywhere below.
//!
//! **Frames are explicit, not recursive.** A JavaScript call pushes a frame onto
//! [`Interp::frames`] and the loop continues; it never recurses in Rust. Deep
//! JavaScript recursion therefore costs heap, not C stack, and the depth limit is
//! ours to enforce rather than the host's to crash.
//!
//! **Exceptions live on the value stack.** `catch` pushes a [`Value::CatchOffset`]
//! marker and an exception is found by walking the frame's stack downwards looking
//! for the innermost one. There is no side table, which is why a `try` inside a
//! loop costs nothing until it throws.

use std::collections::{BTreeMap, BTreeSet};

use js_compile::bytecode::{Atom, ClosureType, Constant, Function as CompiledFunction};
use js_compile::opcode::{OpFormat, Opcode};
use js_gc::{Gc, Heap, WeakGc};

use crate::object::{
    Callable, ClosureEnv, CompiledCode, EvalScope, JsObject, LocalVars, NativeFn, ObjectKind,
    Property, PropertyKey, array_index, has_own, lookup_key, same_proto,
};
use crate::value::{Cell, Value, number_to_string};

/// Why execution stopped.
#[derive(Debug, Clone)]
pub enum Signal {
    /// A JavaScript exception carrying the thrown value.
    Throw(Value),
    /// The runtime ran out of memory.
    OutOfMemory,
}

/// What an execution step produced.
pub type JsResult<T> = std::result::Result<T, Signal>;

/// The result of one instruction.
enum Step {
    /// Keep executing the current frame.
    Continue,
    /// The current frame returned this value.
    Return(Value),
    /// The frame parked at a suspension point, with the yielded value on its stack.
    ///
    /// Only a generator or an async function can produce this, and only its own frame —
    /// a nested ordinary call cannot contain a `yield`. The frame is not popped: it is
    /// detached and handed to the generator object, which is what lets it resume later
    /// with its locals and operand stack intact.
    Suspend(SuspendKind),
}

/// Why a frame parked, which decides how the resumption is delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendKind {
    /// `initial_yield`: the body has not started. The first `next()` resumes *after* it
    /// without passing a value, because there is no `yield` expression to receive one.
    Initial,
    /// `yield expr`: the value is on the stack and the caller gets `{value, done:false}`.
    Yield,
    /// `yield* expr`: like `Yield`, but the value is already an `{value, done}` object
    /// from the inner iterator and is handed straight back.
    YieldStar,
    /// `await expr`: the value is a promise the async machinery has to settle first.
    Await,
}

/// Everything one JavaScript realm owns.
///
/// A realm is the set of objects a script can reach without naming them: the global
/// object, the prototype chain roots, and the global binding cells.
pub struct Realm {
    /// The global object.
    pub global: Gc<JsObject>,
    /// `Object.prototype`.
    pub object_proto: Gc<JsObject>,
    /// `Function.prototype`, which is itself callable.
    pub function_proto: Gc<JsObject>,
    /// `Array.prototype`.
    pub array_proto: Gc<JsObject>,
    /// Reference cells for globals declared by the script.
    ///
    /// A declared global is read with `get_var` (by name) but written with
    /// `put_var_ref` (through the cell), so the cell is what makes a function
    /// declaration and its uses agree on one binding.
    pub globals: BTreeMap<String, Gc<Cell>>,
    /// Names of the globals that came from a *declaration*, which are
    /// non-configurable properties of the global object.
    pub declared: Vec<String>,
    /// Names among the lexical globals that are `const`.
    ///
    /// `const`-ness cannot be read back off the bytecode in every case: a closure that
    /// refers to a global `const` gets a `GlobalRef` entry whose flags the forwarding
    /// path does not carry, so a store through it would look like a plain `let`. The
    /// realm records it once, at declaration, and answers for every reader.
    pub lexical_consts: BTreeSet<String>,
    /// The error constructors by class name, so a runtime error can throw a real
    /// `TypeError` instance rather than a look-alike object.
    pub errors: BTreeMap<&'static str, Gc<JsObject>>,
    /// `String.prototype`, used by `ToString` on primitives and by the boxing path.
    pub string_proto: Option<Gc<JsObject>>,
    /// `Number.prototype`.
    pub number_proto: Option<Gc<JsObject>>,
    /// `Boolean.prototype`.
    pub boolean_proto: Option<Gc<JsObject>>,
    /// `BigInt.prototype`.
    pub bigint_proto: Option<Gc<JsObject>>,
    /// `Error.prototype`, the prototype of every error instance.
    pub error_proto: Gc<JsObject>,
    /// `Date.prototype`, so the constructor can build an instance in the
    /// function-call form (`Date()` returns a string) without going through `new`.
    pub date_proto: Gc<JsObject>,
    /// `%GeneratorPrototype%`, the prototype of every generator object.
    pub generator_proto: Gc<JsObject>,
    /// `Promise.prototype`, the prototype of every promise.
    pub promise_proto: Option<Gc<JsObject>>,
    /// `%AsyncGeneratorPrototype%`, the prototype of every `async function*` result.
    pub async_generator_proto: Option<Gc<JsObject>>,
    /// `Symbol.prototype`.
    pub symbol_proto: Option<Gc<JsObject>>,
    /// The symbols behind the engine's own hidden slots: `_value`, `_class`,
    /// `_date_value`.
    ///
    /// These are *symbol* keys on purpose. A `"_value"` property is visible to
    /// `Object.getOwnPropertyNames`, so `new Number(1)` reported an own property the
    /// language does not have and `new Error()` listed `_class` beside `message` and
    /// `stack` — and a caller that copies every own property (which
    /// `Object.create(null, props)` effectively does) tried to write an internal name and
    /// threw. A symbol key is invisible to `getOwnPropertyNames`, `Object.keys`, `for…in`
    /// and `JSON.stringify`, while remaining an ordinary traced property.
    ///
    /// `RefCell` because interning is memoisation rather than observable state: filling the
    /// cache through a shared borrow is a detail, not a mutation of the realm. That is what
    /// lets [`Interp::internal_set`] work from `&self`, which `make_error` needs — an error
    /// built while reporting a failure must not require an exclusive borrow.
    pub internal_slots: std::cell::RefCell<BTreeMap<&'static str, Value>>,

    /// The `Symbol.for` registry: `key -> symbol`.
    ///
    /// Scoped to the realm, which is what makes `Symbol.for("k")` answer the same
    /// symbol every time within one script while a fresh realm starts empty.
    pub symbol_registry: BTreeMap<String, Value>,
    /// `RegExp.prototype`, filled in by the `RegExp` builtin.
    pub regexp_proto: Option<Gc<JsObject>>,
    /// The `RegExp` constructor.
    pub regexp_ctor: Option<Gc<JsObject>>,
    /// `SuppressedError.prototype`, which `using` builds its aggregates from.
    ///
    /// The prototype rather than the constructor on purpose: a script that replaces
    /// `SuppressedError.prototype.constructor` must not be able to divert the engine's own
    /// error wrapping, which the `using` test suite checks explicitly.
    pub suppressed_error_proto: Option<Gc<JsObject>>,
    /// The `DisposableStack` prototype, for `move`'s intrinsic construction.
    pub disposable_stack_proto: Option<Gc<JsObject>>,
    /// `Map.prototype`.
    pub map_proto: Option<Gc<JsObject>>,
    /// `Set.prototype`.
    pub set_proto: Option<Gc<JsObject>>,
    /// Every live `FinalizationRegistry`, held weakly.
    ///
    /// The cleanup callback has to run from a *safe point* — a place where script is not
    /// already executing and no engine borrow is held — and the collector cannot enumerate
    /// heap objects by type. Registering each registry here gives the drain a list to walk.
    ///
    /// The handles are weak on purpose: a registry the script dropped must be collectable,
    /// and it must not be resurrected merely because the drain knew about it. Entries whose
    /// target has expired are swept out as the drain walks, so the vector does not grow
    /// without bound in a program that creates many short-lived registries.
    pub finalization_registries: Vec<WeakGc<JsObject>>,
    /// `ArrayBuffer.prototype`.
    pub array_buffer_proto: Option<Gc<JsObject>>,
    /// `%TypedArray%.prototype`, the prototype every view type inherits from.
    pub typed_array_proto: Option<Gc<JsObject>>,
    /// One prototype per view type, in [`builtins::typedarray::ELEMENT_TYPES`] order.
    ///
    /// A typed array answers `map`/`filter`/`slice` with a *new view of the same type*, and
    /// the result has to carry the per-type prototype — otherwise `r.constructor.name` is
    /// `"Object"` and `r instanceof Int32Array` is false.
    pub typed_array_protos: Vec<Gc<JsObject>>,
    /// `DataView.prototype`.
    pub data_view_proto: Option<Gc<JsObject>>,
    /// The `AsyncDisposableStack` prototype.
    pub async_disposable_stack_proto: Option<Gc<JsObject>>,
    /// `Iterator.prototype`.
    pub iterator_proto: Option<Gc<JsObject>>,
    /// `%IteratorHelperPrototype%`, the prototype every lazy helper object gets.
    ///
    /// It sits between the helper and `Iterator.prototype`, which is how a helper is both
    /// an `Iterator` and recognisable as a helper (`Object.prototype.toString` says
    /// `[object Iterator Helper]`).
    pub iterator_helper_proto: Option<Gc<JsObject>>,
    /// `%WrapForValidIteratorPrototype%`, the prototype `Iterator.from` gives an iterable
    /// that was not already an `Iterator`.
    ///
    /// Distinct from the helper prototype on purpose: it forwards `next` and `return`
    /// untouched and does *not* override `@@toStringTag`, so
    /// `Object.prototype.toString.call(Iterator.from(xs))` says `[object Iterator]`.
    pub iterator_wrap_proto: Option<Gc<JsObject>>,
    /// `%GeneratorFunction.prototype%` — what a generator function inherits from.
    pub generator_function_proto: Option<Gc<JsObject>>,
    /// `%AsyncFunction.prototype%`.
    pub async_function_proto: Option<Gc<JsObject>>,
    /// `%AsyncGeneratorFunction.prototype%`.
    pub async_generator_function_proto: Option<Gc<JsObject>>,
    /// One `%…IteratorPrototype%` per kind, keyed by its `@@toStringTag`.
    ///
    /// Cached because the *identity* is observable: `Object.getPrototypeOf([].values())`
    /// has to equal `Object.getPrototypeOf([][Symbol.iterator]())`, and building a fresh
    /// prototype per iterator made every array iterator's prototype distinct.
    pub iterator_protos: std::collections::BTreeMap<String, Gc<JsObject>>,
    /// `Symbol.iterator`, `Symbol.toPrimitive` and the other well-known symbols.
    ///
    /// Held as plain `Value`s rather than looked up on `Symbol` each time: the runtime
    /// asks for them on every `for…of` and every `ToPrimitive`, and a realm field is
    /// both faster and impossible to break from a script.
    well_known_symbols: WellKnownSymbols,
}

/// One binding of the frame that called `eval`, so the eval'd code can read and
/// write it by name.
struct EvalBinding {
    /// The name it was declared with.
    name: String,
    /// Which entry of [`Interp::frames`] owns it. An `eval` sees the whole chain of
    /// calling frames, not only the innermost one: `function f(){ eval("a") }` inside
    /// a function that declares `a` must find that `a`.
    frame: usize,
    /// Which table of that frame it lives in.
    slot: EvalSlot,
    /// The slot index within that table.
    index: usize,
}

/// Which table of a frame an [`EvalBinding`] indexes.
#[derive(Clone, Copy)]
enum EvalSlot {
    Arg,
    Local,
}

/// One activation record.
struct Frame {
    /// The bytecode being run.
    code: Gc<CompiledCode>,
    /// The cells this closure captured.
    env: Gc<ClosureEnv>,
    /// The `this` binding.
    this: Value,
    /// Argument slots, sized to the declared parameter count.
    args: Vec<Value>,
    /// The values the caller actually passed, which is what `arguments` exposes.
    passed: Vec<Value>,
    /// Local slots.
    locals: Vec<Value>,
    /// Reference cells for this frame's own captured bindings.
    cells: Vec<Gc<Cell>>,
    /// The operand stack.
    stack: Vec<Value>,
    /// Variables a direct `eval` has declared in this call; see [`LocalVars`].
    local_vars: Option<Gc<LocalVars>>,

    /// Byte offset of the next instruction.
    pc: usize,
    /// Byte offset of the instruction being executed, which is what a stack trace
    /// reports. `pc` has already moved past it by the time an instruction runs.
    at: usize,
    /// The callee, so a native builtin can tell which constructor it is.
    callee: Value,
    /// `new.target`: the constructor under `new`, `undefined` for a plain call.
    new_target: Value,
    /// Whether this frame's body ended in a `tail_call`.
    ///
    /// The reference's `tail_call` is not a frame-reusing tail call: it calls the callee
    /// with its own frame just like `call` does and only skips storing the result
    /// (`goto done` in `quickjs.c`). So a runaway tail recursion still hits
    /// `InternalError: stack overflow` there, and the flag reproduces that: the frame is
    /// pushed as usual, and this marker makes its return value the *caller's* return
    /// value without running the instructions after the call.
    tail: bool,
}

impl js_gc::Trace for Frame {
    fn trace(&self, tracer: &mut js_gc::Tracer) {
        self.code.trace(tracer);
        self.env.trace(tracer);
        self.this.trace(tracer);
        for value in &self.args {
            value.trace(tracer);
        }
        for value in &self.passed {
            value.trace(tracer);
        }
        for value in &self.locals {
            value.trace(tracer);
        }
        for cell in &self.cells {
            cell.trace(tracer);
        }
        if let Some(local_vars) = &self.local_vars {
            tracer.visit(local_vars);
        }
        for value in &self.stack {
            value.trace(tracer);
        }
        self.callee.trace(tracer);
        self.new_target.trace(tracer);
    }
}

/// The magic number `next()` resumes a `yield` with. `0` is falsy, which is the value the
/// emitted `if_false` treats as "keep going".
const RESUME_NEXT: i32 = 0;
/// The magic number `return()` resumes a `yield` with; any non-zero value makes the
/// generator fall into its `return_async`.
const RESUME_RETURN: i32 = 1;

/// One microtask.
///
/// The queue holds *data* rather than closures because its contents are GC values: a
/// closure would be invisible to the collector, and a pending reaction is exactly the
/// kind of thing that must keep its handlers and its promise alive.
pub enum Job {
    /// A `then` reaction about to run.
    Reaction {
        /// The `onFulfilled`/`onRejected` to call, or `None` for the pass-through case.
        handler: Option<Value>,
        /// The value or reason to call it with.
        argument: Value,
        /// Whether the source promise rejected, which decides the pass-through kind.
        rejected: bool,
        /// The promise the answer settles.
        promise: Gc<JsObject>,
    },
    /// Adopt a thenable: call its `then` with the adopting promise's resolve/reject.
    Thenable {
        /// The `then` that was found on the value.
        then: Value,
        /// The thenable itself, which becomes `then`'s `this`.
        value: Value,
        /// The promise being resolved.
        promise: Gc<JsObject>,
    },
    /// A pending `import(specifier)`.
    Import {
        /// The specifier, already converted to a string.
        specifier: String,
        /// The module that asked, which the specifier resolves against.
        referrer: String,
        /// The promise `import()` handed back.
        promise: Gc<JsObject>,
    },
    /// An async generator resuming after the value it awaited settled.
    GeneratorResume {
        /// The async generator to continue.
        generator: Gc<JsObject>,
        /// The promise the pending method returned.
        promise: Gc<JsObject>,
        /// Whether this delivers the value or raises the reason.
        kind: ResumeKind,
        /// The settled value, or the rejection reason.
        value: Value,
    },
    /// An async function resuming after the value it awaited settled.
    Resume {
        /// The body, parked in a coroutine.
        coroutine: Gc<JsObject>,
        /// The promise the async function handed its caller.
        promise: Gc<JsObject>,
        /// Whether this resumption delivers a value or raises one.
        kind: ResumeKind,
        /// The settled value, or the rejection reason.
        value: Value,
    },
}

/// One macro-task: work the event loop owes the script but has not run yet.
///
/// The microtask queue ([`Job`]) is different: those run before the host regains control.
/// A timer is the opposite — it is explicitly "later", and only an event loop can honour
/// that. Keeping it here rather than in the host is what lets `run_one_turn` be pure
/// logic: the engine decides *what* is due, the host decides *how to wait*.
///
/// The callback is a [`Value`], so the [`Gc`] inside it keeps the function alive by
/// reference counting even though nothing else references it — the same rule the `HostFn`
/// documentation states from the other side (a host closure must not hold a `Gc`, so the
/// `Gc` lives here instead).
pub enum Task {
    /// A `setTimeout` that is due at `due_at`.
    ///
    /// There is no `setInterval` variant because the reference has no `os.setInterval` at
    /// all: `quickjs-libc.c` defines only `setTimeout` and `clearTimeout`, and
    /// `typeof os.setInterval` is `"undefined"` under `qjs --std`. A repeat field here
    /// would give this engine a timer the reference cannot produce, with nothing to
    /// compare it against.
    Timer {
        /// The id `setTimeout` returned, for `clearTimeout`.
        id: i32,
        /// What to call when it fires.
        callback: Value,
        /// When it becomes runnable, as a monotonic instant.
        due_at: std::time::Instant,
    },
}

/// A descriptor the host has to poll, and which directions have a handler.
///
/// The engine cannot poll for itself: `quickjs-pure` has no dependencies and no platform
/// knowledge, and `poll(2)` is neither. So the loop hands this list to the host, the host
/// blocks on it, and the host reports back through [`Interp::fire_fd`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FdInterest {
    /// The descriptor to watch.
    pub fd: i32,
    /// Whether `os.setReadHandler` registered a callback.
    pub read: bool,
    /// Whether `os.setWriteHandler` registered a callback.
    pub write: bool,
}

/// The callbacks registered for one descriptor, per direction.
#[derive(Default)]
pub(crate) struct FdHandlers {
    /// `os.setReadHandler`'s callback.
    pub read: Option<Value>,
    /// `os.setWriteHandler`'s callback.
    pub write: Option<Value>,
}

/// The file name `eval` reports, which the reference does not inherit from the caller.
///
/// Kept distinct from [`EVAL_SCRIPT_FILE`] because the two are named separately in the
/// reference and the names show up in stack traces.
const EVAL_FILE: &str = "<input>";

/// The file name `std.evalScript` reports.
const EVAL_SCRIPT_FILE: &str = "<evalScript>";

/// A delay in milliseconds as a `Duration`, clamped to something `Instant` accepts.
///
/// Negative and `NaN` both mean "as soon as possible": the reference runs the delay through
/// `JS_ToInt64`, so `setTimeout(f, NaN)` fires immediately rather than being rejected.
fn clamp_delay(delay_ms: f64) -> std::time::Duration {
    // `is_nan()` spelled out rather than `!(x > 0.0)`: the latter is a negated comparison
    // on a float, which reads as a typo even when it is deliberate. Both NaN and a
    // non-positive delay mean "due now".
    if delay_ms.is_nan() || delay_ms <= 0.0 {
        return std::time::Duration::ZERO;
    }
    std::time::Duration::from_secs_f64(delay_ms / 1000.0)
}

impl js_gc::Trace for Task {
    fn trace(&self, tracer: &mut js_gc::Tracer) {
        let Task::Timer { callback, .. } = self;
        callback.trace(tracer);
    }
}

/// What one turn of the event loop did, so the caller cannot spin by accident.
///
/// A bare `bool` invites the mistake of looping on it without sleeping, which burns a
/// core until the timer is due. Naming the three cases makes the wait explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// Something ran. Call again immediately — there may be more ready work.
    Ran,
    /// Nothing is runnable, but descriptors are registered: the host must poll
    /// [`Interp::pending_fds`] for at most this long, then report readiness with
    /// [`Interp::fire_fd`].
    ///
    /// A separate variant from [`Turn::Wait`] on purpose. Sleeping instead of polling is
    /// the failure this project treats as worst — a handler that silently never fires —
    /// so the two cannot be confused by accident.
    Poll(std::time::Duration),
    /// Nothing is runnable yet, but something is pending. Block at most this long, then
    /// call again.
    Wait(std::time::Duration),
    /// Nothing is pending at all: the loop is done.
    Done,
}

impl js_gc::Trace for Job {
    fn trace(&self, tracer: &mut js_gc::Tracer) {
        match self {
            Job::Reaction {
                handler,
                argument,
                promise,
                ..
            } => {
                if let Some(handler) = handler {
                    handler.trace(tracer);
                }
                argument.trace(tracer);
                tracer.visit(promise);
            }
            Job::Thenable {
                then,
                value,
                promise,
            } => {
                then.trace(tracer);
                value.trace(tracer);
                tracer.visit(promise);
            }
            Job::Resume {
                coroutine,
                promise,
                value,
                ..
            } => {
                tracer.visit(coroutine);
                tracer.visit(promise);
                value.trace(tracer);
            }
            Job::Import { promise, .. } => tracer.visit(promise),
            Job::GeneratorResume {
                generator,
                promise,
                value,
                ..
            } => {
                tracer.visit(generator);
                tracer.visit(promise);
                value.trace(tracer);
            }
        }
    }
}

/// Which of `%GeneratorPrototype%`'s three methods resumed a generator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    /// The generator was just created and is being run to its first suspension.
    Start,
    /// `next(v)`.
    Next,
    /// `return(v)`.
    Return,
    /// `throw(e)`.
    Throw,
}

/// A generator object's saved execution.
///
/// The reference keeps this as a `JSAsyncFunctionState` hanging off the object; the shape
/// here is the same, with a whole [`Frame`] standing in for its `JSStackFrame` plus the
/// separately allocated buffers.
pub struct GeneratorState {
    /// The detached frame, or `None` once the generator has finished or while it runs.
    frame: Option<Frame>,
    /// How far it got.
    phase: GeneratorPhase,
    /// Whether the last suspension was an `await` rather than a `yield`.
    ///
    /// The two resume with *different stack shapes*: a `yield` is re-entered with the
    /// value and a magic number, because the emitted code branches on which method
    /// resumed it, while an `await` simply continues with its value. Getting this wrong
    /// leaves an extra value on the operand stack that the next instruction then
    /// consumes — a wrong answer rather than a crash.
    parked_on_await: bool,
}

/// Which of §27.5.3.3's states a generator is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GeneratorPhase {
    /// Created, `initial_yield` reached, body not entered.
    SuspendedStart,
    /// Parked at a `yield`.
    SuspendedYield,
    /// Between a resume and its suspension, which a re-entrant `next()` must refuse.
    Executing,
    /// Finished; the frame is gone.
    Completed,
}

impl js_gc::Trace for GeneratorState {
    fn trace(&self, tracer: &mut js_gc::Tracer) {
        if let Some(frame) = &self.frame {
            frame.trace(tracer);
        }
    }
}

/// The realm's well-known symbols, indexed by the order [`builtins::symbol`] creates
/// them in.
///
/// Each is a field rather than a map entry because the runtime reads them on hot paths
/// (`for…of`, `ToPrimitive`) and because a missing one would be a bug, not a lookup
/// failure — reaching a default here means the builtin did not install, and every
/// accessor panics rather than silently answering `undefined`.
///
/// [`builtins::symbol`]: crate::builtins::symbol
#[derive(Default)]
pub struct WellKnownSymbols {
    /// The symbol values, positionally. Empty until `Symbol` installs.
    symbols: Vec<Value>,
}

/// Which well-known symbol a caller wants.
///
/// A typed accessor beats passing a name around: a typo is a compile error rather than
/// an `undefined` key that quietly matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WellKnown {
    /// `Symbol.toPrimitive`.
    ToPrimitive,
    /// `Symbol.iterator`.
    Iterator,
    /// `Symbol.match`.
    Match,
    /// `Symbol.matchAll`.
    MatchAll,
    /// `Symbol.replace`.
    Replace,
    /// `Symbol.search`.
    Search,
    /// `Symbol.split`.
    Split,
    /// `Symbol.toStringTag`.
    ToStringTag,
    /// `Symbol.isConcatSpreadable`.
    IsConcatSpreadable,
    /// `Symbol.hasInstance`.
    HasInstance,
    /// `Symbol.species`.
    Species,
    /// `Symbol.unscopables`.
    Unscopables,
    /// `Symbol.asyncIterator`.
    AsyncIterator,
    /// `Symbol.dispose` — the well-known name of a disposable resource's disposer.
    Disposable,
    /// `Symbol.asyncDispose` — the awaited form.
    AsyncDisposable,
}

impl WellKnown {
    /// The index `Symbol`'s installer creates this symbol at.
    fn index(self) -> usize {
        match self {
            WellKnown::ToPrimitive => 0,
            WellKnown::Iterator => 1,
            WellKnown::Match => 2,
            WellKnown::MatchAll => 3,
            WellKnown::Replace => 4,
            WellKnown::Search => 5,
            WellKnown::Split => 6,
            WellKnown::ToStringTag => 7,
            WellKnown::IsConcatSpreadable => 8,
            WellKnown::HasInstance => 9,
            WellKnown::Species => 10,
            WellKnown::Unscopables => 11,
            WellKnown::AsyncIterator => 12,
            WellKnown::Disposable => 13,
            WellKnown::AsyncDisposable => 14,
        }
    }
}

impl Realm {
    /// Record the well-known symbols, in the order `Symbol`'s installer made them.
    pub fn set_well_known_symbols(&mut self, symbols: Vec<Value>) {
        self.well_known_symbols.symbols = symbols;
    }

    /// A well-known symbol.
    pub fn well_known(&self, which: WellKnown) -> Value {
        self.well_known_symbols
            .symbols
            .get(which.index())
            .cloned()
            .unwrap_or(Value::Undefined)
    }

    /// The well-known symbol as a property key.
    pub fn well_known_key(&self, which: WellKnown) -> PropertyKey {
        match self.well_known(which) {
            Value::Symbol(symbol) => PropertyKey::Symbol(symbol),
            // Unreachable once `Symbol` has installed; an empty string key matches
            // nothing, which degrades to "the object has no such method".
            _ => PropertyKey::from(String::new()),
        }
    }
}

/// The interpreter: a realm plus a frame stack.
pub struct Interp<'a> {
    /// The heap everything is allocated on.
    pub heap: &'a Heap,
    /// The realm.
    pub realm: Realm,
    /// Active frames, outermost first.
    frames: Vec<Frame>,
    /// A strict-mode `eval`'s own variable scope, while its script runs.
    ///
    /// Strict eval gets a *fresh* variable environment, so a `var` inside it does not
    /// become a global. Name lookups still fall through to the realm, which is why
    /// `eval("a")` sees a global `a` and `eval("a = 3")` writes it.
    eval_scope: Option<BTreeMap<String, Value>>,
    /// How deep the interpreter currently is inside an `eval`, so a `var` it declares can
    /// be created configurable — the distinction `(1, eval)("var g")` exists to exploit.
    eval_depth: usize,
    /// The calling frame's own bindings, which the running `eval` can read and write.
    ///
    /// Held separately from [`Interp::eval_scope`] because these already exist: reads
    /// come from the frame, and a write is applied back to the frame (or to the cell
    /// the frame captured) when the eval finishes. That is what makes
    /// `f("a = 3")` visible to the caller as `a === 3`.
    eval_bindings: Vec<EvalBinding>,
    /// The calling frame's `arguments`, while an `eval` runs.
    ///
    /// Kept apart from [`Interp::eval_scope`] because a sloppy eval has no scope of its
    /// own yet still has to see the caller's `arguments`.
    eval_arguments: Option<Value>,
    /// The `with` objects live at the moment the running `eval` started, innermost first.
    ///
    /// `None` outside an eval, which is what keeps a finished block's object from
    /// shadowing ordinary bytecode reads.
    eval_with: Option<Vec<Gc<JsObject>>>,
    /// The bindings the running direct `eval` makes visible, as name → cell.
    ///
    /// Set for the duration of an eval and then restored, but every closure created
    /// during it keeps a handle (see [`ClosureEnv::eval_scope`]), which is what lets
    /// `eval("f = () => i")` work after the eval has returned.
    active_eval_scope: Option<Gc<EvalScope>>,
    /// The frame that called the direct `eval` currently running, if any.
    ///
    /// A sloppy eval declares its `var`s in that frame's variable environment rather than
    /// on the global object, which is what keeps
    /// `function f(){ eval("var x=1") } f(); x` from defining a global `x` — and what
    /// gives each call its own `x`.
    eval_caller: Option<Option<usize>>,
    /// The caller's `new.target`, while an `eval` runs.
    ///
    /// The eval'd code is a script with no constructor of its own, so it inherits the
    /// one the enclosing function was called with.
    eval_new_target: Option<Value>,
    /// `new.target` for the frame about to be pushed.
    ///
    /// `construct` parks the constructor here and `push_frame` takes it, which is how the
    /// value reaches the frame without threading a parameter through every call site.
    pending_new_target: Option<Value>,
    /// Names of the native functions currently executing, outermost first.
    ///
    /// A native has no [`Frame`] — it runs on the Rust stack — but the reference's
    /// trace still names it with `(native)` instead of a position, so the names are
    /// tracked separately and stitched into [`Interp::stack_trace`].
    ///
    /// The third field is how many bytecode frames were live when the native was entered,
    /// which is what places it *among* them rather than before all of them. Native
    /// functions used to be emitted up front on the grounds that they never re-enter
    /// JavaScript; `std.evalScript` does, so that reasoning no longer holds.
    native_frames: Vec<(String, Gc<JsObject>, usize)>,
    /// The native function currently executing, for the ones that have to know *which*
    /// function they are — a typed-array constructor reads its element type off itself.
    ///
    /// `current_callee` cannot answer this: a native call pushes no frame of its own, so it
    /// reports whatever the enclosing bytecode frame was called with.
    current_native: Option<Gc<JsObject>>,
    /// Text written by `print` and friends, in order.
    ///
    /// Buffered rather than streamed so a test can inspect it; the CLI flushes it
    /// once at the end, which is also when upstream's line buffering becomes
    /// visible for the cases compared here.
    pub output: String,
    /// Where `print` also writes, as it goes. `output` keeps the full log for
    /// [`crate::Context::output`]; this is the live copy, flushed per line.
    ///
    /// Both exist on purpose. A host that gives a script a sink sees output *when it
    /// happens* — which is what a timer callback's `console.log` has to do, and what the
    /// reference does with `fputs` + `fflush` — while embedders and the 595 assertions in
    /// this repository keep reading the accumulated `String`. Buffering only (no sink)
    /// would show a script's output all at once when it finished.
    pub output_sink: Option<Box<dyn std::io::Write>>,
    /// The most frames that may be live at once.
    pub max_frames: usize,
    /// The microtask queue, but see `Interp::jobs` — shared with the module machinery.
    ///
    /// Script execution does not return until this has drained, which is what makes
    /// `Promise.resolve().then(f)` run `f` before the next statement of the *host* rather
    /// than never. Reactions append to it while it drains, so the loop is a `while` and
    /// not a `for`.
    pub(crate) jobs: std::collections::VecDeque<Job>,
    /// Macrotasks: the timers an event loop has to wait for. See [`Task`].
    ///
    /// A `Vec` rather than a heap because the queue is short (a script rarely has more than
    /// a handful) and `run_one_turn` has to scan it anyway to find the earliest deadline.
    pub(crate) tasks: Vec<Task>,
    /// Active `backtrace_barrier` depths: how many bytecode frames were live when the
    /// barrier'd evaluation started.
    ///
    /// A trace is truncated to the frames at or inside the innermost barrier, which is what
    /// lets a REPL report an error from the line it just evaluated without also reporting
    /// its own readline loop.
    trace_barriers: Vec<usize>,
    /// `os.signal` handlers, keyed by signal number.
    ///
    /// Kept here for the same reason as `fd_handlers`: the collector walks these values, so
    /// a handler cannot be freed while it is still registered.
    pub(crate) signal_handlers: std::collections::BTreeMap<i32, Value>,
    /// `os.setReadHandler`/`os.setWriteHandler` callbacks, keyed by descriptor.
    ///
    /// Kept here rather than in a closure so the collector walks the callbacks: a handler
    /// that is only reachable from a host closure could be freed while still registered.
    pub(crate) fd_handlers: std::collections::BTreeMap<i32, FdHandlers>,
    /// The next timer id.
    ///
    /// Starts at 1 and wraps back to 1 at `INT32_MAX`, matching `js_os_setTimeout`, because
    /// the id reaches the script as an `Int32` and `clearTimeout(0)` must never match.
    next_timer_id: i32,
    /// Errors thrown by timer callbacks, already rendered.
    ///
    /// Kept apart from `unhandled`: that list is rejected promises, and a timer callback
    /// that throws has no promise behind it. Both are reported by the host at the end of
    /// the loop, but only one is fixed up by a later `.catch`.
    timer_errors: Vec<String>,
    /// The module registry, keyed by canonical name.
    pub modules: std::collections::BTreeMap<String, Gc<crate::module::ModuleRecord>>,
    /// How the host turns a specifier into bytes.
    pub module_loader: Option<Box<dyn crate::module::ModuleLoader>>,
    /// The module the host was asked to run, for `import.meta.main`.
    pub main_module_name: Option<String>,
    /// Module bodies currently running, innermost last.
    ///
    /// A dynamic `import()` resolves its specifier relative to the module that ran it, and
    /// this is the only place that knows which module that is.
    pub module_stack: Vec<String>,
    /// Rejections that no handler has claimed, with the promise they belong to.
    ///
    /// A `then` registered later prunes its entry, which is why the promise is kept: the
    /// reason alone cannot say whether somebody eventually listened.
    unhandled: Vec<(Gc<JsObject>, Value)>,
}

impl<'a> Interp<'a> {
    /// Build an interpreter over `heap`, with a fresh realm.
    pub fn new(heap: &'a Heap) -> Interp<'a> {
        let object_proto = JsObject::ordinary(heap, None);
        let function_proto = JsObject::function(
            heap,
            Some(object_proto.clone()),
            Callable::Native {
                func: |_, _, _| Ok(Value::Undefined),
                own_instance: false,
            },
        );
        let array_proto = JsObject::array(heap, Some(object_proto.clone()), 0);
        let global = JsObject::ordinary(heap, Some(object_proto.clone()));
        let mut interp = Interp {
            heap,
            realm: Realm {
                global,
                object_proto,
                function_proto,
                array_proto,
                globals: BTreeMap::new(),
                declared: Vec::new(),
                lexical_consts: BTreeSet::new(),
                errors: BTreeMap::new(),
                string_proto: None,
                number_proto: None,
                boolean_proto: None,
                bigint_proto: None,
                error_proto: JsObject::ordinary(heap, None),
                date_proto: JsObject::ordinary(heap, None),
                generator_proto: JsObject::ordinary(heap, None),
                promise_proto: None,
                async_generator_proto: None,
                symbol_proto: None,
                symbol_registry: BTreeMap::new(),
                internal_slots: std::cell::RefCell::new(BTreeMap::new()),
                regexp_proto: None,
                regexp_ctor: None,
                suppressed_error_proto: None,
                disposable_stack_proto: None,
                map_proto: None,
                set_proto: None,
                finalization_registries: Vec::new(),
                array_buffer_proto: None,
                typed_array_proto: None,
                typed_array_protos: Vec::new(),
                data_view_proto: None,
                async_disposable_stack_proto: None,
                iterator_proto: None,
                iterator_helper_proto: None,
                iterator_wrap_proto: None,
                generator_function_proto: None,
                async_function_proto: None,
                async_generator_function_proto: None,
                iterator_protos: std::collections::BTreeMap::new(),
                well_known_symbols: WellKnownSymbols::default(),
            },
            frames: Vec::new(),
            eval_scope: None,
            eval_depth: 0,
            eval_bindings: Vec::new(),
            eval_arguments: None,
            eval_with: None,
            active_eval_scope: None,
            eval_caller: None,
            pending_new_target: None,
            eval_new_target: None,
            native_frames: Vec::new(),
            current_native: None,
            output: String::new(),
            output_sink: None,
            max_frames: 4096,
            jobs: std::collections::VecDeque::new(),
            tasks: Vec::new(),
            trace_barriers: Vec::new(),
            signal_handlers: std::collections::BTreeMap::new(),
            fd_handlers: std::collections::BTreeMap::new(),
            next_timer_id: 1,
            timer_errors: Vec::new(),
            modules: std::collections::BTreeMap::new(),
            module_loader: None,
            main_module_name: None,
            module_stack: Vec::new(),
            unhandled: Vec::new(),
        };
        crate::builtins::install(&mut interp);
        interp
    }

    // -- allocation helpers -------------------------------------------------

    /// A new string value.
    /// A `BigInt` value from a computed big integer.
    pub fn bigint(&self, value: crate::bigint::BigInt) -> Value {
        Value::BigInt(Gc::new(self.heap, value))
    }

    /// A string value from bytes that are already WTF-8, which is how a string with an
    /// unpaired surrogate reaches a `Value`.
    pub fn string_wtf8(&self, text: crate::value::Wtf8) -> Value {
        Value::string_wtf8(self.heap, text)
    }

    /// A new string value, from text that is valid UTF-8.
    pub fn string(&self, text: impl Into<String>) -> Value {
        Value::string(self.heap, text)
    }

    /// A new ordinary object with the given prototype.
    pub fn new_object(&self, proto: Option<Gc<JsObject>>) -> Gc<JsObject> {
        JsObject::ordinary(self.heap, proto)
    }

    /// A prototype for a built-in iterator, inheriting from `Iterator.prototype`.
    ///
    /// Every built-in iterator has one — `%ArrayIteratorPrototype%`,
    /// `%StringIteratorPrototype%` and so on — and the whole point of the chain is that
    /// such an iterator *is* an `Iterator`: `[1,2].values().map(f)` resolves `map` by
    /// walking up to `Iterator.prototype`. These used to be plain objects hanging off
    /// `Object.prototype`, so `.map` was `undefined` and `Object.prototype.toString`
    /// said `[object Object]` instead of `[object Array Iterator]`.
    ///
    /// The prototype carries `next`, `[Symbol.iterator]` answering `this`, and the
    /// `@@toStringTag` that names it. `next` is passed in because each iterator kind has
    /// its own.
    pub fn new_iterator_prototype(
        &mut self,
        tag: &str,
        next: crate::object::NativeFn,
    ) -> Gc<JsObject> {
        if let Some(existing) = self.realm.iterator_protos.get(tag) {
            return existing.clone();
        }
        let parent = self.realm.iterator_proto.clone();
        let proto = self.new_object(parent);
        let next = self.new_native("next", 0, next);
        proto.define(
            "next",
            Property::with_attributes(Value::Object(next), true, false),
        );
        let key = self.realm.well_known_key(WellKnown::Iterator);
        let identity = self.new_native("[Symbol.iterator]", 0, self_iterator);
        proto.define_key(
            &key,
            Property::with_attributes(Value::Object(identity), true, false),
        );
        let tag_key = self.realm.well_known_key(WellKnown::ToStringTag);
        let tag_value = self.string(tag);
        proto.define_key(&tag_key, Property::with_attributes(tag_value, false, false));
        self.realm
            .iterator_protos
            .insert(tag.to_string(), proto.clone());
        proto
    }

    /// `[[OwnPropertyKeys]]`, with the `ownKeys` trap when there is one.
    ///
    /// A trap's list is used **as returned**: unlike an ordinary object, whose integer
    /// keys the engine sorts, a proxy is specified to hand back exactly what the handler
    /// produced. That is why the two branches are not merged.
    pub fn object_own_property_keys(
        &mut self,
        object: &Gc<JsObject>,
    ) -> JsResult<Vec<PropertyKey>> {
        if let Some(keys) = self.proxy_own_keys(object)? {
            return Ok(keys);
        }
        // Integer-like keys ascending, then the string keys in insertion order, then the
        // symbols — the order every schema that walks an object expects.
        let entries = object.property_entries();
        let mut numeric: Vec<u32> = Vec::new();
        let mut strings: Vec<PropertyKey> = Vec::new();
        let mut symbols: Vec<PropertyKey> = Vec::new();
        for (key, _) in entries {
            match &key {
                PropertyKey::Symbol(_) => symbols.push(key),
                PropertyKey::String(_) => match key.as_str().and_then(array_index) {
                    Some(index) => {
                        if !numeric.contains(&index) {
                            numeric.push(index);
                        }
                    }
                    None => strings.push(key),
                },
            }
        }
        numeric.sort_unstable();
        let mut keys: Vec<PropertyKey> = numeric
            .into_iter()
            .map(|index| PropertyKey::from(index.to_string()))
            .collect();
        keys.extend(strings);
        keys.extend(symbols);
        Ok(keys)
    }

    /// The state behind a promise object, or `None` when `object` is not one.
    pub fn promise_state(&self, object: &Gc<JsObject>) -> Option<crate::object::PromiseRef> {
        object.with(|data| match &data.kind {
            ObjectKind::Promise(state) => Some(state.clone()),
            _ => None,
        })
    }

    /// A new pending promise.
    pub fn new_promise(&mut self) -> Gc<JsObject> {
        let proto = self.realm.promise_proto.clone();
        let promise = JsObject::ordinary(self.heap, proto);
        promise.with_mut(|data| {
            data.kind = ObjectKind::Promise(std::rc::Rc::new(std::cell::RefCell::new(
                crate::object::PromiseState::default(),
            )));
        });
        promise
    }

    /// Fulfil `promise`, unless it has already settled.
    ///
    /// Settling is one-shot, and a script can see that: `resolve` called twice, or called
    /// after `reject`, changes nothing.
    pub fn fulfill_promise(&mut self, promise: &Gc<JsObject>, value: Value) {
        let Some(state) = self.promise_state(promise) else {
            return;
        };
        let reactions = {
            let mut state = state.borrow_mut();
            if !matches!(state.status, crate::object::PromiseStatus::Pending) {
                return;
            }
            state.status = crate::object::PromiseStatus::Fulfilled(value.clone());
            std::mem::take(&mut state.reactions)
        };
        self.queue_reactions(reactions, value, false);
    }

    /// Reject `promise`, unless it has already settled.
    pub fn reject_promise(&mut self, promise: &Gc<JsObject>, reason: Value) {
        let Some(state) = self.promise_state(promise) else {
            return;
        };
        let reactions = {
            let mut state = state.borrow_mut();
            if !matches!(state.status, crate::object::PromiseStatus::Pending) {
                return;
            }
            state.status = crate::object::PromiseStatus::Rejected(reason.clone());
            std::mem::take(&mut state.reactions)
        };
        // Remember it only when nothing is listening *now*; a `then` registered later
        // prunes the entry in `perform_then`.
        if reactions.is_empty() {
            self.unhandled.push((promise.clone(), reason.clone()));
        }
        self.queue_reactions(reactions, reason, true);
    }

    /// Queue one job per reaction.
    ///
    /// This is what makes `then` asynchronous even on an already-settled promise:
    /// `p.then(f); g()` calls `g` first.
    fn queue_reactions(
        &mut self,
        reactions: Vec<crate::object::PromiseReaction>,
        argument: Value,
        rejected: bool,
    ) {
        for reaction in reactions {
            let handler = if rejected {
                reaction.on_rejected.clone()
            } else {
                reaction.on_fulfilled.clone()
            };
            self.jobs.push_back(Job::Reaction {
                handler,
                argument: argument.clone(),
                rejected,
                promise: reaction.promise,
            });
        }
    }

    /// `NewPromiseCapability` as the executor receives it: the promise plus its
    /// resolve/reject pair.
    ///
    /// The pair are callable objects rather than Rust closures, because the script gets
    /// them and may stash them for later.
    pub fn new_promise_capability(&mut self) -> (Gc<JsObject>, Value, Value) {
        let promise = self.new_promise();
        let resolve = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::PromiseResolve(promise.clone()),
        );
        let reject = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::PromiseReject(promise.clone()),
        );
        (promise, Value::Object(resolve), Value::Object(reject))
    }

    /// `std.evalScript(source, { async: true })`: the completion value delivered through a
    /// promise, as `{ value: completion }`.
    ///
    /// The three cases are the reference's, and they differ on purpose:
    ///
    /// * a **syntax** error throws synchronously — `JS_Eval` reports a parse failure as an
    ///   exception before any promise exists;
    /// * a **runtime** error rejects the returned promise;
    /// * success resolves it with `{ value }`, where the value is the script's completion
    ///   value **literally**. It is not awaited: `std.evalScript("Promise.resolve(7)",
    ///   {async: true})` answers `{value: <the promise>}`, and only an explicit top-level
    ///   `await` produces `{value: 7}`. That is the reference's `JS_Eval` with
    ///   `JS_EVAL_FLAG_ASYNC`, which stores the completion value rather than adopting it.
    ///
    /// # What this does not do
    ///
    /// The reference's `JS_EVAL_FLAG_ASYNC` also permits **top-level `await`**: the script
    /// is evaluated in an async context, so `await` outside a function is legal. That needs
    /// the parser and the suspension machinery to accept a suspended top-level script, and
    /// is not implemented — such source is a syntax error here. Everything else about the
    /// flag is reproduced.
    pub fn eval_in_global_async(&mut self, source: &str, barrier: bool) -> JsResult<Value> {
        // The barrier goes up before compiling: a *parse* failure is reported with a trace
        // too, and the reference truncates that one at the same place as a runtime failure.
        if barrier {
            let depth = self.frames.len();
            self.trace_barriers.push(depth);
        }
        // Compiled before the promise exists, so a parse failure propagates as a throw
        // instead of being folded into a rejection.
        let compiled = match self.compile_global_script(source, EVAL_SCRIPT_FILE) {
            Ok(compiled) => compiled,
            Err(error) => {
                if barrier {
                    self.trace_barriers.pop();
                }
                return Err(error);
            }
        };
        let (promise, resolve, reject) = self.new_promise_capability();
        let strict = compiled.strict;
        match self.run_eval_script(compiled, strict) {
            Ok(value) => {
                // `{ value }` is a plain object, so resolving with it adopts nothing even
                // when the completion value is itself a promise.
                let wrapped = self.new_object(Some(self.realm.object_proto.clone()));
                self.set_property(&wrapped, "value", value)?;
                self.call_value(resolve, Value::Undefined, vec![Value::Object(wrapped)])?;
            }
            Err(Signal::Throw(error)) => {
                self.call_value(reject, Value::Undefined, vec![error])?;
            }
            Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
        }
        if barrier {
            self.trace_barriers.pop();
        }
        Ok(Value::Object(promise))
    }

    /// A promise that resolves after `delay_ms`, which is `os.sleepAsync`.
    ///
    /// Built on [`Interp::schedule_timer`] rather than a second queue, so the resolution is
    /// ordered against `os.setTimeout` by the same event loop. The resolver is stored in the
    /// timer table, where the collector already scans it.
    pub fn sleep_async(&mut self, delay_ms: f64) -> JsResult<Value> {
        let (promise, resolve, _reject) = self.new_promise_capability();
        let resolve = self.new_host_function(
            "resolve",
            1,
            Box::new(
                move |interp: &mut Interp<'_>, _this: Value, args: &[Value]| {
                    let value = args.first().cloned().unwrap_or(Value::Undefined);
                    interp.call_value(resolve.clone(), Value::Undefined, vec![value])
                },
            ),
        );
        // The timer keeps the resolver alive: it is a property of the timer entry, and the
        // timer table is part of what the collector walks.
        self.schedule_timer(Value::Object(resolve), delay_ms);
        Ok(Value::Object(promise))
    }

    /// `PromiseResolve(value)`: the value itself when it is already a promise, otherwise a
    /// new promise resolved with it.
    pub fn promise_resolve_value(&mut self, value: Value) -> JsResult<Value> {
        if let Value::Object(object) = &value
            && self.promise_state(object).is_some()
        {
            return Ok(value);
        }
        let promise = self.new_promise();
        self.resolve_promise_with(&promise, value)?;
        Ok(Value::Object(promise))
    }

    /// `ResolvePromise`: fulfil, unless the value is a thenable — then follow it.
    pub fn resolve_promise_with(&mut self, promise: &Gc<JsObject>, value: Value) -> JsResult<()> {
        // Resolving a promise with itself is a `TypeError`. The check exists because the
        // alternative is a chain of self-reactions that never settles.
        if let Value::Object(object) = &value
            && object.ptr_eq(promise)
        {
            // The spec rejects *and* throws, so both the promise and the caller see it.
            let thrown = self.type_error("cannot resolve a promise with itself");
            let Signal::Throw(error) = thrown.clone() else {
                unreachable!("type_error always throws")
            };
            self.reject_promise(promise, error);
            return Err(thrown);
        }
        let then = match &value {
            Value::Object(object) => self.get_property(object, "then")?,
            _ => None,
        };
        let callable_then = matches!(&then, Some(Value::Object(object)) if object.is_callable());
        if !callable_then {
            // Not a thenable: a non-callable `then` makes the value fulfil as it is.
            self.fulfill_promise(promise, value);
            return Ok(());
        }
        // Adopting a thenable is asynchronous — `then` is called from a job, never from
        // inside `resolve`, so a script cannot make a promise settle synchronously.
        self.jobs.push_back(Job::Thenable {
            then: then.expect("checked callable"),
            value,
            promise: promise.clone(),
        });
        Ok(())
    }

    /// `Promise.prototype.then`: register a reaction and answer the derived promise.
    pub fn perform_then(
        &mut self,
        promise: &Gc<JsObject>,
        on_fulfilled: Option<Value>,
        on_rejected: Option<Value>,
    ) -> JsResult<Value> {
        let Some(state) = self.promise_state(promise) else {
            return Err(self.type_error("not a promise"));
        };
        // Whether a rejection handler is being attached decides whether a recorded
        // rejection stops being unhandled, so it is read before the values are moved.
        let has_rejection_handler = on_rejected.is_some();
        let derived = self.new_promise();
        let reaction = crate::object::PromiseReaction {
            on_fulfilled,
            on_rejected,
            promise: derived.clone(),
        };
        let settled = {
            let mut state = state.borrow_mut();
            // A rejection somebody reacts to is no longer unhandled.
            state.handled = true;
            match &state.status {
                crate::object::PromiseStatus::Pending => {
                    state.reactions.push(reaction.clone());
                    None
                }
                crate::object::PromiseStatus::Fulfilled(value) => Some((value.clone(), false)),
                crate::object::PromiseStatus::Rejected(reason) => Some((reason.clone(), true)),
            }
        };
        // A `catch`/`then` attached to an already-rejected promise, or to one that rejects
        // later, means somebody is listening after all.
        if has_rejection_handler {
            self.unhandled.retain(|(owner, _)| !owner.ptr_eq(promise));
        }
        if let Some((argument, rejected)) = settled {
            let handler = if rejected {
                reaction.on_rejected
            } else {
                reaction.on_fulfilled
            };
            self.jobs.push_back(Job::Reaction {
                handler,
                argument,
                rejected,
                promise: reaction.promise,
            });
        }
        Ok(Value::Object(derived))
    }

    /// The rejections nobody listened to, formatted the way the reference reports them.
    ///
    /// Each line is `Possibly unhandled promise rejection: <value>` followed by the stack
    /// when there is one. The adjective is the reference's: a rejection is only *possibly*
    /// unhandled at the point the queue drains, because a `then` may still be attached
    /// before the host acts on it.
    pub fn take_unhandled_rejections(&mut self) -> Vec<String> {
        let entries = std::mem::take(&mut self.unhandled);
        // Somebody may have attached a handler after the rejection was recorded, so the
        // promise is the authority rather than the list.
        let still_unhandled: Vec<Value> = entries
            .into_iter()
            .filter_map(|(promise, reason)| {
                let handled = self
                    .promise_state(&promise)
                    .map(|state| state.borrow().handled)
                    .unwrap_or(true);
                (!handled).then_some(reason)
            })
            .collect();
        still_unhandled
            .into_iter()
            .map(|reason| {
                let text = self.describe_exception(&reason);
                let stack = match &reason {
                    Value::Object(object) => self
                        .get_property(object, "stack")
                        .ok()
                        .flatten()
                        .map(|value| value.to_js_string())
                        // `stack` carries a trailing newline of its own, which would leave
                        // a blank line between two reported rejections.
                        .map(|text| text.trim_end().to_string())
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                if stack.is_empty() {
                    format!("Possibly unhandled promise rejection: {text}")
                } else {
                    format!("Possibly unhandled promise rejection: {text}\n{stack}")
                }
            })
            .collect()
    }

    /// Run the microtask queue to exhaustion.
    ///
    /// Jobs queued while draining run in the same drain, FIFO, which is what makes
    /// `Promise.resolve().then(f).then(g)` call `f` before `g`.
    /// Parse `text` in the extended dialect that `std.parseExtJSON` exposes.
    ///
    /// Comments, unquoted keys, single quotes, trailing commas, `+` and `.1` numbers,
    /// `Infinity`/`NaN`, and `0x`/`0o`/`0b` literals — the same relaxations the reference
    /// gates behind `JS_PARSE_JSON_EXT`. `JSON.parse` does not see any of them.
    pub fn parse_ext_json(&mut self, text: &str) -> JsResult<Value> {
        crate::builtins::json::parse_text(self, text, true)
    }

    /// Schedule `callback` to run after `delay_ms` milliseconds, returning its id.
    ///
    /// A negative or `NaN` delay runs as soon as possible rather than being rejected: the
    /// reference passes the delay through `JS_ToInt64`, so `setTimeout(f, NaN)` schedules
    /// immediately, and `setTimeout(f, -1)` is ordinary code.
    ///
    /// The callback is called with **no arguments**. The reference stores only the function
    /// (`th->func = JS_DupValue(ctx, func)`) and calls it bare, so the `setTimeout(f, 0, a,
    /// b)` passthrough that browsers offer does not exist here — verified against `qjs`:
    /// extra arguments arrive as `undefined`.
    pub fn schedule_timer(&mut self, callback: Value, delay_ms: f64) -> i32 {
        let id = self.next_timer_id;
        // Wrap rather than grow: the id reaches the script as an `Int32`.
        self.next_timer_id = if self.next_timer_id == i32::MAX {
            1
        } else {
            self.next_timer_id + 1
        };
        let due_at = std::time::Instant::now() + clamp_delay(delay_ms);
        self.tasks.push(Task::Timer {
            id,
            callback,
            due_at,
        });
        id
    }

    /// Cancel a pending timer. `false` when the id is unknown or already fired, which is
    /// not an error — `clearTimeout` on a stale id is a no-op in the reference too.
    pub fn cancel_timer(&mut self, id: i32) -> bool {
        let before = self.tasks.len();
        self.tasks.retain(|task| match task {
            Task::Timer { id: task_id, .. } => *task_id != id,
        });
        self.tasks.len() != before
    }

    /// How long until the earliest pending timer, or `None` when none is pending.
    pub fn next_timeout(&self) -> Option<std::time::Duration> {
        let now = std::time::Instant::now();
        self.tasks
            .iter()
            .map(|task| match task {
                Task::Timer { due_at, .. } => due_at.saturating_duration_since(now),
            })
            .min()
    }

    /// Run one turn of the event loop.
    ///
    /// Structurally this is `js_std_loop`'s body minus the blocking: drain microtasks, then
    /// run at most one due timer, then drain again. The blocking belongs to the caller
    /// because `quickjs-pure` has no dependencies and no platform knowledge — [`Turn::Wait`]
    /// is how it asks.
    ///
    /// One timer per turn rather than all of them: a timer callback may schedule another
    /// timer, and running the whole batch would let a self-rescheduling `setTimeout(f, 0)`
    /// starve the microtask queue.
    pub fn run_one_turn(&mut self) -> JsResult<Turn> {
        self.drain_jobs()?;
        let now = std::time::Instant::now();
        // Earliest due first: timers are not ordered by insertion, and `setTimeout(f, 100)`
        // followed by `setTimeout(g, 1)` must run `g` first.
        let due = self
            .tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| match task {
                Task::Timer { due_at, .. } => *due_at <= now,
            })
            .min_by_key(|(_, task)| match task {
                Task::Timer { due_at, .. } => *due_at,
            })
            .map(|(index, _)| index);
        let Some(index) = due else {
            // The reference caps its wait at `min_delay = 10000` (ten seconds) when a
            // descriptor is registered but no timer is closer, so a script that never
            // touches the descriptor still gets control back. Without a cap a handler on a
            // descriptor that is never ready would block for as long as the host allows.
            let timeout = self
                .next_timeout()
                .unwrap_or(std::time::Duration::from_secs(10))
                .min(std::time::Duration::from_secs(10));
            if !self.fd_handlers.is_empty() {
                // Blocking on a timer while descriptors are registered would starve them,
                // and blocking on the descriptors while a timer is due would delay it; the
                // host polls with the earlier of the two deadlines, which this timeout is.
                return Ok(Turn::Poll(timeout));
            }
            return Ok(match self.next_timeout() {
                Some(wait) => Turn::Wait(wait),
                None => Turn::Done,
            });
        };
        let Task::Timer { callback, .. } = self.tasks.remove(index);
        let this = Value::Undefined;
        // A throwing timer callback must not stop the loop: the reference reports it and
        // keeps going, because the alternative is that one bad timer silently stops every
        // later one.
        // No arguments: see `schedule_timer` — the reference calls the callback bare.
        match self.call_value(callback, this, Vec::new()) {
            Ok(_) => {}
            Err(Signal::Throw(value)) => {
                let text = self.describe_exception(&value);
                self.timer_errors.push(text);
            }
            Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
        }
        self.drain_jobs()?;
        Ok(Turn::Ran)
    }

    /// Register or clear one direction of an `os.setReadHandler`/`os.setWriteHandler` pair.
    ///
    /// `callback` of `None` unregisters that direction, and an entry with neither direction
    /// left is removed — the reference does the same, so a script can rely on
    /// `os.setReadHandler(fd, null)` stopping the loop from waiting on that descriptor.
    pub fn set_fd_handler(&mut self, fd: i32, readable: bool, callback: Option<Value>) {
        let entry = self.fd_handlers.entry(fd).or_default();
        if readable {
            entry.read = callback;
        } else {
            entry.write = callback;
        }
        if entry.read.is_none() && entry.write.is_none() {
            self.fd_handlers.remove(&fd);
        }
    }

    /// Register or clear an `os.signal` handler.
    ///
    /// `None` removes it; the host separately restores the default disposition or sets
    /// ignore, because which of those happened is the script's choice (`null` versus
    /// `undefined`) and not something this table can express.
    pub fn set_signal_handler(&mut self, signal: i32, handler: Option<Value>) {
        match handler {
            Some(handler) => {
                self.signal_handlers.insert(signal, handler);
            }
            None => {
                self.signal_handlers.remove(&signal);
            }
        }
    }

    /// Run the handler for `signal`, reporting whether one ran.
    ///
    /// A throwing handler is reported and the loop continues, like a timer's.
    pub fn fire_signal(&mut self, signal: i32) -> JsResult<bool> {
        let Some(handler) = self.signal_handlers.get(&signal).cloned() else {
            return Ok(false);
        };
        match self.call_value(handler, Value::Undefined, Vec::new()) {
            Ok(_) => {}
            Err(Signal::Throw(value)) => {
                let text = self.describe_exception(&value);
                self.timer_errors.push(text);
            }
            Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
        }
        self.drain_jobs()?;
        Ok(true)
    }

    /// The descriptors the host has to poll, and in which direction.
    ///
    /// Empty means the loop has nothing to wait for on any descriptor. The host turns each
    /// entry into a `pollfd` with `POLLIN`/`POLLOUT` — the same events the reference's
    /// `os_poll` asks for.
    pub fn pending_fds(&self) -> Vec<FdInterest> {
        self.fd_handlers
            .iter()
            .map(|(fd, entry)| FdInterest {
                fd: *fd,
                read: entry.read.is_some(),
                write: entry.write.is_some(),
            })
            .collect()
    }

    /// Call the handler registered for `fd`, reporting whether one ran.
    ///
    /// `readable`/`writable` come from `poll(2)`. The reference calls **one** handler per
    /// poll iteration and then restarts the loop, because the handler may have changed the
    /// registration list underneath it; doing the same here keeps a handler that
    /// unregisters itself from being called with stale state.
    ///
    /// A throwing handler is reported and the loop continues — a bad handler must not stop
    /// every later one, exactly as with timers.
    pub fn fire_fd(&mut self, fd: i32, readable: bool, writable: bool) -> JsResult<bool> {
        let Some(entry) = self.fd_handlers.get(&fd) else {
            return Ok(false);
        };
        let callback = if readable {
            entry.read.clone()
        } else if writable {
            entry.write.clone()
        } else {
            None
        };
        let Some(callback) = callback else {
            return Ok(false);
        };
        self.drain_jobs()?;
        match self.call_value(callback, Value::Undefined, Vec::new()) {
            Ok(_) => {}
            Err(Signal::Throw(value)) => {
                let text = self.describe_exception(&value);
                self.timer_errors.push(text);
            }
            Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
        }
        self.drain_jobs()?;
        Ok(true)
    }

    /// Take the errors timer callbacks threw, leaving the list empty.
    pub fn take_timer_errors(&mut self) -> Vec<String> {
        std::mem::take(&mut self.timer_errors)
    }

    /// Whether anything is still pending: a microtask or a timer.
    pub fn has_pending_work(&self) -> bool {
        !self.jobs.is_empty() || !self.tasks.is_empty() || !self.fd_handlers.is_empty()
    }

    /// Whether a timer or a microtask is pending, ignoring descriptor handlers.
    pub fn has_pending_timers_or_jobs(&self) -> bool {
        !self.jobs.is_empty() || !self.tasks.is_empty()
    }

    /// Run every queued microtask, including ones queued while draining.
    pub fn drain_jobs(&mut self) -> JsResult<()> {
        // A self-perpetuating chain (`function f(){ Promise.resolve().then(f) } f()`)
        // would spin forever, so the drain is bounded the way the call stack is.
        let mut budget = 1_000_000usize;
        while let Some(job) = self.jobs.pop_front() {
            if budget == 0 {
                return Err(Signal::Throw(
                    self.make_error("InternalError", "promise job queue did not drain"),
                ));
            }
            budget -= 1;
            self.run_job(job)?;
            // `FinalizationRegistry` at the job boundary: this is the safe point where the
            // engine may run script that the script itself did not ask for. Doing it here
            // rather than from a drop hook is what keeps the callback out of arbitrary
            // engine bookkeeping — see `builtins::weak`'s header.
            crate::builtins::weak::drain_finalizers(self)?;
        }
        // A script that queued no jobs may still have dropped targets, so the checkpoint
        // runs once more after the queue empties.
        crate::builtins::weak::drain_finalizers(self)?;
        Ok(())
    }

    /// Every live `FinalizationRegistry`, as `(handle, state)` pairs.
    ///
    /// The registry list is weak, so a registry the script dropped answers `upgrade` with
    /// `None` and is swept out here. This is also where the realm learns that a registry
    /// died, which is why it is a mutation rather than a read.
    pub fn live_registries(&mut self) -> Vec<(Gc<JsObject>, crate::object::FinalizationState)> {
        let handles = std::mem::take(&mut self.realm.finalization_registries);
        let mut live = Vec::with_capacity(handles.len());
        for handle in handles {
            let Some(object) = handle.upgrade() else {
                continue;
            };
            let Some(state) = object.finalization_state() else {
                continue;
            };
            live.push((object.clone(), *state));
            self.realm.finalization_registries.push(object.downgrade());
        }
        live
    }

    /// Run one microtask.
    fn run_job(&mut self, job: Job) -> JsResult<()> {
        match job {
            Job::Reaction {
                handler: None,
                argument,
                rejected,
                promise,
            } => {
                // No handler: the settlement passes straight through to the derived
                // promise, which is why `p.catch(f)` ignores fulfilments.
                if rejected {
                    self.reject_promise(&promise, argument);
                } else {
                    self.fulfill_promise(&promise, argument);
                }
                Ok(())
            }
            Job::Reaction {
                handler: Some(handler),
                argument,
                promise,
                ..
            } => match self.call_value(handler, Value::Undefined, vec![argument]) {
                Ok(answer) => {
                    if let Err(Signal::Throw(error)) = self.resolve_promise_with(&promise, answer) {
                        self.reject_promise(&promise, error);
                    }
                    Ok(())
                }
                Err(Signal::Throw(reason)) => {
                    self.reject_promise(&promise, reason);
                    Ok(())
                }
                Err(other) => Err(other),
            },
            Job::Thenable {
                then,
                value,
                promise,
            } => {
                // §27.2.2.2: call the thenable's `then` with this promise's own pair, so
                // the value it produces settles `promise`.
                // The thenable's `then` gets this promise's own pair, so whatever it
                // produces settles `promise`.
                let resolve = JsObject::function(
                    self.heap,
                    Some(self.realm.function_proto.clone()),
                    Callable::PromiseResolve(promise.clone()),
                );
                let reject = JsObject::function(
                    self.heap,
                    Some(self.realm.function_proto.clone()),
                    Callable::PromiseReject(promise.clone()),
                );
                match self.call_value(
                    then,
                    value,
                    vec![Value::Object(resolve), Value::Object(reject)],
                ) {
                    Ok(_) => Ok(()),
                    Err(Signal::Throw(reason)) => {
                        self.reject_promise(&promise, reason);
                        Ok(())
                    }
                    Err(other) => Err(other),
                }
            }
            Job::Resume {
                coroutine,
                promise,
                kind,
                value,
            } => self.drive_async(&coroutine, &promise, kind, value),
            Job::GeneratorResume {
                generator,
                promise,
                kind,
                value,
            } => self.step_async_generator(&generator, &promise, kind, value),
            Job::Import {
                specifier,
                referrer,
                promise,
            } => {
                self.run_dynamic_import(&specifier, &referrer, &promise);
                Ok(())
            }
        }
    }

    /// One element of a `Promise` combinator settled.
    ///
    /// The shared state object holds the results array, the number still outstanding, and
    /// the combinator's own resolve/reject pair. `all` fills by index so the answer keeps
    /// the input order; `race` settles on the first element and ignores the rest, which a
    /// one-shot settle gives for free.
    fn settle_combinator(
        &mut self,
        state: &Gc<JsObject>,
        index: usize,
        fulfilled: bool,
        value: Value,
    ) -> JsResult<()> {
        let kind = state
            .own_property("kind")
            .and_then(|property| property.value)
            .map(|value| value.to_int32())
            .unwrap_or(0);
        let outcome = state
            .own_property("outcome")
            .and_then(|property| property.value);
        let Some(Value::Object(outcome)) = outcome else {
            return Ok(());
        };
        if kind == 3 {
            // `race`: the first settlement wins; the rest are ignored by the one-shot rule.
            if fulfilled {
                self.resolve_promise_with(&outcome, value)?;
            } else {
                self.reject_promise(&outcome, value);
            }
            return Ok(());
        }
        let results = state
            .own_property("results")
            .and_then(|property| property.value)
            .and_then(|value| value.as_object().cloned());
        let remaining = state
            .own_property("remaining")
            .and_then(|property| property.value)
            .map(|value| value.to_int32())
            .unwrap_or(0);
        match kind {
            0 => {
                // `all`: the first rejection wins outright.
                if !fulfilled {
                    self.reject_promise(&outcome, value);
                    return Ok(());
                }
                let Some(results) = &results else {
                    return Ok(());
                };
                results.array_set(index, value);
            }
            1 => {
                // `allSettled`: never rejects; each slot records what happened.
                let Some(results) = &results else {
                    return Ok(());
                };
                let record = self.new_object(Some(self.realm.object_proto.clone()));
                if fulfilled {
                    record.define_data("status", self.string("fulfilled"));
                    record.define_data("value", value);
                } else {
                    record.define_data("status", self.string("rejected"));
                    record.define_data("reason", value);
                }
                results.array_set(index, Value::Object(record));
            }
            2 => {
                // `any`: the first fulfilment wins; a full sweep of rejections does not
                // settle here — it is handled by the count below.
                if fulfilled {
                    self.resolve_promise_with(&outcome, value)?;
                    return Ok(());
                }
            }
            _ => {}
        }
        let left = remaining - 1;
        state.define_data("remaining", Value::Int(left));
        if left != 0 {
            return Ok(());
        }
        match kind {
            0 | 1 => {
                let Some(results) = results else {
                    return Ok(());
                };
                self.resolve_promise_with(&outcome, Value::Object(results))?;
            }
            2 => {
                let error = self.make_error("AggregateError", "all promises were rejected");
                self.reject_promise(&outcome, error);
            }
            _ => {}
        }
        Ok(())
    }

    /// Build an async function's result: a promise whose body runs as a coroutine.
    pub fn new_async_function(
        &mut self,
        function: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Value> {
        let promise = self.new_promise();
        let coroutine = self.new_coroutine(function, this, args)?;
        if let Err(Signal::Throw(reason)) =
            self.drive_async(&coroutine, &promise, ResumeKind::Start, Value::Undefined)
        {
            self.reject_promise(&promise, reason);
        }
        Ok(Value::Object(promise))
    }

    /// The coroutine an async body runs in.
    ///
    /// Generator-shaped and never exposed to the script: what makes `await` work is the
    /// same frame-detach-and-resume the generator protocol uses, so this reuses it rather
    /// than growing a second state machine. The difference is only in the ending — a
    /// `return` fulfils the caller's promise here instead of producing `{value, done}`.
    /// Run `code` as an async function body, answering the promise it settles.
    ///
    /// Used for a module with top-level `await`: its body is an async function whose
    /// caller is the host, so there is no function object to pass through
    /// [`Interp::new_async_function`].
    pub fn run_async_body(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
    ) -> JsResult<Value> {
        let promise = self.new_promise();
        let coroutine = self.new_coroutine_from(code, env, Value::Undefined, Vec::new())?;
        if let Err(Signal::Throw(reason)) =
            self.drive_async(&coroutine, &promise, ResumeKind::Start, Value::Undefined)
        {
            self.reject_promise(&promise, reason);
        }
        Ok(Value::Object(promise))
    }

    fn new_coroutine(
        &mut self,
        function: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Gc<JsObject>> {
        let Some(object) = function.as_object().cloned() else {
            return Err(self.type_error("not a function"));
        };
        let Some((code, env)) = bytecode_of(&object) else {
            return Err(self.type_error("not a function"));
        };
        self.new_coroutine_from(code, env, this, args)
    }

    /// Build a coroutine over an already-resolved body.
    fn new_coroutine_from(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Gc<JsObject>> {
        let function = Value::Undefined;
        self.push_frame(code, env, this, args, function)?;
        let frame = self.frames.pop().expect("push_frame just pushed it");
        let state = GeneratorState {
            frame: Some(frame),
            phase: GeneratorPhase::SuspendedStart,
            parked_on_await: false,
        };
        let coroutine = self.new_object(Some(self.realm.generator_proto.clone()));
        coroutine.with_mut(|data| {
            data.kind = ObjectKind::Generator(Box::new(state));
        });
        Ok(coroutine)
    }

    /// Step a coroutine: `(value, done)`, where `done == false` means it parked on an
    /// `await` and `value` is what it awaited.
    fn resume_coroutine(
        &mut self,
        coroutine: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<(Value, bool)> {
        let Some(mut state) = coroutine.with_mut(|data| match &mut data.kind {
            ObjectKind::Generator(state) => Some(std::mem::replace(
                state.as_mut(),
                GeneratorState {
                    frame: None,
                    phase: GeneratorPhase::Executing,
                    parked_on_await: false,
                },
            )),
            _ => None,
        }) else {
            return Err(self.type_error("not a coroutine"));
        };
        let result = self.step_generator(&mut state, kind, value);
        coroutine.with_mut(|data| {
            data.kind = ObjectKind::Generator(Box::new(state));
        });
        let (value, done, _ready, _reason) = result?;
        Ok((value, done))
    }

    /// Advance an async body and settle its promise with the outcome.
    fn drive_async(
        &mut self,
        coroutine: &Gc<JsObject>,
        promise: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<()> {
        match self.resume_coroutine(coroutine, kind, value) {
            Ok((value, true)) => {
                // `Ok` with `done` means the body finished *normally*, even when it was
                // resumed by a `throw` — `try { await p } catch {}` completes by
                // returning. Only the `Err` arm below is a rejection.
                //
                // Resolving rather than fulfilling matters: `return promise` in an async
                // body adopts the inner promise, so `async function f(){ return
                // Promise.resolve(1) }` answers a promise *of 1*, not a promise of a
                // promise.
                if let Err(Signal::Throw(reason)) = self.resolve_promise_with(promise, value) {
                    self.reject_promise(promise, reason);
                }
                Ok(())
            }
            Ok((value, false)) => {
                // Parked on `await`: follow what it awaited and resume with the outcome.
                let awaited = match self.promise_resolve_value(value) {
                    Ok(awaited) => awaited,
                    Err(Signal::Throw(error)) => {
                        self.reject_promise(promise, error);
                        return Ok(());
                    }
                    Err(other) => return Err(other),
                };
                self.hook_await(awaited, coroutine, promise)
            }
            Err(Signal::Throw(reason)) => {
                self.reject_promise(promise, reason);
                Ok(())
            }
            Err(other) => Err(other),
        }
    }

    /// Arrange for `coroutine` to resume when `awaited` settles.
    fn hook_await(
        &mut self,
        awaited: Value,
        coroutine: &Gc<JsObject>,
        promise: &Gc<JsObject>,
    ) -> JsResult<()> {
        let resumed = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::AwaitResume {
                coroutine: coroutine.clone(),
                promise: promise.clone(),
                kind: ResumeKind::Next,
            },
        );
        let raised = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::AwaitResume {
                coroutine: coroutine.clone(),
                promise: promise.clone(),
                kind: ResumeKind::Throw,
            },
        );
        // `perform_then` on a non-promise would be wrong, and `promise_resolve_value`
        // guarantees one; the fallback keeps a mis-shaped value from panicking.
        let Value::Object(awaited) = &awaited else {
            self.jobs.push_back(Job::Resume {
                coroutine: coroutine.clone(),
                promise: promise.clone(),
                kind: ResumeKind::Next,
                value: awaited,
            });
            return Ok(());
        };
        self.perform_then(
            awaited,
            Some(Value::Object(resumed)),
            Some(Value::Object(raised)),
        )?;
        Ok(())
    }

    /// `[[GetOwnProperty]]` on an ordinary object: the property table, then the two
    /// things an engine keeps outside it — array elements and an array's `length`.
    fn ordinary_own_property(object: &Gc<JsObject>, key: &PropertyKey) -> Option<Property> {
        if let Some(property) = object.own_property_key(key) {
            return Some(property);
        }
        // An array's elements live in their own vector rather than the property table, so
        // a descriptor for one has to be synthesized — with the attributes an element has.
        if let Some(index) = key.as_str().and_then(array_index)
            && let Some(value) = object.array_get(index as usize)
            && !matches!(value, Value::Hole)
        {
            return Some(Property::data(value));
        }
        // `length` is an own property of an array that is not in its property table
        // either: `[].hasOwnProperty("length")` is true, and its descriptor is writable
        // and non-enumerable.
        if key.as_str() == Some("length")
            && let Some(length) = object.array_length()
        {
            return Some(Property {
                value: Some(crate::object::length_value(length)),
                writable: object.length_writable(),
                enumerable: false,
                configurable: false,
                getter: None,
                setter: None,
            });
        }
        None
    }

    /// `[[GetOwnProperty]]`, with the `getOwnPropertyDescriptor` trap when there is one.
    pub fn object_get_own_property(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
    ) -> JsResult<Option<Property>> {
        if let Some(answer) = self.proxy_get_own_property(object, key)? {
            return Ok(answer);
        }
        Ok(Self::ordinary_own_property(object, key))
    }

    /// `[[DefineOwnProperty]]`, with the `defineProperty` trap when there is one.
    pub fn object_define_own_property(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
        property: Property,
    ) -> JsResult<bool> {
        if let Some(accepted) = self.proxy_define_property(object, key, property.clone())? {
            return Ok(accepted);
        }
        // An array index is stored in the element vector, which can hold a plain value and
        // nothing else. An accessor — or a data property with non-default attributes —
        // therefore has to live in the property table, and the element slot is blanked so
        // the table is what a read finds. Without this `Object.defineProperty(a, "0",
        // {get})` stored the accessor where no read ever looked, and the array kept
        // answering its old value.
        // An array's `length` is not stored in the property table, so defining it has to
        // run `ArraySetLength`: it truncates or extends the element vector and refuses a
        // value that is not a valid index. Without this `Object.defineProperty(a, "length",
        // {value: 1})` reported success and changed nothing.
        if key.as_str() == Some("length") && object.is_array() {
            // `{writable: false}` on `length` does not change the length, but it does make
            // it read-only — after which neither a later `{writable: true}` nor a store
            // past the end may succeed.
            if property.value.is_none() {
                if !object.length_writable() && property.writable {
                    return Ok(false);
                }
                object.set_length_writable(property.writable);
                return Ok(true);
            }
            if !object.length_writable() {
                return Ok(false);
            }
        }
        if key.as_str() == Some("length")
            && object.is_array()
            && let Some(value) = property.value.clone()
        {
            let requested = value.to_number();
            let valid = requested.is_finite()
                && requested >= 0.0
                && requested.fract() == 0.0
                && requested <= u32::MAX as f64;
            if !valid {
                return Err(self.range_error("invalid array length"));
            }
            // Truncating deletes the elements above the new length, and a non-configurable
            // one cannot be deleted: `Object.defineProperty(a, "3", {configurable: false})`
            // makes `a.length = 2` fail rather than quietly drop it.
            let current = object.array_length().unwrap_or(0);
            let requested = requested as usize;
            if requested < current
                && (requested..current).any(|index| object.element_is_undeletable(index as u32))
            {
                return Ok(false);
            }
            object.array_set_length(requested);
            return Ok(true);
        }
        let plain = property.writable
            && property.enumerable
            && property.configurable
            && !property.is_accessor();
        if let Some(index) = key.as_str().and_then(array_index)
            && object.is_array()
        {
            // Growing the array needs a writable `length`: after
            // `Object.defineProperty(a, "length", {writable: false})` a new index cannot
            // be added.
            if (index as usize) >= object.array_length().unwrap_or(0) && !object.length_writable() {
                return Ok(false);
            }
            if plain && let Some(value) = property.value {
                // An ordinary element: the vector is where a read looks first, so it has
                // to be the vector that changes.
                object.delete_key(key);
                object.array_set(index as usize, value);
                return Ok(true);
            }
            object.define_key(key, property);
            // The slot stays, so `length` does not shrink; a hole is not an element, so
            // the table entry is now what a read finds.
            object.array_set(index as usize, Value::Hole);
            return Ok(true);
        }
        object.define_key(key, property);
        Ok(true)
    }

    /// Build a proxy object over `target`, with its traps read from `handler`.
    pub fn new_proxy(&self, target: Gc<JsObject>, handler: Value) -> Gc<JsObject> {
        JsObject::proxy(self.heap, target, handler)
    }

    /// The `revoke` function for a proxy's slots.
    pub fn new_revoker(&self, slots: crate::object::ProxyRef) -> Gc<JsObject> {
        JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Revoker(slots),
        )
    }

    /// A new array.
    pub fn new_array(&self, elements: Vec<Value>) -> Gc<JsObject> {
        let array = JsObject::array(
            self.heap,
            Some(self.realm.array_proto.clone()),
            elements.len(),
        );
        for (index, element) in elements.into_iter().enumerate() {
            array.array_set(index, element);
        }
        array
    }

    /// A new native function object.
    ///
    /// The observable `name` and `length` are installed as properties here, which is
    /// what `Function.prototype.name` and `.length` read; nothing else stores them,
    /// so `f.name = "x"` behaves like the writable property it is.
    /// A native constructor that builds its own instance after validating its arguments.
    ///
    /// `new` hands it `this = undefined` rather than a pre-made object, so any argument
    /// check runs before `new.target.prototype` is read.
    pub fn new_native_ctor(
        &self,
        name: impl Into<String>,
        length: u32,
        func: NativeFn,
    ) -> Gc<JsObject> {
        let function = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Native {
                func,
                own_instance: true,
            },
        );
        function.set_constructible(true);
        function.define(
            "length",
            Property::name_or_length(Value::Int(length as i32)),
        );
        function.define("name", Property::name_or_length(self.string(name)));
        function
    }

    /// A native constructor that lets `new` build the instance first.
    ///
    /// The ordinary case for a built-in constructor: `new Array(3)` has no argument
    /// validation that must precede reading `new.target.prototype`, so the object can be
    /// created before the implementation runs. Distinct from [`Interp::new_native`],
    /// which makes a *method* and therefore not a constructor at all.
    pub fn new_native_constructor(
        &self,
        name: impl Into<String>,
        length: u32,
        func: NativeFn,
    ) -> Gc<JsObject> {
        let function = self.new_native(name, length, func);
        function.set_constructible(true);
        function
    }

    /// A new host function object: a native closure that carries its own state.
    ///
    /// Same observable surface as [`Interp::new_native`] — `name`, `length` and, for a
    /// constructor, `prototype` — but the body is a closure, so it can hold an open-file
    /// table or a timer queue. See [`crate::object::HostFn`] for the one rule that makes
    /// this safe: the closure must not capture a `Gc`.
    pub fn new_host_function(
        &self,
        name: impl Into<String>,
        length: u32,
        func: crate::object::HostFn,
    ) -> Gc<JsObject> {
        self.new_host_inner(name.into(), length, func, false)
    }

    /// A host constructor: `new` hands it `this = undefined`, so it validates its
    /// arguments before building an instance, exactly like [`Interp::new_native_ctor`].
    pub fn new_host_ctor(
        &self,
        name: impl Into<String>,
        length: u32,
        func: crate::object::HostFn,
    ) -> Gc<JsObject> {
        let function = self.new_host_inner(name.into(), length, func, true);
        function.set_constructible(true);
        // A constructor needs a `prototype` for `new` and for `instanceof`; the body
        // installs its own methods on it, so it starts as a plain object.
        let proto = JsObject::ordinary(self.heap, Some(self.realm.object_proto.clone()));
        function.define(
            "prototype",
            Property {
                value: Some(Value::Object(proto)),
                writable: true,
                enumerable: false,
                configurable: false,
                ..Property::default()
            },
        );
        function
    }

    fn new_host_inner(
        &self,
        name: String,
        length: u32,
        func: crate::object::HostFn,
        own_instance: bool,
    ) -> Gc<JsObject> {
        let function = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Host {
                func: std::rc::Rc::new(std::cell::RefCell::new(func)),
                own_instance,
            },
        );
        function.define(
            "length",
            Property::name_or_length(Value::Int(length as i32)),
        );
        function.define("name", Property::name_or_length(self.string(name)));
        function
    }

    /// A new native function object.
    pub fn new_native(&self, name: impl Into<String>, length: u32, func: NativeFn) -> Gc<JsObject> {
        let function = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Native {
                func,
                own_instance: false,
            },
        );
        // `length` before `name`: `FunctionCreate` sets `[[ParameterCount]]` first, and
        // `Object.getOwnPropertyNames` reports insertion order, so the reference lists
        // `length,name` and this listed `name,length`. A bytecode function already gets it
        // the right way round; only native ones were built backwards.
        function.define(
            "length",
            Property::name_or_length(Value::Int(length as i32)),
        );
        function.define("name", Property::name_or_length(self.string(name)));
        function
    }

    /// A new bound function.
    pub fn new_bound(&self, target: Value, this: Value, args: Vec<Value>) -> Gc<JsObject> {
        // §10.4.1.3 BoundFunctionCreate: a bound function has `[[Call]]` because it was
        // made by calling `bind`, and `[[Construct]]` exactly when its target has one.
        // `new (function(){}.bind(null))()` works; `new (()=>{}).bind(null)()` does not.
        let constructible = target
            .as_object()
            .is_some_and(|target| target.is_constructor());
        let bound = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Bound { target, this, args },
        );
        bound.set_constructible(constructible);
        bound
    }

    /// A new reference cell.
    pub fn new_cell(&self, value: Value) -> Gc<Cell> {
        Gc::new(self.heap, Cell::new(value))
    }

    // -- running ------------------------------------------------------------

    /// Evaluate `code` as a script body and return its completion value.
    /// Compile and run `source` for the engine's own use, returning its completion value.
    ///
    /// This is how a builtin can be written in JavaScript: [`crate::builtins::from_async`]
    /// needs to be a real async function, and the engine already runs those as coroutines,
    /// so compiling the algorithm is cheaper than rebuilding a coroutine by hand.
    ///
    /// Differences from [`Self::eval_in_global`], both deliberate:
    ///
    /// * It does **not** drain the microtask queue. Install runs while the realm is still
    ///   being built, and the source is an IIFE that only constructs a closure, so there
    ///   is nothing to drain — and draining here would run script-queued jobs in the
    ///   middle of `install`.
    /// * It reports a syntax error as an `InternalError` throw rather than as the usual
    ///   `SyntaxError` with a position: a malformed builtin source is this crate's bug,
    ///   and it should not look like the user wrote something wrong.
    pub fn eval_builtin_source(&mut self, source: &str) -> JsResult<Value> {
        let program = match js_parse::parse_script(source) {
            Ok(program) => program,
            Err(error) => {
                let message = format!("builtin source does not parse: {error}");
                return Err(Signal::Throw(self.make_error("InternalError", &message)));
            }
        };
        let mut code = match js_compile::compile_script(&program) {
            Ok(code) => code,
            Err(error) => {
                let message = format!("builtin source does not compile: {}", error.message);
                return Err(Signal::Throw(self.make_error("InternalError", &message)));
            }
        };
        // The frame has to name something, and `<builtin>` is what a stack trace through
        // this code should say.
        code.set_filename("<builtin>");
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        let closure_vars = compiled.code.closure_vars.clone();
        let env = self.global_env(&closure_vars);
        let this = Value::Object(self.realm.global.clone());
        self.execute(compiled, env, this, Vec::new(), Value::Undefined)
    }

    /// Run an already-parsed script and drain the microtasks it queued.
    ///
    /// The drain is what lets an `async` function's body advance past its first `await`
    /// before the host sees the result: nothing else pumps the job queue.
    pub fn run_script(&mut self, code: CompiledFunction) -> JsResult<Value> {
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        let closure_vars = compiled.code.closure_vars.clone();
        let env = self.global_env(&closure_vars);
        let this = Value::Object(self.realm.global.clone());
        let callee = Value::Undefined;
        let result = self.execute(compiled, env, this, Vec::new(), callee)?;
        // The script is done, so the microtasks it queued run before the host sees the
        // result. Without this an `async` function's body would never advance past its
        // first `await`, because nothing else drains the queue.
        self.drain_jobs()?;
        Ok(result)
    }

    /// Compile and run `source` in global scope, returning its completion value.
    ///
    /// This is what `eval` and `new Function` use. Running it as a script means it
    /// sees the global object and nothing else, which is right for `eval("1+1")` and
    /// for a `Function` body; a *direct* eval that reads a local variable would need
    /// the compiler to hand its variable environment over, which is not implemented.
    pub fn eval_in_global(&mut self, source: &str) -> JsResult<Value> {
        self.eval_in_global_named(source, EVAL_FILE, false)
    }

    /// `std.evalScript(source)`: the same evaluation, reported under its own file name.
    ///
    /// The reference names the two separately — `eval` shows `<input>`, `std.evalScript`
    /// shows `<evalScript>` — and the name is visible in every stack trace that passes
    /// through the code, so they cannot share one constant.
    pub fn eval_script_in_global(&mut self, source: &str, barrier: bool) -> JsResult<Value> {
        self.eval_in_global_named(source, EVAL_SCRIPT_FILE, barrier)
    }

    fn eval_in_global_named(&mut self, source: &str, file: &str, barrier: bool) -> JsResult<Value> {
        // Marked for the duration, so `declare_var_global` knows a `var` from here is an
        // eval-created global binding — configurable, unlike a script's own `var`.
        self.eval_depth += 1;
        if barrier {
            let depth = self.frames.len();
            self.trace_barriers.push(depth);
        }
        let result = self.eval_in_global_inner(source, file);
        if barrier {
            self.trace_barriers.pop();
        }
        self.eval_depth -= 1;
        result
    }

    fn eval_in_global_inner(&mut self, source: &str, file: &str) -> JsResult<Value> {
        let compiled = self.compile_global_script(source, file)?;
        let strict = compiled.strict;
        self.run_eval_script(compiled, strict)
    }

    /// Parse and compile `source` as a global script, without running it.
    ///
    /// Split out so [`Interp::eval_in_global_async`] can report a syntax error as a throw —
    /// before a promise exists to reject — while still evaluating in the same eval context
    /// (direct-eval strictness, `new.target`, `super`).
    fn compile_global_script(&mut self, source: &str, file: &str) -> JsResult<CompiledFunction> {
        // A *direct* eval inherits the strictness of the code that called it, which is
        // why `eval("x = 1")` in a strict function is a ReferenceError instead of an
        // implicit global. The caller's frame is the innermost one here, because a
        // native function does not push a frame of its own.
        let strict = self.current_strict();
        // `eval` runs in the caller's function context, so `new.target` is legal here
        // even though the source is a bare script — and so is `super.x` when the caller
        // is a method. The eval'd code has no `[[HomeObject]]` of its own; it reads the
        // caller's, which is what `special_object 4` already does.
        let context = js_parse::EvalContext {
            in_method: self.enclosing_home_object().is_some(),
            // `super()` belongs to a derived constructor, and an eval'd body is not a
            // constructor at all: the caller frame's `this` is already built.
            in_derived_constructor: false,
        };
        let parsed = js_parse::parse_eval_script_in(source, strict, context);
        let program = match parsed {
            Ok(program) => program,
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_at(
                    file,
                    error.span.start.line,
                    error.span.start.column,
                    &format!("invalid source: {error}"),
                )));
            }
        };
        let mut compiled = match js_compile::compile_script(&program) {
            Ok(compiled) => compiled,
            // A construct the compiler does not handle yet is reported the same way a
            // parse error is: it happens before anything runs, so the trace is the
            // position in the source followed by the caller.
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_at(
                    file,
                    error.span.start.line,
                    error.span.start.column,
                    &error.message,
                )));
            }
        };
        compiled.set_filename(file);
        Ok(compiled)
    }

    /// Run an `eval`'d script in the caller's scope.
    ///
    /// Three things have to come from the calling frame, because the eval'd code is a
    /// script and would otherwise see only the global object:
    ///
    /// * `this`, so `f.call(1, "this")` evaluates to `1`;
    /// * `arguments`, exposed as a binding of the eval's own scope, so
    ///   `f("arguments.length", 1)` sees the caller's arguments;
    /// * a fresh variable environment for `var`, so a strict `eval` does not leak
    ///   `eval("var v = 1")` into the global object.
    ///
    /// Locals of the calling function are *not* reachable: that needs the compiler to
    /// hand its variable environment over, which this milestone does not do.
    fn run_eval_script(&mut self, code: CompiledFunction, code_strict: bool) -> JsResult<Value> {
        // `this` and `arguments` come from the innermost frame only — those are not
        // lexical — while the name bindings come from the whole chain of frames, part
        // of it being ordinary scope resolution.
        // An arrow has no `this` of its own, so an `eval` inside one inherits the
        // enclosing function's receiver — the lexical `this` the arrow was compiled to
        // read from its closure.
        // `this` and `arguments` come from the innermost *non-arrow* frame, but the
        // variable environment is the innermost frame of all: an arrow's own body is where
        // `var`s declared by an `eval` inside it belong.
        let caller_index = self
            .frames
            .iter()
            .rposition(|frame| !frame.code.code.is_arrow)
            .or_else(|| self.frames.len().checked_sub(1));
        let env_index = self.frames.len().checked_sub(1);
        let caller = caller_index.and_then(|index| self.frames.get(index));
        let (this, passed) = match caller {
            Some(frame) => (frame.this.clone(), frame.passed.clone()),
            // No frame at all: the eval is running at the top level, so there is
            // nothing to inherit and the global receiver is right.
            None => (Value::Object(self.realm.global.clone()), Vec::new()),
        };
        // Innermost frame first, so the first binding found for a name is the one that
        // shadows the rest. A script frame (`<eval>`) contributes nothing.
        let mut bindings: Vec<EvalBinding> = Vec::new();
        // The calling function's own bindings, resolved by the compiler at the call
        // site. That table is the only reliable answer when a name is declared twice in
        // sibling scopes: the flat scan below cannot tell which `i` a `for (let i …)`
        // body means, and guessing by declaration order picks the wrong one as soon as
        // two loops in a row use the same name.
        // Only a *direct* eval sees the caller's scope. The compiler records a site for
        // every direct call — a bare `eval(...)` callee — so finding one at the current
        // position is the test: `(0, eval)(...)` and `f.call(null, ...)` resolve the
        // callee to a value first and leave no site behind.
        let mut direct = false;
        let mut resolved_here = false;
        if let Some(frame) = self.frames.last() {
            let (line, column) = frame
                .code
                .code
                .position_at(frame.at as u32)
                .unwrap_or_default();
            // The nearest site at or before the current position, not an exact match:
            // the recorded position of the `call` instruction is the callee's, which
            // sits a few columns inside the call expression the compiler recorded. A
            // site is unique per call, so "the closest one behind us" identifies it.
            if let Some(site) = frame
                .code
                .code
                .eval_sites
                .iter()
                .filter(|site| (site.line, site.column) <= (line, column))
                .max_by_key(|site| (site.line, site.column))
            {
                direct = true;
                resolved_here = true;
                for binding in &site.bindings {
                    // An enclosing function's frame sits `owner` levels below the one
                    // running the call.
                    let Some(frame) = self.frames.len().checked_sub(1 + binding.owner as usize)
                    else {
                        continue;
                    };
                    bindings.push(EvalBinding {
                        name: binding.name.clone(),
                        frame,
                        slot: if binding.is_arg {
                            EvalSlot::Arg
                        } else {
                            EvalSlot::Local
                        },
                        index: binding.index as usize,
                    });
                }
            }
        }
        for frame_index in (0..self.frames.len()).rev() {
            // Skip the innermost frame's flat scan when the compiler already answered
            // for it; an enclosing frame still needs one, because the eval resolves
            // those names dynamically.
            if resolved_here && frame_index == self.frames.len() - 1 {
                continue;
            }
            let frame = &self.frames[frame_index];
            for (index, arg) in frame.code.code.args.iter().enumerate() {
                bindings.push(EvalBinding {
                    name: arg.name.clone(),
                    frame: frame_index,
                    slot: EvalSlot::Arg,
                    index,
                });
            }
            for (index, local) in frame.code.code.vars.iter().enumerate() {
                bindings.push(EvalBinding {
                    name: local.name.clone(),
                    frame: frame_index,
                    slot: EvalSlot::Local,
                    index,
                });
            }
        }
        let mut seen = BTreeSet::new();
        bindings.retain(|binding| seen.insert(binding.name.clone()));

        // The caller's `arguments`, as an array-like. Reading `length` and the indexed
        // elements is all the spec's ordinary uses need.
        let arguments = Value::Object(self.new_array(passed));

        // Only a *strict* eval gets a variable environment of its own. A sloppy one
        // declares its `var`s in the caller's — which for a top-level eval is the global
        // object, so `eval("var gv=1")` leaves `gv` globally visible. Deciding this on
        // strictness alone is the spec's rule; getting it wrong made every sloppy `var`
        // vanish into a scope nobody reads back from.
        let previous_scope = if code_strict {
            self.eval_scope.replace(BTreeMap::new())
        } else {
            None
        };
        let previous_arguments = self.eval_arguments.replace(arguments);
        let previous_with = self.eval_with.replace(self.with_objects());
        let inherited_new_target = self
            .frames
            .iter()
            .rev()
            .find(|frame| !frame.code.code.is_arrow)
            .map(|frame| frame.new_target.clone());
        let previous_new_target = self
            .eval_new_target
            .replace(inherited_new_target.unwrap_or(Value::Undefined));
        let eval_cells = self.eval_cells_for(&bindings);
        let previous_bindings = std::mem::replace(&mut self.eval_bindings, bindings);
        // A sloppy direct eval declares its `var`s in the caller's variable environment;
        // `declare_eval_local` needs to know which frame that is.
        // An indirect eval gets the global environment, so nothing of the caller's is
        // recorded as the place its `var`s belong.
        let previous_caller = self
            .eval_caller
            .replace(if direct { env_index } else { None });
        // The cell-based view of the same bindings, which is what a closure created
        // inside the eval keeps. Only cell-backed bindings can be shared, so a frame
        // whose compiler did not put a binding in a cell simply does not appear here —
        // reads and writes still work through `eval_bindings`, they just cannot outlive
        // the eval.
        let previous_eval_scope = self
            .active_eval_scope
            .replace(Gc::new(self.heap, EvalScope { names: eval_cells }));
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        let closure_vars = compiled.code.closure_vars.clone();
        let env = self.global_env(&closure_vars);
        let callee = Value::Undefined;
        let result = self.execute(compiled, env, this, Vec::new(), callee);
        self.eval_scope = previous_scope;
        self.eval_arguments = previous_arguments;
        self.eval_with = previous_with;
        self.eval_new_target = previous_new_target;
        self.eval_bindings = previous_bindings;
        self.eval_caller = previous_caller;
        self.active_eval_scope = previous_eval_scope;
        result
    }

    /// The cell-backed subset of `bindings`, in the same priority order.
    ///
    /// Both lookup routes have to agree — the eval's own reads go through
    /// [`Interp::eval_binding_value`] and the closures it builds go through
    /// [`ClosureEnv::eval_scope`] — so they are derived from one resolution rather than
    /// scanning the frames twice and disagreeing about a shadowed name. A binding that
    /// is not in a cell simply does not appear: reads and writes still work, they just
    /// cannot be handed to a closure that outlives the eval.
    fn eval_cells_for(&self, bindings: &[EvalBinding]) -> Vec<(String, Gc<Cell>)> {
        let mut names: Vec<(String, Gc<Cell>)> = Vec::new();
        for binding in bindings {
            let Some(frame) = self.frames.get(binding.frame) else {
                continue;
            };
            let cell = match binding.slot {
                EvalSlot::Arg => frame
                    .code
                    .arg_cell
                    .get(binding.index)
                    .copied()
                    .unwrap_or(-1),
                EvalSlot::Local => frame
                    .code
                    .loc_cell
                    .get(binding.index)
                    .copied()
                    .unwrap_or(-1),
            };
            if cell < 0 {
                continue;
            }
            if let Some(cell) = frame.cells.get(cell as usize) {
                names.push((binding.name.clone(), cell.clone()));
            }
        }
        names
    }

    /// A `SyntaxError` that reports where in the source it happened.
    ///
    /// Compile-time failures never run, so they have no frame of their own; the
    /// reference still puts the position first in the trace, with no function name and
    /// no parentheses, and the caller's frames after it. It also defines `fileName`,
    /// `lineNumber` and `columnNumber` on the object (a SpiderMonkey convention the
    /// reference borrows), which is how a script reads the position without parsing
    /// the trace.
    fn syntax_error_at(&mut self, file: &str, line: u32, column: u32, message: &str) -> Value {
        let blamed = format!("    at {file}:{line}:{column}\n{}", self.stack_trace());
        let value = self.make_error("SyntaxError", message);
        if let Value::Object(object) = &value {
            let text = self.string(blamed);
            object.define("stack", Property::with_attributes(text, true, true));
            let name = self.string(file);
            object.define("fileName", Property::with_attributes(name, true, true));
            object.define(
                "lineNumber",
                Property::with_attributes(Value::Int(line as i32), true, true),
            );
            object.define(
                "columnNumber",
                Property::with_attributes(Value::Int(column as i32), true, true),
            );
        }
        value
    }

    /// Run a compiled script whose completion value is a function, returning it.
    ///
    /// `new Function(...)` compiles `(function(params) { body })` and this runs it to
    /// get the closure out, which keeps the constant-pool plumbing in one place.
    pub fn run_function_source(&mut self, code: CompiledFunction) -> JsResult<Value> {
        self.run_script(code)
    }

    /// The environment of a script or of any function compiled at global scope:
    /// every entry is a global binding, so each one resolves to the realm's cell.
    fn global_env(&mut self, closure_vars: &[js_compile::bytecode::ClosureVar]) -> Gc<ClosureEnv> {
        let mut cells = Vec::with_capacity(closure_vars.len());
        for closure_var in closure_vars {
            let cell = match closure_var.closure_type {
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport
                    if closure_var.is_lexical =>
                {
                    // A lexical declaration: the cell is created here holding the
                    // dead-zone marker, which is what makes the binding exist and
                    // unreadable before its initialiser runs.
                    self.declare_global(&closure_var.name, closure_var.is_const);
                    self.realm.globals[&closure_var.name].clone()
                }
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport => {
                    // A `var` or function declaration becomes a property of the global
                    // object, and it exists from here on — which is why
                    // `print(v); var v = 1` prints `undefined` instead of raising.
                    // Declaring it at instantiation time (rather than at the
                    // initialiser) is also what makes it non-configurable: the store
                    // then lands on an existing property instead of creating one.
                    self.declare_var_global(&closure_var.name);
                    // The cell is a placeholder that nothing indexes; it cannot be
                    // omitted because the slot has to hold something.
                    self.new_cell(Value::Undefined)
                }
                _ => {
                    // An undeclared global is read by name; the cell is a
                    // placeholder that nothing indexes.
                    self.new_cell(Value::Undefined)
                }
            };
            cells.push(cell);
        }
        Gc::new(
            self.heap,
            ClosureEnv {
                cells,
                eval_scope: None,
                local_vars: None,
                local_phase: 1,
            },
        )
    }

    /// Push a frame and run until it returns.
    pub(crate) fn execute(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
        this: Value,
        args: Vec<Value>,
        callee: Value,
    ) -> JsResult<Value> {
        let depth = self.frames.len();
        self.push_frame(code, env, this, args, callee)?;
        loop {
            match self.step() {
                Ok(Step::Continue) => {}
                Ok(Step::Return(value)) => {
                    self.frames.pop();
                    // A frame that ended in a `tail_call` hands its value straight to
                    // its caller: nothing after the call can run, so every frame above
                    // the callee unwinds with the same value. See [`Frame::tail`] —
                    // the reference does not reuse frames either, so recursion depth
                    // and the stack-overflow limit behave the same way.
                    while self.frames.len() > depth && self.frame().tail {
                        self.frames.pop();
                    }
                    if self.frames.len() == depth {
                        return Ok(value);
                    }
                    // The caller resumes with the value on its stack.
                    self.frames
                        .last_mut()
                        .expect("a caller is live")
                        .stack
                        .push(value);
                }
                Err(Signal::Throw(value)) => {
                    // A handler lives on the stack of the frame that is throwing —
                    // `catch` pushed it there — so this frame gets the first chance
                    // to catch before it is popped.
                    let value = value;
                    loop {
                        if self.frames.len() <= depth {
                            return Err(Signal::Throw(value));
                        }
                        if self.catch_here(value.clone()).is_some() {
                            break;
                        }
                        self.frames.pop();
                    }
                }
                // A `yield` in a frame that `execute` is running directly means a
                // generator body was entered as an ordinary call, which only happens if
                // the generator check in `call_or_push` was bypassed.
                Ok(Step::Suspend(_)) => {
                    return Err(Signal::Throw(self.make_error(
                        "InternalError",
                        "a generator body ran as an ordinary call",
                    )));
                }
                Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
            }
        }
    }

    /// Install a handler for `value` if this frame has one, resuming at it.
    ///
    /// The scan is the reference's: walk the frame's stack downwards looking for the
    /// innermost [`Value::CatchOffset`], discarding whatever sat above it. Offset
    /// `0` is an enumerator marker rather than a handler — it exists so that a
    /// `break` out of a `for-of` can find and close the iterator — so it is skipped
    /// here and the unwinding continues.
    fn catch_here(&mut self, value: Value) -> Option<()> {
        let frame = self.frames.last_mut()?;
        while let Some(top) = frame.stack.pop() {
            if let Value::CatchOffset(offset) = top {
                if offset == 0 {
                    continue;
                }
                frame.stack.push(value);
                frame.pc = offset as usize;
                return Some(());
            }
        }
        None
    }

    fn push_frame(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
        this: Value,
        mut args: Vec<Value>,
        callee: Value,
    ) -> JsResult<()> {
        if self.frames.len() >= self.max_frames {
            return Err(Signal::Throw(
                self.string("Maximum call stack size exceeded"),
            ));
        }
        let arg_count = code.code.args.len();
        // How many were actually passed, which `arguments.length` reports and
        // `assert(x)` depends on: a call with one argument must not look like a call
        // with one argument and an explicit `undefined`.
        let passed = args.len();
        // Missing arguments are `undefined` in their slots; extra ones are dropped
        // from the slots but still reachable through `arguments`.
        let mut passed_values = args.clone();
        args.resize(arg_count, Value::Undefined);
        args.truncate(arg_count);
        let cell_count = code.code.var_ref_count as usize;
        let mut cells = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            cells.push(self.new_cell(Value::Undefined));
        }
        let locals = vec![Value::Undefined; code.code.vars.len()];
        // A captured argument lives in its cell, not in the argument slot, so the
        // incoming value has to be moved there before the body runs.
        for (slot, cell) in code.arg_cell.iter().enumerate() {
            if *cell >= 0
                && let Some(value) = args.get(slot).cloned()
            {
                cells[*cell as usize].set(value);
            }
        }
        // Sloppy mode substitutes the global object for a missing receiver and boxes a
        // primitive one, so `function f(){ return typeof this }` called as `f.call(1)`
        // reports `object`. Strict mode passes the value through untouched.
        let this = if code.code.strict {
            this
        } else if this.is_nullish() {
            Value::Object(self.realm.global.clone())
        } else if matches!(this, Value::Object(_)) {
            this
        } else {
            match crate::builtins::this_object(self, &this) {
                Ok(boxed) => Value::Object(boxed),
                Err(_) => this,
            }
        };
        self.frames.push(Frame {
            code,
            env,
            this,
            args,
            passed: passed_values.drain(..passed).collect(),
            locals,
            cells,
            local_vars: None,
            stack: Vec::with_capacity(16),
            pc: 0,
            at: 0,
            callee,
            new_target: self.pending_new_target.take().unwrap_or(Value::Undefined),
            tail: false,
        });
        Ok(())
    }

    /// The cell a direct `eval` created for the closure entry at `slot`, if any.
    ///
    /// Only names the eval created are here: a name the frame already declared statically
    /// is written through to its own slot instead, so this cannot shadow a real local.
    fn eval_created_local(&self, slot: usize) -> Option<Gc<Cell>> {
        let name = &self.frame().code.code.closure_vars.get(slot)?.name;
        let (local_vars, phase) = self.visible_local_vars()?;
        if !local_vars.contains(name) {
            return None;
        }
        local_vars.get(name, phase)
    }

    /// The variables a direct `eval` declared, as the code currently running sees them.
    ///
    /// Inside an eval that is the *caller's* set — the eval's own variable environment
    /// is the caller's, which is why `eval("var c = 1")` leaves `c` behind. Everywhere
    /// else it is the current frame's, falling back to the frame the closure was created
    /// in.
    fn visible_local_vars(&self) -> Option<(Gc<LocalVars>, u8)> {
        // While an `eval` runs, its own frame is on top but its variable environment is
        // the *caller's* — so the caller is what the lookup has to start from.
        let (index, phase) = match self.eval_caller.flatten() {
            Some(caller) => (
                caller,
                if self.in_parameter_list_at(caller) {
                    0
                } else {
                    1
                },
            ),
            None => (
                self.frames.len().checked_sub(1)?,
                if self.in_parameter_list() { 0 } else { 1 },
            ),
        };
        let frame = self.frames.get(index)?;
        // The frame's own variables, or — for an arrow, which has no variable environment
        // of its own — the one it was created in, which `ClosureEnv` carries.
        if let Some(local_vars) = frame.local_vars.clone() {
            return Some((local_vars, phase));
        }
        frame
            .env
            .local_vars
            .clone()
            .map(|local_vars| (local_vars, frame.env.local_phase))
    }

    /// Whether the innermost frame is still evaluating its parameter list.
    ///
    /// A direct `eval` that runs there declares its `var`s in the parameter environment,
    /// and a closure created there may only see that much of [`LocalVars`].
    fn in_parameter_list(&self) -> bool {
        self.in_parameter_list_at(self.frames.len().saturating_sub(1))
    }

    /// [`Interp::in_parameter_list`] for a specific frame.
    fn in_parameter_list_at(&self, frame: usize) -> bool {
        let Some(frame) = self.frames.get(frame) else {
            return false;
        };
        let code = &frame.code.code;
        if !code.has_parameter_expressions {
            return false;
        }
        // `(0, 0)` means the compiler did not record a boundary, so treat the whole
        // function as its body and let a direct `eval` act accordingly.
        if code.body_start == (0, 0) {
            return false;
        }
        match code.position_at(frame.at as u32) {
            Some(position) => position < code.body_start,
            None => false,
        }
    }

    fn frame(&self) -> &Frame {
        self.frames.last().expect("a frame is live")
    }

    fn frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a frame is live")
    }

    // -- slot access --------------------------------------------------------

    fn read_slot(&self, cell: i32, value: &Value) -> Value {
        match cell {
            -1 => value.clone(),
            index => self
                .frame()
                .cells
                .get(index as usize)
                .expect("a cell index in range")
                .get(),
        }
    }

    fn get_arg(&self, slot: usize) -> Value {
        let frame = self.frame();
        let cell = frame.code.arg_cell.get(slot).copied().unwrap_or(-1);
        let value = frame.args.get(slot).cloned().unwrap_or(Value::Undefined);
        self.read_slot(cell, &value)
    }

    fn put_arg(&mut self, slot: usize, value: Value) {
        let cell = self.frame().code.arg_cell.get(slot).copied().unwrap_or(-1);
        if cell == -1 {
            if let Some(slot) = self.frame_mut().args.get_mut(slot) {
                *slot = value;
            }
        } else {
            self.frame().cells[cell as usize].set(value);
        }
    }

    fn get_loc(&self, slot: usize) -> Value {
        let frame = self.frame();
        let cell = frame.code.loc_cell.get(slot).copied().unwrap_or(-1);
        let value = frame.locals.get(slot).cloned().unwrap_or(Value::Undefined);
        self.read_slot(cell, &value)
    }

    fn put_loc(&mut self, slot: usize, value: Value) {
        let cell = self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1);
        if cell == -1 {
            if let Some(slot) = self.frame_mut().locals.get_mut(slot) {
                *slot = value;
            }
        } else {
            self.frame().cells[cell as usize].set(value);
        }
    }

    // -- globals ------------------------------------------------------------

    /// The name a global-slot operand refers to.
    fn global_name(&self, slot: usize) -> String {
        self.frame()
            .code
            .code
            .closure_vars
            .get(slot)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    /// The facts about a global binding that decide where it lives.
    ///
    /// A script's globals split in two, and the split is observable: `var` and
    /// function declarations become properties of the global object (`globalThis.v`
    /// works, `delete` refuses, `for-in` lists them), while `let`/`const`/`class`
    /// declarations live in the global *lexical* environment and are invisible there
    /// (`"c" in globalThis` is false). Keeping the difference in the closure var's
    /// `is_lexical` flag means the answer is already in the bytecode.
    fn global_binding(&self, slot: usize) -> (String, bool, bool) {
        let name = self.global_name(slot);
        // "Lexical" is *not* read from the closure var's flag: a `GlobalRef` created
        // while forwarding a reference through a nested function is built with the
        // flags cleared, so `const c` would look like an ordinary `var` from inside a
        // closure. Presence in the realm's lexical table is the same fact, recorded
        // once, and every slot sees it.
        let lexical = self.realm.globals.contains_key(&name);
        let is_const = self.realm.lexical_consts.contains(&name);
        (name, lexical, is_const)
    }

    /// The value of `name` in the innermost active `with` object, if any.
    ///
    /// The active objects are the [`VarKind::With`] locals of every frame still running,
    /// innermost frame first and innermost block first within one. Walking frames rather
    /// than keeping a separate stack is what makes a closure called from inside a `with`
    /// see it, and what keeps the object alive only as long as the block is.
    /// The innermost active `with` object that has `name`, if any.
    fn with_owner(&mut self, name: &str) -> Option<Gc<JsObject>> {
        let objects = self.eval_with.clone().unwrap_or_default();
        for object in objects {
            let key = self.string(name.to_string());
            if self
                .has_property(&key, &Value::Object(object.clone()))
                .ok()?
            {
                return Some(object);
            }
        }
        None
    }

    /// Every active `with` object, innermost first.
    fn with_objects(&self) -> Vec<Gc<JsObject>> {
        let mut objects = Vec::new();
        for frame in self.frames.iter().rev() {
            for (index, var) in frame.code.code.vars.iter().enumerate().rev() {
                if var.kind != js_compile::VarKind::With {
                    continue;
                }
                // A captured binding lives in its cell and not in the slot list, so the
                // cell is consulted first — a closure created inside the block is exactly
                // what makes the object captured. Before the store runs the slot still
                // holds `undefined`, and a `with` object is always boxed, so a non-object
                // entry is simply not in scope yet.
                let value = frame
                    .code
                    .loc_cell
                    .get(index)
                    .copied()
                    .filter(|cell| *cell >= 0)
                    .and_then(|cell| frame.cells.get(cell as usize))
                    .map(|cell| cell.get())
                    .or_else(|| frame.locals.get(index).cloned());
                if let Some(Value::Object(object)) = value {
                    objects.push(object);
                }
            }
        }
        objects
    }

    fn with_lookup(&mut self, name: &str) -> JsResult<Option<Value>> {
        for object in self.eval_with.clone().unwrap_or_default() {
            let key = self.string(name.to_string());
            if self.has_property(&key, &Value::Object(object.clone()))? {
                return Ok(Some(self.read_field(&Value::Object(object), name)?));
            }
        }
        Ok(None)
    }

    fn get_global(&mut self, slot: usize) -> JsResult<Value> {
        let (name, lexical, _) = self.global_binding(slot);
        if name == "arguments"
            && let Some(arguments) = &self.eval_arguments
        {
            return Ok(arguments.clone());
        }
        // A `with` block around the *call site* is part of the scope an `eval` sees, so
        // the objects live when the eval started are consulted before anything else. The
        // snapshot is taken at entry rather than scanning the frames on every read: a
        // block that has already finished still leaves its object in the slot, and a
        // bytecode read outside the block must not see it.
        if let Some(value) = self.with_lookup(&name)? {
            return Ok(value);
        }
        // A variable a direct `eval` declared shadows everything the compiler resolved,
        // including an enclosing function's binding: the eval put it in the caller's own
        // variable environment, which is nearer than any captured scope.
        if let Some((local_vars, phase)) = self.visible_local_vars()
            && let Some(cell) = local_vars.get(&name, phase)
        {
            return Ok(cell.get());
        }
        // The frame that called `eval` is the innermost scope the eval'd code sees, so
        // its local `a` wins over a global `a`.
        if let Some(value) = self.eval_binding_value(&name) {
            return Ok(value);
        }
        // A closure created by an `eval` resolves its free names against the bindings
        // that eval saw, even after the eval (and its frames) are gone.
        if let Some(cell) = self
            .frame()
            .env
            .eval_scope
            .as_ref()
            .and_then(|scope| scope.get(&name))
        {
            let value = cell.get();
            if matches!(value, Value::Uninitialized) {
                return Err(self.throw_reference(&name));
            }
            return Ok(value);
        }
        // A binding declared by the strict `eval` currently running shadows anything
        // further out, so `eval("var v = 1")`'s `v` is found before `globalThis.v`.
        if let Some(scope) = &self.eval_scope
            && let Some(value) = scope.get(&name)
        {
            return Ok(value.clone());
        }
        if lexical {
            // A lexical global is a binding in a side table, so the dead zone is
            // visible and it is nowhere on the global object.
            if let Some(cell) = self.realm.globals.get(&name) {
                let value = cell.get();
                if matches!(value, Value::Uninitialized) {
                    return Err(self.throw_reference(&name));
                }
                return Ok(value);
            }
            return Err(self.throw_reference(&name));
        }
        // A `var` global is an ordinary property, so the prototype chain and the
        // global object's own builtins are all reachable through it.
        match self.get_property(&self.realm.global.clone(), &name)? {
            Some(value) => Ok(value),
            None => Err(self.throw_reference(&name)),
        }
    }

    /// Store to a global binding.
    ///
    /// Returns a signal when the store is refused: a `const` global may only be
    /// written by its initialiser, and a non-configurable `var` global may not be
    /// deleted but is always writable.
    fn put_global(&mut self, slot: usize, value: Value) -> JsResult<()> {
        let (name, lexical, is_const) = self.global_binding(slot);

        // An assignment inside a `with` writes through to the object that supplies the
        // name, which for an `eval` means consulting the same chain `get_global` does.
        if let Some(owner) = self.with_owner(&name) {
            self.set_property_with(&owner, &name, value, DeleteFailure::Ignore)?;
            return Ok(());
        }
        // A variable the running eval *declared* takes the write before any caller
        // binding: `eval("var c = 1")` creates `c` in the eval's own variable
        // environment even when an enclosing function has a `var c` of its own.
        if self.eval_caller.flatten().is_some()
            && let Some((local_vars, phase)) = self.visible_local_vars()
            && local_vars.contains(&name)
            && let Some(cell) = local_vars.get(&name, phase)
        {
            cell.set(value);
            return Ok(());
        }
        // A binding of the frame that called `eval` takes the write, which is what
        // makes `f("a = 3")` change the caller's `a`.
        if self.set_eval_binding(&name, &value)? {
            return Ok(());
        }
        // A closure that outlived its `eval` writes through to the same cell it reads.
        if let Some(cell) = self
            .frame()
            .env
            .eval_scope
            .as_ref()
            .and_then(|scope| scope.get(&name))
            .cloned()
        {
            cell.set(value);
            return Ok(());
        }
        // Inside a strict `eval`, a name it declared itself is stored there. An
        // *assignment* to a name nobody declared is not a declaration, so it falls
        // through: strict mode raises `ReferenceError` for it, which is what
        // `eval("1 + (@not_def = 1)")` has to do.
        if let Some(scope) = self.eval_scope.as_mut()
            && scope.contains_key(&name)
        {
            scope.insert(name, value);
            return Ok(());
        }
        if lexical {
            if let Some(cell) = self.realm.globals.get(&name).cloned() {
                if is_const && !matches!(cell.get(), Value::Uninitialized) {
                    return Err(self.read_only_global(&name));
                }
                cell.set(value);
                return Ok(());
            }
            // Writing an undeclared lexical global is a `ReferenceError` in strict
            // mode and a silent global creation otherwise; creating the cell here
            // keeps both paths from losing the value.
            let cell = self.new_cell(value);
            self.realm.globals.insert(name, cell);
            return Ok(());
        }
        // A variable a direct `eval` declared in this call shadows the global object, even
        // when the global already has a property of that name — `var c = "global"` at the
        // top level must not capture `eval("var c = 1")` inside a function.
        if let Some((local_vars, phase)) = self.visible_local_vars()
            && local_vars.contains(&name)
            && let Some(cell) = local_vars.get(&name, phase)
        {
            cell.set(value);
            return Ok(());
        }
        let global = self.realm.global.clone();
        match global.own_property(&name) {
            // A `var` global is writable, so the store goes straight through — but
            // through the property, so `globalThis.v = 3` and `v = 3` agree. A failed
            // store (a non-writable or accessor-without-setter global) is a silent
            // no-op in sloppy mode and a `TypeError` in strict mode, which is the same
            // split every other assignment has.
            Some(_) => {
                let on_failure = if self.current_strict() {
                    DeleteFailure::Throw
                } else {
                    DeleteFailure::Ignore
                };
                self.set_property_with(&global, &name, value, on_failure)
            }
            // Only an implicit global reaches here. In sloppy mode `x = 1` with no
            // declaration creates a *configurable* property, which is what makes it
            // deletable and is the one way a global differs from a declared `var`.
            // In strict mode the same store is a `ReferenceError` instead: that is the
            // whole point of strict mode, and creating the global silently meant
            // `"use strict"; x = 1` looked like it worked.
            None => {
                if self.current_strict() {
                    return Err(self.throw_reference(&name));
                }
                // A direct eval declares here rather than on the global object: its
                // `var`s belong to the caller's variable environment. `declare_eval_local`
                // also covers a plain assignment to a variable an earlier eval made.
                if let Some(cell) = self.declare_eval_local(&name) {
                    cell.set(value);
                    return Ok(());
                }
                global.define_data(&name, value);
                Ok(())
            }
        }
    }

    /// The value of a binding of the frame that called `eval`, if there is one.
    fn eval_binding_value(&self, name: &str) -> Option<Value> {
        let binding = self
            .eval_bindings
            .iter()
            .find(|binding| binding.name == name)?;
        let owner = self.frames.get(binding.frame)?;
        let (slot, index) = (binding.slot, binding.index);
        Some(match slot {
            EvalSlot::Arg => match owner.code.arg_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => owner
                    .cells
                    .get(cell as usize)
                    .map(|cell| cell.get())
                    .unwrap_or(Value::Undefined),
                _ => owner.args.get(index).cloned().unwrap_or(Value::Undefined),
            },
            EvalSlot::Local => match owner.code.loc_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => owner
                    .cells
                    .get(cell as usize)
                    .map(|cell| cell.get())
                    .unwrap_or(Value::Undefined),
                _ => owner.locals.get(index).cloned().unwrap_or(Value::Undefined),
            },
        })
    }

    /// Write to a binding of the frame that called `eval`.
    ///
    /// `Ok(false)` means no binding matched and the caller should keep looking. A
    /// matched binding can still refuse the store, which is why this is fallible: a
    /// `const` is read-only everywhere, and a named function expression's own name is
    /// read-only in strict mode.
    fn set_eval_binding(&mut self, name: &str, value: &Value) -> JsResult<bool> {
        let Some(binding) = self
            .eval_bindings
            .iter()
            .find(|binding| binding.name == name)
        else {
            return Ok(false);
        };
        let (slot, index, frame) = (binding.slot, binding.index, binding.frame);
        let strict = self.current_strict();
        let Some(owner) = self.frames.get_mut(frame) else {
            return Ok(false);
        };
        match slot {
            EvalSlot::Arg => match owner.code.arg_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => {
                    if let Some(cell) = owner.cells.get(cell as usize) {
                        cell.set(value.clone());
                    }
                }
                _ => {
                    if let Some(slot) = owner.args.get_mut(index) {
                        *slot = value.clone();
                    }
                }
            },
            EvalSlot::Local => {
                let declared = owner.code.code.vars.get(index);
                // A `const` is read-only even through `eval`. The realm records this for
                // globals (see `lexical_consts`), but a local's `const`-ness is only in
                // the bytecode, so this is where it has to be enforced.
                if declared.is_some_and(|var| var.is_const) {
                    return Err(self.read_only_local(name));
                }
                // The name of a named function expression is read-only too, but only
                // strict mode reports the attempt: a sloppy `eval("m = 1")` where `m` is
                // such a name is ignored, the same as a direct store would be.
                if declared.is_some_and(|var| var.kind == js_compile::VarKind::FunctionName) {
                    return if strict {
                        Err(self.read_only_local(name))
                    } else {
                        Ok(true)
                    };
                }
                match owner.code.loc_cell.get(index).copied().unwrap_or(-1) {
                    cell if cell >= 0 => {
                        if let Some(cell) = owner.cells.get(cell as usize) {
                            cell.set(value.clone());
                        }
                    }
                    _ => {
                        if let Some(slot) = owner.locals.get_mut(index) {
                            *slot = value.clone();
                        }
                    }
                }
            }
        }
        Ok(true)
    }

    /// The `TypeError` an assignment to a read-only binding raises.
    fn read_only_local(&self, name: &str) -> Signal {
        Signal::Throw(self.make_error("TypeError", &format!("'{name}' is read-only")))
    }

    /// Declare `name` as a variable of the frame that called the running `eval`.
    ///
    /// `None` when there is no direct eval in progress, or it was called from the top
    /// level — then the caller's variable environment *is* the global one and the
    /// existing path is right.
    fn declare_eval_local(&mut self, name: &str) -> Option<Gc<Cell>> {
        let caller = self.eval_caller.flatten()?;
        // The bottom frame is the top-level script, whose variable environment *is* the
        // global one — so `eval("var x = 1")` at the top level still creates a global.
        if caller == 0 {
            return None;
        }
        // A name the eval's *own* variable environment already binds is not a new
        // variable: `eval("var arguments = 1")` inside a parameter list writes that list's
        // `arguments`, and creating a second binding would leave the first — the one
        // closures read — untouched. A binding of an *outer* frame does not count: a
        // declaration shadows it rather than writing through.
        if self
            .eval_bindings
            .iter()
            .any(|binding| binding.name == name && binding.frame == caller)
        {
            return None;
        }
        let phase = if self.in_parameter_list_at(caller) {
            0
        } else {
            1
        };
        self.frame_local_vars(caller)?;
        let local_vars = self.frames.get(caller)?.local_vars.clone()?;
        if let Some(cell) = local_vars.get(name, 1) {
            return Some(cell);
        }
        let cell = self.new_cell(Value::Undefined);
        Some(local_vars.declare(name, phase, cell))
    }

    /// Make sure `frame` has a `LocalVars`, creating it on demand.
    fn frame_local_vars(&mut self, frame: usize) -> Option<Gc<LocalVars>> {
        if let Some(local_vars) = &self.frames.get(frame)?.local_vars {
            return Some(local_vars.clone());
        }
        let local_vars = Gc::new(self.heap, LocalVars::new());
        self.frames.get_mut(frame)?.local_vars = Some(local_vars.clone());
        Some(local_vars)
    }

    /// Whether the realm already has a binding for `name`, lexically or as an *own*
    /// property of the global object. Used to decide whether a store inside an
    /// `eval` writes through to the realm or declares a binding of its own.
    ///
    /// Own rather than inherited: `eval("var toString = 1")` must not be taken for a
    /// write to `Object.prototype.toString`, and must not shadow it either.
    fn global_knows(&mut self, name: &str) -> bool {
        if self.realm.globals.contains_key(name) {
            return true;
        }
        self.realm.global.own_property(name).is_some()
    }

    /// Whether the code currently running is strict.
    ///
    /// Read off the frame's function, which is where the parser's answer ended up; a
    /// native function never runs strict-only checks, and there is no frame at all
    /// before the first script.
    pub fn current_strict(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|frame| frame.code.code.strict)
    }

    /// Initialise a declared global, which is what ends its dead zone.
    ///
    /// A `var` global becomes a property of the global object: writable, so a later
    /// assignment goes through, and non-configurable, so `delete` refuses.
    fn init_global(&mut self, slot: usize, value: Value) -> JsResult<()> {
        let (name, lexical, _) = self.global_binding(slot);
        // A `var` declared by the strict `eval` currently running belongs to the
        // eval's scope, not to the global object: `eval("var v = 1")` must not make
        // `typeof v` answer `"number"` afterwards. A name the realm already has is
        // initialised in place, which is what a re-declaration inside eval does.
        if self.eval_scope.is_some() && !lexical && !self.global_knows(&name) {
            self.eval_scope
                .as_mut()
                .expect("checked above")
                .insert(name, value);
            return Ok(());
        }
        if let Some(scope) = self.eval_scope.as_mut()
            && scope.contains_key(&name)
        {
            scope.insert(name, value);
            return Ok(());
        }
        if lexical {
            // The cell exists (created at declaration time) holding the dead-zone
            // marker; storing the value is what makes the binding readable.
            if let Some(cell) = self.realm.globals.get(&name).cloned() {
                cell.set(value);
                return Ok(());
            }
            let cell = self.new_cell(value);
            self.realm.globals.insert(name, cell);
            return Ok(());
        }
        let global = self.realm.global.clone();
        // Writable and enumerable, but *not* configurable: `delete globalThis.v` has
        // to answer `false` and leave the binding in place. `with_attributes` ties
        // `configurable` to `writable`, so the property is built field by field.
        global.define(
            &name,
            Property {
                value: Some(value),
                writable: true,
                enumerable: true,
                configurable: false,
                ..Property::default()
            },
        );
        Ok(())
    }

    /// The `TypeError` a store to a `const` global raises.
    fn read_only_global(&self, name: &str) -> Signal {
        Signal::Throw(self.make_error("TypeError", &format!("'{name}' is read-only")))
    }

    // -- properties ---------------------------------------------------------

    /// Read `object[key]`, following the prototype chain.
    ///
    /// An inherited accessor runs with the *receiver* as `this`, which is why the
    /// lookup returns the owner rather than just the value.
    pub fn get_property(&mut self, object: &Gc<JsObject>, key: &str) -> JsResult<Option<Value>> {
        self.get_property_key(object, &PropertyKey::from(key))
    }

    /// [`Interp::get_property`] with a key that may be a symbol.
    pub fn get_property_key(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
    ) -> JsResult<Option<Value>> {
        self.get_property_key_for(object, None, key)
    }

    /// [`Interp::get_property_key`], with an explicit `this` for an inherited accessor.
    ///
    /// The receiver matters when the property is found on a *prototype*: a getter has to
    /// see the object the read started from. For a primitive that is the primitive
    /// itself — `Symbol("x").description` runs the getter with the symbol as `this`,
    /// not with `Symbol.prototype` — which is why the receiver cannot be inferred from
    /// the owner.
    pub fn get_property_key_for(
        &mut self,
        object: &Gc<JsObject>,
        receiver: Option<&Value>,
        key: &PropertyKey,
    ) -> JsResult<Option<Value>> {
        // A proxy answers for itself, before any of the shortcuts below: `new
        // Proxy([], {get: …})` must not read `length` out of an element vector it does not
        // have.
        if object.proxy_state().is_some() {
            let receiver = receiver
                .cloned()
                .unwrap_or_else(|| Value::Object(object.clone()));
            return self.proxy_get(object, &receiver, key);
        }
        // The element-vector and `length` shortcuts are string-only: an array index
        // has no symbol spelling.
        if let Some(text) = key.as_str() {
            if let Some(value) = self.own_element(object, text) {
                return Ok(Some(value));
            }
            // `length` is not stored as a property: an array's length is its element
            // count, and keeping one copy of it is what makes `push`/`pop` and
            // `a.length = n` agree.
            if let Some(length) = object.array_length()
                && text == "length"
            {
                // Not `Value::Int(length as i32)`: see [`length_value`].
                return Ok(Some(crate::object::length_value(length)));
            }
        }
        match lookup_key(object, key) {
            Some((_, property)) => {
                if property.is_accessor() {
                    match &property.getter {
                        Some(getter) if !matches!(getter, Value::Undefined) => {
                            let receiver = receiver
                                .cloned()
                                .unwrap_or_else(|| Value::Object(object.clone()));
                            self.call_value(getter.clone(), receiver, Vec::new())
                                .map(Some)
                        }
                        // An accessor with no getter reads as `undefined`.
                        _ => Ok(Some(Value::Undefined)),
                    }
                } else {
                    Ok(property.value)
                }
            }
            None => Ok(None),
        }
    }

    /// `super.key` — start the search at `proto`, but report to `receiver`.
    ///
    /// The difference from [`Interp::get_property`] is exactly the receiver: a getter
    /// found on the prototype of a class has to see the *instance* as `this`, and a
    /// plain property read off `proto` would lose that.
    fn super_read(&mut self, receiver: &Value, proto: &Value, key: &Value) -> JsResult<Value> {
        let start = self.require_object(proto)?;
        let key = self.propkey(key);
        // `super.x` on a null prototype (a class extending `null`) resolves to nothing.
        if matches!(proto, Value::Null) {
            return Ok(Value::Undefined);
        }
        match lookup_key(&start, &key) {
            Some((_, property)) if property.is_accessor() => match &property.getter {
                Some(getter) if !matches!(getter, Value::Undefined) => {
                    self.call_value(getter.clone(), receiver.clone(), Vec::new())
                }
                _ => Ok(Value::Undefined),
            },
            Some((_, property)) => Ok(property.value.unwrap_or(Value::Undefined)),
            None => Ok(Value::Undefined),
        }
    }

    /// `super.key = value` — a store that starts at `proto` but lands on `receiver`.
    ///
    /// A setter found on the prototype runs with `receiver` as `this`; a data property
    /// is shadowed by a new own property on the receiver, the same rule an ordinary
    /// assignment follows.
    fn super_write(
        &mut self,
        receiver: &Value,
        proto: &Value,
        key: &Value,
        value: Value,
    ) -> JsResult<()> {
        let target = self.require_object(receiver)?;
        let key = self.propkey(key);
        if !matches!(proto, Value::Null)
            && let Some((owner, property)) = lookup_key(&self.require_object(proto)?, &key)
        {
            if property.is_accessor() {
                if let Some(setter) = &property.setter
                    && !matches!(setter, Value::Undefined)
                {
                    self.call_value(setter.clone(), receiver.clone(), vec![value])?;
                }
                return Ok(());
            }
            // A data property on the prototype is writable or not; reading `owner` at
            // all is only for the attribute check.
            if !property.writable {
                return Err(self.type_error(format!("'{key}' is read-only")));
            }
            let _ = owner;
        }
        self.set_property_with_key(&target, &key, value, DeleteFailure::Throw)
    }

    /// The object a `super` operation needs, or the `TypeError` the spec raises.
    fn require_object(&self, value: &Value) -> JsResult<Gc<JsObject>> {
        match value.as_object() {
            Some(object) => Ok(object.clone()),
            None => Err(Signal::Throw(
                self.make_error("TypeError", "value is not an object"),
            )),
        }
    }

    /// Build the class object `define_class` produces.
    ///
    /// `[parent, ctor]` becomes `[ctor, proto]`: the constructor becomes a constructor
    /// whose prototype property is a fresh object inheriting from the parent's
    /// prototype, and whose own prototype is the parent constructor — which is what makes
    /// a static method inherited too.
    fn define_class(&mut self, name: &str, flags: u32) -> JsResult<(Value, Gc<JsObject>)> {
        let ctor = self.pop();
        let parent = self.pop();
        let Value::Object(function) = &ctor else {
            return Err(Signal::Throw(
                self.make_error("TypeError", "class constructor must be a function"),
            ));
        };
        // A heritage of `null` keeps `Object.prototype` as the constructor's prototype
        // but gives the instances no prototype at all.
        let (parent_ctor, parent_proto) = if flags & 1 != 0 {
            match &parent {
                Value::Null => (Some(self.realm.function_proto.clone()), None),
                Value::Object(parent_class) => {
                    if !parent_class.is_callable() {
                        return Err(Signal::Throw(
                            self.make_error("TypeError", "parent class must be constructor"),
                        ));
                    }
                    let proto = self.get_property(parent_class, "prototype")?;
                    match proto {
                        Some(Value::Object(proto)) => (Some(parent_class.clone()), Some(proto)),
                        Some(Value::Null) => (Some(parent_class.clone()), None),
                        _ => {
                            return Err(Signal::Throw(self.make_error(
                                "TypeError",
                                "parent prototype must be an object or null",
                            )));
                        }
                    }
                }
                _ => {
                    return Err(Signal::Throw(
                        self.make_error("TypeError", "parent class must be constructor"),
                    ));
                }
            }
        } else {
            (
                Some(self.realm.function_proto.clone()),
                Some(self.realm.object_proto.clone()),
            )
        };
        let proto = self.new_object(parent_proto);
        function.set_proto(parent_ctor);
        // A class is not callable without `new`, and its `prototype` is not writable —
        // both follow from being a class and are what `class A {}; A()` throws on.
        function.set_class_constructor(true);
        function.set_home_object(None);
        self.set_function_name(&ctor, name);
        self.define_class_properties(function, &ctor, &proto)?;
        Ok((ctor, proto))
    }

    /// Give the constructor its `prototype` and `length`, and the prototype its
    /// `constructor` back-pointer.
    fn define_class_properties(
        &mut self,
        function: &Gc<JsObject>,
        ctor: &Value,
        proto: &Gc<JsObject>,
    ) -> JsResult<()> {
        proto.define(
            "constructor",
            Property::with_attributes(ctor.clone(), true, false),
        );
        function.define(
            "prototype",
            Property::prototype_of(Value::Object(proto.clone()), false),
        );
        if let Some((code, _)) = bytecode_of(function) {
            let length = code.code.defined_args;
            function.define(
                "length",
                Property::name_or_length(Value::Int(length as i32)),
            );
        }
        Ok(())
    }

    /// The own array element `key` names, if this object has one.
    fn own_element(&self, object: &Gc<JsObject>, key: &str) -> Option<Value> {
        let index = array_index(key)?;
        // A typed array's elements live in its buffer, not in a property list: reading one
        // goes through the view's element type so a bounds check and a detached buffer are
        // both handled there.
        if object.typed_array_parts().is_some() {
            return object.typed_array_element(index as usize);
        }
        // A hole answers `None`, not `undefined`: it is *not* an own property, so the read
        // has to continue up the prototype chain. Mapping it to `undefined` here ended the
        // search early, which is why `Array.prototype[1] = 9` could not be seen through
        // `[1,,3][1]`.
        match object.array_get(index as usize) {
            Some(Value::Hole) | None => None,
            other => other,
        }
    }

    /// Store `object[key] = value`, honouring setters and inherited properties.
    pub fn set_property(&mut self, object: &Gc<JsObject>, key: &str, value: Value) -> JsResult<()> {
        self.set_property_with(object, key, value, DeleteFailure::Throw)
    }

    /// Store a property, choosing what to do when the object refuses.
    ///
    /// A rejected store is a `TypeError` in strict mode and a silent no-op in sloppy
    /// mode, which is the one place assignment differs by mode. Native code (the array
    /// methods, `JSON`) always asks for the throw.
    pub fn set_property_with(
        &mut self,
        object: &Gc<JsObject>,
        key: &str,
        value: Value,
        on_failure: DeleteFailure,
    ) -> JsResult<()> {
        self.set_property_with_key(object, &PropertyKey::from(key), value, on_failure)
    }

    /// [`Interp::set_property_with`] with a key that may be a symbol.
    pub fn set_property_with_key(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
        value: Value,
        on_failure: DeleteFailure,
    ) -> JsResult<()> {
        // A proxy's `set` trap decides, and its `false` means the same thing an
        // ordinary refusal means: a `TypeError` in strict code, nothing in sloppy code.
        if object.proxy_state().is_some() {
            let receiver = Value::Object(object.clone());
            if let Some(accepted) = self.proxy_set(object, key, value.clone(), &receiver)? {
                return match (accepted, on_failure) {
                    (true, _) | (false, DeleteFailure::Ignore) => Ok(()),
                    (false, DeleteFailure::Throw) => {
                        Err(self.type_error(format!("proxy 'set' refused to write '{key}'")))
                    }
                };
            }
        }
        // The element and `length` shortcuts only exist for string keys: an array
        // index has no symbol spelling, and a symbol named `length` is an ordinary
        // property.
        let text = key.as_str();
        // A typed array's elements are a window onto its buffer: the store has to go
        // through the view's element type so the value is wrapped (or clamped) and so an
        // out-of-range or detached write is a silent no-op, as the spec says.
        if let Some(text) = text
            && let Some(index) = array_index(text)
            && let Some(element) = object.typed_array_element_type()
        {
            // §10.4.5.5: an out-of-bounds index answers `true` *before* `V` is converted,
            // so a `valueOf` with a side effect must not run. Checking the length first is
            // what keeps `valueOfCalled` at 0 in `key-is-out-of-bounds-receiver-is-…`.
            let length = object.typed_array_length().unwrap_or(0);
            if index as usize >= length {
                return Ok(());
            }
            // A BigInt view refuses a Number and a numeric one refuses a Symbol; the spec
            // makes both a `TypeError` rather than a silent skip. The conversion runs here,
            // in `Interp`, because it is script-observable and the storage borrow inside
            // `typed_array_set` must not be held across it.
            let stored = self.element_value(element, value)?;
            if let Some(Err(reason)) = object.typed_array_set(index as usize, stored) {
                return Err(self.type_error(reason));
            }
            return Ok(());
        }
        if let Some(text) = text
            && let Some(index) = array_index(text)
            && object.is_array()
        {
            // An accessor defined on an index lives in the property table while the
            // element slot is a hole, so the store has to go through the property first.
            // Writing the vector directly skipped the setter entirely.
            if let Some(existing) = object.own_property_key(key) {
                if existing.is_accessor() {
                    return match existing.setter.clone() {
                        Some(setter) if !matches!(setter, Value::Undefined) => {
                            let receiver = Value::Object(object.clone());
                            self.call_value(setter, receiver, vec![value]).map(|_| ())
                        }
                        _ => match on_failure {
                            DeleteFailure::Throw => Err(self.type_error(format!(
                                "cannot set property '{text}', it has only a getter"
                            ))),
                            DeleteFailure::Ignore => Ok(()),
                        },
                    };
                }
                if !existing.writable {
                    return match on_failure {
                        DeleteFailure::Throw => {
                            Err(self.type_error(format!("'{text}' is read-only")))
                        }
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
            }
            {
                // Growing the array needs a writable `length`.
                if (index as usize) >= object.array_length().unwrap_or(0)
                    && !object.length_writable()
                {
                    return match on_failure {
                        DeleteFailure::Throw => Err(self.type_error("'length' is read-only")),
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                // Adding an index past the end makes the array longer, which a
                // non-extensible array refuses.
                if index as usize >= object.array_length().unwrap_or(0) && !object.is_extensible() {
                    return match on_failure {
                        DeleteFailure::Throw => Err(self.type_error(format!(
                            "cannot add property '{key}', object is not extensible"
                        ))),
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                if object.element_is_locked(index) {
                    return match on_failure {
                        DeleteFailure::Throw => {
                            Err(self.type_error(format!("'{key}' is read-only")))
                        }
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                object.array_set(index as usize, value);
                return Ok(());
            }
        }
        if text == Some("length") && object.is_array() {
            if !object.length_writable() {
                return match on_failure {
                    DeleteFailure::Throw => Err(self.type_error("'length' is read-only")),
                    DeleteFailure::Ignore => Ok(()),
                };
            }
            let length = value.to_number();
            if length.is_finite() && length >= 0.0 {
                let length = length as usize;
                // `a.length = n` with `n` below a non-configurable element cannot
                // drop that element, so the whole assignment fails. The spec makes
                // this a `TypeError` rather than a silent no-op, in any mode.
                if let Some(blocked) = (length..object.array_length().unwrap_or(0))
                    .find(|index| object.element_is_undeletable(*index as u32))
                {
                    // Sloppy mode stops the truncation at the blocking element
                    // instead of failing; strict mode raises.
                    // Both modes stop at the blocking element; they differ in
                    // whether that is reported. The reference truncates *then*
                    // throws, so the array is left in the shortened state.
                    object.array_truncate(blocked + 1);
                    return match on_failure {
                        DeleteFailure::Throw => {
                            Err(self.type_error(format!("cannot delete property '{blocked}'")))
                        }
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                object.array_set_length(length);
                return Ok(());
            }
        }
        if !object.is_extensible() && !object.has_own_key(key) && lookup_key(object, key).is_none()
        {
            return match on_failure {
                DeleteFailure::Throw => Err(self.type_error(format!(
                    "cannot add property {}, object is not extensible",
                    key
                ))),
                DeleteFailure::Ignore => Ok(()),
            };
        }
        if let Some((owner, property)) = lookup_key(object, key) {
            if property.is_accessor() {
                if let Some(setter) = &property.setter
                    && !matches!(setter, Value::Undefined)
                {
                    let receiver = Value::Object(object.clone());
                    self.call_value(setter.clone(), receiver, vec![value])?;
                }
                return Ok(());
            }
            // An inherited *data* property is shadowed by an own property on the
            // receiver; a non-writable one is not.
            if owner.ptr_eq(object) && !property.writable {
                return match on_failure {
                    DeleteFailure::Throw => Err(self.type_error(format!("'{key}' is read-only"))),
                    DeleteFailure::Ignore => Ok(()),
                };
            }
            // An existing writable own property keeps its attributes: only the value
            // moves. Redefining instead would reset them, which is how a
            // non-configurable global `var` used to become deletable.
            if owner.ptr_eq(object) && object.write_value_key(key, value.clone()) {
                return Ok(());
            }
        }
        // No own data property: create one, shadowing whatever the prototype holds.
        object.define_data_key(key, value);
        Ok(())
    }

    /// Delete `object[key]`.
    pub fn delete_property(&mut self, object: &Gc<JsObject>, key: &str) -> JsResult<bool> {
        self.delete_property_key(object, &PropertyKey::from(key))
    }

    /// [`Interp::delete_property`] with a key that may be a symbol.
    pub fn delete_property_key(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
    ) -> JsResult<bool> {
        // A proxy decides for itself, and may even throw; the invariant checks live in
        // `proxy_delete`.
        if let Some(answer) = self.proxy_delete(object, key)? {
            return Ok(answer);
        }
        if let Some(index) = key.as_str().and_then(array_index)
            && let Some(length) = object.array_length()
            && (index as usize) < length
        {
            // An accessor defined on an index lives in the property table rather than the
            // element vector, so both have to be cleared: `delete` after
            // `Object.defineProperty(a, "1", {get})` used to leave the getter answering.
            object.delete_key(key);
            // A hole rather than a shift: `delete a[0]` leaves `length` alone and the
            // index stops answering `in`.
            if object.element_is_undeletable(index) {
                return Ok(false);
            }
            object.array_set(index as usize, Value::Hole);
            return Ok(true);
        }
        Ok(object.delete_key(key))
    }

    // -- proxy traps --------------------------------------------------------
    //
    // A proxy's internal methods all look the same: ask the handler for the trap, call it
    // with the target and the operation's own arguments, then check the answer against
    // the target. The checks are not decoration — they are what stops a proxy from
    // reporting a `non-configurable` property as missing, which would let a frozen object
    // be rewritten by standing in front of it.

    /// The target and handler of `object`, or `None` when it is not a proxy.
    ///
    /// A *revoked* proxy is a `TypeError` rather than `None`, because every operation on
    /// one has to fail: the slots the traps would have been read from are gone.
    fn proxy_parts(&mut self, object: &Gc<JsObject>) -> JsResult<Option<(Gc<JsObject>, Value)>> {
        let Some(state) = object.proxy_state() else {
            return Ok(None);
        };
        let state = state.borrow();
        match (&state.target, &state.handler) {
            (Some(target), Some(handler)) => Ok(Some((target.clone(), handler.clone()))),
            _ => Err(self.type_error("cannot use a proxy that has been revoked")),
        }
    }

    /// Call `handler[name]`, or answer `None` when the trap is absent.
    ///
    /// A trap that is present but not callable is a `TypeError` rather than "no trap":
    /// the spec checks `IsCallable` and refuses, so `{ get: 1 }` must not quietly fall
    /// back to the target.
    fn proxy_trap(
        &mut self,
        handler: &Value,
        name: &str,
        args: Vec<Value>,
    ) -> JsResult<Option<Value>> {
        let Value::Object(table) = handler else {
            return Err(self.type_error("proxy handler is not an object"));
        };
        let trap = self.get_property(table, name)?.unwrap_or(Value::Undefined);
        match trap {
            Value::Undefined => Ok(None),
            Value::Object(function) if function.is_callable() => {
                let answer = self.call_value(Value::Object(function), handler.clone(), args)?;
                Ok(Some(answer))
            }
            _ => Err(self.type_error(format!("proxy trap '{name}' is not a function"))),
        }
    }

    /// Whether `object`'s own property `key` is one the target refuses to lose.
    ///
    /// A non-configurable property is fixed: no trap may hide it, delete it, or change
    /// its value behind the target's back. This goes through
    /// [`Self::ordinary_own_property`] rather than the property table so that an array's
    /// elements and its `length` — both fixed, both outside the table — are covered.
    fn is_fixed_property(target: &Gc<JsObject>, key: &PropertyKey) -> Option<Property> {
        Self::ordinary_own_property(target, key).filter(|property| !property.configurable)
    }

    /// `[[Get]]` on a proxy. `Some` means the proxy answered, `None` means "not a proxy".
    fn proxy_get(
        &mut self,
        object: &Gc<JsObject>,
        receiver: &Value,
        key: &PropertyKey,
    ) -> JsResult<Option<Value>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let Some(result) = self.proxy_trap(
            &handler,
            "get",
            vec![Value::Object(target.clone()), key_value, receiver.clone()],
        )?
        else {
            // No trap: forward to the target's own `[[Get]]`, keeping the receiver so an
            // inherited getter still sees the proxy.
            return Ok(Some(
                self.get_property_key_for(&target, Some(receiver), key)?
                    .unwrap_or(Value::Undefined),
            ));
        };
        if let Some(fixed) = Self::is_fixed_property(&target, key) {
            if fixed.is_accessor() {
                // A fixed accessor with no getter must read as `undefined`.
                let has_getter = fixed
                    .getter
                    .as_ref()
                    .is_some_and(|getter| !matches!(getter, Value::Undefined));
                if !has_getter && !matches!(result, Value::Undefined) {
                    return Err(self.type_error(format!(
                        "proxy 'get' must report undefined for the non-configurable property '{key}'"
                    )));
                }
            } else if !fixed.writable {
                let fixed_value = fixed.value.unwrap_or(Value::Undefined);
                if !fixed_value.same_value(&result) {
                    return Err(self.type_error(format!(
                        "proxy 'get' disagrees with the non-configurable property '{key}'"
                    )));
                }
            }
        }
        Ok(Some(result))
    }

    /// `[[Set]]` on a proxy.
    ///
    /// The boolean is the trap's answer: the caller turns `false` into a `TypeError` in
    /// strict mode and a silent no-op in sloppy mode, which is the same split an ordinary
    /// read-only store has.
    fn proxy_set(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
        value: Value,
        receiver: &Value,
    ) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let Some(result) = self.proxy_trap(
            &handler,
            "set",
            vec![
                Value::Object(target.clone()),
                key_value,
                value.clone(),
                receiver.clone(),
            ],
        )?
        else {
            // No trap: store on the target, which is where an ordinary proxy write lands.
            self.set_property_with_key(&target, key, value, DeleteFailure::Ignore)?;
            return Ok(Some(true));
        };
        if !result.to_boolean() {
            return Ok(Some(false));
        }
        if let Some(fixed) = Self::is_fixed_property(&target, key) {
            if fixed.is_accessor() {
                let has_setter = fixed
                    .setter
                    .as_ref()
                    .is_some_and(|setter| !matches!(setter, Value::Undefined));
                if !has_setter {
                    return Err(self.type_error(format!(
                        "proxy 'set' succeeded on the non-configurable accessor '{key}', which has no setter"
                    )));
                }
            } else if !fixed.writable {
                let fixed_value = fixed.value.unwrap_or(Value::Undefined);
                if !fixed_value.same_value(&value) {
                    return Err(self.type_error(format!(
                        "proxy 'set' changed the non-configurable property '{key}'"
                    )));
                }
            }
        }
        // A new property cannot appear on a target that refuses new properties.
        if Self::ordinary_own_property(&target, key).is_none() && !target.is_extensible() {
            return Err(self.type_error(format!(
                "proxy 'set' added '{key}' to a non-extensible target"
            )));
        }
        Ok(Some(true))
    }

    /// `[[HasProperty]]` on a proxy, which is the `in` operator.
    fn proxy_has(&mut self, object: &Gc<JsObject>, key: &PropertyKey) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let Some(result) = self.proxy_trap(
            &handler,
            "has",
            vec![Value::Object(target.clone()), key_value],
        )?
        else {
            return Ok(Some(self.has_own_or_inherited(&target, key)?));
        };
        if !result.to_boolean() && Self::is_fixed_property(&target, key).is_some() {
            return Err(self.type_error(format!(
                "proxy 'has' denied the non-configurable property '{key}'"
            )));
        }
        // A non-extensible target cannot grow, so every own property it *has* is fixed.
        if !result.to_boolean()
            && !target.is_extensible()
            && Self::ordinary_own_property(&target, key).is_some()
        {
            return Err(self.type_error(format!(
                "proxy 'has' denied the own property '{key}' of a non-extensible target"
            )));
        }
        Ok(Some(result.to_boolean()))
    }

    /// `[[Delete]]` on a proxy.
    fn proxy_delete(&mut self, object: &Gc<JsObject>, key: &PropertyKey) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let Some(result) = self.proxy_trap(
            &handler,
            "deleteProperty",
            vec![Value::Object(target.clone()), key_value],
        )?
        else {
            return Ok(Some(self.delete_property_key(&target, key)?));
        };
        if result.to_boolean() && Self::is_fixed_property(&target, key).is_some() {
            return Err(self.type_error(format!(
                "proxy 'deleteProperty' removed the non-configurable property '{key}'"
            )));
        }
        if result.to_boolean()
            && !target.is_extensible()
            && Self::ordinary_own_property(&target, key).is_some()
        {
            return Err(self.type_error(format!(
                "proxy 'deleteProperty' removed '{key}' from a non-extensible target"
            )));
        }
        Ok(Some(result.to_boolean()))
    }

    /// The own keys a proxy reports, after the `ownKeys` trap and its invariants.
    fn proxy_own_keys(&mut self, object: &Gc<JsObject>) -> JsResult<Option<Vec<PropertyKey>>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let Some(result) =
            self.proxy_trap(&handler, "ownKeys", vec![Value::Object(target.clone())])?
        else {
            // No trap: the target's own keys, already in the order an object reports.
            return Ok(Some(self.object_own_property_keys(&target)?));
        };
        // `CreateListFromArrayLike` with the string-or-symbol restriction.
        let list = self.list_from_array_like(&result, true)?;
        let mut keys: Vec<PropertyKey> = Vec::with_capacity(list.len());
        for value in list {
            let key = self.propkey(&value);
            if keys.contains(&key) {
                return Err(self.type_error(format!(
                    "proxy 'ownKeys' returned the duplicate key '{key}'"
                )));
            }
            keys.push(key);
        }
        for (key, property) in target.property_entries() {
            if !property.configurable && !keys.contains(&key) {
                return Err(self.type_error(format!(
                    "proxy 'ownKeys' dropped the non-configurable key '{key}'"
                )));
            }
            if !target.is_extensible() && !keys.contains(&key) {
                return Err(self.type_error(format!(
                    "proxy 'ownKeys' dropped '{key}', which a non-extensible target owns"
                )));
            }
        }
        if !target.is_extensible() {
            let own = target.property_entries().len();
            if keys.len() != own {
                return Err(
                    self.type_error("proxy 'ownKeys' does not match its non-extensible target")
                );
            }
        }
        Ok(Some(keys))
    }

    /// `[[GetOwnProperty]]` on a proxy.
    fn proxy_get_own_property(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
    ) -> JsResult<Option<Option<Property>>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let Some(result) = self.proxy_trap(
            &handler,
            "getOwnPropertyDescriptor",
            vec![Value::Object(target.clone()), key_value],
        )?
        else {
            // No trap: the target's own descriptor is the answer. This has to go through
            // the same synthesis an ordinary object gets, because a proxy's own property
            // table is empty by design — reading it directly reported `undefined` for
            // every key, including an array's indices and `length`.
            return Ok(Some(Self::ordinary_own_property(&target, key)));
        };
        let answer = if matches!(result, Value::Undefined) {
            None
        } else {
            Some(self.property_from_descriptor(&result)?)
        };
        let fixed = Self::is_fixed_property(&target, key);
        match (&answer, &fixed) {
            // A non-configurable property cannot be reported as absent.
            (None, Some(_)) => {
                return Err(self.type_error(format!(
                    "proxy 'getOwnPropertyDescriptor' hid the non-configurable property '{key}'"
                )));
            }
            (Some(descriptor), Some(fixed)) => {
                if descriptor.configurable {
                    return Err(self.type_error(format!(
                        "proxy 'getOwnPropertyDescriptor' made '{key}' configurable"
                    )));
                }
                if fixed.is_accessor() != descriptor.is_accessor() {
                    return Err(self.type_error(format!(
                        "proxy 'getOwnPropertyDescriptor' changed the kind of '{key}'"
                    )));
                }
                if !fixed.is_accessor() && !fixed.writable {
                    let fixed_value = fixed.value.clone().unwrap_or(Value::Undefined);
                    let reported = descriptor.value.clone().unwrap_or(Value::Undefined);
                    if !fixed_value.same_value(&reported) {
                        return Err(self.type_error(format!(
                            "proxy 'getOwnPropertyDescriptor' changed the value of '{key}'"
                        )));
                    }
                }
            }
            _ => {}
        }
        Ok(Some(answer))
    }

    /// `[[DefineOwnProperty]]` on a proxy.
    fn proxy_define_property(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
        property: Property,
    ) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let key_value = self.key_value(key);
        let descriptor = self.descriptor_from_property(&property);
        let Some(result) = self.proxy_trap(
            &handler,
            "defineProperty",
            vec![Value::Object(target.clone()), key_value, descriptor],
        )?
        else {
            return Ok(None);
        };
        if !result.to_boolean() {
            return Ok(Some(false));
        }
        let fixed = Self::is_fixed_property(&target, key);
        let creating = Self::ordinary_own_property(&target, key).is_none();
        if creating && !target.is_extensible() {
            return Err(self.type_error(format!(
                "proxy 'defineProperty' added '{key}' to a non-extensible target"
            )));
        }
        if let Some(fixed) = fixed {
            if property.configurable {
                return Err(self.type_error(format!(
                    "proxy 'defineProperty' made the non-configurable '{key}' configurable"
                )));
            }
            if !fixed.writable && property.writable && !fixed.is_accessor() {
                return Err(self.type_error(format!(
                    "proxy 'defineProperty' made the read-only '{key}' writable"
                )));
            }
        }
        Ok(Some(true))
    }

    /// `[[GetPrototypeOf]]` on a proxy.
    fn proxy_get_prototype_of(
        &mut self,
        object: &Gc<JsObject>,
    ) -> JsResult<Option<Option<Gc<JsObject>>>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let Some(result) = self.proxy_trap(
            &handler,
            "getPrototypeOf",
            vec![Value::Object(target.clone())],
        )?
        else {
            return Ok(Some(target.proto()));
        };
        let proto = match &result {
            Value::Null => None,
            Value::Object(proto) => Some(proto.clone()),
            _ => {
                return Err(self.type_error("proxy 'getPrototypeOf' must return an object or null"));
            }
        };
        if !target.is_extensible() && !same_proto(proto.as_ref(), target.proto().as_ref()) {
            return Err(self.type_error(
                "proxy 'getPrototypeOf' changed the prototype of a non-extensible target",
            ));
        }
        Ok(Some(proto))
    }

    /// `[[SetPrototypeOf]]` on a proxy.
    fn proxy_set_prototype_of(
        &mut self,
        object: &Gc<JsObject>,
        proto: Option<Gc<JsObject>>,
    ) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let proto_value = match &proto {
            Some(proto) => Value::Object(proto.clone()),
            None => Value::Null,
        };
        let Some(result) = self.proxy_trap(
            &handler,
            "setPrototypeOf",
            vec![Value::Object(target.clone()), proto_value],
        )?
        else {
            target.set_proto(proto);
            return Ok(Some(true));
        };
        if result.to_boolean()
            && !target.is_extensible()
            && !same_proto(proto.as_ref(), target.proto().as_ref())
        {
            return Err(self.type_error(
                "proxy 'setPrototypeOf' changed the prototype of a non-extensible target",
            ));
        }
        Ok(Some(result.to_boolean()))
    }

    /// `[[IsExtensible]]` on a proxy.
    fn proxy_is_extensible(&mut self, object: &Gc<JsObject>) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let Some(result) = self.proxy_trap(
            &handler,
            "isExtensible",
            vec![Value::Object(target.clone())],
        )?
        else {
            return Ok(Some(target.is_extensible()));
        };
        // The answer is not the proxy's to invent: it must agree with the target, or a
        // script could learn `false` while the target keeps accepting new properties.
        if result.to_boolean() != target.is_extensible() {
            return Err(self.type_error("proxy 'isExtensible' disagrees with its target"));
        }
        Ok(Some(result.to_boolean()))
    }

    /// `[[PreventExtensions]]` on a proxy.
    fn proxy_prevent_extensions(&mut self, object: &Gc<JsObject>) -> JsResult<Option<bool>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        let Some(result) = self.proxy_trap(
            &handler,
            "preventExtensions",
            vec![Value::Object(target.clone())],
        )?
        else {
            target.set_extensible(false);
            return Ok(Some(true));
        };
        if result.to_boolean() && target.is_extensible() {
            return Err(self.type_error(
                "proxy 'preventExtensions' reported success but the target is still extensible",
            ));
        }
        Ok(Some(result.to_boolean()))
    }

    /// `[[Call]]` on a proxy, through the `apply` trap.
    fn proxy_apply(
        &mut self,
        object: &Gc<JsObject>,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Option<Value>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        // The trap gets the arguments as one array, which is the shape the spec passes
        // and the shape `apply` traps are written against (`args[0]` is the first one).
        let arguments = self.new_array(args.clone());
        let this_for_trap = this.clone();
        let Some(result) = self.proxy_trap(
            &handler,
            "apply",
            vec![
                Value::Object(target.clone()),
                this_for_trap,
                Value::Object(arguments),
            ],
        )?
        else {
            return Ok(Some(self.call_value(Value::Object(target), this, args)?));
        };
        Ok(Some(result))
    }

    /// `[[Construct]]` on a proxy, through the `construct` trap.
    fn proxy_construct(
        &mut self,
        object: &Gc<JsObject>,
        args: Vec<Value>,
        new_target: &Value,
    ) -> JsResult<Option<Value>> {
        let Some((target, handler)) = self.proxy_parts(object)? else {
            return Ok(None);
        };
        if !target.is_callable() {
            return Err(self.type_error("proxy target is not a constructor"));
        }
        let arguments = self.new_array(args.clone());
        let Some(result) = self.proxy_trap(
            &handler,
            "construct",
            vec![
                Value::Object(target.clone()),
                Value::Object(arguments),
                new_target.clone(),
            ],
        )?
        else {
            return Ok(Some(self.construct(
                Value::Object(target),
                new_target.clone(),
                args,
            )?));
        };
        if !matches!(result, Value::Object(_)) {
            return Err(self.type_error("proxy 'construct' must return an object"));
        }
        Ok(Some(result))
    }

    /// `[[HasProperty]]` on an ordinary object, which the proxy fallback needs.
    pub(crate) fn has_own_or_inherited(
        &mut self,
        object: &Gc<JsObject>,
        key: &PropertyKey,
    ) -> JsResult<bool> {
        // The object itself first: it may be a proxy, and then the answer is its trap's.
        if let Some(answer) = self.proxy_has(object, key)? {
            return Ok(answer);
        }
        // `has_own` rather than `own_property_key`: an array's elements and its derived
        // `length` are invisible to the property table, and `Array.prototype` is an array.
        if has_own(object, key) {
            return Ok(true);
        }
        // A hole is *not* the end of the search: the chain may still have the key, which
        // is why `Array.prototype[4] = 5` makes `4 in [1,2,3,4,]` true.
        for ancestor in object.chain() {
            // A proxy further up the chain answers for itself.
            if let Some(answer) = self.proxy_has(&ancestor, key)? {
                return Ok(answer);
            }
            if has_own(&ancestor, key) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// `CreateListFromArrayLike`, optionally restricted to property keys.
    ///
    /// The `ownKeys` trap is the one place a script's array reaches the engine this way,
    /// and refusing a non-key element there is what makes `ownKeys: () => [1.5]` a
    /// `TypeError` instead of a property named `"1.5"`.
    fn list_from_array_like(&mut self, value: &Value, keys_only: bool) -> JsResult<Vec<Value>> {
        let object = self.object_from(value)?;
        let length = self
            .get_property(&object, "length")?
            .map(|value| value.to_number())
            .unwrap_or(0.0);
        if !length.is_finite() || length < 0.0 {
            return Err(self.type_error("invalid array-like length"));
        }
        let length = length as usize;
        let mut items = Vec::with_capacity(length.min(1024));
        for index in 0..length {
            let item = self
                .get_property(&object, &index.to_string())?
                .unwrap_or(Value::Undefined);
            if keys_only && matches!(&item, Value::Symbol(_)) {
                items.push(item);
                continue;
            }
            if keys_only && !matches!(&item, Value::String(_)) {
                return Err(self.type_error("proxy 'ownKeys' must return strings and symbols"));
            }
            items.push(item);
        }
        Ok(items)
    }

    /// A `Property` from the descriptor object a trap returned.
    fn property_from_descriptor(&mut self, value: &Value) -> JsResult<Property> {
        let object = self.object_from(value)?;
        let field = |interp: &mut Self, name: &str| -> JsResult<Option<Value>> {
            interp.get_property(&object, name)
        };
        let getter = field(self, "get")?;
        let setter = field(self, "set")?;
        let accessor = getter.is_some() || setter.is_some();
        let boolean = |interp: &mut Self, name: &str| -> JsResult<bool> {
            Ok(interp
                .get_property(&object, name)?
                .is_some_and(|value| value.to_boolean()))
        };
        let enumerable = boolean(self, "enumerable")?;
        let configurable = boolean(self, "configurable")?;
        if accessor {
            if field(self, "value")?.is_some() || field(self, "writable")?.is_some() {
                return Err(
                    self.type_error("a descriptor cannot be both a data property and an accessor")
                );
            }
            Ok(Property {
                getter: getter.or(Some(Value::Undefined)),
                setter: setter.or(Some(Value::Undefined)),
                enumerable,
                configurable,
                writable: false,
                value: None,
            })
        } else {
            Ok(Property {
                value: Some(field(self, "value")?.unwrap_or(Value::Undefined)),
                writable: boolean(self, "writable")?,
                enumerable,
                configurable,
                getter: None,
                setter: None,
            })
        }
    }

    /// The descriptor object for a `Property`, which the `defineProperty` trap receives.
    fn descriptor_from_property(&mut self, property: &Property) -> Value {
        let object = self.new_object(None);
        if property.is_accessor() {
            let getter = property.getter.clone().unwrap_or(Value::Undefined);
            let setter = property.setter.clone().unwrap_or(Value::Undefined);
            object.define_data("get", getter);
            object.define_data("set", setter);
        } else {
            let value = property.value.clone().unwrap_or(Value::Undefined);
            object.define_data("value", value);
            object.define_data("writable", Value::Bool(property.writable));
        }
        object.define_data("enumerable", Value::Bool(property.enumerable));
        object.define_data("configurable", Value::Bool(property.configurable));
        Value::Object(object)
    }

    // -- calling ------------------------------------------------------------

    /// Call `callee` with `this` and `args`.
    pub fn call_value(&mut self, callee: Value, this: Value, args: Vec<Value>) -> JsResult<Value> {
        let Value::Object(function) = &callee else {
            // `typeof x === "function"` is not enough to make `x()` legal: only a
            // callable object is. `null` and `undefined` need no special wording
            // here — their `ToString` already reads the way the reference's does.
            let what = format!("{} is not a function", callee.to_js_string());
            return Err(Signal::Throw(self.make_error("TypeError", &what)));
        };
        // A proxy of a function is called through its `apply` trap. It has no
        // `ObjectKind::Function` of its own, so this has to come before the dispatch
        // below, and a proxy of a non-function has nothing to forward to.
        if function.proxy_state().is_some() {
            return match self.proxy_apply(function, this, args)? {
                Some(result) => Ok(result),
                None => Err(Signal::Throw(
                    self.make_error("TypeError", "proxy target is not callable"),
                )),
            };
        }
        let callable = function.with(|data| match &data.kind {
            ObjectKind::Function(callable) => match callable {
                Callable::Native { func, .. } => Some(NativeCall::Native(*func)),
                // The `Rc` is cloned out so the borrow of this object's data is released
                // before the closure runs: the closure may touch the same object.
                Callable::Host { func, .. } => Some(NativeCall::Host(func.clone())),
                Callable::Bound {
                    target,
                    this,
                    args: leading,
                } => Some(NativeCall::Bound {
                    target: target.clone(),
                    this: this.clone(),
                    leading: leading.clone(),
                }),
                Callable::Bytecode { code, env } => {
                    Some(NativeCall::Bytecode(code.clone(), env.clone()))
                }
                Callable::Revoker(slots) => Some(NativeCall::Revoker(slots.clone())),
                Callable::PromiseResolve(promise) => {
                    Some(NativeCall::PromiseResolve(promise.clone()))
                }
                Callable::PromiseReject(promise) => {
                    Some(NativeCall::PromiseReject(promise.clone()))
                }
                Callable::AwaitResume {
                    coroutine,
                    promise,
                    kind,
                } => Some(NativeCall::AwaitResume {
                    coroutine: coroutine.clone(),
                    promise: promise.clone(),
                    kind: *kind,
                }),
                Callable::CombinatorSettle {
                    state,
                    index,
                    fulfilled,
                } => Some(NativeCall::CombinatorSettle {
                    state: state.clone(),
                    index: *index,
                    fulfilled: *fulfilled,
                }),
                Callable::FinallyShim {
                    callback,
                    fulfilled,
                } => Some(NativeCall::FinallyShim {
                    callback: callback.clone(),
                    fulfilled: *fulfilled,
                }),
                Callable::ModuleBinding { cell } => Some(NativeCall::ModuleBinding(cell.clone())),
                Callable::YieldResult { promise, done } => Some(NativeCall::YieldResult {
                    promise: promise.clone(),
                    done: *done,
                }),
                Callable::DisposeStep {
                    remaining,
                    promise,
                    error,
                    rejected,
                } => Some(NativeCall::DisposeStep {
                    remaining: remaining.clone(),
                    promise: promise.clone(),
                    error: error.clone(),
                    rejected: *rejected,
                }),
                Callable::GeneratorResume {
                    generator,
                    promise,
                    kind,
                } => Some(NativeCall::GeneratorResume {
                    generator: generator.clone(),
                    promise: promise.clone(),
                    kind: *kind,
                }),
                Callable::SettlePromise {
                    promise,
                    value,
                    rejected,
                } => Some(NativeCall::SettlePromise {
                    promise: promise.clone(),
                    value: value.clone(),
                    rejected: *rejected,
                }),
            },
            _ => None,
        });
        match callable {
            Some(NativeCall::Host(func)) => {
                // Same frame bookkeeping as a `Native`, so a stack trace shows the host
                // function by name rather than stopping at the caller.
                let frame_less = function
                    .own_property("_frame_less")
                    .and_then(|property| property.value)
                    .is_some_and(|value| value.to_boolean());
                let name = function
                    .own_property("name")
                    .and_then(|property| property.value)
                    .map(|value| value.to_js_string())
                    .unwrap_or_default();
                let outer = if frame_less {
                    None
                } else {
                    self.native_frames
                        .push((name, function.clone(), self.frames.len()));
                    Some(self.current_native.replace(function.clone()))
                };
                let result = {
                    let mut body = func.borrow_mut();
                    body(self, this, &args)
                };
                if !frame_less {
                    self.native_frames.pop();
                    self.current_native = outer.flatten();
                }
                result
            }
            Some(NativeCall::Native(func)) => {
                // A function marked `_frame_less` is one the reference implements
                // without a call frame — `eval` is the only one so far.
                let frame_less = function
                    .own_property("_frame_less")
                    .and_then(|property| property.value)
                    .is_some_and(|value| value.to_boolean());
                let name = function
                    .own_property("name")
                    .and_then(|property| property.value)
                    .map(|value| value.to_js_string())
                    .unwrap_or_default();
                let outer = if frame_less {
                    None
                } else {
                    self.native_frames
                        .push((name, function.clone(), self.frames.len()));
                    Some(self.current_native.replace(function.clone()))
                };
                let result = func(self, this, &args);
                if !frame_less {
                    self.native_frames.pop();
                    self.current_native = outer.flatten();
                }
                result
            }
            Some(NativeCall::Revoker(slots)) => {
                // Idempotent: revoking twice answers `undefined` again rather than
                // complaining, which is what the spec says and what a `finally` block
                // relies on.
                let mut state = slots.borrow_mut();
                state.target = None;
                state.handler = None;
                Ok(Value::Undefined)
            }
            Some(NativeCall::PromiseResolve(promise)) => {
                // One-shot: a second call is silently ignored, which
                // `fulfill_promise`/`reject_promise` enforce by refusing to settle twice.
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                self.resolve_promise_with(&promise, value)?;
                Ok(Value::Undefined)
            }
            Some(NativeCall::PromiseReject(promise)) => {
                let reason = args.first().cloned().unwrap_or(Value::Undefined);
                self.reject_promise(&promise, reason);
                Ok(Value::Undefined)
            }
            Some(NativeCall::CombinatorSettle {
                state,
                index,
                fulfilled,
            }) => {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                self.settle_combinator(&state, index, fulfilled, value)?;
                Ok(Value::Undefined)
            }
            Some(NativeCall::AwaitResume {
                coroutine,
                promise,
                kind,
            }) => {
                // Resume *inline*, in the reaction that delivered the value. Deferring it
                // by another job inserts a tick between every `await` and its
                // continuation, which reorders `await` against unrelated `then` chains —
                // `f().then(g)` then runs before `f`'s own next statement.
                self.drive_async(
                    &coroutine,
                    &promise,
                    kind,
                    args.first().cloned().unwrap_or(Value::Undefined),
                )
                .map(|()| Value::Undefined)
            }
            Some(NativeCall::FinallyShim {
                callback,
                fulfilled,
            }) => {
                // §27.2.5.4.1: run `onFinally` with no arguments, then hand back a promise
                // that settles with the *original* outcome once the callback's result has
                // settled. The callback's own value is discarded — that is the whole
                // difference between `finally` and `then`.
                let original = args.first().cloned().unwrap_or(Value::Undefined);
                let result = match &callback {
                    Some(callback) => {
                        self.call_value(callback.clone(), Value::Undefined, Vec::new())?
                    }
                    None => Value::Undefined,
                };
                let resolved = self.promise_resolve_value(result)?;
                let Value::Object(resolved) = resolved else {
                    return Ok(original);
                };
                // The wait is a chained promise rather than a bare return, so a callback
                // that returns a slow promise really does delay the continuation.
                let waiting = self.new_promise();
                let pass = JsObject::function(
                    self.heap,
                    Some(self.realm.function_proto.clone()),
                    Callable::SettlePromise {
                        promise: waiting.clone(),
                        value: original,
                        rejected: !fulfilled,
                    },
                );
                let pass = Value::Object(pass);
                // Whichever way the callback's promise settles, the original outcome wins.
                self.perform_then(&resolved, Some(pass.clone()), Some(pass))?;
                Ok(Value::Object(waiting))
            }
            Some(NativeCall::DisposeStep {
                remaining,
                promise,
                error,
                rejected,
            }) => {
                let error = if rejected {
                    let reason = args.first().cloned().unwrap_or(Value::Undefined);
                    self.merge_dispose_error(reason, error)
                } else {
                    error
                };
                self.dispose_step(&remaining, &promise, error)?;
                Ok(Value::Undefined)
            }
            Some(NativeCall::ModuleBinding(cell)) => Ok(cell.get()),
            Some(NativeCall::YieldResult { promise, done }) => {
                let value = args.first().cloned().unwrap_or(Value::Undefined);
                let result = self.iterator_result(value, done)?;
                self.fulfill_promise(&promise, result);
                Ok(Value::Undefined)
            }
            Some(NativeCall::GeneratorResume {
                generator,
                promise,
                kind,
            }) => {
                // Inline for the same reason as `AwaitResume`: a deferred resume adds a
                // tick and reorders the generator against other chains.
                self.step_async_generator(
                    &generator,
                    &promise,
                    kind,
                    args.first().cloned().unwrap_or(Value::Undefined),
                )
                .map(|()| Value::Undefined)
            }
            Some(NativeCall::SettlePromise {
                promise,
                value,
                rejected,
            }) => {
                if rejected {
                    self.reject_promise(&promise, value);
                } else {
                    self.fulfill_promise(&promise, value);
                }
                Ok(Value::Undefined)
            }
            Some(NativeCall::Bound {
                target,
                this: bound_this,
                leading,
            }) => {
                let mut all = leading;
                all.extend(args);
                self.call_value(target, bound_this, all)
            }
            Some(NativeCall::Bytecode(code, env)) => {
                if code.code.kind.is_generator() {
                    return self.new_generator(callee, this, args);
                }
                if code.code.kind.is_async() {
                    // Same coroutine machinery as a generator; the difference is only in
                    // what the completion does — see `new_async_function`.
                    return self.new_async_function(callee, this, args);
                }
                self.execute(code, env, this, args, callee)
            }
            None => Err(Signal::Throw(
                self.make_error("TypeError", "not a function"),
            )),
        }
    }

    /// Give a freshly built function its own `prototype` object.
    ///
    /// Every function that can be used with `new` carries one, and its
    /// `constructor` points back at the function — a reference cycle, which is
    /// exactly what the collector's cycle pass exists to reclaim.
    fn install_function_prototype(&mut self, function: &Gc<JsObject>) {
        let prototype = self.new_object(Some(self.realm.object_proto.clone()));
        let constructor = Value::Object(function.clone());
        prototype.define(
            "constructor",
            Property::with_attributes(constructor, true, false),
        );
        let value = Value::Object(prototype);
        function.define("prototype", Property::prototype_of(value, true));
    }

    /// The object `special_object` builds.
    ///
    /// Only `arguments` is implemented. It is an ordinary object with a `length` and
    /// one property per passed argument, which is what `typeof`, `arguments[i]` and
    /// `arguments.length` observe. Mapped arguments — sloppy mode's aliasing between
    /// `arguments[i]` and the parameter it came from — is *not* implemented, so a
    /// write through `arguments` does not reach the parameter.
    fn special_object(&mut self, magic: u32) -> JsResult<Value> {
        match magic {
            0 | 1 => {
                let args = self.frame().passed.clone();
                let proto = self.realm.object_proto.clone();
                let object = self.new_object(Some(proto));
                // The marker `Object.prototype.toString` needs: an arguments object is an
                // ordinary object apart from reporting `[object Arguments]`, and that has
                // no `Symbol.toStringTag` of its own. Set before the indices so it does not
                // show up ahead of them in a key listing anyway — it is non-enumerable.
                self.internal_set(&object, "_class", self.string("Arguments"));
                // `length` is writable and configurable but neither enumerable nor a
                // plain data property: `Object.keys(arguments)` lists the indices only,
                // which is how `{...arguments}` copies them without the length.
                object.define(
                    "length",
                    Property::with_attributes(Value::Int(args.len() as i32), true, false),
                );
                for (index, value) in args.into_iter().enumerate() {
                    object.define_data(&index.to_string(), value);
                }
                // An arguments object is iterable: `[...arguments]` and
                // `for (const a of arguments)` both work, through the *array* iterator
                // because the object is array-like. Without this the whole spread form
                // reported "not iterable".
                let iterator_key = self.realm.well_known_key(WellKnown::Iterator);
                let values = self
                    .realm
                    .array_proto
                    .own_property("values")
                    .and_then(|property| property.value);
                if let Some(values) = values {
                    object.define_key(
                        &iterator_key,
                        Property::with_attributes(values, true, false),
                    );
                }
                Ok(Value::Object(object))
            }
            // `this function`: what a named function expression binds its own name to.
            2 => Ok(self
                .current_callee()
                .map_or(Value::Undefined, Value::Object)),
            // `new.target`: set by `construct`, `undefined` for an ordinary call, and
            // inherited from the caller inside an `eval`.
            3 => Ok(self
                .eval_new_target
                .clone()
                .unwrap_or_else(|| self.frame().new_target.clone())),
            // `[[HomeObject]]` of the function being run: what `super` is relative to.
            // An arrow has none of its own, so the lookup walks out to the enclosing
            // method — which is why `super` works inside an arrow inside a method.
            4 => Ok(self
                .frames
                .iter()
                .rev()
                .find_map(|frame| frame.callee.as_object().and_then(|f| f.home_object()))
                .map_or(Value::Undefined, Value::Object)),
            // `import.meta`: the innermost module body's meta object. Outside a module
            // the compiler never emits this, so an empty fallback is unreachable rather
            // than permissive.
            6 => {
                let name = self.module_stack.last().cloned();
                let meta = name
                    .and_then(|name| self.modules.get(&name).cloned())
                    .and_then(|module| module.with(|r| r.import_meta.clone()));
                Ok(meta.map_or(Value::Undefined, Value::Object))
            }
            other => Err(Signal::Throw(self.make_error(
                "InternalError",
                &format!("special_object {other} is not implemented yet"),
            ))),
        }
    }

    /// `[[HomeObject]]` of the innermost function that has one, if any.
    ///
    /// An arrow has none, so the search walks out to the enclosing method — the same
    /// rule `special_object 4` follows, and the reason `super` works inside an arrow
    /// inside a method.
    fn enclosing_home_object(&self) -> Option<Gc<JsObject>> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.callee.as_object().and_then(|f| f.home_object()))
    }

    /// The dispose method a `using` declaration should snapshot, or an error.
    ///
    /// `await`-using prefers `Symbol.asyncDispose` and falls back to `Symbol.dispose`, which
    /// is what makes a resource that only knows the synchronous protocol usable in both
    /// forms.
    pub fn using_method_for(&mut self, value: &Value, await_form: bool) -> JsResult<Value> {
        // `null` and `undefined` are explicitly allowed and simply dispose to nothing.
        if value.is_nullish() {
            return Ok(Value::Undefined);
        }
        let Some(object) = value.as_object() else {
            return Err(self.type_error("value is not disposable: it is not an object"));
        };
        let mut method = if await_form {
            let found = self.get_property_key(
                object,
                &self.realm.well_known_key(WellKnown::AsyncDisposable),
            )?;
            match found {
                Some(found) if !found.is_nullish() => Some(found),
                _ => self
                    .get_property_key(object, &self.realm.well_known_key(WellKnown::Disposable))?,
            }
        } else {
            self.get_property_key(object, &self.realm.well_known_key(WellKnown::Disposable))?
        };
        let Some(found) = method.take() else {
            return Err(self.type_error("value is not disposable"));
        };
        if found.is_nullish() {
            return Err(self.type_error("value is not disposable"));
        }
        let callable = found.as_object().is_some_and(|object| object.is_callable());
        if !callable {
            return Err(self.type_error("dispose method is not a function"));
        }
        Ok(found)
    }

    /// Call one resource's disposer, folding a failure into `error_state`.
    ///
    /// The value is read through [`Interp::get_loc`] rather than off `locals` directly,
    /// because a captured `using` binding lives in its reference cell and the local slot is
    /// stale — a closure over the resource is legal and must not change what is disposed.
    fn using_dispose_once(&mut self, slot: usize, error_state: Value) -> JsResult<Value> {
        let value = self.get_loc(slot);
        let method = self.get_loc(slot + 1);
        // A resource that was never initialised — its declaration threw — or that is
        // `null`/`undefined` has nothing to dispose.
        if value.is_nullish() || matches!(value, Value::Uninitialized) {
            return Ok(error_state);
        }
        if matches!(method, Value::Undefined | Value::Uninitialized) {
            return Ok(error_state);
        }
        match self.call_value(method, value, Vec::new()) {
            Ok(_) => Ok(error_state),
            Err(Signal::Throw(thrown)) => Ok(self.merge_dispose_error(thrown, error_state)),
            Err(other) => Err(other),
        }
    }

    /// Combine a disposal failure with whatever was already going wrong.
    pub fn merge_dispose_error(&mut self, new_error: Value, error_state: Value) -> Value {
        if matches!(error_state, Value::Uninitialized) {
            return new_error;
        }
        self.new_suppressed_error(new_error, error_state)
    }

    /// A `SuppressedError` instance built straight from its prototype.
    ///
    /// Not through the constructor: a script that rewrites
    /// `SuppressedError.prototype.constructor` must not be able to divert the engine's own
    /// error wrapping.
    pub fn new_suppressed_error(&mut self, error: Value, suppressed: Value) -> Value {
        let proto = self
            .realm
            .suppressed_error_proto
            .clone()
            .unwrap_or_else(|| self.realm.error_proto.clone());
        let object = self.new_object(Some(proto));
        object.define("error", Property::with_attributes(error, true, true));
        object.define(
            "suppressed",
            Property::with_attributes(suppressed, true, true),
        );
        object.define(
            "message",
            Property::with_attributes(self.string("SuppressedError"), true, true),
        );
        self.internal_set(&object, "_class", self.string("Error"));
        self.add_stack(&object);
        Value::Object(object)
    }

    /// Call `callee`, or push its frame and let the main loop run it.
    ///
    /// This is what keeps deep JavaScript recursion off the Rust stack: a bytecode
    /// callee becomes a frame on [`Interp::frames`] and the instruction loop simply
    /// continues, so `function f(){ return f() }` costs heap and stops at
    /// [`Interp::max_frames`] instead of aborting the process. Only a native callee
    /// is entered directly, and natives do not re-enter JavaScript in this
    /// milestone.
    ///
    /// `Ok(None)` means a frame was pushed: the caller must not push a result,
    /// because the returning frame will.
    /// The elements of an array-like value, as a call argument list.
    ///
    /// `apply` accepts any array-like, not only a real array: the reference reads
    /// `length` and then each index, so `{length: 2, 0: "a", 1: "b"}` works too.
    pub fn array_like_to_args(&mut self, value: &Value) -> JsResult<Vec<Value>> {
        let Some(object) = value.as_object().cloned() else {
            if value.is_nullish() {
                // `f.apply(this, null)` passes no arguments at all.
                return Ok(Vec::new());
            }
            return Err(self.type_error("createListFromArrayLike called on non-object"));
        };
        let length = match self.get_property(&object, "length")? {
            Some(length) => length.to_number(),
            None => 0.0,
        };
        if !length.is_finite() || length <= 0.0 {
            return Ok(Vec::new());
        }
        let length = (length as usize).min(1_000_000);
        let mut args = Vec::with_capacity(length);
        for index in 0..length {
            let element = self.get_property(&object, &index.to_string())?;
            args.push(element.unwrap_or(Value::Undefined));
        }
        Ok(args)
    }

    /// Turn a call to a generator function into a generator object.
    ///
    /// The frame is built exactly as an ordinary call would build it, then detached and
    /// stored in the generator before any body instruction runs — which is what makes
    /// `function* g(){ sideEffect() }` inert until `next()`. The one thing that does run
    /// is `initial_yield`, the first instruction of every suspending body.
    pub fn new_generator(
        &mut self,
        function: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Value> {
        let Some(object) = function.as_object().cloned() else {
            return Err(self.type_error("not a generator function"));
        };
        let Some((code, env)) = bytecode_of(&object) else {
            return Err(self.type_error("not a generator function"));
        };
        let is_async = code.code.kind.is_async();
        self.push_frame(code, env, this, args, function)?;
        let frame = self.frames.pop().expect("push_frame just pushed it");
        // A fresh generator is *suspended at start*: the body has not run, so a
        // `return`/`throw` on it finishes without executing anything.
        let mut state = GeneratorState {
            frame: Some(frame),
            phase: GeneratorPhase::SuspendedStart,
            parked_on_await: false,
        };
        // Run to the first suspension. For a generator that is `initial_yield`, so no
        // body code runs; for a malformed body it would be the first `yield`.
        // An `async function*` answers an async generator: same coroutine, but its
        // methods return promises and `for await` reaches it through `Symbol.asyncIterator`.
        let proto = if is_async {
            self.realm
                .async_generator_proto
                .clone()
                .unwrap_or_else(|| self.realm.generator_proto.clone())
        } else {
            self.realm.generator_proto.clone()
        };
        let generator = self.new_object(Some(proto));
        match self.step_generator(&mut state, ResumeKind::Start, Value::Undefined) {
            Ok(_) => {}
            Err(error) => return Err(error),
        }
        generator.with_mut(|data| {
            data.kind = ObjectKind::Generator(Box::new(state));
        });
        Ok(Value::Object(generator))
    }

    /// Advance a generator object, as `%GeneratorPrototype%.next` and friends do.
    ///
    /// Returns the `{value, done}` object itself rather than the pair, because a
    /// `yield*` suspension already has one — the inner iterator's own result — and the
    /// protocol says to hand that straight through rather than nest it.
    /// Advance a generator and report *why* it parked.
    ///
    /// The sync `next()` does not need the reason — a `yield` and an `await` both mean
    /// "hand this value to the caller" — but an async generator does: an `await` has to
    /// settle before it can answer, while a `yield` is the answer.
    fn resume_generator_detailed(
        &mut self,
        generator: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<(Value, bool, bool, Option<SuspendKind>)> {
        let Some(mut state) = generator.with_mut(|data| match &mut data.kind {
            ObjectKind::Generator(state) => Some(std::mem::replace(
                state.as_mut(),
                GeneratorState {
                    frame: None,
                    phase: GeneratorPhase::Executing,
                    parked_on_await: false,
                },
            )),
            _ => None,
        }) else {
            return Err(self.type_error("not a generator"));
        };
        let result = self.step_generator(&mut state, kind, value);
        generator.with_mut(|data| {
            data.kind = ObjectKind::Generator(Box::new(state));
        });
        result
    }

    /// Advance a generator object, as `%GeneratorPrototype%.next` and friends do.
    ///
    /// Returns the `{value, done}` object itself rather than the pair, because a
    /// `yield*` suspension already has one — the inner iterator's own result — and the
    /// protocol says to hand that straight through rather than nest it.
    pub fn resume_generator(
        &mut self,
        generator: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<Value> {
        let (value, done, ready, _reason) =
            self.resume_generator_detailed(generator, kind, value)?;
        if ready {
            return Ok(value);
        }
        self.iterator_result(value, done)
    }

    /// `next`/`return`/`throw` on an async generator, which answer a promise.
    ///
    /// The body is the same coroutine a sync generator runs; what changes is that an
    /// `await` inside it has to settle before the method can answer, so the step is
    /// driven off the job queue rather than returning inline.
    pub fn resume_async_generator(
        &mut self,
        generator: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<Value> {
        let promise = self.new_promise();
        self.step_async_generator(generator, &promise, kind, value)?;
        Ok(Value::Object(promise))
    }

    /// Dispose a list of `(resource, disposeMethod)` pairs, awaiting each disposer.
    ///
    /// Returns the promise `AsyncDisposableStack.disposeAsync` hands back. The first
    /// disposer runs **synchronously** — the reference starts it before returning, which
    /// a test can observe by logging after the call — and the rest follow once their
    /// predecessor settles.
    pub fn dispose_async_all(&mut self, resources: Vec<(Value, Value)>) -> JsResult<Value> {
        let promise = self.new_promise();
        let mut pairs = Vec::with_capacity(resources.len());
        for (value, method) in resources {
            let pair = self.new_array(vec![value, method]);
            pairs.push(Value::Object(pair));
        }
        let remaining = self.new_array(pairs);
        self.dispose_step(&remaining, &promise, Value::Uninitialized)?;
        Ok(Value::Object(promise))
    }

    /// One link of the `disposeAsync` chain.
    ///
    /// Loops while the disposers finish synchronously, and parks on the first thenable it
    /// meets — pushing itself back in as the continuation, with the accumulated error
    /// carried along so a later failure can still be folded into the `SuppressedError`.
    fn dispose_step(
        &mut self,
        remaining: &Gc<JsObject>,
        promise: &Gc<JsObject>,
        mut error: Value,
    ) -> JsResult<()> {
        loop {
            let length = remaining.array_length().unwrap_or(0);
            if length == 0 {
                if matches!(error, Value::Uninitialized) {
                    self.fulfill_promise(promise, Value::Undefined);
                } else {
                    self.reject_promise(promise, error);
                }
                return Ok(());
            }
            let Some(Value::Object(pair)) = remaining
                .array_get(length - 1)
                .filter(|v| !matches!(v, Value::Hole))
            else {
                remaining.array_truncate(length - 1);
                continue;
            };
            remaining.array_truncate(length - 1);
            let value = pair.array_get(0).unwrap_or(Value::Undefined);
            let Some(Value::Object(method)) = pair.array_get(1) else {
                continue;
            };
            if !method.is_callable() {
                continue;
            }
            match self.call_value(Value::Object(method), value, Vec::new()) {
                Ok(result) => {
                    if !self.is_thenable(&result)? {
                        // Finished synchronously; the next disposer is the next iteration.
                        continue;
                    }
                    let awaited = match self.promise_resolve_value(result) {
                        Ok(awaited) => awaited,
                        Err(Signal::Throw(reason)) => {
                            self.reject_promise(promise, reason);
                            return Ok(());
                        }
                        Err(other) => return Err(other),
                    };
                    let Value::Object(awaited) = awaited else {
                        continue;
                    };
                    let ok = JsObject::function(
                        self.heap,
                        Some(self.realm.function_proto.clone()),
                        Callable::DisposeStep {
                            remaining: remaining.clone(),
                            promise: promise.clone(),
                            error: error.clone(),
                            rejected: false,
                        },
                    );
                    let failed = JsObject::function(
                        self.heap,
                        Some(self.realm.function_proto.clone()),
                        Callable::DisposeStep {
                            remaining: remaining.clone(),
                            promise: promise.clone(),
                            error,
                            rejected: true,
                        },
                    );
                    self.perform_then(
                        &awaited,
                        Some(Value::Object(ok)),
                        Some(Value::Object(failed)),
                    )?;
                    return Ok(());
                }
                Err(Signal::Throw(thrown)) => {
                    error = self.merge_dispose_error(thrown, error);
                }
                Err(other) => return Err(other),
            }
        }
    }

    /// Whether `value` would be followed by `ResolvePromise` rather than fulfilled as-is.
    fn is_thenable(&mut self, value: &Value) -> JsResult<bool> {
        let Value::Object(object) = value else {
            return Ok(false);
        };
        Ok(matches!(
            self.get_property(object, "then")?,
            Some(Value::Object(then)) if then.is_callable()
        ))
    }

    /// One step of an async generator, settling `promise` with an `{value, done}` result.
    fn step_async_generator(
        &mut self,
        generator: &Gc<JsObject>,
        promise: &Gc<JsObject>,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<()> {
        let stepped = self.resume_generator_detailed(generator, kind, value);
        let (value, done, ready, reason) = match stepped {
            Ok(stepped) => stepped,
            Err(Signal::Throw(reason)) => {
                self.reject_promise(promise, reason);
                return Ok(());
            }
            Err(other) => return Err(other),
        };
        // Only an `await` has to wait before the method can answer.
        if !done && reason == Some(SuspendKind::Await) {
            let awaited = match self.promise_resolve_value(value) {
                Ok(awaited) => awaited,
                Err(Signal::Throw(reason)) => {
                    self.reject_promise(promise, reason);
                    return Ok(());
                }
                Err(other) => return Err(other),
            };
            let Value::Object(awaited) = awaited else {
                return Ok(());
            };
            let resumed = JsObject::function(
                self.heap,
                Some(self.realm.function_proto.clone()),
                Callable::GeneratorResume {
                    generator: generator.clone(),
                    promise: promise.clone(),
                    kind: ResumeKind::Next,
                },
            );
            let raised = JsObject::function(
                self.heap,
                Some(self.realm.function_proto.clone()),
                Callable::GeneratorResume {
                    generator: generator.clone(),
                    promise: promise.clone(),
                    kind: ResumeKind::Throw,
                },
            );
            self.perform_then(
                &awaited,
                Some(Value::Object(resumed)),
                Some(Value::Object(raised)),
            )?;
            return Ok(());
        }
        // `yield v` in an *async* generator awaits `v`, and so does the `return v` that
        // completes it — both arrive as the settled value rather than as the promise
        // object. A plain value skips the round trip, which is also what keeps `yield 1`
        // from costing an extra tick.
        let awaits_value = self.is_thenable(&value)?;
        if awaits_value {
            let awaited = match self.promise_resolve_value(value) {
                Ok(awaited) => awaited,
                Err(Signal::Throw(reason)) => {
                    self.reject_promise(promise, reason);
                    return Ok(());
                }
                Err(other) => return Err(other),
            };
            let Value::Object(awaited) = awaited else {
                return Ok(());
            };
            // A rejected yield throws *at the yield* inside the body, so the rejection
            // resumes the generator rather than the method's promise.
            let answer = JsObject::function(
                self.heap,
                Some(self.realm.function_proto.clone()),
                Callable::YieldResult {
                    promise: promise.clone(),
                    done,
                },
            );
            let raise = JsObject::function(
                self.heap,
                Some(self.realm.function_proto.clone()),
                Callable::GeneratorResume {
                    generator: generator.clone(),
                    promise: promise.clone(),
                    kind: ResumeKind::Throw,
                },
            );
            self.perform_then(
                &awaited,
                Some(Value::Object(answer)),
                Some(Value::Object(raise)),
            )?;
            return Ok(());
        }
        let result = if ready {
            // `yield*` handed back the inner iterator's own result object.
            value
        } else {
            self.iterator_result(value, done)?
        };
        self.fulfill_promise(promise, result);
        Ok(())
    }

    /// The `{value, done}` object an iterator method answers with.
    pub fn iterator_result(&mut self, value: Value, done: bool) -> JsResult<Value> {
        let result = self.new_object(Some(self.realm.object_proto.clone()));
        result.define_data("value", value);
        result.define_data("done", Value::Bool(done));
        Ok(Value::Object(result))
    }

    /// Advance a generator, returning the `{value, done}` pair the protocol wants.
    fn step_generator(
        &mut self,
        state: &mut GeneratorState,
        kind: ResumeKind,
        value: Value,
    ) -> JsResult<(Value, bool, bool, Option<SuspendKind>)> {
        match state.phase {
            GeneratorPhase::Executing => {
                return Err(self.type_error("cannot invoke a running generator"));
            }
            GeneratorPhase::Completed => {
                // A finished generator answers every resumption without running.
                return Ok(match kind {
                    ResumeKind::Throw => return Err(Signal::Throw(value)),
                    ResumeKind::Return => (value, true, false, None),
                    ResumeKind::Next | ResumeKind::Start => (Value::Undefined, true, false, None),
                });
            }
            GeneratorPhase::SuspendedStart => {
                if kind != ResumeKind::Next && kind != ResumeKind::Start {
                    // The body never started, so there is nothing to unwind or return
                    // from: `return(v)` finishes, `throw(e)` rethrows at the caller.
                    state.frame = None;
                    state.phase = GeneratorPhase::Completed;
                    return Ok(match kind {
                        ResumeKind::Throw => return Err(Signal::Throw(value)),
                        _ => (value, true, false, None),
                    });
                }
            }
            GeneratorPhase::SuspendedYield => {}
        }

        let Some(mut frame) = state.frame.take() else {
            return Ok((Value::Undefined, true, false, None));
        };
        // `next(v)` and `return(v)` re-enter the `yield` with the value *and* the magic
        // number that says which method resumed it; the emitted `if_false` branches on
        // the magic. `initial_yield` reads neither, and a `throw` does not resume at all.
        let mut raised = None;
        match kind {
            ResumeKind::Next | ResumeKind::Start => {
                if state.phase == GeneratorPhase::SuspendedYield {
                    frame.stack.push(value);
                    // An `await` resumes with its value alone; only a `yield` gets the
                    // magic number that says which of `next`/`return` resumed it.
                    if !state.parked_on_await {
                        frame.stack.push(Value::Int(RESUME_NEXT));
                    }
                }
            }
            ResumeKind::Return => {
                // A `return` on a not-yet-started generator is handled above, so this is
                // always a real resumption.
                frame.stack.push(value);
                frame.stack.push(Value::Int(RESUME_RETURN));
            }
            ResumeKind::Throw => raised = Some(value),
        }
        state.phase = GeneratorPhase::Executing;

        let depth = self.frames.len();
        self.frames.push(frame);
        // A `throw` unwinds from where the frame parked, exactly as if the exception had
        // happened at the `yield`.
        if let Some(exception) = raised {
            if self.catch_here(exception.clone()).is_none() {
                self.frames.pop();
                state.phase = GeneratorPhase::Completed;
                return Err(Signal::Throw(exception));
            }
        }

        // Only the generator's *own* frame ends the generator. The body may call ordinary
        // functions, and those push frames of their own: a `Step::Return` from one of them
        // is a nested call finishing, not the generator completing. Miscounting that is
        // silent — the generator reports the callee's value as its own result and marks
        // itself done — so the depth comparison is load-bearing, not defensive.
        let own_frame = depth + 1;
        loop {
            match self.step() {
                Ok(Step::Continue) => {}
                Ok(Step::Suspend(reason)) if self.frames.len() == own_frame => {
                    let mut frame = self.frames.pop().expect("a frame is live");
                    // `yield` left the value on the stack; `initial_yield` left nothing.
                    let yielded = if reason == SuspendKind::Initial {
                        Value::Undefined
                    } else {
                        frame.stack.pop().unwrap_or(Value::Undefined)
                    };
                    state.frame = Some(frame);
                    state.parked_on_await = reason == SuspendKind::Await;
                    state.phase = if reason == SuspendKind::Initial {
                        GeneratorPhase::SuspendedStart
                    } else {
                        GeneratorPhase::SuspendedYield
                    };
                    // `yield*` hands the inner iterator's own result object straight
                    // back, so the caller sees one `{value, done}` rather than a nested
                    // one — which is why the third field says "already a result".
                    return Ok((
                        yielded,
                        false,
                        reason == SuspendKind::YieldStar,
                        Some(reason),
                    ));
                }
                Ok(Step::Suspend(_)) => {
                    // A `yield` in a frame other than the generator's would mean a
                    // generator body entered as an ordinary call, which
                    // `call_or_push` prevents.
                    self.unwind_to(depth);
                    state.phase = GeneratorPhase::Completed;
                    return Err(Signal::Throw(self.make_error(
                        "InternalError",
                        "a generator body ran as an ordinary call",
                    )));
                }
                Ok(Step::Return(value)) => {
                    self.frames.pop();
                    if self.frames.len() > depth {
                        // A nested call finished: hand the value back to its caller and
                        // keep running the generator body.
                        self.frames
                            .last_mut()
                            .expect("a caller is live")
                            .stack
                            .push(value);
                        continue;
                    }
                    state.phase = GeneratorPhase::Completed;
                    // The reference closes the generator's closure variables here; ours
                    // live in cells the frame owned, which the drop below releases.
                    return Ok((value, true, false, None));
                }
                Err(Signal::Throw(exception)) => {
                    // The frame that threw gets the first chance to catch, exactly as in
                    // the ordinary call loop; a frame above the generator's is popped and
                    // the exception carries on down.
                    let exception = exception;
                    loop {
                        if self.frames.len() <= depth {
                            // The generator's own frame is gone, so nothing here caught it.
                            state.phase = GeneratorPhase::Completed;
                            return Err(Signal::Throw(exception));
                        }
                        if self.catch_here(exception.clone()).is_some() {
                            break;
                        }
                        self.frames.pop();
                    }
                }
                Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
            }
        }
    }

    /// Pop frames until `depth` are left.
    fn unwind_to(&mut self, depth: usize) {
        self.frames.truncate(depth);
    }
    fn call_or_push(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Option<Value>> {
        if let Value::Object(function) = &callee
            && let Some((code, env)) = bytecode_of(function)
        {
            // Calling a generator function does not run anything: it captures the
            // arguments in a frame and hands back the generator object.
            if code.code.kind.is_generator() {
                return self.new_generator(callee, this, args).map(Some);
            }
            // An async function answers a promise and runs its body as a coroutine, so it
            // takes the same path as a plain call into it.
            if code.code.kind.is_async() {
                return self.new_async_function(callee, this, args).map(Some);
            }
            self.push_frame(code, env, this, args, callee)?;
            return Ok(None);
        }
        self.call_value(callee, this, args).map(Some)
    }

    /// `new callee(args)`.
    ///
    /// A constructor differs from a plain call in three ways: it gets a fresh object
    /// as `this` whose prototype is the constructor's `prototype` property, its
    /// return value is *replaced* by that object unless it returns an object itself,
    /// and a native constructor is expected to initialise the object it is given
    /// rather than build one.
    pub fn construct(
        &mut self,
        callee: Value,
        new_target: Value,
        args: Vec<Value>,
    ) -> JsResult<Value> {
        // `new` on a bound function constructs its *target*, prepending the arguments
        // the bind fixed and ignoring the bound `this` — a bound function is a
        // constructor only when its target is one, and `new` always makes a fresh
        // object of the target's prototype rather than reusing the bound receiver.
        // Without this unwrapping, `constructor1.bind(null, 1)` called under `new`
        // ran the body with `this === null` and threw on `this.x = a`.
        let mut callee = callee;
        let mut new_target = new_target;
        let mut args = args;
        loop {
            let Some(object) = callee.as_object().cloned() else {
                return Err(Signal::Throw(
                    self.make_error("TypeError", "not a constructor"),
                ));
            };
            // A proxy of a function constructs through its `construct` trap, and the
            // trap's answer is the result directly — no fresh object is made here.
            if object.proxy_state().is_some() {
                return match self.proxy_construct(&object, args, &new_target)? {
                    Some(result) => Ok(result),
                    None => Err(Signal::Throw(
                        self.make_error("TypeError", "proxy target is not a constructor"),
                    )),
                };
            }
            // A function can opt out of `[[Construct]]` outright. `Symbol` does: there is
            // no wrapper object to build, and `new Symbol()` is a `TypeError` naming it.
            if object
                .own_property("_not_a_constructor")
                .and_then(|property| property.value)
                .is_some_and(|value| value.to_boolean())
            {
                let name = object
                    .own_property("name")
                    .and_then(|property| property.value)
                    .map(|value| value.to_js_string())
                    .unwrap_or_default();
                return Err(Signal::Throw(
                    self.make_error("TypeError", &format!("{name} is not a constructor")),
                ));
            }
            // A generator function has no `[[Construct]]`, and the reference names it in
            // the message: `new (function* G(){})()` reports "G is not a constructor".
            if object.with(|data| match &data.kind {
                ObjectKind::Function(Callable::Bytecode { code, .. }) => {
                    code.code.kind.is_generator()
                }
                _ => false,
            }) {
                let name = object
                    .own_property("name")
                    .and_then(|property| property.value)
                    .map(|value| value.to_js_string())
                    .unwrap_or_default();
                return Err(Signal::Throw(
                    self.make_error("TypeError", &format!("{name} is not a constructor")),
                ));
            }
            let bound = object.with(|data| match &data.kind {
                ObjectKind::Function(Callable::Bound {
                    target,
                    args: leading,
                    ..
                }) => Some((target.clone(), leading.clone())),
                _ => None,
            });
            let Some((target, leading)) = bound else {
                // Not a bound function, so this object is the callee. §7.2.4
                // `IsConstructor` decides whether `new` may proceed, and the two ad-hoc
                // refusals above cover only the built-ins the reference also refuses by
                // name. Everything else — a method, an arrow, an async function — has no
                // `[[Construct]]` either, and answering "yes" here made
                // `new (Array.prototype.map)` build an object instead of throwing.
                if !object.is_constructor() {
                    let name = object
                        .own_property("name")
                        .and_then(|property| property.value)
                        .map(|value| value.to_js_string())
                        .unwrap_or_default();
                    return Err(Signal::Throw(
                        self.make_error("TypeError", &format!("{name} is not a constructor")),
                    ));
                }
                break;
            };
            let mut all = leading;
            all.extend(args);
            args = all;
            // A bound function that is its own `new.target` stands in for its target:
            // `new (C.bind(null))()` makes a `C` instance, so the object has to come
            // from `C.prototype`, not from the bound wrapper (which has no `prototype`).
            let same_wrapper = match (new_target.as_object(), callee.as_object()) {
                (Some(new_target), Some(bound)) => new_target.ptr_eq(bound),
                _ => false,
            };
            if same_wrapper {
                new_target = target.clone();
            }
            callee = target;
        }
        let Value::Object(function) = &callee else {
            unreachable!("the loop above only breaks on an object");
        };
        // The fresh object's prototype comes from `new.target`, not from the callee: a
        // derived constructor reaches here through `super()`, and the instance must be a
        // `new.target` instance, so `class D extends C { constructor(){ super() } }` makes
        // an object inheriting `D.prototype` rather than `C.prototype`.
        let prototype_source = match new_target.as_object() {
            Some(target) => target.clone(),
            None => function.clone(),
        };
        // A native constructor builds its own instance, through `new_ctor_instance`, once it
        // has validated its arguments. Pre-building one here would read
        // `new.target.prototype` too early: `Reflect.construct(Promise, [], badTarget)` has
        // to throw `TypeError` for a non-callable executor *before* a throwing `prototype`
        // getter runs, and `new DataView(detached, …)` likewise for the detached check.
        let is_native = function.native_creates_own_instance();
        let this = if is_native {
            Value::Undefined
        } else {
            let prototype = self.get_property(&prototype_source, "prototype")?;
            let prototype = match prototype {
                Some(Value::Object(proto)) => Some(proto),
                // `GetPrototypeFromConstructor`: when the requested `prototype` is not an
                // object, the answer is the realm's *intrinsic* prototype for this
                // constructor, not `%Object.prototype%`.
                _ => {
                    let fallback = self.get_property(function, "prototype")?;
                    match fallback {
                        Some(Value::Object(proto)) => Some(proto),
                        _ => Some(self.realm.object_proto.clone()),
                    }
                }
            };
            Value::Object(self.new_object(prototype))
        };
        // `new.target` inside the body is the constructor that was written, so a bound
        // function's target sees itself rather than the bound wrapper.
        self.pending_new_target = Some(new_target);
        let result = self.call_value(callee, this.clone(), args)?;
        self.pending_new_target = None;
        // A constructor that returns an object overrides `this`; any other return
        // value (including `undefined`) is ignored.
        Ok(match result {
            Value::Object(_) => result,
            _ => this,
        })
    }

    /// A `TypeError`/`ReferenceError`/`RangeError` object, as the value to throw.
    ///
    /// The error classes themselves arrive with the builtins milestone; what
    /// matters here is that the thrown value stringifies the way a script printing
    /// the caught value expects.
    pub fn make_error(&self, name: &str, message: &str) -> Value {
        let proto = self
            .realm
            .errors
            .get(name)
            .map(|constructor| self.prototype_of(constructor))
            .unwrap_or_else(|| self.realm.error_proto.clone());
        let error = self.new_object(Some(proto));
        // `name` and `message` are own properties of the instance, which is what
        // makes `e.name` cheap and `Error.prototype.name` the fallback for a plain
        // `Error`.
        error.define(
            "name",
            Property::with_attributes(self.string(name), true, false),
        );
        error.define(
            "message",
            Property::with_attributes(self.string(message), true, false),
        );
        // The internal class, which is what tells a printer this is an error rather than a
        // plain object with `name` and `message` properties. The constructor path sets it
        // (`builtins/error.rs`); without it here an engine-thrown `TypeError` printed as
        // `{ stack: … }` while `new TypeError(...)` printed as `TypeError: …`. It is
        // literally `"Error"` in both places — the *name* is read from `name`, which is why
        // a `RangeError` still prints as `RangeError`.
        self.internal_set(&error, "_class", self.string("Error"));
        // The trace is captured where the error is *made*, which is where the reference
        // captures it too: `JS_ThrowTypeError` and friends call `build_backtrace` on the
        // way out, so a caught engine error already has its stack even if the handler
        // never rethrows it.
        self.add_stack(&error);
        Value::Object(error)
    }

    /// Give an error object its `stack` property, from the current frame stack.
    ///
    /// Only objects get one — `throw 42` has no stack anywhere — and an object that
    /// already has a `stack` keeps it, so an error caught and rethrown, or one whose
    /// `stack` a script set, is not rewritten.
    pub fn add_stack(&self, object: &Gc<JsObject>) {
        self.add_stack_skipping(object, 0);
    }

    /// [`Interp::add_stack`], omitting the `skip` innermost frames.
    ///
    /// An `Error` constructor passes 1: the reference builds those traces with
    /// `JS_BACKTRACE_FLAG_SKIP_FIRST_LEVEL`, so `new Error()` does not report the
    /// constructor itself, while a `RangeError` raised inside `toISOString` does
    /// report `at toISOString (native)`.
    pub fn add_stack_skipping(&self, object: &Gc<JsObject>, skip: usize) {
        if object.has_own("stack") {
            return;
        }
        let trace = self.stack_trace_skipping(skip);
        let text = self.string(trace);
        object.define("stack", Property::with_attributes(text, true, true));
    }

    /// The function object currently being called, if any.
    ///
    /// A native function is a bare `fn` pointer, so it cannot capture which class
    /// it belongs to; it reads that from the callee instead. This is also what a
    /// stack trace would use.
    pub fn current_callee(&self) -> Option<Gc<JsObject>> {
        match &self.frames.last()?.callee {
            Value::Object(object) => Some(object.clone()),
            _ => None,
        }
    }

    /// The `prototype` property of a constructor, falling back to
    /// `Object.prototype` for anything that is not shaped like one.
    pub fn prototype_of(&self, constructor: &Gc<JsObject>) -> Gc<JsObject> {
        constructor
            .own_property("prototype")
            .and_then(|property| property.value)
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_else(|| self.realm.object_proto.clone())
    }

    /// Throw a `TypeError` carrying `message`.
    pub fn type_error(&self, message: impl AsRef<str>) -> Signal {
        Signal::Throw(self.make_error("TypeError", message.as_ref()))
    }

    /// Throw a `RangeError` carrying `message`.
    pub fn range_error(&self, message: impl AsRef<str>) -> Signal {
        Signal::Throw(self.make_error("RangeError", message.as_ref()))
    }

    /// The `ReferenceError` a read of an uninitialised binding raises.
    /// The name of the binding `slot` refers to, for an error message.
    fn binding_name(&self, slot: usize, kind: VarSlot) -> String {
        match kind {
            VarSlot::Loc => self
                .frame()
                .code
                .code
                .vars
                .get(slot)
                .map(|v| v.name.clone()),
            VarSlot::Closure => self
                .frame()
                .code
                .code
                .closure_vars
                .get(slot)
                .map(|c| c.name.clone()),
        }
        .unwrap_or_default()
    }

    /// Whether `slot` names a `const` binding, which rejects every store after its
    /// initialiser.
    fn is_read_only(&self, slot: usize, kind: VarSlot) -> bool {
        match kind {
            VarSlot::Loc => self
                .frame()
                .code
                .code
                .vars
                .get(slot)
                .is_some_and(|v| v.is_const),
            VarSlot::Closure => self
                .frame()
                .code
                .code
                .closure_vars
                .get(slot)
                .is_some_and(|c| c.is_const),
        }
    }

    /// The `TypeError` a store to a `const` binding raises.
    ///
    /// `const` is enforced here rather than at compile time because the binding can be
    /// assigned from a place the compiler cannot classify — `c = 2` inside a closure
    /// is just a `put_var_ref_check`. `PutLocCheck`/`PutVarRefCheck` therefore check
    /// both halves of what "checked store" means: the dead zone is over, and the
    /// binding is writable.
    fn read_only(&self, slot: usize, kind: VarSlot) -> Signal {
        let name = self.binding_name(slot, kind);
        Signal::Throw(self.make_error("TypeError", &format!("'{name}' is read-only")))
    }

    fn dead_zone(&self, slot: usize, kind: VarSlot) -> Signal {
        let name = match kind {
            VarSlot::Loc => self
                .frame()
                .code
                .code
                .vars
                .get(slot)
                .map(|v| v.name.clone()),
            VarSlot::Closure => self
                .frame()
                .code
                .code
                .closure_vars
                .get(slot)
                .map(|c| c.name.clone()),
        }
        .unwrap_or_default();
        Signal::Throw(self.make_error(
            "ReferenceError",
            &format!("cannot access '{name}' before initialization"),
        ))
    }

    fn throw_reference(&self, name: &str) -> Signal {
        Signal::Throw(self.make_error("ReferenceError", &format!("{name} is not defined")))
    }

    // -- the instruction loop -----------------------------------------------

    fn step(&mut self) -> JsResult<Step> {
        let pc = self.frame().pc;
        let code = self.frame().code.clone();
        let bytes = &code.code.code;
        if pc >= bytes.len() {
            return Ok(Step::Return(Value::Undefined));
        }
        let Some(decoded) = decode(bytes, pc) else {
            return Err(Signal::Throw(
                self.make_error("InternalError", "undecodable bytecode"),
            ));
        };
        let (op, a, b, next) = decoded;
        self.frame_mut().at = pc;
        self.frame_mut().pc = next;
        self.execute_instruction(op, a, b, next)
    }

    fn push(&mut self, value: Value) {
        self.frame_mut().stack.push(value);
    }

    fn pop(&mut self) -> Value {
        self.frame_mut().stack.pop().unwrap_or(Value::Undefined)
    }

    fn pop_n(&mut self, count: usize) -> Vec<Value> {
        let frame = self.frame_mut();
        let start = frame.stack.len().saturating_sub(count);
        frame.stack.split_off(start)
    }

    fn peek(&self, back: usize) -> Value {
        let frame = self.frame();
        frame
            .stack
            .get(frame.stack.len().wrapping_sub(1 + back))
            .cloned()
            .unwrap_or(Value::Undefined)
    }

    #[allow(clippy::too_many_lines)]
    fn execute_instruction(&mut self, op: Opcode, a: u32, b: u32, next: usize) -> JsResult<Step> {
        use Opcode::*;
        match op {
            // -- constants and stack shuffling -----------------------------
            Nop => {}
            PushMinus1 => self.push(Value::Int(-1)),
            Push0 | Push1 | Push2 | Push3 | Push4 | Push5 | Push6 | Push7 => {
                let value = match op {
                    Push0 => 0,
                    Push1 => 1,
                    Push2 => 2,
                    Push3 => 3,
                    Push4 => 4,
                    Push5 => 5,
                    Push6 => 6,
                    _ => 7,
                };
                self.push(Value::Int(value));
            }
            PushI8 | PushI16 | PushI32 => self.push(Value::Int(a as i32)),
            PushBigintI32 => {
                // The immediate form of a `BigInt` literal; the reference has the same
                // split between this and a constant-pool entry.
                let value = crate::bigint::BigInt::from_i64(a as i64);
                self.push(self.bigint(value));
            }
            PushTrue => self.push(Value::Bool(true)),
            PushFalse => self.push(Value::Bool(false)),
            PushEmptyString => self.push(self.string("")),
            Undefined => self.push(Value::Undefined),
            Null => self.push(Value::Null),
            Object => {
                let proto = self.realm.object_proto.clone();
                self.push(Value::Object(self.new_object(Some(proto))));
            }
            PushThis => {
                let this = self.frame().this.clone();
                self.push(this);
            }
            PushConst | PushConst8 => {
                let value = self.constant(a as usize)?;
                self.push(value);
            }
            PushAtomValue => {
                let text = self.atom_text(a as usize);
                self.push(self.string(text));
            }
            Dup => {
                let value = self.peek(0);
                self.push(value);
            }
            Swap => {
                // `a b -> b a`
                let bottom = self.pop();
                let top = self.pop();
                self.push(bottom);
                self.push(top);
            }
            Perm3 => {
                // `a b c -> b a c`, the reference's reordering for a spread method
                // call: `obj func array` becomes the `func this array` that `apply`
                // takes.
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push(b);
                self.push(a);
                self.push(c);
            }
            Dup1 => {
                let value = self.peek(1);
                self.push(value);
            }
            Drop => {
                self.pop();
            }
            Nip => {
                // `a b -> b`
                let top = self.pop();
                self.pop();
                self.push(top);
            }
            Perm4 => {
                // `obj prop a b -> a obj prop b`, the reference's reordering for a
                // postfix update of a computed element: it puts the *old* value
                // underneath the receiver and key so `put_array_el` can consume them
                // and leave the old value as the expression's result.
                let d = self.pop();
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push(c);
                self.push(a);
                self.push(b);
                self.push(d);
            }
            Insert2 => {
                // `obj a -> a obj a`
                let value = self.pop();
                let object = self.pop();
                self.push(value.clone());
                self.push(object);
                self.push(value);
            }
            Insert3 => {
                // `obj prop a -> a obj prop a`
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                self.push(value.clone());
                self.push(object);
                self.push(key);
                self.push(value);
            }
            Swap2 => {
                // `a b c d -> c d a b`
                let d = self.pop();
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push(c);
                self.push(d);
                self.push(a);
                self.push(b);
            }
            Rot3l => {
                // `x a b -> a b x`
                let b = self.pop();
                let a = self.pop();
                let x = self.pop();
                self.push(a);
                self.push(b);
                self.push(x);
            }
            Rot3r => {
                // `a b c -> c a b`
                let c = self.pop();
                let b = self.pop();
                let a_value = self.pop();
                self.push(c);
                self.push(a_value);
                self.push(b);
            }

            Rest => {
                // `...rest` in a parameter list: the arguments from index `a` on, as a
                // real array. It takes nothing off the stack — the arguments are in the
                // frame, not on it.
                let frame = self.frame();
                let from = a as usize;
                let collected: Vec<Value> = frame.passed.iter().skip(from).cloned().collect();
                let array = self.new_array(collected);
                self.push(Value::Object(array));
            }

            // -- slots ------------------------------------------------------
            GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.get_loc(slot);
                self.push(value);
            }
            PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_loc(slot, value);
            }
            SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_loc(slot, value.clone());
                self.push(value);
            }
            // The `_check` forms are the temporal dead zone: they throw when the
            // slot still holds [`Value::Uninitialized`]. Only `get_loc_check`
            // *reads*, so only the two writes may pop — mishandling that quietly
            // consumes the value the caller pushed just before.
            GetLocCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.get_loc(slot);
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Loc));
                }
                self.push(value);
            }
            PutLocCheck | SetLocCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Loc));
                }
                // The initialiser goes through `put_loc`, not this, so any store that
                // reaches here is a later assignment — which a `const` refuses.
                if self.is_read_only(slot, VarSlot::Loc) {
                    return Err(self.read_only(slot, VarSlot::Loc));
                }
                if op == SetLocCheck {
                    self.put_loc(slot, value.clone());
                    self.push(value);
                } else {
                    self.put_loc(slot, value);
                }
            }
            PutLocCheckInit => {
                // The one store that *initialises* a lexical slot: `this` in a derived
                // constructor, which only exists after `super()` has run. A second
                // initialisation is the error the reference reports as
                // "'this' can be initialized only once".
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                if !matches!(self.get_loc(slot), Value::Uninitialized) {
                    return Err(Signal::Throw(self.make_error(
                        "ReferenceError",
                        "'this' can be initialized only once",
                    )));
                }
                self.put_loc(slot, value);
            }
            CheckCtor => {
                // A class constructor may only run under `new`, which is what makes
                // `class A {}; A()` a TypeError.
                if matches!(self.frame().new_target, Value::Undefined) {
                    return Err(Signal::Throw(self.make_error(
                        "TypeError",
                        "class constructors must be invoked with 'new'",
                    )));
                }
            }
            InitCtor => {
                // The body a derived class gets when it declares no constructor:
                // forward every argument to the parent constructor, keeping `new.target`.
                if matches!(self.frame().new_target, Value::Undefined) {
                    return Err(Signal::Throw(self.make_error(
                        "TypeError",
                        "class constructors must be invoked with 'new'",
                    )));
                }
                let Some(callee) = self.current_callee() else {
                    return Err(Signal::Throw(
                        self.make_error("TypeError", "no constructor to inherit from"),
                    ));
                };
                let parent = callee.proto().map_or(Value::Null, Value::Object);
                let new_target = self.frame().new_target.clone();
                let args = self.frame().passed.clone();
                let result = self.construct(parent, new_target, args)?;
                self.push(result);
            }
            SetLocUninitialized => {
                let slot = self.slot_operand(op, a);
                self.put_loc(slot, Value::Uninitialized);
            }
            CloseLoc => {
                // "Close" a binding: give it a *fresh* cell holding the current
                // value. Closures created before this point keep the old cell and the
                // ones created after see the new one, which is what makes every
                // iteration of `for (let i ...)` capture its own `i`, and what freezes
                // a block-scoped binding when the block ends.
                //
                // Clearing the slot instead (which this used to do) broke both: a
                // closure over a captured block variable read `undefined`, and a loop
                // terminated after one iteration.
                let slot = self.slot_operand(op, a);
                let index = self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1);
                if index >= 0 {
                    let current = self.frame().cells[index as usize].get();
                    let fresh = self.new_cell(current);
                    if let Some(cell) = self.frame_mut().cells.get_mut(index as usize) {
                        *cell = fresh;
                    }
                }
            }
            GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.get_arg(slot);
                self.push(value);
            }
            PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_arg(slot, value);
            }
            SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_arg(slot, value.clone());
                self.push(value);
            }

            // -- closure variables ------------------------------------------
            GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 => {
                let slot = self.slot_operand(op, a);
                // A name a direct `eval` created shadows the binding the compiler chose.
                // `function t(){ var c = "outer"; function f(a = eval("var c = 1")) { } }`
                // leaves `t`'s `c` alone but makes every read inside `f` see the eval's,
                // and only a runtime lookup can know that.
                if let Some(cell) = self.eval_created_local(slot) {
                    self.push(cell.get());
                    return self.step();
                }
                let value = self.cell_value(slot)?;
                self.push(value);
            }
            GetVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.cell_value(slot)?;
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Closure));
                }
                self.push(value);
            }
            PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 | PutVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                if let Some(cell) = self.eval_created_local(slot) {
                    cell.set(value);
                    return self.step();
                }
                // Same rule as `PutLocCheck`: a checked store to a captured binding
                // refuses a `const` once its initialiser has run.
                if op == PutVarRefCheck && self.is_read_only(slot, VarSlot::Closure) {
                    return Err(self.read_only(slot, VarSlot::Closure));
                }
                self.set_cell(slot, value)?;
            }
            SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                if let Some(cell) = self.eval_created_local(slot) {
                    cell.set(value.clone());
                    self.push(value);
                    return self.step();
                }
                self.set_cell(slot, value.clone())?;
                self.push(value);
            }

            // -- globals ----------------------------------------------------
            GetVar => {
                let value = self.get_global(a as usize)?;
                self.push(value);
            }
            GetVarUndef => {
                let value = self
                    .get_global(a as usize)
                    .or_else(|_| Ok(Value::Undefined))?;
                self.push(value);
            }
            DeleteVar => {
                // `delete name` for a name that is not a declared binding. A lexical
                // global lives in the reference table and cannot be deleted; anything
                // reachable only as a property of the global object is *implicit* —
                // `x = 1` with no declaration — and goes away if it is configurable,
                // which is what `delete` on an implicit global answers.
                let name = self.atom_text(a as usize);
                let global = self.realm.global.clone();
                let deleted = if self.realm.globals.contains_key(&name) {
                    false
                } else if global.own_property(&name).is_some() {
                    self.delete_property(&global, &name)?
                } else {
                    true
                };
                self.push(Value::Bool(deleted));
            }
            PutVar => {
                let value = self.pop();
                self.put_global(a as usize, value)?;
            }
            PutVarInit => {
                let slot = a as usize;
                let value = self.pop();
                self.init_global(slot, value)?;
            }

            // -- arithmetic -------------------------------------------------
            Add => self.binary_add()?,
            Sub | Mul | Div | Mod | Pow | And | Or | Xor | Shl | Sar | Shr => {
                let raw_right = self.pop();
                let raw_left = self.pop();
                // BigInt arithmetic is a separate path, and not only for the type of the
                // result: `ToNumber` of a bigint throws, so mixing the two must be an error
                // rather than a silent conversion.
                if matches!(raw_left, Value::BigInt(_)) || matches!(raw_right, Value::BigInt(_)) {
                    let left = self.primitive_of(raw_left)?;
                    let right = self.primitive_of(raw_right)?;
                    let value = self.bigint_arithmetic(op, left, right)?;
                    self.push(value);
                } else {
                    let right = self.numeric(raw_right)?;
                    let left = self.numeric(raw_left)?;
                    self.push(arithmetic(op, left, right)?);
                }
            }
            Neg => {
                let raw = self.pop();
                match raw {
                    // A bigint negates exactly, however wide it is.
                    Value::BigInt(big) => {
                        let negated = big.negate();
                        self.push(self.bigint(negated));
                    }
                    other => {
                        let value = self.numeric(other)?;
                        self.push(negate(value));
                    }
                }
            }
            Plus => {
                let value = self.pop();
                // `+x` is `ToNumber`, and `ToNumber` refuses a bigint: unlike `++`,
                // unary plus is defined to produce a *number*, which a bigint cannot
                // become without loss. It used to fall through to `NaN`.
                if matches!(value, Value::BigInt(_)) {
                    return Err(self.type_error("cannot convert bigint to number"));
                }
                let numeric = self.numeric_of(value)?;
                self.push(numeric);
            }
            Inc => {
                let raw = self.pop();
                let (_, updated) = self.advance_numeric(raw, true)?;
                self.push(updated);
            }
            Dec => {
                let raw = self.pop();
                let (_, updated) = self.advance_numeric(raw, false)?;
                self.push(updated);
            }
            PostInc | PostDec => {
                // `x -> x x+1`: the *new* value ends up on top, so the store the
                // compiler emits right after (`put_var`) writes the incremented
                // one, while the old value stays underneath for the expression's
                // own result. Getting this backwards makes `i++` a no-op, which
                // turns `while (i < 3)` into an infinite loop.
                let raw = self.pop();
                let (old, updated) = self.advance_numeric(raw, op == PostInc)?;
                self.push(old);
                self.push(updated);
            }
            Not => {
                let raw = self.pop();
                match raw {
                    // `~x` on a bigint is `-x - 1`, exact at any width.
                    Value::BigInt(big) => {
                        let inverted = big.bitnot();
                        self.push(self.bigint(inverted));
                    }
                    other => {
                        let value = self.numeric(other)?;
                        self.push(Value::Int(!value.to_int32()));
                    }
                }
            }
            Lnot => {
                let value = self.pop();
                self.push(Value::Bool(!value.to_boolean()));
            }

            // -- comparison -------------------------------------------------
            Lt | Lte | Gt | Gte => {
                let right = self.pop();
                let left = self.pop();
                let answer = self.relational(op, left, right);
                self.push(answer);
            }
            Eq | Neq => {
                let right = self.pop();
                let left = self.pop();
                let equal = self.abstract_equals(left, right)?;
                self.push(Value::Bool(if op == Eq { equal } else { !equal }));
            }
            StrictEq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(left.strict_equals(&right)));
            }
            StrictNeq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(!left.strict_equals(&right)));
            }
            IsUndefined => {
                // `=== undefined`, which is what a destructuring default tests. It is
                // not `IsUndefinedOrNull`: `[a = 1] = [null]` keeps `null`.
                let value = self.pop();
                self.push(Value::Bool(matches!(value, Value::Undefined)));
            }
            IsUndefinedOrNull => {
                let value = self.pop();
                self.push(Value::Bool(value.is_nullish()));
            }
            Typeof => {
                let value = self.pop();
                self.push(self.string(value.type_of()));
            }
            ToObject => {
                // A destructuring *object* pattern starts here, which is why
                // `var {} = null` throws. Only `undefined` and `null` are refused; a
                // primitive is boxed.
                let value = self.pop();
                let object = self.object_from(&value)?;
                self.push(Value::Object(object));
            }
            Instanceof => {
                let constructor = self.pop();
                let object = self.pop();
                let answer = self.instance_of(&object, &constructor)?;
                self.push(Value::Bool(answer));
            }
            In => {
                let object = self.pop();
                let key = self.pop();
                let answer = self.has_property(&key, &object)?;
                self.push(Value::Bool(answer));
            }

            // -- control flow -----------------------------------------------
            Goto | Goto8 | Goto16 => {
                self.frame_mut().pc = a as usize;
            }
            IfTrue | IfTrue8 => {
                let value = self.pop();
                if value.to_boolean() {
                    self.frame_mut().pc = a as usize;
                }
            }
            IfFalse | IfFalse8 => {
                let value = self.pop();
                if !value.to_boolean() {
                    self.frame_mut().pc = a as usize;
                }
            }
            Catch => {
                // Pushes the handler target onto the value stack; `unwind` finds it.
                self.push(Value::CatchOffset(a));
            }
            Gosub => {
                self.push(Value::ReturnAddress(next as u32));
                self.frame_mut().pc = a as usize;
            }
            Ret => {
                let value = self.pop();
                match value {
                    Value::ReturnAddress(address) => self.frame_mut().pc = address as usize,
                    _ => {
                        return Err(Signal::Throw(
                            self.make_error("InternalError", "invalid return address"),
                        ));
                    }
                }
            }
            NipCatch => {
                // `catch_offset ... ret_val -> ret_val`: drop everything down to and
                // including the marker, keeping the top value.
                let top = self.pop();
                loop {
                    match self.pop() {
                        Value::CatchOffset(_) | Value::Undefined => break,
                        _ => {}
                    }
                    if self.frame().stack.is_empty() {
                        break;
                    }
                }
                self.push(top);
            }
            Throw => {
                let value = self.pop();
                return Err(Signal::Throw(value));
            }
            ThrowError => {
                let name = self.atom_text(a as usize);
                let (class, message) = match b {
                    0 => ("TypeError", format!("'{name}' is read-only")),
                    1 => ("SyntaxError", format!("redeclaration of '{name}'")),
                    2 => ("ReferenceError", format!("'{name}' is not initialized")),
                    // `delete super.x` is a syntax-level mistake the reference reports
                    // with this wording, not a property-access failure.
                    3 => (
                        "ReferenceError",
                        "unsupported reference to 'super'".to_string(),
                    ),
                    4 => (
                        "TypeError",
                        "iterator does not have a throw method".to_string(),
                    ),
                    5 => (
                        "ReferenceError",
                        "invalid assignment left-hand side".to_string(),
                    ),
                    _ => ("TypeError", format!("invalid operation on '{name}'")),
                };
                return Err(Signal::Throw(self.make_error(class, &message)));
            }
            InitialYield => return Ok(Step::Suspend(SuspendKind::Initial)),
            Yield => return Ok(Step::Suspend(SuspendKind::Yield)),
            YieldStar | AsyncYieldStar => return Ok(Step::Suspend(SuspendKind::YieldStar)),
            Await => return Ok(Step::Suspend(SuspendKind::Await)),
            ReturnAsync => {
                // The reference's `return_async` returns `undefined` and lets the resume
                // machinery pick the real value off `sp[-1]`, which is how it tells a
                // completion from a suspension. Carrying the value explicitly says the
                // same thing without the implicit stack slot.
                let value = self.pop();
                return Ok(Step::Return(value));
            }
            Return => {
                let value = self.pop();
                return Ok(Step::Return(value));
            }
            ReturnUndef => return Ok(Step::Return(Value::Undefined)),

            // -- calls ------------------------------------------------------
            Call | Call0 | Call1 | Call2 | Call3 => {
                let argc = self.argc_operand(op, a);
                let args = self.pop_n(argc);
                let callee = self.pop();
                // A plain call has no receiver; the callee decides what to do with
                // `undefined` (a sloppy function substitutes the global object).
                if let Some(result) = self.call_or_push(callee, Value::Undefined, args)? {
                    self.push(result);
                }
            }
            CallConstructor => {
                // `constructor this_obj args… -> object`. The duplicated
                // constructor is `new.target`; this milestone does not expose it,
                // so the extra copy is simply consumed here.
                let argc = a as usize;
                let args = self.pop_n(argc);
                let new_target = self.pop();
                let callee = self.pop();
                let result = self.construct(callee, new_target, args)?;
                self.push(result);
            }
            CallMethod => {
                let argc = a as usize;
                let args = self.pop_n(argc);
                let callee = self.pop();
                let this = self.pop();
                if let Some(result) = self.call_or_push(callee, this, args)? {
                    self.push(result);
                }
            }
            Apply => {
                // `func this array -> result`, the spread-call form: the compiler
                // builds one array of arguments, and all that is left here is to
                // unpack it. Magic 1 is `new` (`apply 1`), which constructs.
                let array = self.pop();
                let this = self.pop();
                let callee = self.pop();
                let args = self.array_like_to_args(&array)?;
                if a & 1 != 0 {
                    let result = self.construct(callee, this, args)?;
                    self.push(result);
                } else if let Some(result) = self.call_or_push(callee, this, args)? {
                    self.push(result);
                }
            }
            TailCall | TailCallMethod => {
                // `tail_call` is `call` whose result is returned immediately — the
                // reference jumps straight to `done` instead of storing the value and
                // running on. The callee still gets its own frame, so deep tail
                // recursion hits the stack limit exactly like plain recursion does;
                // marking this frame lets the return path unwind without needing
                // instructions after the call.
                let argc = if op == TailCallMethod {
                    a as usize
                } else {
                    self.argc_operand(op, a)
                };
                let args = self.pop_n(argc);
                let callee = self.pop();
                let this = if op == TailCallMethod {
                    self.pop()
                } else {
                    Value::Undefined
                };
                if let Value::Object(function) = &callee
                    && let Some((code, env)) = bytecode_of(function)
                {
                    // A generator or async body is not run here at all: calling it only
                    // *creates* the generator/coroutine, so it must not inherit this
                    // frame. Doing so made `function f(){ return g() }` push `g`'s body
                    // onto `f`'s frame, and the first `yield`/`await` then reported
                    // "a generator body ran as an ordinary call".
                    if code.code.kind.is_generator() || code.code.kind.is_async() {
                        let value = self.call_value(callee, this, args)?;
                        return Ok(Step::Return(value));
                    }
                    self.frame_mut().tail = true;
                    self.push_frame(code, env, this, args, callee)?;
                    return Ok(Step::Continue);
                }
                // A native callee has no frame to reuse, so its value is simply
                // returned.
                let value = self.call_value(callee, this, args)?;
                return Ok(Step::Return(value));
            }

            // -- objects ----------------------------------------------------
            SpecialObject => {
                let value = self.special_object(a)?;
                self.push(value);
            }
            GetLength => {
                // `get_length` is the `.length` read; an array and a string answer
                // from their own length, anything else from the property.
                let target = self.pop();
                let value = match &target {
                    Value::Object(object) => match object.array_length() {
                        Some(length) => crate::object::length_value(length),
                        None => self.read_field(&target, "length")?,
                    },
                    Value::String(text) => Value::Int(text.utf16_len() as i32),
                    _ => self.read_field(&target, "length")?,
                };
                self.push(value);
            }
            GetField | GetField2 => {
                let object = self.pop();
                let key = self.atom_text(a as usize);
                let value = self.read_field(&object, &key)?;
                if op == GetField2 {
                    self.push(object);
                }
                self.push(value);
            }
            GetArrayEl | GetArrayEl2 | GetArrayEl3 => {
                let key = self.pop();
                let object = self.pop();
                let key = self.propkey(&key);
                let value = self.read_field_key(&object, &key)?;
                if matches!(op, GetArrayEl2 | GetArrayEl3) {
                    self.push(object);
                }
                if op == GetArrayEl3 {
                    // The key goes back in its *value* form, which the compiler stores
                    // into the target of a destructuring assignment.
                    self.push(match &key {
                        PropertyKey::Symbol(symbol) => Value::Symbol(symbol.clone()),
                        PropertyKey::String(text) => self.string_wtf8(text.clone()),
                    });
                }
                self.push(value);
            }
            PutField => {
                let value = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                self.write_field(&object, &key, value)?;
            }
            PutArrayEl => {
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                let key = self.propkey(&key);
                self.write_field_key(&object, &key, value)?;
            }
            DefineField => {
                let value = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                // `{m: function(){}}` names the function `m`, the same way assigning
                // it to a property does. Without this the frame is anonymous and a
                // stack trace reads `at  (file:1:2)`.
                self.set_function_name(&value, &key);
                if let Some(target) = object.as_object() {
                    // Only an *array* stores into the element vector. A numeric key
                    // on an ordinary object is an ordinary property, which is what
                    // makes `{1: 'a'}` iterate as a key rather than disappear.
                    let stored = target.is_array()
                        && array_index(&key)
                            .is_some_and(|index| target.array_set(index as usize, value.clone()));
                    if !stored {
                        target.define_data(&key, value);
                    }
                }
                self.push(object);
            }
            DefineArrayEl => {
                // `obj key value -> obj key`. The *value* form of the key is what goes
                // back on the stack — the compiler stores it into the pattern's binding —
                // so the symbol is not stringified on the way through.
                let value = self.pop();
                let key_value = self.pop();
                let object = self.peek(0);
                let key = self.propkey(&key_value);
                self.write_field_key(&object, &key, value)?;
                self.push(key_value);
            }
            SetHomeObject => {
                // `obj func -> obj func`: records the object a method was defined on so
                // `super` inside it resolves against that object's prototype.
                let function = self.peek(0);
                let home = self.peek(1);
                if let (Value::Object(function), Value::Object(home)) = (&function, &home) {
                    function.set_home_object(Some(home.clone()));
                }
            }
            Regexp => {
                // `pattern flags -> regexp`. A literal compiles here rather than through the
                // constructor, which is both what the reference's dedicated opcode is for
                // and what keeps `/a/g.source` from being affected by a patched `RegExp`.
                let flags = self.pop();
                let pattern = self.pop();
                let pattern = self.to_js_string(&pattern)?;
                let flags = self.to_js_string(&flags)?;
                let object = crate::builtins::regexp::new_regexp(self, &pattern, &flags)?;
                self.push(Value::Object(object));
            }
            UsingDisposeInit => {
                // The error state a disposal accumulates into. Uninitialised means "no
                // failure yet"; anything else is the exception being propagated.
                self.push(Value::Uninitialized);
            }
            UsingCheck => {
                // `value -> value, method`: validate and snapshot the disposer now, so a
                // later change to `Symbol.dispose` cannot change what gets called, and so a
                // value that is not disposable fails before the body runs.
                let value = self.pop();
                let method = self.using_method_for(&value, a as u8 == 1)?;
                self.push(value);
                self.push(method);
            }
            UsingDispose => {
                // `error_state -> error_state'`: dispose the variable at `slot` — its value
                // is `slot`, its snapshotted method `slot + 1` — folding any failure into
                // the pending error.
                let slot = self.slot_operand(op, a);
                let error_state = self.pop();
                let merged = self.using_dispose_once(slot, error_state)?;
                self.push(merged);
            }
            UsingDisposeMerge => {
                let new_error = self.pop();
                let error_state = self.pop();
                let merged = self.merge_dispose_error(new_error, error_state);
                self.push(merged);
            }
            UsingDisposeEnd => {
                // A disposal that failed replaces the block's normal completion, so the
                // error state becomes a real throw here rather than at the call site.
                let error_state = self.pop();
                if !matches!(error_state, Value::Uninitialized) {
                    return Err(Signal::Throw(error_state));
                }
            }
            UsingDisposeAsync => {
                // `await using` needs a promise to await and this runtime has no
                // `Promise` builtin, so the disposer is called synchronously and its result
                // discarded. That is correct for a disposer that returns nothing and is
                // reported as a gap rather than silently skipped.
                let slot = self.slot_operand(op, a);
                let error_state = self.using_dispose_once(slot, Value::Uninitialized)?;
                self.push(error_state);
            }
            GetSuper => {
                // `obj -> obj's prototype`: the starting point of a `super` lookup.
                let object = self.pop();
                let proto = match &object {
                    Value::Object(object) => object.proto().map_or(Value::Null, Value::Object),
                    _ => Value::Null,
                };
                self.push(proto);
            }
            GetSuperValue => {
                // `receiver proto key -> value`, searching from `proto` but reporting to
                // `receiver`, which is what `super.g` must do for a getter to see `this`.
                let key = self.pop();
                let proto = self.pop();
                let receiver = self.peek(0);
                let value = self.super_read(&receiver, &proto, &key)?;
                self.push(value);
            }
            PutSuperValue => {
                // `receiver proto key value ->`: a store that starts at `proto` but
                // assigns to `receiver`.
                let value = self.pop();
                let key = self.pop();
                let proto = self.pop();
                let receiver = self.pop();
                self.super_write(&receiver, &proto, &key, value)?;
            }
            DefineClass => {
                // `parent ctor -> ctor proto`: the constructor becomes the class and a
                // fresh prototype object lands on top for the methods to go on.
                let name = self.atom_text(a as usize);
                let (ctor, proto) = self.define_class(&name, b)?;
                self.push(ctor);
                self.push(Value::Object(proto));
            }
            DefineClassComputed => {
                // `parent ctor key -> ctor proto key`: the computed name stays put so
                // `define_field`-style consumers can still read it.
                let key = self.pop();
                let name = self.propkey_of(&key).to_string_lossy().into_owned();
                let (ctor, proto) = self.define_class(&name, b)?;
                self.push(ctor);
                self.push(Value::Object(proto));
                self.push(key);
            }
            DefineMethod => {
                let function = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                // A method's `[[HomeObject]]` is the object it was defined on, which is
                // what `super` inside it resolves against.
                if let (Value::Object(function), Value::Object(object)) = (&function, &object) {
                    function.set_home_object(Some(object.clone()));
                }
                self.set_function_name(&function, &key_with_accessor_prefix(&key, b));
                if let Some(array) = object.as_object() {
                    define_function_property(array, &PropertyKey::from(key.as_str()), function, b);
                }
                self.push(object);
            }
            DefineMethodComputed => {
                let function = self.pop();
                let key = self.pop();
                let object = self.pop();
                let key = self.propkey(&key);
                self.set_function_name(&function, &key_with_accessor_prefix(&key.to_string(), a));
                if let Some(array) = object.as_object() {
                    define_function_property(array, &key, function, a);
                }
                self.push(object);
            }
            SetProto => {
                let proto = self.pop();
                let object = self.pop();
                if let Some(target) = object.as_object() {
                    target.set_proto(proto.as_object().cloned());
                }
                self.push(object);
            }
            CopyDataProperties => {
                let exclusion = self.pop();
                let source = self.pop();
                let target = self.peek(0);
                self.copy_properties(&target, &source, &exclusion)?;
                self.push(source);
                self.push(exclusion);
            }
            ToPropkey => {
                // A symbol survives unchanged: this is `ToPropertyKey`, not `ToString`,
                // and stringifying here would turn `{[Symbol("b")]: 2}` into an
                // ordinary `"Symbol(b)"` property.
                let value = self.pop();
                let key = match self.propkey(&value) {
                    PropertyKey::Symbol(symbol) => Value::Symbol(symbol),
                    PropertyKey::String(text) => self.string_wtf8(text),
                };
                self.push(key);
            }
            SetName => {
                let function = self.peek(0);
                let name = self.atom_text(a as usize);
                self.set_function_name(&function, &name);
            }
            SetNameComputed => {
                let key = self.peek(1);
                let function = self.peek(0);
                let name = self.propkey_of(&key).to_string_lossy().into_owned();
                self.set_function_name(&function, &name);
            }

            // -- arrays -----------------------------------------------------
            ArrayFrom => {
                let count = a as usize;
                let elements = self.pop_n(count);
                let array = self.new_array(elements);
                self.push(Value::Object(array));
            }
            Append => {
                // `array index iterable -> array new_index`
                let iterable = self.pop();
                self.pop();
                let array = self.peek(0);
                let added = self.append_enumerate(&array, &iterable)?;
                self.push(Value::Int(added));
            }

            // -- iteration --------------------------------------------------
            ForInStart => {
                let value = self.pop();
                let record = self.for_in_record(&value)?;
                self.push(Value::Object(record));
            }
            ForInNext => {
                // `record -> record key done`. The flag is *done*, not "found":
                // the compiler tests it with `if_false` to jump back into the body,
                // so reporting `true` while keys remain would end the loop at once.
                let record = self.peek(0);
                match self.for_in_advance(&record)? {
                    Some(key) => {
                        let text = self.string(key);
                        self.push(text);
                        self.push(Value::Bool(false));
                    }
                    None => {
                        self.push(Value::Undefined);
                        self.push(Value::Bool(true));
                    }
                }
            }
            ForOfStart => {
                let value = self.pop();
                let iterator = self.get_iterator(&value)?;
                self.push(iterator.object);
                self.push(iterator.next);
                self.push(Value::CatchOffset(0));
            }
            ForOfNext => {
                // `iter next catch_offset -> iter next catch_offset value done`.
                //
                // The operand is how many values sit *above* the iterator triple: a
                // destructuring rest element keeps its target array and running index
                // there while draining the iterator, so the triple is not at the top.
                let extra = a as usize;
                let iterator_slot = 2 + extra;
                // An exhausted iterator is replaced by `undefined` (see below), so a
                // second step must not call `next` again — `Array.from` and a
                // destructuring pattern both keep stepping past the end.
                let (value, done) = if matches!(self.peek(iterator_slot), Value::Undefined) {
                    (Value::Undefined, true)
                } else {
                    let iterator = Iterator {
                        object: self.peek(iterator_slot),
                        next: self.peek(1 + extra),
                    };
                    self.iterator_step(&iterator)?
                };
                if done {
                    // The result's `value` is meaningless once the iterator is done:
                    // `function* g(){ return 0 } var [x] = g()` leaves `x` undefined
                    // rather than binding the generator's return value.
                    let top = self.frame().stack.len() - 1 - iterator_slot;
                    self.frame_mut().stack[top] = Value::Undefined;
                    self.push(Value::Undefined);
                    self.push(Value::Bool(true));
                } else {
                    self.push(value);
                    self.push(Value::Bool(false));
                }
            }
            IteratorNext => {
                // `iter next catch val -> iter next catch val'`: replace the argument with
                // the result of `next(val)`. The triple below stays put, which is why the
                // three instructions of the `yield*` loop all address the stack the same way.
                let value = self.pop();
                let object = self.peek(2);
                let next = self.peek(1);
                let result = self.call_value(next, object, vec![value])?;
                self.push(result);
            }
            IteratorCall => {
                // `iter next catch val -> iter next catch val' flag`.
                //
                // Bit 0 picks `.throw` over `.return`, bit 1 calls it with no argument.
                // The flag is `true` when the method was missing, which is how the
                // caller learns that the inner iterator cannot take part in the
                // unwinding.
                let value = self.pop();
                let object = self.peek(2);
                let key = if a & 1 != 0 { "throw" } else { "return" };
                let method = self.read_field(&object, key)?;
                if matches!(method, Value::Undefined | Value::Null) {
                    self.push(value);
                    self.push(Value::Bool(true));
                } else {
                    let argument = if a & 2 != 0 { Vec::new() } else { vec![value] };
                    let result = self.call_value(method, object, argument)?;
                    self.push(result);
                    self.push(Value::Bool(false));
                }
            }
            IteratorCheckObject => {
                if !matches!(self.peek(0), Value::Object(_)) {
                    return Err(Signal::Throw(
                        self.make_error("TypeError", "iterator must return an object"),
                    ));
                }
            }
            IteratorClose => {
                // `iter next catch_offset ->`
                self.pop();
                self.pop();
                self.pop();
            }
            Import => {
                // `import(specifier)`: the options slot is on top and is not read yet.
                let _options = self.pop();
                let specifier = self.pop();
                let promise = self.dynamic_import(specifier)?;
                self.push(promise);
            }
            ForAwaitOfStart => {
                // Same triple as `for of`, but from `Symbol.asyncIterator` with a
                // `Symbol.iterator` fallback.
                let value = self.pop();
                let iterator = self.get_async_iterator(&value)?;
                self.push(iterator.object);
                self.push(iterator.next);
                self.push(Value::CatchOffset(0));
            }
            ForAwaitOfNext => {
                // `for await` steps the iterator and leaves the *result* wrapped in a
                // promise, so the `await` the compiler emits right after this can suspend
                // on it. Unwrapping happens in `iterator_get_value_done`.
                let extra = a as usize;
                let iterator_slot = 2 + extra;
                let result = if matches!(self.peek(iterator_slot), Value::Undefined) {
                    Value::Undefined
                } else {
                    let iterator = Iterator {
                        object: self.peek(iterator_slot),
                        next: self.peek(1 + extra),
                    };
                    let next = iterator.next.clone();
                    self.call_value(next, iterator.object, Vec::new())?
                };
                let promise = self.promise_resolve_value(result)?;
                self.push(promise);
            }
            IteratorGetValueDone => {
                // `result -> value done`. A missing result object means the iterator was
                // already exhausted, which the loop treats as `done`.
                let result = self.pop();
                let Value::Object(object) = &result else {
                    self.push(Value::Undefined);
                    self.push(Value::Bool(true));
                    return Ok(Step::Continue);
                };
                let done = self
                    .get_property(object, "done")?
                    .is_some_and(|value| value.to_boolean());
                if done {
                    self.push(Value::Undefined);
                    self.push(Value::Bool(true));
                } else {
                    let value = self
                        .get_property(object, "value")?
                        .unwrap_or(Value::Undefined);
                    self.push(value);
                    self.push(Value::Bool(false));
                }
            }

            Delete => {
                // `delete obj[key]`, where the key was just pushed. Deleting an array
                // index leaves a *hole* rather than closing the gap, and a
                // non-configurable property answers `false` and stays.
                let key = self.pop();
                let target = self.pop();
                let key = self.propkey(&key);
                let text_label = key.as_str().unwrap_or("").to_string();
                let deleted = match &target {
                    Value::Object(object) => {
                        let object = object.clone();
                        self.delete_property_key(&object, &key)?
                    }
                    // A string's `length` and its index properties are
                    // non-configurable, so deleting one answers `false`.
                    Value::String(text) => {
                        !(text_label == "length"
                            || array_index(&text_label)
                                .is_some_and(|index| (index as usize) < text.utf16_len()))
                    }
                    // "delete null.a" is a TypeError, not a no-op: the operator needs an
                    // object to delete from, and only a *boxable* primitive has one.
                    Value::Undefined | Value::Null => {
                        return Err(Signal::Throw(
                            self.make_error("TypeError", "cannot convert to object"),
                        ));
                    }
                    // Any other primitive has no own properties; deleting nothing
                    // succeeds.
                    _ => true,
                };
                self.push(Value::Bool(deleted));
            }

            // -- exceptions and bookkeeping ---------------------------------
            Fclosure | Fclosure8 => {
                let value = self.closure(a as usize)?;
                self.push(value);
            }

            other => {
                let name = other.info().name;
                return Err(Signal::Throw(self.make_error(
                    "InternalError",
                    &format!("{name} is not implemented yet"),
                )));
            }
        }
        Ok(Step::Continue)
    }

    // -- operand helpers ----------------------------------------------------

    /// The slot index an operand names, taking short forms into account.
    fn slot_operand(&self, op: Opcode, a: u32) -> usize {
        use Opcode::*;
        match op {
            GetLoc0 | PutLoc0 | SetLoc0 | GetArg0 | PutArg0 | SetArg0 | GetVarRef0 | PutVarRef0
            | SetVarRef0 => 0,
            GetLoc1 | PutLoc1 | SetLoc1 | GetArg1 | PutArg1 | SetArg1 | GetVarRef1 | PutVarRef1
            | SetVarRef1 => 1,
            GetLoc2 | PutLoc2 | SetLoc2 | GetArg2 | PutArg2 | SetArg2 | GetVarRef2 | PutVarRef2
            | SetVarRef2 => 2,
            GetLoc3 | PutLoc3 | SetLoc3 | GetArg3 | PutArg3 | SetArg3 | GetVarRef3 | PutVarRef3
            | SetVarRef3 => 3,
            _ => a as usize,
        }
    }

    /// The argument count of a call, taking `call0`…`call3` into account.
    fn argc_operand(&self, op: Opcode, a: u32) -> usize {
        use Opcode::*;
        match op {
            Call0 => 0,
            Call1 => 1,
            Call2 => 2,
            Call3 => 3,
            _ => a as usize,
        }
    }

    fn atom_text(&self, index: usize) -> String {
        self.frame()
            .code
            .code
            .atoms
            .get(index)
            .map(|atom| match atom {
                Atom::Str(text) => text.clone(),
                Atom::Index(value) => value.to_string(),
            })
            .unwrap_or_default()
    }

    fn constant(&mut self, index: usize) -> JsResult<Value> {
        let constant = self.frame().code.code.cpool.get(index).cloned();
        Ok(match &constant {
            Some(Constant::Number(number)) => Value::Float(*number),
            Some(Constant::String(text)) => self.string(text.clone()),
            Some(Constant::BigInt { digits, radix }) => {
                match crate::bigint::BigInt::from_radix(digits, *radix) {
                    Some(big) => self.bigint(big),
                    // A literal the parser accepted is always parseable; a failure here
                    // would be a compiler bug, so it reads as an `InternalError`.
                    None => {
                        return Err(Signal::Throw(
                            self.make_error("InternalError", "unparseable bigint literal"),
                        ));
                    }
                }
            }
            Some(Constant::Function(_)) => {
                return Err(Signal::Throw(
                    self.make_error("InternalError", "a bare function constant"),
                ));
            }
            // A tagged template's call site: a frozen array of the cooked chunks with a
            // frozen `raw` array beside it, which is what `String.raw` reads back.
            Some(Constant::Template { cooked, raw }) => {
                let cooked_values: Vec<Value> = cooked
                    .iter()
                    .map(|chunk| match chunk {
                        Some(text) => self.string(text.clone()),
                        None => Value::Undefined,
                    })
                    .collect();
                let raw_values: Vec<Value> =
                    raw.iter().map(|text| self.string(text.clone())).collect();
                let raw_array = self.new_array(raw_values);
                raw_array.set_extensible(false);
                let template = self.new_array(cooked_values);
                template.define(
                    "raw",
                    Property::with_attributes(Value::Object(raw_array), false, false),
                );
                template.set_extensible(false);
                Value::Object(template)
            }
            _ => Value::Undefined,
        })
    }

    /// The reference cell a closure-variable slot names.
    ///
    /// Two kinds of binding share this addressing: a genuine closure variable
    /// (captured from an enclosing function) lives in this frame's environment,
    /// while a *declared global* lives in the realm so that a script, a function
    /// declaration and their uses all see one binding. A plain `Global` — an
    /// undeclared name that may be satisfied by the global object — has no cell at
    /// all, which is how `typeof x` still works for a name that does not exist.
    /// The reference cell a closure-variable slot names.
    ///
    /// The environment is resolved once, when the closure is built, and this is
    /// just the lookup afterwards. A *declared global* is in there too, pointing at
    /// the realm's cell, so a script, a hoisted function declaration and their uses
    /// all share one binding. A plain `Global` — a name the script never declared —
    /// is read and written through `get_var`/`put_var` by name instead, which is
    /// what lets the global object satisfy it.
    fn var_ref_cell(&self, slot: usize) -> Option<Gc<Cell>> {
        self.frame().env.cells.get(slot).cloned()
    }

    /// Whether `slot` refers to a global binding through the reference table.
    ///
    /// A global's cell in the closure environment only exists so the slot has
    /// something to point at; the binding itself lives on the global object (a `var`)
    /// or in the realm's lexical table (a `let`). Reads and writes therefore have to
    /// be routed, not performed on the placeholder.
    fn is_global_slot(&self, slot: usize) -> bool {
        self.frame()
            .code
            .code
            .closure_vars
            .get(slot)
            .is_some_and(|c| {
                // A module's declarations and imports are *not* globals: their cells are
                // the module environment's, filled by the host, so the placeholder routing
                // must not apply to them.
                matches!(
                    c.closure_type,
                    ClosureType::GlobalDecl | ClosureType::Global | ClosureType::GlobalRef
                )
            })
    }

    fn cell_value(&mut self, slot: usize) -> JsResult<Value> {
        if self.is_global_slot(slot) {
            return self.get_global(slot);
        }
        Ok(self
            .var_ref_cell(slot)
            .map(|cell| cell.get())
            .unwrap_or(Value::Undefined))
    }

    fn set_cell(&mut self, slot: usize, value: Value) -> JsResult<()> {
        // A function declaration is hoisted by storing into a global reference before
        // the body runs, so this path is also what puts it on the global object.
        if self.is_global_slot(slot) {
            return self.put_global(slot, value);
        }
        if let Some(cell) = self.var_ref_cell(slot) {
            cell.set(value);
        }
        Ok(())
    }

    // -- closures -----------------------------------------------------------

    /// Build the closure for the function constant `index`.
    ///
    /// Every entry in the child's closure table names where its binding lives in
    /// *this* frame: a local or argument slot that was marked captured, or a cell
    /// this frame is itself holding. That is how a variable three functions deep
    /// stays one binding.
    fn closure(&mut self, index: usize) -> JsResult<Value> {
        let Some(Constant::Function(function)) = self.frame().code.code.cpool.get(index).cloned()
        else {
            return Err(Signal::Throw(
                self.make_error("InternalError", "fclosure of a non-function"),
            ));
        };
        let mut cells = Vec::with_capacity(function.closure_vars.len());
        for closure_var in &function.closure_vars {
            let cell = match closure_var.closure_type {
                ClosureType::Arg => {
                    let slot = closure_var.var_idx as usize;
                    self.frame().code.arg_cell.get(slot).copied().unwrap_or(-1)
                }
                ClosureType::Local => {
                    let slot = closure_var.var_idx as usize;
                    self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1)
                }
                ClosureType::Ref | ClosureType::GlobalRef => {
                    // A `Ref` names a cell in this frame's own closure table; a
                    // `GlobalRef` also starts there, then resolves by name below.
                    let slot = closure_var.var_idx as usize;
                    if let Some(cell) = self.frame().env.cells.get(slot) {
                        cells.push(cell.clone());
                        continue;
                    }
                    -1
                }
                _ => -1,
            };
            if cell >= 0 {
                let cell = self.frame().cells[cell as usize].clone();
                cells.push(cell);
                continue;
            }
            // A global: the cell is created on demand and shared through the realm,
            // so every function sees one binding.
            let name = closure_var.name.clone();
            let cell = match self.realm.globals.get(&name) {
                Some(cell) => cell.clone(),
                None => {
                    let cell = self.new_cell(Value::Undefined);
                    self.realm.globals.insert(name.clone(), cell.clone());
                    cell
                }
            };
            cells.push(cell);
        }
        let name = function.name.clone();
        // `.length` is the arity before any default or rest parameter, not the number
        // of slots a pattern parameter needs.
        let length = function.defined_args;
        // `[[Construct]]` per §10.2.1: an ordinary function has it, a generator, an async
        // function and an arrow function do not. A *method* also does not, but that is not
        // known here — the method is marked when the `[[HomeObject]]` is attached (see
        // `set_home_object`), which is the moment it becomes a method.
        let function_kind = function.kind;
        let constructible =
            !function.is_arrow && function_kind == js_compile::bytecode::FunctionKind::Normal;
        let code = Gc::new(self.heap, CompiledCode::new(*function));
        // A closure built inside an `eval` inherits the eval's view of the enclosing
        // bindings, so it can still read them once the eval has returned.
        let eval_scope = self
            .active_eval_scope
            .clone()
            .or_else(|| self.frame().env.eval_scope.clone());
        // A closure sees the variables a direct `eval` declared in the frame it was
        // created in — the ones declared before *and after* it, so the same object is
        // shared; only the phase bounds which of them are visible.
        let local_vars = self.visible_local_vars().map(|(vars, _)| vars);
        let local_phase = if self.in_parameter_list() { 0 } else { 1 };
        let env = Gc::new(
            self.heap,
            ClosureEnv {
                cells,
                eval_scope,
                local_vars,
                local_phase,
            },
        );
        // A generator or async function inherits from `%GeneratorFunction.prototype%` or
        // `%AsyncFunction.prototype%`, not straight from `Function.prototype`. That link is
        // the only thing that carries `Symbol.toStringTag`, so without it
        // `delete g.constructor.prototype[Symbol.toStringTag]` has nothing to delete and
        // `Object.prototype.toString` keeps reporting `GeneratorFunction`.
        let proto = match function_kind {
            js_compile::bytecode::FunctionKind::AsyncGenerator => self
                .realm
                .async_generator_function_proto
                .clone()
                .or_else(|| self.realm.function_proto.clone().into()),
            js_compile::bytecode::FunctionKind::Generator => self
                .realm
                .generator_function_proto
                .clone()
                .or_else(|| self.realm.function_proto.clone().into()),
            js_compile::bytecode::FunctionKind::Async => self
                .realm
                .async_function_proto
                .clone()
                .or_else(|| self.realm.function_proto.clone().into()),
            js_compile::bytecode::FunctionKind::Normal => self.realm.function_proto.clone().into(),
        };
        let object = JsObject::function(self.heap, proto, Callable::Bytecode { code, env });
        object.set_constructible(constructible);
        object.define(
            "length",
            Property::name_or_length(Value::Int(length as i32)),
        );
        // Defined even when empty: `(function(){}).name` is `""`, not absent, and a test
        // that checks the own-property descriptor fails if the property is missing.
        object.define("name", Property::name_or_length(self.string(name)));
        // Only a function that can be constructed has a `prototype`: §10.2.5 makes it
        // when `[[Construct]]` is created, so a generator, an async function and an arrow
        // function have none of their own. Installing it unconditionally gave
        // `Array.fromAsync` (an async function) an own `prototype` property, which
        // `builtin.js` checks for absence.
        if constructible {
            self.install_function_prototype(&object);
        }
        Ok(Value::Object(object))
    }

    // -- conversions used by the operators ----------------------------------

    fn numeric_of(&mut self, value: Value) -> JsResult<Value> {
        // `+x` is `ToNumber`, which for a string parses it and for an object runs
        // `ToPrimitive` first. Returning a string unchanged would let `+"1"` stay a
        // string, which then behaves differently in every later comparison.
        match value {
            Value::Int(_) | Value::Float(_) => Ok(value),
            other => Ok(Value::Float(self.to_js_number(&other)?)),
        }
    }

    /// `++`/`--`, which step a bigint by `1n` and everything else by `1`.
    ///
    /// Returns the *converted* old value and the new one, because the two callers need
    /// different halves: `++x` yields the new value, while `x++` yields the old one and
    /// still stores the new. The old value is `ToNumeric(x)`, not `x` — `a.x++` where
    /// `a.x` is `true` evaluates to `1`, so returning the raw operand handed back a
    /// boolean.
    ///
    /// A bigint stays a bigint here: `ToNumeric` keeps the type, so routing `i++` through
    /// the number path turned it into `NaN`, and every `for (i = 0n; i < n; i++)` loop in
    /// a bigint algorithm then ran exactly once.
    fn advance_numeric(&mut self, value: Value, up: bool) -> JsResult<(Value, Value)> {
        if let Value::BigInt(big) = &value {
            let one = crate::bigint::BigInt::from_i64(1);
            let next = if up { big.add(&one) } else { big.sub(&one) };
            return Ok((value, self.bigint(next)));
        }
        let old = self.numeric(value)?;
        let new = if up {
            increment(old.clone())
        } else {
            decrement(old.clone())
        };
        Ok((old, new))
    }

    fn numeric(&mut self, value: Value) -> JsResult<Value> {
        // `ToNumber` on an object runs `ToPrimitive` first, which is how
        // `2 * {valueOf(){return 3}}` sees the 3 — and how a `valueOf` that throws is
        // heard at all. Reading the object's own tag instead (`other.to_number()`)
        // turned the whole operand into `NaN` and swallowed the exception.
        match value {
            Value::Int(_) | Value::Float(_) => Ok(value),
            other => Ok(Value::Float(self.to_js_number(&other)?)),
        }
    }

    /// The operand of a `BigInt` operation, or the `TypeError` that mixing raises.
    fn bigint_operand(&self, value: &Value) -> JsResult<crate::bigint::BigInt> {
        match value {
            Value::BigInt(big) => Ok((**big).clone()),
            // The reference words this the same way for every operator, so a failing
            // script reads identically; the operator itself adds nothing a caller can use.
            _ => Err(Signal::Throw(
                self.make_error("TypeError", "cannot convert bigint to number"),
            )),
        }
    }

    /// Arithmetic where at least one operand is a `BigInt`.
    ///
    /// The two numeric types never mix, so a bigint paired with anything else is an error
    /// rather than a conversion: `1n + 1` throws, and so does `1n < 1` only in the sense
    /// that the relational operators handle it separately.
    fn bigint_arithmetic(&mut self, op: Opcode, left: Value, right: Value) -> JsResult<Value> {
        use Opcode::*;
        let a = self.bigint_operand(&left)?;
        let b = self.bigint_operand(&right)?;
        let result = match op {
            Add => a.add(&b),
            Sub => a.sub(&b),
            Mul => a.mul(&b),
            Div | Mod => {
                let Some((quotient, remainder)) = a.div_rem(&b) else {
                    return Err(Signal::Throw(
                        self.make_error("RangeError", "division by zero"),
                    ));
                };
                if op == Div { quotient } else { remainder }
            }
            Pow => {
                let Some(value) = a.pow(&b) else {
                    return Err(Signal::Throw(self.make_error(
                        "RangeError",
                        "BigInt exponent must be non-negative and not absurdly large",
                    )));
                };
                value
            }
            And => a.bitand(&b),
            Or => a.bitor(&b),
            Xor => a.bitxor(&b),
            Shl | Sar => {
                // A shift count is a `BigInt` here, and both directions are exact; a
                // negative count would be the other direction, which the spec forbids.
                let Some(count) = b.to_i64() else {
                    return Err(Signal::Throw(
                        self.make_error("RangeError", "BigInt shift count is too large"),
                    ));
                };
                if count < 0 {
                    return Err(Signal::Throw(self.make_error(
                        "RangeError",
                        "BigInt shift count must be non-negative",
                    )));
                }
                if op == Shl {
                    a.shl(count as u64)
                } else {
                    a.shr(count as u64)
                }
            }
            // `>>>` is defined only for 32-bit integers, so a bigint has no unsigned shift.
            _ => {
                return Err(Signal::Throw(self.make_error(
                    "TypeError",
                    "BigInts have no unsigned right shift, use >> instead",
                )));
            }
        };
        Ok(self.bigint(result))
    }

    /// `<`, `<=`, `>`, `>=`, which accept a `BigInt` and a Number alongside each other.
    ///
    /// Unlike arithmetic, relation is defined across the two numeric types: the comparison
    /// is on the mathematical values, and nothing is rounded first.
    fn relational(&mut self, op: Opcode, left: Value, right: Value) -> Value {
        use Opcode::*;
        let ordering = match (&left, &right) {
            (Value::String(a), Value::String(b)) => {
                Some(a.to_string_lossy().cmp(&b.to_string_lossy()))
            }
            (Value::BigInt(a), Value::BigInt(b)) => Some(a.cmp(b)),
            // A bigint against a number compares the *mathematical* values, so `2n < 2.5`
            // is true even though `2.5` has no bigint form.
            (Value::BigInt(a), other) if !matches!(other, Value::BigInt(_)) => {
                let b = other.to_number();
                if b.is_nan() {
                    None
                } else {
                    compare_bigint_and_f64(a, b)
                }
            }
            (other, Value::BigInt(b)) if !matches!(other, Value::BigInt(_)) => {
                let a = other.to_number();
                if a.is_nan() {
                    None
                } else {
                    compare_bigint_and_f64(b, a).map(std::cmp::Ordering::reverse)
                }
            }
            _ => left.to_number().partial_cmp(&right.to_number()),
        };
        let _ = self;
        let answer = match (op, ordering) {
            (_, None) => false,
            (Lt, Some(ordering)) => ordering.is_lt(),
            (Lte, Some(ordering)) => ordering.is_le(),
            (Gt, Some(ordering)) => ordering.is_gt(),
            (Gte, Some(ordering)) => ordering.is_ge(),
            _ => false,
        };
        Value::Bool(answer)
    }

    fn binary_add(&mut self) -> JsResult<()> {
        let right = self.pop();
        let left = self.pop();
        let left = self.primitive_of(left)?;
        let right = self.primitive_of(right)?;
        // `+` concatenates when either operand is a string *after* `ToPrimitive`, and
        // that check comes before the bigint one: `"" + 1n` is a string, not a
        // mixed-type error.
        if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
            // `ToString` on both sides, which rejects a symbol: `"" + Symbol()` is a
            // `TypeError`, the one place a symbol and a string do not mix.
            let left = self.checked_to_wtf8(&left)?;
            let right = self.checked_to_wtf8(&right)?;
            // Not `format!`: `Wtf8::concat` is what keeps a surrogate pair split across the
            // join canonical, so `"\uD800" + "\uDC00"` equals `"\u{10000}"`.
            let text = left.concat(&right);
            self.push(self.string_wtf8(text));
            return Ok(());
        }
        // The two numeric types never mix, and `+` is no exception: `1n + 1` is an error,
        // not a conversion.
        if matches!(left, Value::BigInt(_)) || matches!(right, Value::BigInt(_)) {
            let value = self.bigint_arithmetic(Opcode::Add, left, right)?;
            self.push(value);
            return Ok(());
        }
        let left = self.numeric(left)?;
        let right = self.numeric(right)?;
        self.push(add_numbers(left, right));
        Ok(())
    }

    fn primitive_of(&mut self, value: Value) -> JsResult<Value> {
        self.to_primitive(value, Hint::Default)
    }

    /// `ToPrimitive`, which is what makes `"" + obj` produce `obj.toString()`.
    ///
    /// The order depends on the hint: the "number" order tries `valueOf` first and
    /// `toString` second, the "string" order the other way round, and the default
    /// hint uses the number order. A result that is still an object is a `TypeError`,
    /// because no built-in `valueOf`/`toString` returns one.
    pub fn to_primitive(&mut self, value: Value, hint: Hint) -> JsResult<Value> {
        let Value::Object(object) = &value else {
            return Ok(value);
        };
        // A Date overrides the default hint to mean "string", which is what
        // `Date.prototype[Symbol.toPrimitive]` does with hint `"default"`. Without
        // this, `"" + new Date()` would print the milliseconds, not the date.
        let hint = if hint == Hint::Default && is_date_object(self, object) {
            Hint::String
        } else {
            hint
        };
        // `Symbol.toPrimitive` wins over `valueOf`/`toString` when it exists, and it is
        // handed the hint as a string. This is the hook a Date uses for its default hint
        // and a script uses to take over conversion entirely.
        let key = self.realm.well_known_key(WellKnown::ToPrimitive);
        if let Some(Value::Object(function)) = self.get_property_key(object, &key)? {
            if !function.is_callable() {
                return Err(self.type_error("Symbol.toPrimitive is not a function"));
            }
            let hint = match hint {
                Hint::String => "string",
                Hint::Number => "number",
                Hint::Default => "default",
            };
            let hint = self.string(hint);
            let result = self.call_value(
                Value::Object(function),
                Value::Object(object.clone()),
                vec![hint],
            )?;
            if matches!(result, Value::Object(_)) {
                return Err(self.type_error("cannot convert object to primitive value"));
            }
            return Ok(result);
        }
        let order: [&str; 2] = match hint {
            Hint::String => ["toString", "valueOf"],
            Hint::Number | Hint::Default => ["valueOf", "toString"],
        };
        let receiver = object.clone();
        for method in order {
            if let Some(function) = self.get_property(&receiver, method)?
                && matches!(&function, Value::Object(object) if object.is_callable())
            {
                let result =
                    self.call_value(function, Value::Object(receiver.clone()), Vec::new())?;
                if !matches!(result, Value::Object(_)) {
                    return Ok(result);
                }
            }
        }
        Err(self.type_error("cannot convert object to primitive value"))
    }

    /// `ToString` with the object case going through `ToPrimitive`.
    pub fn to_js_string(&mut self, value: &Value) -> JsResult<String> {
        if matches!(value, Value::Object(_)) {
            let primitive = self.to_primitive(value.clone(), Hint::String)?;
            return Ok(primitive.to_js_string());
        }
        Ok(value.to_js_string())
    }

    /// `ToString` that keeps the string's code units instead of flattening them.
    ///
    /// This is what `+` needs: `String.fromCharCode(0xD800) + "a"` must keep the unpaired
    /// surrogate, and a lossy `String` in between would turn it into U+FFFD.
    pub fn checked_to_wtf8(&mut self, value: &Value) -> JsResult<crate::value::Wtf8> {
        if matches!(value, Value::Symbol(_)) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "cannot convert symbol to string"),
            ));
        }
        // An object runs `ToPrimitive` first, which may itself produce a symbol.
        let value = self.to_primitive(value.clone(), Hint::String)?;
        match value {
            Value::Symbol(_) => Err(Signal::Throw(
                self.make_error("TypeError", "cannot convert symbol to string"),
            )),
            Value::String(text) => Ok(text.wtf8().clone()),
            other => Ok(crate::value::Wtf8::new(other.to_js_string())),
        }
    }

    /// `ToNumber` with the object case going through `ToPrimitive`.
    pub fn to_js_number(&mut self, value: &Value) -> JsResult<f64> {
        let value = if matches!(value, Value::Object(_)) {
            self.to_primitive(value.clone(), Hint::Number)?
        } else {
            value.clone()
        };
        // `ToNumber` refuses a BigInt: there is no lossless number for a 2^70 integer, so
        // the spec makes this a `TypeError` rather than `NaN`. `Value::to_number` answers
        // `NaN` to stay total, and the operators check for the bigint themselves — but
        // `Math.max(1n)`, `new Int32Array(1n)` and every other `ToNumber` caller needs to
        // refuse, and answering `NaN` made them silently wrong instead.
        // Both are checked *after* `ToPrimitive`, so a boxed Symbol or BigInt is refused
        // too: `ToNumber(Object(Symbol()))` unwraps to the symbol and then fails.
        match value {
            Value::BigInt(_) => Err(self.type_error("cannot convert bigint to number")),
            Value::Symbol(_) => Err(self.type_error("cannot convert symbol to number")),
            other => Ok(other.to_number()),
        }
    }

    /// `ToNumber` or `ToBigInt`, whichever `element` stores, as a primitive ready for
    /// [`ElementType::write`].
    ///
    /// This is separated from the write because both conversions are observable script —
    /// `ToPrimitive` calls `valueOf` — and a conversion that runs script must not happen
    /// with a buffer's `RefCell` borrowed. Every element-write path calls this first:
    /// the constructor's initialiser list, `fill`/`set`/`sort`'s write-back, `DataView`'s
    /// setters, and a plain `a[i] = v`.
    pub(crate) fn element_value(
        &mut self,
        element: crate::builtins::typedarray::ElementType,
        value: Value,
    ) -> JsResult<Value> {
        // `ToPrimitive` with the number hint, which is what turns `{valueOf(){return 5}}`
        // into `5` and `{valueOf(){return 1n}}` into a BigInt. A primitive passes through.
        let primitive = match &value {
            Value::Object(_) => self.to_primitive(value, Hint::Number)?,
            _ => value,
        };
        if element.is_bigint() {
            let big = crate::builtins::bigint::to_bigint(self, &primitive)?;
            Ok(Value::BigInt(Gc::new(self.heap, big)))
        } else {
            // `ToNumber` refuses a BigInt and a Symbol rather than answering `NaN`.
            Ok(Value::Float(self.to_js_number(&primitive)?))
        }
    }

    /// `ToPropertyKey`: what a computed member access uses as its key.
    ///
    /// A symbol is returned as itself, never stringified — that is what keeps
    /// `o[sym]` and `o["Symbol(a)"]` apart. Everything else becomes its string form.
    fn propkey(&mut self, value: &Value) -> PropertyKey {
        match value {
            Value::Symbol(symbol) => PropertyKey::Symbol(symbol.clone()),
            _ => PropertyKey::String(self.propkey_of(value)),
        }
    }

    /// `ToPropertyKey` for the callers that can only use a string key.
    fn propkey_of(&mut self, value: &Value) -> crate::value::Wtf8 {
        use crate::value::Wtf8;
        match value {
            // A string key keeps its code units. Going through a lossy `String` would make
            // `o["\uD800"]` and `o["\uFFFD"]` the same property.
            Value::String(text) => text.wtf8().clone(),
            Value::Int(_) | Value::Float(_) => Wtf8::new(number_to_string(value.to_number())),
            // An object key runs `ToPropertyKey`, which needs `ToPrimitive` first;
            // falling back to its `ToString` would print `[object Object]`.
            Value::Object(_) => match self.to_primitive(value.clone(), Hint::String) {
                Ok(Value::String(text)) => text.wtf8().clone(),
                Ok(other) => Wtf8::new(other.to_js_string()),
                Err(_) => Wtf8::new("[object Object]"),
            },
            other => Wtf8::new(other.to_js_string()),
        }
    }

    // -- object operations used by instructions -----------------------------

    /// Read `object[key]`, returning `undefined` when the property is missing.
    ///
    /// Unlike [`Interp::get_property`] this takes a runtime `Value`, so it works on a
    /// primitive (looking through its prototype) as well as an object — which is what
    /// `get_field`/`get_array_el` need and what a builtin reading a property off an
    /// argument needs too.
    pub fn read_field(&mut self, object: &Value, key: &str) -> JsResult<Value> {
        self.read_field_key(object, &PropertyKey::from(key))
    }

    /// [`Interp::read_field`] with a key that may be a symbol.
    ///
    /// A symbol key never reaches the string shortcuts: a string's `length` and its
    /// indices are string properties, and a symbol named `length` is a different
    /// property that a string primitive cannot have.
    pub fn read_field_key(&mut self, object: &Value, key: &PropertyKey) -> JsResult<Value> {
        let text = key.as_str().unwrap_or("");
        let Value::Object(object) = object else {
            // Reading a property of `undefined`/`null` is the classic TypeError.
            if object.is_nullish() {
                return Err(Signal::Throw(self.make_error(
                    "TypeError",
                    &format!("cannot read property '{key}' of {}", object.to_js_string()),
                )));
            }
            // A string has `length` and index access without being boxed.
            if let Value::String(value) = object {
                if text == "length" {
                    return Ok(Value::Int(value.utf16_len() as i32));
                }
                if let Some(index) = array_index(text) {
                    return Ok(match value.wtf8().unit_at(index as usize) {
                        Some(unit) => self.string_wtf8(unit),
                        None => Value::Undefined,
                    });
                }
            }
            // A property of a primitive is looked up on its prototype: this is what
            // makes `"a".toUpperCase()` reach `String.prototype` without boxing.
            let prototype = match object {
                Value::String(_) => self.realm.string_proto.clone(),
                Value::Int(_) | Value::Float(_) => self.realm.number_proto.clone(),
                Value::Bool(_) => self.realm.boolean_proto.clone(),
                // A symbol's methods live on `Symbol.prototype`, reached the same way —
                // `sym.description` reads through the prototype, not off the symbol.
                Value::Symbol(_) => self.realm.symbol_proto.clone(),
                // …and a bigint's on `BigInt.prototype`, which is how
                // `(255n).toString(16)` reaches its method.
                Value::BigInt(_) => self.realm.bigint_proto.clone(),
                _ => None,
            };
            if let Some(prototype) = prototype
                && let Some(value) = self.get_property_key_for(&prototype, Some(object), key)?
            {
                return Ok(value);
            }
            return Ok(Value::Undefined);
        };
        Ok(self
            .get_property_key(object, key)?
            .unwrap_or(Value::Undefined))
    }

    fn write_field(&mut self, object: &Value, key: &str, value: Value) -> JsResult<()> {
        self.write_field_key(object, &PropertyKey::from(key), value)
    }

    fn write_field_key(&mut self, object: &Value, key: &PropertyKey, value: Value) -> JsResult<()> {
        let Value::Object(object) = object else {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                &format!("cannot set property '{key}' of {}", object.to_js_string()),
            )));
        };
        // `put_field`/`put_array_el` are plain assignments, so the mode decides.
        let on_failure = if self.frame().code.code.strict {
            DeleteFailure::Throw
        } else {
            DeleteFailure::Ignore
        };
        self.set_property_with_key(object, key, value, on_failure)
    }

    /// `ToString`, as the spec defines it: a symbol is a `TypeError`.
    ///
    /// [`Value::to_js_string`] cannot fail, so it answers the descriptive form for a
    /// symbol. That is right for `String(sym)` and for error messages, and wrong for
    /// everything the spec routes through `ToString` — `"" + sym`, `${sym}`,
    /// `Number.parseInt(sym)` — so those callers use this instead.
    pub fn checked_to_string(&mut self, value: &Value) -> JsResult<String> {
        if matches!(value, Value::Symbol(_)) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "cannot convert symbol to string"),
            ));
        }
        // An object runs `ToPrimitive` first, which may itself produce a symbol —
        // `({[Symbol.toPrimitive]: () => sym}) + ""` has to throw too.
        let value = self.to_primitive(value.clone(), Hint::String)?;
        if matches!(value, Value::Symbol(_)) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "cannot convert symbol to string"),
            ));
        }
        Ok(value.to_js_string())
    }

    /// `ToPropertyKey` for a builtin that takes a key argument.
    ///
    /// Distinct from [`Interp::propkey`], which is `fn`-private to the instruction
    /// loop; the builtins reach this one.
    pub fn property_key(&mut self, value: &Value) -> PropertyKey {
        self.propkey(value)
    }

    /// `ToUint32` with `ArraySetLength`'s refusal, for an array's `length`.
    ///
    /// `ToUint32` is `ToNumber` *then* the modulo, but an object that yields a fraction or
    /// `NaN` falls through to `ToString` and is parsed as decimal digits. That is why
    /// `{toString: () => "2"}` is a legal length while `{valueOf: () => 2.5}` is a
    /// `RangeError` — and why this cannot be a plain `ToNumber`.
    pub fn array_length_of(&mut self, value: &Value) -> JsResult<u32> {
        let number = match value {
            Value::Int(number) => *number as f64,
            Value::Float(number) => *number,
            // `null`/`undefined`/`false` convert through `ToPrimitive` and then arithmetically:
            // `null` is 0, `false` is 0, `true` is 1. Refusing them made
            // `Object.defineProperty(a, "length", {value: null})` a `RangeError` where the
            // reference answers 0.
            Value::Null | Value::Bool(_) => value.to_number(),
            _ => {
                let primitive = self.to_primitive(value.clone(), Hint::Number)?;
                match primitive {
                    Value::Int(number) => number as f64,
                    Value::Float(number) => number,
                    other => {
                        // `ToString` then `ToNumber`, so a string length goes through the
                        // full numeric-literal grammar: `"0x00B"` is 11 and `"  5  "` is 5.
                        // `Value::to_number` on the string already implements exactly that.
                        let text = self.to_js_string(&other)?;
                        crate::value::string_to_number(&text)
                    }
                }
            }
        };
        // `ToUint32` of anything outside `0 ..= 2^32-1` that is still an integer is a
        // modulo in the spec, but `ArraySetLength` refuses it outright: `a.length = 2**32`
        // is a `RangeError`, not `0`.
        if !number.is_finite()
            || number.fract() != 0.0
            || !(0.0..=u32::MAX as f64).contains(&number)
        {
            return Err(self.range_error("invalid array length"));
        }
        Ok(number as u32)
    }

    /// `ToPropertyKey` that can fail.
    ///
    /// [`Interp::property_key`] swallows a throwing `ToPrimitive` and answers
    /// `"[object Object]"`, which is fine for a bytecode operand that has already been
    /// coerced but wrong for `Reflect.get`, where a key whose `toString` throws has to
    /// propagate that error (`return-abrupt-from-property-key`).
    pub fn property_key_checked(&mut self, value: &Value) -> JsResult<PropertyKey> {
        use crate::value::Wtf8;
        match value {
            Value::Symbol(symbol) => Ok(PropertyKey::Symbol(symbol.clone())),
            _ => match self.to_primitive(value.clone(), Hint::String)? {
                Value::String(text) => Ok(PropertyKey::String(text.wtf8().clone())),
                // A `ToPrimitive` that lands on a symbol is a symbol key, not `String(sym)`.
                Value::Symbol(symbol) => Ok(PropertyKey::Symbol(symbol)),
                other => Ok(PropertyKey::String(Wtf8::new(other.to_js_string()))),
            },
        }
    }

    /// A [`PropertyKey`] as the `Value` a script sees — what `Reflect.ownKeys` and
    /// `Object.getOwnPropertySymbols` put in their result array.
    pub fn key_value(&self, key: &PropertyKey) -> Value {
        match key {
            PropertyKey::String(text) => self.string_wtf8(text.clone()),
            PropertyKey::Symbol(symbol) => Value::Symbol(symbol.clone()),
        }
    }

    /// Fill in a String exotic object: `_value`, `length` and the index properties.
    ///
    /// `new String("ab")` used to get only `_value`, so `new String("ab").length` was
    /// `undefined`, `new String("ab")[0]` was `undefined`, and every `Array.prototype`
    /// method called on a box saw an array-like of length 0 — 36 test262 files exercise
    /// exactly that. `ToObject` on a *primitive* string built the indices correctly, so
    /// the two paths had drifted; this is the one both now use.
    ///
    /// The indices are *code units*, so an astral character occupies two of them — the
    /// same rule `s.length` and `s[i]` follow.
    /// Give an already-created object the internal slot that makes it a wrapper of
    /// `value`, so the primitive's prototype methods work on it.
    ///
    /// Used for `%Boolean.prototype%`, `%Number.prototype%` and `%String.prototype%`,
    /// which are wrappers of their own type holding the empty value.
    pub(crate) fn init_primitive_wrapper(&mut self, object: &Gc<JsObject>, value: Value) {
        self.internal_set(object, "_value", value);
    }

    pub(crate) fn init_string_wrapper(
        &mut self,
        object: &Gc<JsObject>,
        text: &Gc<crate::value::JsString>,
    ) {
        object.define(
            "length",
            Property::with_attributes(Value::Int(text.utf16_len() as i32), false, false),
        );
        for index in 0..text.utf16_len() {
            if let Some(unit) = text.wtf8().unit_at(index) {
                object.define(
                    &index.to_string(),
                    // Own, non-writable, enumerable, non-configurable — §10.4.3.4.
                    Property::with_attributes(self.string_wtf8(unit), false, true),
                );
            }
        }
        object.define_key(
            &self.internal_slot("_value"),
            Property::with_attributes(Value::String(text.clone()), false, false),
        );
    }

    /// `ToObject`: box a primitive, refuse `undefined` and `null`.
    ///
    /// A destructuring *object* pattern starts with this, which is why `var {} = null`
    /// throws. The wrapper carries its primitive in `_value` — the same slot
    /// `new Number(x)` and friends fill in — so `valueOf` and the prototype methods work
    /// on a boxed value no constructor ever built.
    pub(crate) fn object_from(&mut self, value: &Value) -> JsResult<Gc<JsObject>> {
        let (prototype, primitive) = match value {
            Value::Object(object) => return Ok(object.clone()),
            Value::String(text) => {
                let object = self.new_object(self.realm.string_proto.clone());
                self.init_string_wrapper(&object, text);
                return Ok(object);
            }
            Value::Int(_) | Value::Float(_) => (self.realm.number_proto.clone(), value.clone()),
            Value::Bool(_) => (self.realm.boolean_proto.clone(), value.clone()),
            // A symbol and a bigint box into wrappers whose methods live on
            // `Symbol.prototype` and `BigInt.prototype` — `Object(sym).description` and
            // `Object(1n).toString(2)` both go through the wrapper. Falling into the
            // branch below instead produced an object with *no* prototype, so those
            // methods were unreachable on a box.
            Value::Symbol(_) => (self.realm.symbol_proto.clone(), value.clone()),
            Value::BigInt(_) => (self.realm.bigint_proto.clone(), value.clone()),
            Value::Undefined | Value::Null => {
                return Err(Signal::Throw(self.make_error(
                    "TypeError",
                    &format!("cannot convert {} to object", value.to_js_string()),
                )));
            }
            _ => (None, value.clone()),
        };
        let object = self.new_object(prototype);
        self.internal_set(&object, "_value", primitive);
        Ok(object)
    }

    /// `a == b`, the coercing comparison.
    ///
    /// An object operand is converted with `ToPrimitive` *through the interpreter*,
    /// which is what makes `[] == ""` true: `[]` becomes `""` via
    /// `Array.prototype.toString`. [`Value::loose_equals`] cannot do that on its own
    /// because the conversion is allowed to call script code, so the two primitive
    /// operands are handed back to it.
    fn abstract_equals(&mut self, left: Value, right: Value) -> JsResult<bool> {
        if !matches!(left, Value::Object(_)) && !matches!(right, Value::Object(_)) {
            return Ok(left.loose_equals(&right));
        }
        // `null == undefined` holds even though neither is an object.
        if matches!(left, Value::Undefined | Value::Null)
            && matches!(right, Value::Undefined | Value::Null)
        {
            return Ok(true);
        }
        if matches!(left, Value::Undefined | Value::Null)
            || matches!(right, Value::Undefined | Value::Null)
        {
            return Ok(false);
        }
        // Two objects compare by identity. Only an object *against a primitive* is
        // converted, which is why `{} == {}` is false while `[] == ""` is true.
        if let (Value::Object(a), Value::Object(b)) = (&left, &right) {
            return Ok(a.ptr_eq(b));
        }
        let left = self.to_primitive(left, Hint::Default)?;
        let right = self.to_primitive(right, Hint::Default)?;
        Ok(left.loose_equals(&right))
    }

    fn has_property(&mut self, key: &Value, object: &Value) -> JsResult<bool> {
        let Value::Object(object) = object else {
            return Err(Signal::Throw(
                self.make_error("TypeError", "cannot use 'in' on a non-object"),
            ));
        };
        let key = self.propkey(key);
        // `in` is `[[HasProperty]]`, over the whole chain and through a proxy's `has`
        // trap. One implementation, so an array's derived `length` and its inherited holes
        // cannot be right in one caller and wrong in another.
        self.has_own_or_inherited(object, &key)
    }

    fn instance_of(&mut self, object: &Value, constructor: &Value) -> JsResult<bool> {
        let Value::Object(target) = object else {
            return Ok(false);
        };
        let Value::Object(function) = constructor else {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                "right-hand side of 'instanceof' is not callable",
            )));
        };
        let prototype = self.get_property(function, "prototype")?;
        let Some(Value::Object(prototype)) = prototype else {
            return Ok(false);
        };
        // `instanceof` walks `[[GetPrototypeOf]]`, so a proxy answers through its trap —
        // which is also what makes `proxyOfInstance instanceof C` work.
        let mut current = Some(target.clone());
        while let Some(object) = current {
            if object.ptr_eq(&prototype) {
                return Ok(true);
            }
            current = self.object_prototype_of(&object)?;
        }
        Ok(false)
    }

    /// `[[GetPrototypeOf]]`, with the proxy trap when there is one.
    pub fn object_prototype_of(&mut self, object: &Gc<JsObject>) -> JsResult<Option<Gc<JsObject>>> {
        if let Some(proto) = self.proxy_get_prototype_of(object)? {
            return Ok(proto);
        }
        Ok(object.proto())
    }

    /// `[[SetPrototypeOf]]`, with the proxy trap when there is one.
    pub fn object_set_prototype_of(
        &mut self,
        object: &Gc<JsObject>,
        proto: Option<Gc<JsObject>>,
    ) -> JsResult<bool> {
        if let Some(answer) = self.proxy_set_prototype_of(object, proto.clone())? {
            return Ok(answer);
        }
        object.set_proto(proto);
        Ok(true)
    }

    /// `[[IsExtensible]]`, with the proxy trap when there is one.
    pub fn object_is_extensible(&mut self, object: &Gc<JsObject>) -> JsResult<bool> {
        if let Some(answer) = self.proxy_is_extensible(object)? {
            return Ok(answer);
        }
        Ok(object.is_extensible())
    }

    /// `[[PreventExtensions]]`, with the proxy trap when there is one.
    pub fn object_prevent_extensions(&mut self, object: &Gc<JsObject>) -> JsResult<bool> {
        if let Some(answer) = self.proxy_prevent_extensions(object)? {
            return Ok(answer);
        }
        object.set_extensible(false);
        Ok(true)
    }

    fn copy_properties(
        &mut self,
        target: &Value,
        source: &Value,
        exclusion: &Value,
    ) -> JsResult<()> {
        // `CopyDataProperties` starts with `ToObject(source)`, so a *string* primitive
        // does contribute: its index properties are own, enumerable and writable, which
        // is why `{..."ab"}` is `{"0":"a","1":"b"}`. Numbers and booleans box to objects
        // with no own properties, so they copy nothing, and spreading `undefined`/`null`
        // is a no-op.
        let Some(target) = target.as_object() else {
            return Ok(());
        };
        let source_object = match source {
            Value::Object(object) => object.clone(),
            Value::String(text) => {
                // Untaken keys are the string's indices, which are code units.
                for index in 0..text.utf16_len() {
                    if let Some(unit) = text.wtf8().unit_at(index) {
                        target.define_data(&index.to_string(), self.string_wtf8(unit));
                    }
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        // An object *rest* element passes the keys already taken by the literal part as
        // an array of strings; a spread passes `null`, which excludes nothing. Only the
        // string keys of the exclusion array are meaningful, so an array is read by its
        // elements rather than by its own property names.
        let mut excluded: Vec<crate::value::Wtf8> = Vec::new();
        if let Value::Object(list) = exclusion
            && let Some(length) = list.array_length()
        {
            for index in 0..length {
                if let Some(value) = list.array_get(index) {
                    excluded.push(self.propkey_of(&value));
                }
            }
        }
        let source = &source_object;
        // Through `[[OwnPropertyKeys]]` and `[[Get]]`, so `{...proxy}` asks the handler.
        // Reading the property table directly made every spread of a proxy empty.
        let keys = self.object_own_property_keys(source)?;
        for as_key in keys {
            if as_key.is_symbol() {
                // A spread copies string-keyed properties only.
                continue;
            }
            if excluded.contains(&crate::value::Wtf8::new(
                as_key.as_str().unwrap_or_default(),
            )) {
                continue;
            }
            let enumerable = self
                .object_get_own_property(source, &as_key)?
                .is_some_and(|property| property.enumerable);
            if !enumerable {
                continue;
            }
            let value = self
                .get_property_key(source, &as_key)?
                .unwrap_or(Value::Undefined);
            target.define_data_key(&as_key, value);
        }
        Ok(())
    }

    // -- iteration ---------------------------------------------------------

    /// One object's own keys in enumeration order: array indices ascending, then the
    /// rest in insertion order.
    ///
    /// An index is ASCII, so a key with no `&str` form is never one. The flag says
    /// whether the key is enumerable, which the caller resolves per key because an
    /// array's elements are not in the same store as its named properties.
    fn order_own_keys(own: Vec<(crate::value::Wtf8, bool)>) -> Vec<(crate::value::Wtf8, bool)> {
        // An index can arrive twice, once from the array's element store and once from
        // its property map, so the numeric keys go through a map that keeps the
        // enumerable answer if either source gave one.
        let mut numeric: std::collections::BTreeMap<u32, bool> = std::collections::BTreeMap::new();
        for (key, enumerable) in &own {
            if let Some(index) = key.as_str().and_then(array_index) {
                let entry = numeric.entry(index).or_insert(false);
                *entry = *entry || *enumerable;
            }
        }
        let mut ordered: Vec<(crate::value::Wtf8, bool)> = numeric
            .into_iter()
            .map(|(index, enumerable)| (crate::value::Wtf8::new(index.to_string()), enumerable))
            .collect();
        ordered.extend(
            own.into_iter()
                .filter(|(key, _)| key.as_str().and_then(array_index).is_none()),
        );
        ordered
    }

    /// Build the key list a `for-in` loop walks.
    ///
    /// The order is the specification's: own properties before inherited ones, and
    /// within one object the integer-like keys in ascending numeric order ahead of the
    /// string keys in insertion order. A shadowed key only appears once, and a key that
    /// is a non-enumerable own property anywhere nearer is skipped entirely.
    fn for_in_record(&mut self, value: &Value) -> JsResult<Gc<JsObject>> {
        let record = self.new_object(None);
        let mut keys: Vec<crate::value::Wtf8> = Vec::new();
        // A proxy supplies its own key list, and — the reason this is not just another
        // branch of the ordinary walk — *re-decides* whether each key is enumerable as
        // the loop goes. `record.__proxy` marks that, and `for_in_advance` re-reads the
        // descriptor there.
        if let Value::Object(object) = value
            && object.proxy_state().is_some()
        {
            let own = self
                .proxy_own_keys(object)?
                .unwrap_or_else(|| object.own_property_keys());
            let mut ordered: Vec<PropertyKey> = Vec::with_capacity(own.len());
            let mut numeric: Vec<u32> = Vec::new();
            let mut rest: Vec<PropertyKey> = Vec::new();
            for key in own {
                match key.as_str().and_then(array_index) {
                    Some(index) => numeric.push(index),
                    None => rest.push(key),
                }
            }
            numeric.sort_unstable();
            ordered.extend(
                numeric
                    .into_iter()
                    .map(|index| PropertyKey::from(index.to_string())),
            );
            ordered.extend(rest);
            for (index, key) in ordered.into_iter().enumerate() {
                record.define_data(&index.to_string(), self.key_value(&key));
            }
            record.define_data("__index", Value::Int(0));
            // The mark that makes `for_in_advance` re-ask the proxy per key.
            record.define_data("__proxy", Value::Object(object.clone()));
            return Ok(record);
        }
        if let Value::Object(object) = value {
            let mut chain = vec![object.clone()];
            chain.extend(object.chain());
            // Every own string key the walk has seen, enumerable or not. A
            // non-enumerable own property still hides a same-named enumerable one
            // further up the chain, so `Object.defineProperty(a, "x", {value: 1})`
            // keeps `x` out of the loop even when the prototype has one.
            let mut visited: Vec<crate::value::Wtf8> = Vec::new();
            for object in chain {
                let mut own: Vec<(crate::value::Wtf8, bool)> = object
                    .own_property_keys()
                    .iter()
                    .filter_map(|key| key.as_wtf8().cloned())
                    .map(|key| {
                        let enumerable = object
                            .own_property(&key.to_string_lossy())
                            .map(|property| property.enumerable)
                            .unwrap_or(false);
                        (key, enumerable)
                    })
                    .collect();
                if let Some(length) = object.array_length() {
                    own.retain(|(key, _)| key.as_str() != Some("length"));
                    for index in 0..length {
                        if object.array_get(index).is_some() {
                            own.push((crate::value::Wtf8::new(index.to_string()), true));
                        }
                    }
                }
                // Integer-like keys come first in ascending order, then the rest in
                // insertion order — and the ordering is *per object*, not for the chain
                // as a whole: one pass over every collected key lifted a prototype's
                // `"4"` above the target's own `x`/`y`, which is not the order
                // `for…in` reports.
                for (key, enumerable) in Self::order_own_keys(own) {
                    if visited.contains(&key) {
                        continue;
                    }
                    visited.push(key.clone());
                    if enumerable {
                        keys.push(key);
                    }
                }
            }
        }
        for (index, key) in keys.into_iter().enumerate() {
            record.define_data(&index.to_string(), self.string_wtf8(key));
        }
        record.define_data("__index", Value::Int(0));
        Ok(record)
    }

    /// The next key of a `for…in` record, `None` when the loop is done.
    ///
    /// For a proxy the descriptor is re-read on *every* step rather than filtered once up
    /// front: the spec's `EnumerateObjectProperties` asks `[[GetOwnProperty]]` per key as
    /// the loop reaches it, so a handler that stops reporting a key partway through drops
    /// it from the rest of the iteration. Snapshotting the list instead kept yielding a
    /// key the handler had already withdrawn.
    fn for_in_advance(&mut self, record: &Value) -> JsResult<Option<String>> {
        let Value::Object(record) = record else {
            return Ok(None);
        };
        let proxy = record
            .own_property("__proxy")
            .and_then(|property| property.value);
        loop {
            let index = record
                .own_property("__index")
                .and_then(|property| property.value)
                .map(|value| value.to_int32())
                .unwrap_or(0);
            let Some(candidate) = record
                .own_property(&index.to_string())
                .and_then(|property| property.value)
            else {
                return Ok(None);
            };
            record.define_data("__index", Value::Int(index + 1));
            let text = candidate.to_js_string();
            let Some(Value::Object(proxy)) = &proxy else {
                return Ok(Some(text));
            };
            // The candidate is already a key string here, so the lookup does not need a
            // second `ToPropertyKey` conversion.
            let key = PropertyKey::from(text.as_str());
            let live = match self.proxy_get_own_property(proxy, &key)? {
                Some(descriptor) => descriptor,
                // Not a proxy after all, or it lost its state: fall back to the target.
                None => proxy.own_property_key(&key),
            };
            if live.is_some_and(|property| property.enumerable) {
                return Ok(Some(text));
            }
            // Withdrawn or non-enumerable: this key is skipped, and the loop looks at the
            // next one.
        }
    }

    /// The iterator for `for-of`: the object and its `next` method.
    ///
    /// An object that already carries a `next` method is used as it is — that is what
    /// `arr.values()` returns — and only arrays and strings get the built-in
    /// sequence iterator.
    /// Whether `value` has a callable `Symbol.iterator`, i.e. `GetMethod` succeeds.
    ///
    /// Callers that must choose between the iterable and the array-like protocol — the
    /// typed-array constructor is the main one — need to ask *before* committing to one.
    pub fn has_iterator(&mut self, value: &Value) -> JsResult<bool> {
        let key = self.realm.well_known_key(WellKnown::Iterator);
        let method = self.read_field_key(value, &key)?;
        if matches!(method, Value::Undefined | Value::Null) {
            return Ok(false);
        }
        if !matches!(&method, Value::Object(object) if object.is_callable()) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "Symbol.iterator is not callable"),
            ));
        }
        Ok(true)
    }

    /// Collect everything an iterable yields, closing it if the walk is abandoned.
    ///
    /// The cap is a guard against a generator that never ends: an unbounded collect would
    /// be an out-of-memory abort rather than a catchable error.
    pub fn collect_iterable(&mut self, value: &Value) -> JsResult<Vec<Value>> {
        const MAX_ITEMS: usize = 1 << 24;
        let iterator = self.get_iterator(value)?;
        let mut out = Vec::new();
        loop {
            let (element, done) = self.iterator_step(&iterator)?;
            if done {
                return Ok(out);
            }
            out.push(element);
            if out.len() > MAX_ITEMS {
                return Err(self.range_error("iterable is too long to collect"));
            }
        }
    }

    /// `OrdinaryCreateFromConstructor(new.target, default_proto)`.
    ///
    /// A native constructor calls this once it has validated its arguments, which is the
    /// order the spec uses for `Promise`, the typed arrays and `DataView`: a bad argument is
    /// a `TypeError` *before* `new.target.prototype` is read, so a throwing getter there is
    /// never reached. The prototype comes from `new.target` when it is an object, and from
    /// `default_proto` otherwise.
    pub fn new_ctor_instance(&mut self, default_proto: Gc<JsObject>) -> JsResult<Gc<JsObject>> {
        let Some(target) = self.pending_new_target.clone() else {
            return Ok(self.new_object(Some(default_proto)));
        };
        let Some(target) = target.as_object().cloned() else {
            return Ok(self.new_object(Some(default_proto)));
        };
        let proto = match self.get_property(&target, "prototype")? {
            Some(Value::Object(proto)) => proto,
            _ => default_proto,
        };
        Ok(self.new_object(Some(proto)))
    }

    /// Whether this call is a construction, i.e. `new.target` is defined.
    ///
    /// A native constructor receives `this = undefined` (it builds its own instance), so it
    /// cannot use "`this` is not an object" to tell `Ctor()` from `new Ctor()` — this can.
    /// The native function currently executing, if any.
    pub fn current_native(&self) -> Option<Gc<JsObject>> {
        self.current_native.clone()
    }

    /// Whether this call is a construction, i.e. `new.target` is defined.
    pub fn is_constructing(&self) -> bool {
        self.pending_new_target.is_some()
    }

    /// Whether the pending `new.target` needs its own `prototype` honoured by the constructor.
    ///
    /// True whenever `new.target` is not the callee itself, or when it has no usable
    /// `prototype` — the cases where reading the property is observable at all.
    pub fn new_target_is_custom(&mut self, callee: &Gc<JsObject>) -> bool {
        match &self.pending_new_target {
            Some(Value::Object(target)) => !target.ptr_eq(callee),
            Some(_) => true,
            None => false,
        }
    }

    /// The symbol key for one of the engine's own hidden slots.
    ///
    /// Cached on the realm, so every object of a kind shares one key and a slot can be
    /// read back by name.
    pub fn internal_slot(&self, name: &'static str) -> PropertyKey {
        if let Some(Value::Symbol(symbol)) = self.realm.internal_slots.borrow().get(name) {
            return PropertyKey::Symbol(symbol.clone());
        }
        let symbol = Gc::new(
            self.heap,
            crate::value::JsSymbol::new(Some(name.to_string())),
        );
        self.realm
            .internal_slots
            .borrow_mut()
            .insert(name, Value::Symbol(symbol.clone()));
        PropertyKey::Symbol(symbol)
    }

    /// Read a hidden slot, or `None` when the object has none of that name.
    pub fn internal_get(&self, object: &Gc<JsObject>, name: &'static str) -> Option<Value> {
        let slots = self.realm.internal_slots.borrow();
        let Value::Symbol(symbol) = slots.get(name)? else {
            return None;
        };
        object
            .own_property_key(&PropertyKey::Symbol(symbol.clone()))
            .and_then(|property| property.value)
    }

    /// Define a hidden slot.
    pub fn internal_set(&self, object: &Gc<JsObject>, name: &'static str, value: Value) {
        let key = self.internal_slot(name);
        object.define_key(&key, Property::with_attributes(value, false, false));
    }

    /// `GetIterator(value)`: the real `Symbol.iterator` protocol.
    ///
    /// The method is looked up on the value (through its prototype), called with no
    /// arguments, and its result becomes the iterator. A generator gets here through
    /// `%GeneratorPrototype%[Symbol.iterator]`, which answers `this`; an array through
    /// `Array.prototype[Symbol.iterator]`, which builds a one-shot index iterator.
    ///
    /// This used to be a shortcut — "has a callable `next`, or is an array" — because
    /// there were no symbols to key the protocol on. Now that there are, the shortcut is
    /// gone: a `for…of` over `null`, over a plain object, or over an object whose
    /// `Symbol.iterator` is not callable all report the same `TypeError` the reference
    /// does, and a user-defined iterable works without a special case.
    /// `GetIterator(value, async)`: `Symbol.asyncIterator`, falling back to
    /// `Symbol.iterator`.
    ///
    /// The fallback is what makes `for await (x of [1,2])` work: a plain synchronous
    /// iterable is legal there, and each value it produces is simply already settled by
    /// the time the `await` around it runs.
    fn get_async_iterator(&mut self, value: &Value) -> JsResult<Iterator> {
        let async_key = self.realm.well_known_key(WellKnown::AsyncIterator);
        let method = self.read_field_key(value, &async_key)?;
        let has_async = matches!(&method, Value::Object(object) if object.is_callable());
        if !has_async {
            return self.get_iterator(value);
        }
        let iterator = self.call_value(method, value.clone(), Vec::new())?;
        if !matches!(iterator, Value::Object(_)) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "iterator is not an object"),
            ));
        }
        let next = self.read_field(&iterator, "next")?;
        Ok(Iterator {
            object: iterator,
            next,
        })
    }

    pub(crate) fn get_iterator(&mut self, value: &Value) -> JsResult<Iterator> {
        let key = self.realm.well_known_key(WellKnown::Iterator);
        let method = self.read_field_key(value, &key)?;
        let Value::Object(function) = &method else {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                &format!("{} is not iterable", value.to_js_string()),
            )));
        };
        if !function.is_callable() {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                &format!("{} is not iterable", value.to_js_string()),
            )));
        }
        let iterator = self.call_value(method, value.clone(), Vec::new())?;
        // The spec requires an object here; a primitive iterator has no `next` to call
        // and would fail one step later with a confusing message.
        if !matches!(iterator, Value::Object(_)) {
            return Err(Signal::Throw(
                self.make_error("TypeError", "iterator is not an object"),
            ));
        }
        let next = self.read_field(&iterator, "next")?;
        Ok(Iterator {
            object: iterator,
            next,
        })
    }

    /// Drain anything with a `Symbol.iterator` method, or answer `None` when the value is
    /// not iterable.
    ///
    /// Shared so that `Array.from`, the `Promise` combinators and `for…of` all agree on
    /// what "iterable" means. A `Map` and a `Set` have no `length`, so an array-like-only
    /// reader silently produced an empty array for both — `Array.from(new Set([1,2]))`
    /// was `[]`.
    pub fn iterable_values(&mut self, value: &Value) -> JsResult<Option<Vec<Value>>> {
        let key = self.realm.well_known_key(WellKnown::Iterator);
        let method = self.read_field_key(value, &key)?;
        if !matches!(&method, Value::Object(object) if object.is_callable()) {
            return Ok(None);
        }
        let iterator = self.get_iterator(value)?;
        let mut out = Vec::new();
        loop {
            let (element, done) = self.iterator_step(&iterator)?;
            if done {
                return Ok(Some(out));
            }
            out.push(element);
        }
    }

    /// One step of an internal iterator: the value and whether it is exhausted.
    pub(crate) fn iterator_step(&mut self, iterator: &Iterator) -> JsResult<(Value, bool)> {
        // A real iterator (`next` is a function) answers with a result object.
        if matches!(&iterator.next, Value::Object(object) if object.is_callable()) {
            let result =
                self.call_value(iterator.next.clone(), iterator.object.clone(), Vec::new())?;
            let Some(result) = result.as_object().cloned() else {
                return Err(self.type_error("iterator result is not an object"));
            };
            let done = self
                .get_property(&result, "done")?
                .is_some_and(|value| value.to_boolean());
            let value = self
                .get_property(&result, "value")?
                .unwrap_or(Value::Undefined);
            return Ok((value, done));
        }
        // An iterator whose `next` is not callable is treated as exhausted, which is
        // what `for (x of {})` relies on: `for_of_start` has already refused the ones
        // that are not iterable at all.
        Ok((Value::Undefined, true))
    }

    /// `[...iterable]`: append everything the iterator yields, and answer the next
    /// index.
    ///
    /// This goes through the same iterator machinery `for-of` uses rather than copying
    /// an array's elements, because a spread source is any iterable: a string spreads
    /// into its characters and an array iterator spreads into its results. Handling
    /// only arrays made `[..."ab"]` come out empty with no error at all.
    fn append_enumerate(&mut self, array: &Value, iterable: &Value) -> JsResult<i32> {
        let Some(target) = array.as_object().cloned() else {
            return Ok(0);
        };
        let start = target.array_length().unwrap_or(0);
        let mut added = 0;
        let iterator = self.get_iterator(iterable)?;
        loop {
            let (value, done) = self.iterator_step(&iterator)?;
            if done {
                break;
            }
            target.array_set(start + added, value);
            added += 1;
        }
        Ok((start + added) as i32)
    }

    /// How an uncaught exception is reported.
    ///
    /// The reference reports a pending exception through `JS_PrintValue`, which ends
    /// up calling the thrown value's `toString`, so `throw new TypeError("x")` prints
    /// `TypeError: x`. Reading `name`/`message` off the object directly got this wrong
    /// two ways: `name` usually lives on the prototype rather than on the instance, and
    /// a thrown non-error object was not consulted at all, coming out as
    /// `[object Object]`. Routing through `ToString` keeps every case on one path and
    /// honours a user-defined `toString`, as the reference does.
    ///
    /// A `toString` that throws must not take the reporter down with it, so that case
    /// falls back to the type-level rendering. The reference's printer also walks into
    /// plain objects (`throw 1, {a:1}` prints `{ a: 1 }`); that formatting is a gap
    /// shared with `console.log`.
    pub fn describe_exception(&mut self, value: &Value) -> String {
        self.to_js_string(value).unwrap_or_else(|_| display(value))
    }

    /// The current call stack, formatted the way `qjs` prints `error.stack`.
    ///
    /// One `    at name (file:line:col)` line per active frame, innermost first. The
    /// position is the one recorded for the instruction each frame is stopped on, which
    /// is why the compiler builds a pc-to-position table per function: without it a
    /// trace could only name the function, not the line inside it.
    ///
    /// A frame whose code has no positions prints just `(file)`, matching the
    /// reference's output for stripped bytecode.
    pub fn stack_trace(&self) -> String {
        self.stack_trace_skipping(0)
    }

    /// [`Interp::stack_trace`] with the `skip` innermost frames left out.
    fn stack_trace_skipping(&self, skip: usize) -> String {
        // One ordered list, innermost first, so `skip` counts the same thing whether the
        // frames it leaves out are native or bytecode. Indices rather than references: a
        // `Frame` borrows from `self`, and a mixed list of borrows cannot name that lifetime.
        let depth = self.frames.len();
        let mut order: Vec<(bool, usize)> = Vec::with_capacity(depth + self.native_frames.len());
        // A native entered while `at` bytecode frames were live was called *from* the
        // innermost of those, so it belongs between frame `at` and frame `at - 1`. Natives
        // entered at the current depth are therefore innermost of all.
        let push_natives = |order: &mut Vec<(bool, usize)>, wanted: usize| {
            for (index, (_, _, at)) in self.native_frames.iter().enumerate().rev() {
                if *at == wanted {
                    order.push((true, index));
                }
            }
        };
        push_natives(&mut order, depth);
        for index in (0..depth).rev() {
            order.push((false, index));
            push_natives(&mut order, index);
        }
        // A `backtrace_barrier` eval hides its caller: keep only what is at or inside the
        // barrier. Natives entered at exactly the barrier depth are the eval entry point
        // itself (`evalScript (native)`), and the reference leaves those out too.
        if let Some(barrier) = self.trace_barriers.last().copied() {
            order.retain(|(is_native, index)| match is_native {
                true => *index == usize::MAX || self.native_frames[*index].2 > barrier,
                false => *index >= barrier,
            });
        }

        let mut out = String::new();
        for (is_native, index) in order.into_iter().skip(skip) {
            match is_native {
                true => {
                    out.push_str("    at ");
                    out.push_str(&self.native_frames[index].0);
                    out.push_str(" (native)\n");
                }
                false => {
                    let frame = &self.frames[index];
                    let code = &frame.code.code;
                    let file = code.filename.as_deref().unwrap_or("<input>");
                    // The name comes from the function *object*, not from the compiled code:
                    // an anonymous function expression is named by the property it is stored
                    // in (`{valueOf: function(){}}`), which only the object knows.
                    let bound_name = match &frame.callee {
                        Value::Object(object) => {
                            object
                                .own_property("name")
                                .and_then(|property| match property.value {
                                    Some(Value::String(text)) if !text.wtf8().is_empty() => {
                                        Some(text.to_string_lossy().into_owned())
                                    }
                                    _ => None,
                                })
                        }
                        _ => None,
                    };
                    out.push_str("    at ");
                    out.push_str(bound_name.as_deref().unwrap_or(&code.name));
                    match code.position_at(frame.at as u32) {
                        Some((line, column)) => {
                            out.push_str(&format!(" ({file}:{line}:{column})\n"));
                        }
                        None => out.push_str(&format!(" ({file})\n")),
                    }
                }
            }
        }
        out
    }

    /// Give an anonymous function the name of the property it is stored in.
    ///
    /// `{m: function(){}}.m.name` is `"m"`. The name property is not writable, not
    /// enumerable and not configurable, and a function that already has a real name
    /// keeps it.
    ///
    /// "Already named" means a *non-empty* name: every function object is created with
    /// a `name` of `""` so the property exists on all of them, and treating that empty
    /// string as a name left every anonymous function nameless — which is how a stack
    /// trace came out as `at  (file:1:2)` instead of `at valueOf (file:1:2)`.
    fn set_function_name(&mut self, function: &Value, name: &str) {
        let Value::Object(object) = function else {
            return;
        };
        // `SetFunctionName` applies to *functions* and classes only, and only when they are
        // anonymous. Without the callability guard, `{value: someObject}` renamed
        // `someObject` to `"value"` — silently mutating an unrelated object that merely sat
        // on the right-hand side of a property. That is how a `zipKeyed` result object
        // acquired a `name` property, and it applied to `Object.defineProperty` too, which
        // has no `NamedEvaluation` at all.
        if !object.is_callable() {
            return;
        }
        if let Some(Value::String(existing)) = object.own_property("name").and_then(|p| p.value)
            && !existing.wtf8().is_empty()
        {
            return;
        }
        let text = self.string(name);
        object.define("name", Property::name_or_length(text));
    }

    // -- global declarations -----------------------------------------------

    /// Declare a global binding, creating its reference cell.
    /// Declare a `var` or function global, whose home is the global object.
    ///
    /// Writable and enumerable but not configurable: `globalThis.v` reads it,
    /// `for-in` lists it, `delete` refuses. A name that is already there — a built-in
    /// such as `print`, or a redeclaration — is left alone.
    pub fn declare_var_global(&mut self, name: &str) {
        // A strict `eval`'s `var` belongs to the eval's own scope: declaring it on the
        // global object is exactly the leak `typeof my_var === "undefined"` after
        // `eval("var my_var=2")` rules out.
        if let Some(scope) = self.eval_scope.as_mut() {
            scope.entry(name.to_string()).or_insert(Value::Undefined);
            return;
        }
        // A sloppy direct eval declares into its *caller's* variable environment. Doing
        // this on the global object instead made `eval("var c = 1")` inside a parameter
        // list overwrite a global `c`, and made two calls to the same function share one
        // binding.
        if let Some(cell) = self.declare_eval_local(name) {
            cell.set(Value::Undefined);
            return;
        }
        let global = self.realm.global.clone();
        if global.own_property(name).is_some() {
            return;
        }
        // A `var` in a *direct* declaration is non-configurable, but one introduced by an
        // eval — `(1, eval)("var g")` — is created with `configurable: true`, which is the
        // whole point of that trick and what lets `Object.defineProperty` reshape it
        // afterwards. `eval_scope` is `Some` while an eval is running.
        let from_eval = self.eval_depth > 0 || self.eval_scope.is_some();
        global.define(
            name,
            Property {
                value: Some(Value::Undefined),
                writable: true,
                enumerable: true,
                configurable: from_eval,
                ..Property::default()
            },
        );
    }

    /// Declare a *lexical* global binding, which starts in its dead zone.
    ///
    /// Only `let`/`const`/`class` need this: a `var` global is a property of the
    /// global object and is created when its initialiser runs.
    pub fn declare_global(&mut self, name: &str, is_const: bool) {
        if is_const {
            self.realm.lexical_consts.insert(name.to_string());
        }
        if !self.realm.globals.contains_key(name) {
            // `Uninitialized` rather than `Undefined`: the binding exists but is not
            // readable until its initialiser runs, and that is what `let x` before
            // `x = 1` has to observe.
            let cell = self.new_cell(Value::Uninitialized);
            self.realm.globals.insert(name.to_string(), cell);
            self.realm.declared.push(name.to_string());
        }
    }
}

/// The bytecode and environment of a function object, if it is a bytecode function.
///
/// Extracted before a call so no borrow of the object is held across it.
fn bytecode_of(function: &Gc<JsObject>) -> Option<(Gc<CompiledCode>, Gc<ClosureEnv>)> {
    function.with(|data| match &data.kind {
        ObjectKind::Function(Callable::Bytecode { code, env }) => Some((code.clone(), env.clone())),
        _ => None,
    })
}

/// A native call target, extracted before the call so no borrow is held.
enum NativeCall {
    Native(NativeFn),
    /// A host closure. The `Rc` was cloned out of the function object, so calling it does
    /// not hold that object's `RefCell` borrow.
    Host(std::rc::Rc<std::cell::RefCell<crate::object::HostFn>>),
    Bytecode(Gc<CompiledCode>, Gc<ClosureEnv>),
    Bound {
        target: Value,
        this: Value,
        leading: Vec<Value>,
    },
    /// `Proxy.revocable`'s `revoke`: clear the proxy's slots and answer `undefined`.
    Revoker(crate::object::ProxyRef),
    /// A promise's `resolve`.
    PromiseResolve(Gc<JsObject>),
    /// A promise's `reject`.
    PromiseReject(Gc<JsObject>),
    /// One continuation of an `await`.
    AwaitResume {
        coroutine: Gc<JsObject>,
        promise: Gc<JsObject>,
        kind: ResumeKind,
    },
    /// One element of a `Promise` combinator settling.
    CombinatorSettle {
        state: Gc<JsObject>,
        index: usize,
        fulfilled: bool,
    },
    /// One of `Promise.prototype.finally`'s two shims.
    FinallyShim {
        callback: Option<Value>,
        fulfilled: bool,
    },
    /// Settle a promise with a fixed outcome.
    SettlePromise {
        promise: Gc<JsObject>,
        value: Value,
        rejected: bool,
    },
    /// Continue an async generator after its `await` settled.
    GeneratorResume {
        generator: Gc<JsObject>,
        promise: Gc<JsObject>,
        kind: ResumeKind,
    },
    /// Read a module namespace's binding out of its cell.
    ModuleBinding(Gc<Cell>),
    /// Answer a pending async-generator method with `{value, done}`.
    YieldResult {
        promise: Gc<JsObject>,
        done: bool,
    },
    /// One step of `AsyncDisposableStack.disposeAsync`.
    DisposeStep {
        remaining: Gc<JsObject>,
        promise: Gc<JsObject>,
        error: Value,
        rejected: bool,
    },
}

/// What a store does when the object refuses the write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeleteFailure {
    /// Raise a `TypeError`, which is what strict mode and every built-in does.
    Throw,
    /// Ignore the write, which is what a sloppy-mode assignment does.
    Ignore,
}

/// Whether `object` is a `Date` instance, by the class tag it carries.
///
/// Lives here rather than in `builtins::date` so `ToPrimitive` can ask without the
/// builtins module depending on the interpreter's conversion order.
fn is_date_object(interp: &Interp<'_>, object: &Gc<JsObject>) -> bool {
    interp
        .internal_get(object, "_class")
        .is_some_and(|value| value.to_js_string() == "Date")
}

/// Which type `ToPrimitive` prefers when it converts an object.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// The default hint, which behaves like [`Hint::Number`].
    Default,
    /// `Number(obj)` and the arithmetic operators.
    Number,
    /// `String(obj)` and `ToPropertyKey`.
    String,
}

/// Which table a binding slot indexes, for the messages that name it.
#[derive(Clone, Copy)]
enum VarSlot {
    Loc,
    Closure,
}

/// `%IteratorPrototype%[Symbol.iterator]`, which answers the iterator itself.
fn self_iterator(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(this)
}

/// An internal iterator: the sequence plus a cursor.
pub(crate) struct Iterator {
    pub(crate) object: Value,
    pub(crate) next: Value,
}

/// The name an accessor function gets: `get a` for a getter, `set a` for a setter,
/// and plain `a` for a method.
///
/// The reference prefixes accessors (`js_method_set_properties`), and the prefix is
/// visible: `{get a(){}}.a.name` is `"get a"`, and a stack trace from inside a getter
/// reads `at get a (file:1:2)`.
fn key_with_accessor_prefix(key: &str, flags: u32) -> String {
    match flags & 3 {
        1 => format!("get {key}"),
        2 => format!("set {key}"),
        _ => key.to_string(),
    }
}

/// Define a method or accessor property, per the `define_method` flag byte.
///
/// The low two bits pick value/getter/setter and bit 2 marks the property
/// enumerable.
///
/// A getter and a setter arrive as two separate `define_method` instructions, and the
/// second one must only fill in its own half: `{get z(){…}, set z(v){…}}` defines
/// `z` twice, and replacing the property wholesale dropped the getter, so `a.z` read
/// `undefined`. The reference passes `JS_PROP_HAS_GET`/`JS_PROP_HAS_SET` to
/// `JS_DefineProperty`, which leaves the other half alone.
fn define_function_property(object: &Gc<JsObject>, key: &PropertyKey, function: Value, flags: u32) {
    let enumerable = flags & 4 != 0;
    let kind = flags & 3;
    let existing = object.own_property_key(key);
    let property = match kind {
        1 => Property {
            getter: Some(function),
            // The setter half, if this literal already defined one.
            setter: existing
                .filter(Property::is_accessor)
                .and_then(|p| p.setter),
            enumerable,
            configurable: true,
            ..Property::default()
        },
        2 => Property {
            getter: existing
                .filter(Property::is_accessor)
                .and_then(|p| p.getter),
            setter: Some(function),
            enumerable,
            configurable: true,
            ..Property::default()
        },
        _ => Property {
            value: Some(function),
            writable: true,
            enumerable,
            configurable: true,
            ..Property::default()
        },
    };
    object.define_key(key, property);
}

// ---------------------------------------------------------------------------
// Operand decoding
// ---------------------------------------------------------------------------

/// Decode the instruction at `pc` into `(opcode, operand a, operand b, next pc)`.
///
/// Jump operands are resolved to absolute byte offsets here, because the encoding
/// stores them relative to the byte after the opcode and every use site would
/// otherwise repeat that arithmetic.
fn decode(code: &[u8], pc: usize) -> Option<(Opcode, u32, u32, usize)> {
    let op = Opcode::from_u8(*code.get(pc)?)?;
    let info = op.info();
    let base = pc + 1;
    let read_u8 = |offset: usize| -> u32 { code.get(base + offset).copied().unwrap_or(0) as u32 };
    let read_u16 = |offset: usize| -> u32 {
        let mut value = 0u32;
        for index in 0..2 {
            value = (value << 8) | code.get(base + offset + index).copied().unwrap_or(0) as u32;
        }
        value
    };
    let read_u32 = |offset: usize| -> u32 {
        let mut value = 0u32;
        for index in 0..4 {
            value = (value << 8) | code.get(base + offset + index).copied().unwrap_or(0) as u32;
        }
        value
    };
    let jump = |offset: usize, width: usize| -> u32 {
        let raw = match width {
            1 => read_u8(offset) as u8 as i8 as i64,
            2 => read_u16(offset) as u16 as i16 as i64,
            _ => read_u32(offset) as i32 as i64,
        };
        (base as i64 + raw) as u32
    };
    let (a, b) = match info.format {
        OpFormat::None
        | OpFormat::NoneInt
        | OpFormat::NoneLoc
        | OpFormat::NoneArg
        | OpFormat::NoneVarRef
        | OpFormat::Npopx => (0, 0),
        OpFormat::U8 | OpFormat::Loc8 | OpFormat::Const8 => (read_u8(0), 0),
        OpFormat::I8 => (read_u8(0) as u8 as i8 as i32 as u32, 0),
        OpFormat::Label8 => (jump(0, 1), 0),
        OpFormat::U16 | OpFormat::Npop | OpFormat::Loc | OpFormat::Arg | OpFormat::VarRef => {
            (read_u16(0), 0)
        }
        OpFormat::I16 => (read_u16(0) as u16 as i16 as i32 as u32, 0),
        OpFormat::Label16 => (jump(0, 2), 0),
        OpFormat::NpopU16 => (read_u16(0), read_u16(2)),
        OpFormat::U32 | OpFormat::Const | OpFormat::Atom => (read_u32(0), 0),
        OpFormat::I32 | OpFormat::Label => (
            if info.format == OpFormat::Label {
                jump(0, 4)
            } else {
                read_u32(0) as i32 as u32
            },
            0,
        ),
        OpFormat::AtomU8 | OpFormat::AtomLabelU8 => (read_u32(0), read_u8(4)),
        OpFormat::AtomU16 | OpFormat::AtomLabelU16 | OpFormat::LabelU16 => {
            (read_u32(0), read_u16(4))
        }
    };
    Some((op, a, b, pc + info.size as usize))
}

/// How `print` renders a value: the reference's `JS_PrintValue`.
///
/// This is *not* `ToString`. `print({a:1})` shows `{ a: 1 }` and `print([1])` shows
/// `[ 1 ]`, because the host printer dumps the structure rather than converting it.
/// The format is reproduced closely because the test suite compares output text:
///
/// * an empty container has two spaces inside its brackets — `[  ]`, `{  }`;
/// * nesting stops at depth 2 and prints `[Array]` / `[Object]`;
/// * a cycle prints `[circular N]` with `N` the stack index of the object;
/// * an array with an interior hole prints as `idx: value` pairs, while holes at the
///   end print as `<N empty item(s)>`;
/// * a string is quoted inside a container but raw at the top level, which is why
///   `print("x")` and `print(["x"])` differ.
pub fn debug_display(interp: &Interp<'_>, value: &Value) -> String {
    // A string argument is printed raw: `js_print` converts it with `ToString`
    // instead of dumping it, and the quoting only appears inside a container.
    if let Value::String(text) = value {
        return text.to_string_lossy().into_owned();
    }
    let mut printer = Printer {
        interp,
        out: String::new(),
        stack: Vec::new(),
    };
    printer.value(value, 0);
    printer.out
}

/// The internal class name an object carries, when it has one.
fn class_of(interp: &Interp<'_>, object: &Gc<JsObject>) -> Option<String> {
    interp
        .internal_get(object, "_class")
        .map(|value| value.to_js_string())
}

/// A string property, looked up the way `GetProperty` does: own first, then the
/// prototype chain. Needed because the print formatter holds the interpreter
/// immutably and so cannot run getters.
fn inherited(object: &Gc<JsObject>, name: &str) -> Option<String> {
    let mut current = Some(object.clone());
    // Bounded, because a prototype cycle would otherwise loop forever.
    for _ in 0..100 {
        let node = current?;
        if let Some(property) = node.own_property(name)
            && let Some(value) = property.value
        {
            return Some(value.to_js_string());
        }
        current = node.with(|data| data.proto.clone());
    }
    None
}

/// The `JS_PrintValue` state: output, and the objects currently being printed.
struct Printer<'a, 'rt> {
    /// Kept for the class-name lookups that typed arrays and `Date` will need.
    #[allow(dead_code)]
    interp: &'a Interp<'rt>,
    out: String,
    stack: Vec<Gc<JsObject>>,
}

impl Printer<'_, '_> {
    /// Depth past which a container is summarised by its class name.
    const MAX_DEPTH: usize = 2;

    fn value(&mut self, value: &Value, level: usize) {
        match value {
            Value::Undefined
            | Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => self.out.push_str("undefined"),
            Value::Symbol(symbol) => self.out.push_str(&symbol.to_display_string()),
            // The printer writes a bigint the way the language spells one, so a dump is
            // unambiguous about which kind of number it is showing.
            Value::BigInt(big) => {
                self.out.push_str(&big.to_decimal_string());
                self.out.push('n');
            }
            Value::Null => self.out.push_str("null"),
            Value::Bool(true) => self.out.push_str("true"),
            Value::Bool(false) => self.out.push_str("false"),
            Value::Int(number) => self.out.push_str(&number.to_string()),
            Value::Float(number) => {
                self.out
                    .push_str(&crate::value::number_to_string_for_print(*number));
            }
            Value::String(text) => self.print_string(&text.to_string_lossy()),
            Value::Object(object) => self.object(object, level),
        }
    }

    fn object(&mut self, object: &Gc<JsObject>, level: usize) {
        // A cycle is reported rather than followed; the index is where the object
        // already sits in the print stack, which is what the reference shows.
        if let Some(index) = self.stack.iter().position(|seen| seen.ptr_eq(object)) {
            self.out.push_str(&format!("[circular {index}]"));
            return;
        }
        if object.is_callable() {
            let name = object
                .own_property("name")
                .and_then(|property| property.value)
                .map(|value| value.to_js_string())
                .unwrap_or_default();
            if name.is_empty() {
                self.out.push_str("[Function (anonymous)]");
            } else {
                self.out.push_str(&format!("[Function {name}]"));
            }
            return;
        }
        if level >= Self::MAX_DEPTH {
            let class = if object.is_array() { "Array" } else { "Object" };
            self.out.push_str(&format!("[{class}]"));
            return;
        }
        // `Date` and `Error` print as themselves rather than as bags of properties:
        // the ISO string, and `Name: message` followed by the trace.
        if let Some(class) = class_of(self.interp, object) {
            if class == "Date" {
                self.date(object);
                return;
            }
            if class == "Error" {
                self.error(object);
                return;
            }
        }
        self.stack.push(object.clone());
        if object.is_array() {
            self.array(object, level);
        } else {
            self.plain_object(object, level);
        }
        self.stack.pop();
    }

    /// A Date prints as its ISO string; an invalid one falls through to the plain
    /// object form, the way the reference falls back when `toISOString` throws.
    fn date(&mut self, object: &Gc<JsObject>) {
        let value = self
            .interp
            .internal_get(object, "_date_value")
            .map(|value| value.to_number());
        match value.and_then(crate::builtins::date::iso_string) {
            Some(text) => self.out.push_str(&text),
            None => self.plain_object(object, 0),
        }
    }

    /// `Name: message`, then the trace with its trailing newline removed.
    fn error(&mut self, object: &Gc<JsObject>) {
        let name = inherited(object, "name").unwrap_or_else(|| "Error".to_string());
        self.out.push_str(&name);
        if let Some(message) = inherited(object, "message")
            && !message.is_empty()
        {
            self.out.push_str(": ");
            self.out.push_str(&message);
        }
        if let Some(stack) = inherited(object, "stack") {
            self.out.push('\n');
            self.out.push_str(stack.trim_end_matches('\n'));
        }
    }

    fn array(&mut self, object: &Gc<JsObject>, level: usize) {
        let elements = object.array_elements().unwrap_or_default();
        self.out.push_str("[ ");
        // An interior hole means the array is "sparse": the reference prints those
        // as `index: value` pairs and says nothing about the holes themselves, while
        // a hole run at the end becomes a single `<N empty items>` note.
        let last_present = elements
            .iter()
            .rposition(|value| !matches!(value, Value::Hole));
        let has_interior_hole = match last_present {
            Some(last) => elements[..last]
                .iter()
                .any(|value| matches!(value, Value::Hole)),
            None => false,
        };
        let mut first = true;
        if has_interior_hole {
            let mut index = 0;
            while index < elements.len() {
                let value = &elements[index];
                if matches!(value, Value::Hole) {
                    index += 1;
                    continue;
                }
                if !first {
                    self.out.push_str(", ");
                }
                first = false;
                self.out.push_str(&format!("{index}: "));
                self.value(value, level + 1);
                index += 1;
            }
        } else {
            for (index, value) in elements.iter().enumerate() {
                if matches!(value, Value::Hole) {
                    continue;
                }
                if !first {
                    self.out.push_str(", ");
                }
                first = false;
                let _ = index;
                self.value(value, level + 1);
            }
            if let Some(last) = last_present {
                let trailing = elements.len() - last - 1;
                if trailing > 0 {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&format!(
                        "<{trailing} empty item{}>",
                        if trailing > 1 { "s" } else { "" }
                    ));
                }
            } else if !elements.is_empty() {
                let count = elements.len();
                self.out.push_str(&format!(
                    "<{count} empty item{}>",
                    if count > 1 { "s" } else { "" }
                ));
            }
        }
        self.out.push_str(" ]");
    }

    fn plain_object(&mut self, object: &Gc<JsObject>, level: usize) {
        self.out.push_str("{ ");
        let mut first = true;
        for key in object.own_keys() {
            let Some(property) = object.own_property_key(&PropertyKey::from(key.clone())) else {
                continue;
            };
            if !property.enumerable {
                continue;
            }
            if !first {
                self.out.push_str(", ");
            }
            first = false;
            self.out
                .push_str(&js_compile::disasm::print_atom(&Atom::from_text(
                    &key.to_string_lossy(),
                )));
            self.out.push_str(": ");
            // An accessor has no value to dump; the reference names the half that
            // exists instead of calling it.
            if property.is_accessor() {
                let text = match (property.getter.is_some(), property.setter.is_some()) {
                    (true, true) => "[Getter/Setter]",
                    (false, true) => "[Setter]",
                    _ => "[Getter]",
                };
                self.out.push_str(text);
            } else {
                self.value(&property.value.unwrap_or(Value::Undefined), level + 1);
            }
        }
        self.out.push_str(" }");
    }

    /// A string inside a container, quoted with the escapes `js_print_string1` uses.
    fn print_string(&mut self, text: &str) {
        self.out.push('"');
        for character in text.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    self.out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }
}

/// How `print` renders a value.
///
/// `ToString`, except that negative zero keeps its sign — the reference's host
/// printer does that while `String(-0)` is `"0"`.
pub fn display(value: &Value) -> String {
    match value {
        Value::Float(number) => crate::value::number_to_string_for_print(*number),
        Value::Uninitialized | Value::CatchOffset(_) | Value::ReturnAddress(_) => {
            "undefined".to_string()
        }
        other => other.to_js_string(),
    }
}

// ---------------------------------------------------------------------------
// Numeric operators
// ---------------------------------------------------------------------------

/// `a + b` keeping the `Int` representation when it fits.
fn add_numbers(left: Value, right: Value) -> Value {
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => match a.checked_add(*b) {
            Some(sum) => Value::Int(sum),
            None => Value::Float(*a as f64 + *b as f64),
        },
        _ => Value::Float(left.to_number() + right.to_number()),
    }
}

/// The arithmetic operators other than `+`, which needs string concatenation.
fn arithmetic(op: Opcode, left: Value, right: Value) -> JsResult<Value> {
    use Opcode::*;
    let (a, b) = (left.to_number(), right.to_number());
    Ok(match op {
        Sub => match (&left, &right) {
            (Value::Int(x), Value::Int(y)) => match x.checked_sub(*y) {
                Some(value) => Value::Int(value),
                None => Value::Float(a - b),
            },
            _ => Value::Float(a - b),
        },
        Mul => Value::Float(a * b),
        Div => Value::Float(a / b),
        Mod => Value::Float(js_mod(a, b)),
        Pow => Value::Float(a.powf(b)),
        And => Value::Int(left.to_int32() & right.to_int32()),
        Or => Value::Int(left.to_int32() | right.to_int32()),
        Xor => Value::Int(left.to_int32() ^ right.to_int32()),
        Shl => Value::Int(left.to_int32().wrapping_shl(right.to_uint32() & 31)),
        Sar => Value::Int(left.to_int32().wrapping_shr(right.to_uint32() & 31)),
        // `>>>` is the *unsigned* shift, so its result is a uint32 and can exceed
        // `i32::MAX`: `(1<<31) >>> 0` is `2147483648`, not `-2147483648`. A value that
        // fits the small-integer representation stays an `Int`; the rest become a
        // float, which is how the reference keeps `n >>> 0` non-negative.
        Shr => {
            let shifted = left.to_uint32() >> (right.to_uint32() & 31);
            match i32::try_from(shifted) {
                Ok(value) => Value::Int(value),
                Err(_) => Value::Float(shifted as f64),
            }
        }
        _ => Value::Float(a),
    })
}

/// `%`, which follows the sign of the dividend rather than Rust's.
fn js_mod(left: f64, right: f64) -> f64 {
    if right == 0.0 || left.is_nan() || right.is_nan() || left.is_infinite() {
        return f64::NAN;
    }
    if right.is_infinite() {
        return left;
    }
    let remainder = left % right;
    // `-1 % 3` is `-1` in both languages, so only the exact-zero case needs care:
    // the spec makes the sign match the dividend.
    if remainder == 0.0 {
        0.0f64.copysign(left)
    } else {
        remainder
    }
}

fn negate(value: Value) -> Value {
    match value {
        Value::Int(0) => Value::Float(-0.0),
        Value::Int(i) => Value::Int(-i),
        other => Value::Float(-other.to_number()),
    }
}

fn increment(value: Value) -> Value {
    match value {
        Value::Int(i) => match i.checked_add(1) {
            Some(next) => Value::Int(next),
            None => Value::Float(i as f64 + 1.0),
        },
        other => Value::Float(other.to_number() + 1.0),
    }
}

fn decrement(value: Value) -> Value {
    match value {
        Value::Int(i) => match i.checked_sub(1) {
            Some(next) => Value::Int(next),
            None => Value::Float(i as f64 - 1.0),
        },
        other => Value::Float(other.to_number() - 1.0),
    }
}

/// Compare a `BigInt` with an `f64` on their mathematical values.
///
/// Neither side is rounded to the other's precision: the number is split into its integer
/// part (compared as an exact bigint) and its fraction, which is what decides the answer
/// when the integer parts are equal. An infinite value is larger than every bigint.
fn compare_bigint_and_f64(big: &crate::bigint::BigInt, number: f64) -> Option<std::cmp::Ordering> {
    use std::cmp::Ordering;
    if number.is_nan() {
        return None;
    }
    if number.is_infinite() {
        return Some(if number > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        });
    }
    let truncated = number.trunc();
    // A number wider than `i128` still has an exact integer part, but no bigint form we can
    // build cheaply; the sign of the difference is all that is needed, so compare against
    // the bigint's own magnitude instead.
    if truncated.abs() >= 1.7e38 {
        let big_as_f64 = big.to_f64();
        return big_as_f64.partial_cmp(&number);
    }
    let integer = crate::bigint::BigInt::from_f64_truncated(truncated);
    match big.cmp(&integer) {
        Ordering::Equal => {
            // Same integer part: the fraction decides, and a negative number's fraction is
            // below the integer (`-1.5` truncates to `-1`).
            if number > truncated {
                Some(Ordering::Less)
            } else if number < truncated {
                Some(Ordering::Greater)
            } else {
                Some(Ordering::Equal)
            }
        }
        other => Some(other),
    }
}

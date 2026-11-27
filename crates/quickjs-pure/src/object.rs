//! The object model: property tables, arrays, functions, and the prototype chain.
//!
//! QuickJS splits an object into a *shape* (the property names and their
//! attributes, shared between objects with the same layout) and a *property
//! vector* (the values). That split is a performance feature, not a semantic one,
//! and it only pays off once objects are created in bulk; this model keeps the
//! property table per object, in insertion order, and therefore reproduces the
//! ordering rules directly.
//!
//! Insertion order is load-bearing: `for (k in o)` and `Object.keys` iterate in the
//! order properties were defined, with integer-like keys first in ascending order.

use std::cell::RefCell;

use js_compile::bytecode::Function as CompiledFunction;
use js_gc::{Gc, Trace, Tracer, WeakGc};

use std::fmt;
use std::rc::Rc;

use crate::value::{Cell, JsSymbol, Value};

/// The bytes an `ArrayBuffer` owns, plus whether they have been detached.
///
/// `transfer()` moves the bytes to a new buffer and leaves this one empty and detached; a
/// view of a detached buffer reads `undefined` from every index.
pub struct BufferStorage {
    /// The bytes, in memory order.
    pub bytes: Vec<u8>,
    /// Whether `transfer()` has taken the bytes away.
    pub detached: bool,
}

impl JsObject {
    /// The storage of this object, when it is an `ArrayBuffer`.
    pub fn array_buffer_storage(&self) -> Option<Rc<RefCell<BufferStorage>>> {
        match &self.data.borrow().kind {
            ObjectKind::ArrayBuffer(storage) => Some(storage.clone()),
            _ => None,
        }
    }

    /// Make this object an `ArrayBuffer` owning `storage`.
    pub fn set_array_buffer(&self, storage: Rc<RefCell<BufferStorage>>) {
        self.data.borrow_mut().kind = ObjectKind::ArrayBuffer(storage);
    }

    /// The entries of this object, when it is a `Map` or a `Set`.
    pub fn collection_entries(&self) -> Option<Vec<(Value, Value)>> {
        match &self.data.borrow().kind {
            ObjectKind::Collection { entries, .. } => Some(entries.clone()),
            _ => None,
        }
    }

    /// Whether this object is a `Set` rather than a `Map`.
    pub fn collection_is_set(&self) -> Option<bool> {
        match &self.data.borrow().kind {
            ObjectKind::Collection { is_set, .. } => Some(*is_set),
            _ => None,
        }
    }

    /// Make this object a `Map` or `Set` holding `entries`.
    pub fn set_collection(&self, entries: Vec<(Value, Value)>, is_set: bool) {
        self.data.borrow_mut().kind = ObjectKind::Collection { entries, is_set };
    }

    /// This object's weak entries, when it is a `WeakMap` or `WeakSet`.
    ///
    /// Expired handles are filtered out here rather than at the call site, so every reader
    /// sees the same live view of the map and a dead key can never be observed.
    pub fn weak_entries(&self) -> Option<Vec<(Value, Value)>> {
        match &self.data.borrow().kind {
            ObjectKind::WeakCollection { entries, .. } => Some(
                entries
                    .iter()
                    .filter_map(|(key, value)| key.upgrade().map(|key| (key, value.clone())))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// Whether this object is a `WeakSet` rather than a `WeakMap`.
    pub fn weak_is_set(&self) -> Option<bool> {
        match &self.data.borrow().kind {
            ObjectKind::WeakCollection { is_set, .. } => Some(*is_set),
            _ => None,
        }
    }

    /// Make this object a `WeakMap` or `WeakSet`, replacing any entries it had.
    pub fn set_weak_collection(&self, entries: Vec<(WeakKey, Value)>, is_set: bool) {
        self.data.borrow_mut().kind = ObjectKind::WeakCollection { entries, is_set };
    }

    /// Store one entry in a weak collection, replacing the value for a live equal key.
    ///
    /// The comparison is pointer identity: a weak key has to be an object, and two objects
    /// are the same key exactly when they are the same object. Expired entries are dropped
    /// on the way through, which keeps the vector from growing without bound in a program
    /// that churns keys.
    pub fn weak_set(&self, key: WeakKey, value: Value) {
        let mut data = self.data.borrow_mut();
        let ObjectKind::WeakCollection { entries, .. } = &mut data.kind else {
            return;
        };
        entries.retain(|(existing, _)| !existing.is_expired());
        for (existing, slot) in entries.iter_mut() {
            if existing.same(&key) {
                *slot = value;
                return;
            }
        }
        entries.push((key, value));
    }

    /// Remove `key` from a weak collection, answering whether it was there.
    pub fn weak_delete(&self, key: &Value) -> bool {
        let mut data = self.data.borrow_mut();
        let ObjectKind::WeakCollection { entries, .. } = &mut data.kind else {
            return false;
        };
        let before = entries.len();
        entries.retain(|(existing, _)| !existing.is_expired() && !existing.matches(key));
        entries.len() != before
    }

    /// The target of a `WeakRef`, when this object is one.
    pub fn weak_ref_target(&self) -> Option<WeakKey> {
        match &self.data.borrow().kind {
            ObjectKind::WeakRef { target } => Some(target.clone()),
            _ => None,
        }
    }

    /// Make this object a `WeakRef` to `target`.
    pub fn set_weak_ref(&self, target: WeakKey) {
        self.data.borrow_mut().kind = ObjectKind::WeakRef { target };
    }

    /// The registry state, when this object is a `FinalizationRegistry`.
    pub fn finalization_state(&self) -> Option<Box<FinalizationState>> {
        match &self.data.borrow().kind {
            ObjectKind::Finalization(state) => Some(Box::new(FinalizationState {
                registrations: state
                    .registrations
                    .iter()
                    .map(|registration| Registration {
                        target: registration.target.clone(),
                        held_value: registration.held_value.clone(),
                        token: registration.token.clone(),
                    })
                    .collect(),
                cleanup: state.cleanup.clone(),
            })),
            _ => None,
        }
    }

    /// Replace a registry's state.
    pub fn set_finalization_state(&self, state: FinalizationState) {
        self.data.borrow_mut().kind = ObjectKind::Finalization(Box::new(state));
    }

    /// `(buffer, element, offset, length)`, when this object is a typed-array view.
    pub fn typed_array_parts(
        &self,
    ) -> Option<(
        Gc<JsObject>,
        crate::builtins::typedarray::ElementType,
        usize,
        usize,
    )> {
        match &self.data.borrow().kind {
            ObjectKind::TypedArray {
                buffer,
                element,
                offset,
                length,
            } => Some((buffer.clone(), *element, *offset, *length)),
            _ => None,
        }
    }

    /// Whether this callable is a native function rather than compiled JavaScript.
    ///
    /// `construct` uses it to decide who builds the instance: a native constructor creates
    /// its own, after validating its arguments.
    pub fn is_native_function(&self) -> bool {
        matches!(
            &self.data.borrow().kind,
            ObjectKind::Function(Callable::Native { .. })
        )
    }

    /// Whether this native constructor builds its own instance, so `new` must not pass one.
    pub fn native_creates_own_instance(&self) -> bool {
        matches!(
            &self.data.borrow().kind,
            ObjectKind::Function(Callable::Native {
                own_instance: true,
                ..
            })
        )
    }

    /// Make this object a `DataView` over `buffer`.
    pub fn set_data_view(&self, buffer: Gc<JsObject>, offset: usize, length: usize) {
        self.data.borrow_mut().kind = ObjectKind::DataView {
            buffer,
            offset,
            length,
        };
    }

    /// The `(buffer, offset, length)` of a `DataView`, or `None` when this is not one.
    pub fn data_view_parts(&self) -> Option<(Gc<JsObject>, usize, usize)> {
        match &self.data.borrow().kind {
            ObjectKind::DataView {
                buffer,
                offset,
                length,
            } => Some((buffer.clone(), *offset, *length)),
            _ => None,
        }
    }

    /// Make this object a typed-array view of `length` elements.
    pub fn set_typed_array(
        &self,
        buffer: Gc<JsObject>,
        element: crate::builtins::typedarray::ElementType,
        offset: usize,
        length: usize,
    ) {
        self.data.borrow_mut().kind = ObjectKind::TypedArray {
            buffer,
            element,
            offset,
            length,
        };
    }

    /// The element type, when this object is a typed-array view.
    pub fn typed_array_element_type(&self) -> Option<crate::builtins::typedarray::ElementType> {
        self.typed_array_parts().map(|(_, element, _, _)| element)
    }

    /// How many elements the view exposes, or `None` when this is not a view.
    ///
    /// A detached buffer reports `0`: the storage is gone, so there is nothing to index.
    pub fn typed_array_length(&self) -> Option<usize> {
        let (buffer, _, _, length) = self.typed_array_parts()?;
        let storage = buffer.array_buffer_storage()?;
        if storage.borrow().detached {
            return Some(0);
        }
        Some(length)
    }

    /// Read one view element.
    ///
    /// Out-of-range and detached reads are `undefined` rather than an error, which is what
    /// makes `a[999]` and a detached `a[0]` behave the way the language says.
    pub fn typed_array_element(&self, index: usize) -> Option<Value> {
        let (buffer, element, offset, length) = self.typed_array_parts()?;
        let storage = buffer.array_buffer_storage()?;
        let storage = storage.borrow();
        if storage.detached || index >= length {
            return Some(Value::Undefined);
        }
        // The buffer's own handle is where the heap comes from, so a BigInt element can be
        // allocated without every caller having to thread one through.
        Some(element.read(
            buffer.heap(),
            &storage.bytes,
            offset + index * element.size(),
        ))
    }

    /// Store one view element, applying the type's wrap or clamp.
    ///
    /// `Some(Ok(()))` stored, `Some(Err(()))` refused the *value* — a Number into a BigInt
    /// view, which the caller turns into a `TypeError` — and `None` when this is not a
    /// view at all or the index is out of range, which is a silent no-op.
    pub fn typed_array_set(&self, index: usize, value: Value) -> Option<Result<(), &'static str>> {
        let (buffer, element, offset, length) = self.typed_array_parts()?;
        if index >= length {
            return Some(Ok(()));
        }
        let storage = buffer.array_buffer_storage()?;
        let mut storage = storage.borrow_mut();
        if storage.detached {
            return Some(Ok(()));
        }
        let stored = element.write(&mut storage.bytes, offset + index * element.size(), &value);
        Some(stored)
    }
}

/// A property of an object.
#[derive(Clone, Default)]
pub struct Property {
    /// The value of a data property.
    pub value: Option<Value>,
    /// The getter of an accessor property.
    pub getter: Option<Value>,
    /// The setter of an accessor property.
    pub setter: Option<Value>,
    /// Whether the value can be replaced.
    pub writable: bool,
    /// Whether `for`/`Object.keys` sees the property.
    pub enumerable: bool,
    /// Whether the property can be deleted or redefined.
    pub configurable: bool,
}

impl Property {
    /// A writable, enumerable, configurable data property.
    ///
    /// This is what a plain assignment or an object literal creates, and the
    /// attributes only differ for built-ins and `Object.defineProperty`.
    pub fn data(value: Value) -> Property {
        Property {
            value: Some(value),
            writable: true,
            enumerable: true,
            configurable: true,
            ..Property::default()
        }
    }

    /// A data property with explicit attributes.
    ///
    /// `configurable` is derived from `writable`, because that derivation is right for the
    /// case this is overwhelmingly used for: a built-in method or constructor, which the
    /// spec gives `{writable: true, enumerable: false, configurable: true}`.
    ///
    /// It is *not* right for the two other shapes the spec uses on functions, and both were
    /// wrong here for a long time — `name`/`length` are `{writable: false, enumerable:
    /// false, configurable: true}`, and a function's `prototype` is `{writable: true,
    /// enumerable: false, configurable: false}`. Use [`Property::name_or_length`] and
    /// [`Property::prototype_of`] for those rather than spelling them out, so that the two
    /// exceptions to the rule stay in one place.
    pub fn with_attributes(value: Value, writable: bool, enumerable: bool) -> Property {
        Property {
            value: Some(value),
            writable,
            enumerable,
            configurable: writable,
            ..Property::default()
        }
    }

    /// A `name` or `length` property: writable and enumerable are false, configurable true.
    ///
    /// Every function has both, and the spec makes them configurable so that a definition
    /// can replace them (`Object.defineProperty(f, "length", {value: 0})`, and
    /// `Function.prototype.bind` renaming its result). With them non-configurable, those
    /// definitions throw and about five hundred conformance tests fail.
    pub fn name_or_length(value: Value) -> Property {
        Property {
            value: Some(value),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::default()
        }
    }

    /// A function's `prototype`: not configurable, and writable only for user functions.
    ///
    /// A built-in constructor's `prototype` is read-only — assigning to `Object.prototype`
    /// must not stick — while a user function's is writable, since that is how
    /// `Ctor.prototype.method = f` sets up inheritance. Neither is configurable, so
    /// `delete Ctor.prototype` fails in both cases.
    pub fn prototype_of(value: Value, writable: bool) -> Property {
        Property {
            value: Some(value),
            writable,
            enumerable: false,
            configurable: false,
            ..Property::default()
        }
    }

    /// An accessor property.
    /// An accessor property with explicit `enumerable`/`configurable` flags, which is
    /// what `Symbol.prototype.description` needs: it is enumerable: false in the spec.
    pub fn accessor_with(
        getter: Option<Value>,
        setter: Option<Value>,
        enumerable: bool,
    ) -> Property {
        Property {
            getter,
            setter,
            enumerable,
            configurable: true,
            ..Property::default()
        }
    }

    /// An accessor property with the default `enumerable`/`configurable` flags.
    pub fn accessor(getter: Option<Value>, setter: Option<Value>) -> Property {
        Property {
            getter,
            setter,
            enumerable: true,
            configurable: true,
            ..Property::default()
        }
    }

    /// Whether this is an accessor rather than a data property.
    pub fn is_accessor(&self) -> bool {
        self.getter.is_some() || self.setter.is_some()
    }
}

impl Trace for Property {
    fn trace(&self, tracer: &mut Tracer) {
        self.value.trace(tracer);
        self.getter.trace(tracer);
        self.setter.trace(tracer);
    }
}

/// A host function: a native function plus the state it captured.
///
/// `NativeFn` is a bare `fn` pointer, so a builtin that needs state (an open-file table,
/// a timer queue, a sink to write to) has nowhere to put it. `HostFn` boxes the closure
/// instead, and [`Callable::Host`] keeps it on the function object.
///
/// # Why the closure must not capture a `Gc`
///
/// The collector finds live objects by walking the object graph through [`Trace`]. A
/// closure is opaque Rust, so nothing inside it is walked: a `Gc<JsObject>` captured here
/// would be invisible to the collector and could be freed while the closure still holds
/// it — a use-after-free.
///
/// Keep Rust-side state (file descriptors, timer ids, deadlines) in the closure, and keep
/// every JavaScript value somewhere the collector already scans: a property of a real JS
/// object, or a table hanging off [`Interp`]. `os.setTimeout(fn, ms)` stores `fn` in the
/// interpreter's timer table for exactly this reason.
///
/// [`Trace`]: js_gc::Trace
/// [`Interp`]: crate::interp::Interp
pub type HostFn = Box<
    dyn for<'i> FnMut(
        &mut crate::interp::Interp<'i>,
        Value,
        &[Value],
    ) -> crate::interp::JsResult<Value>,
>;

/// A native (Rust) function reachable from JavaScript.
///
/// It receives the interpreter so a builtin can allocate (which needs the heap),
/// call back into JavaScript (which needs the frame stack) and raise exceptions.
pub type NativeFn =
    fn(&mut crate::interp::Interp<'_>, Value, &[Value]) -> crate::interp::JsResult<Value>;

/// The body of a callable object.
pub enum Callable {
    /// A function compiled from JavaScript source.
    Bytecode {
        /// The bytecode and slot mapping.
        code: Gc<CompiledCode>,
        /// The captured environment.
        env: Gc<ClosureEnv>,
    },
    /// A function produced by `Function.prototype.bind`.
    ///
    /// This is a variant rather than a native function carrying the state in
    /// properties because the state is not observable: `bind` has to remember its
    /// target, `this` and leading arguments, and a native trampoline cannot capture
    /// them.
    Bound {
        /// The function being called.
        target: Value,
        /// The `this` every call uses.
        this: Value,
        /// Arguments prepended to every call's own.
        args: Vec<Value>,
    },
    /// A built-in implemented in Rust.
    ///
    /// The `name` and `length` a script observes are ordinary properties on the
    /// function object, not fields here: they are writable (`f.name = "x"` works)
    /// and would otherwise be two sources of truth.
    Native {
        /// The implementation.
        func: NativeFn,
        /// Whether this constructor builds its own instance instead of being handed one.
        ///
        /// The spec has a constructor validate its arguments *before*
        /// `OrdinaryCreateFromConstructor` reads `new.target.prototype`, and only the ones
        /// with real validation need to: `new Promise(nonCallable)` and `new DataView(…)`
        /// must refuse the argument before a throwing `prototype` getter can run. The rest
        /// keep the simpler shape where `new` has already made the object.
        own_instance: bool,
    },
    /// A host function: a native closure that carries its own state.
    ///
    /// Behind an `Rc<RefCell<…>>` because the closure has to be *called* with
    /// `&mut Interp`, and the call site would otherwise still be holding a borrow of this
    /// object's data — the closure itself may reach the same object. Cloning the `Rc` out
    /// and dropping the borrow first is what keeps that from panicking.
    Host {
        /// The body, with whatever it captured.
        func: std::rc::Rc<std::cell::RefCell<HostFn>>,
        /// Whether this constructor builds its own instance; see [`Callable::Native`].
        own_instance: bool,
    },
    /// The `revoke` function `Proxy.revocable` hands back.
    ///
    /// It is a variant rather than a `Native` with the slots in a property because a
    /// `NativeFn` is a plain function pointer: it cannot capture, and the slots are
    /// precisely what this function exists to hold. Keeping them here also means the
    /// collector sees them, so a proxy stays alive while only its `revoke` is reachable.
    Revoker(crate::object::ProxyRef),
    /// The `resolve` a `Promise` executor receives.
    ///
    /// Like [`Callable::Revoker`], this cannot be a plain `Native` with the state in a
    /// property: a `NativeFn` is a function pointer and cannot capture, and the promise
    /// it settles is exactly what it has to remember. Both are one-shot per §27.2.1.3.1 —
    /// the second call is a no-op rather than a second settlement.
    ///
    /// The promise object is held directly rather than the state behind its `Rc`, because
    /// settling has to run *that object's* reactions.
    PromiseResolve(Gc<JsObject>),
    /// The `reject` half of that pair.
    PromiseReject(Gc<JsObject>),
    /// One of the two continuations an `await` parks on.
    ///
    /// Reached through the ordinary reaction machinery, which is why it has to be a
    /// callable object rather than a queue entry: `await x` is specified as
    /// `PerformPromiseThen(x, resume, rethrow)`.
    AwaitResume {
        /// The body to continue.
        coroutine: Gc<JsObject>,
        /// The promise the async function returned.
        promise: Gc<JsObject>,
        /// Whether this continuation delivers the value or raises the reason.
        kind: crate::interp::ResumeKind,
    },
    /// One element of `Promise.all`/`allSettled`/`any`/`race` settling.
    CombinatorSettle {
        /// The hidden object holding the results array, the remaining count and the
        /// combinator's own resolve/reject pair.
        state: Gc<JsObject>,
        /// Which element this callback belongs to.
        index: usize,
        /// Whether the element fulfilled or rejected.
        fulfilled: bool,
    },
    /// Settle a promise with a fixed outcome when called.
    ///
    /// `finally` needs "wait for this promise, then re-deliver that value", and a
    /// `NativeFn` cannot carry the value to re-deliver.
    SettlePromise {
        /// The promise to settle.
        promise: Gc<JsObject>,
        /// The outcome to settle it with, which is the *original* one.
        value: Value,
        /// Whether that outcome is a rejection.
        rejected: bool,
    },
    /// One continuation of an `await` inside an async generator.
    GeneratorResume {
        /// The async generator to continue.
        generator: Gc<JsObject>,
        /// The promise the pending `next`/`return`/`throw` returned.
        promise: Gc<JsObject>,
        /// Whether this delivers the value or raises the reason.
        kind: crate::interp::ResumeKind,
    },
    /// Reads a module namespace's binding out of the cell it is stored in.
    ///
    /// A namespace object is an ordinary object whose exports are getters, so a getter
    /// is what `ns.x` calls — and it has to be a callable object carrying the cell, for
    /// the same reason `PromiseResolve` carries its promise.
    ModuleBinding {
        /// The binding's cell, shared with the exporting module.
        cell: Gc<Cell>,
    },
    /// Answer a pending async-generator method with `{value, done: false}`.
    ///
    /// `yield v` in an async generator awaits `v` first, so the value can arrive after
    /// the body has already parked; this is what turns it into the result object.
    YieldResult {
        /// The promise the pending method returned.
        promise: Gc<JsObject>,
        /// Whether this answers a completed generator rather than a `yield`.
        done: bool,
    },
    /// One step of `AsyncDisposableStack.disposeAsync`.
    ///
    /// The remaining resources ride along as a JS array so the collector can see them:
    /// `disposeAsync` awaits each disposer in turn, which means the list has to survive
    /// across microtasks rather than living on the Rust stack.
    DisposeStep {
        /// Pairs of `[resource, disposeMethod]`, consumed from the end.
        remaining: Gc<JsObject>,
        /// The promise `disposeAsync` returned.
        promise: Gc<JsObject>,
        /// The `SuppressedError` chain built so far, or `Uninitialized` for none.
        error: Value,
        /// Whether this continuation is being called with a rejection reason.
        rejected: bool,
    },
    /// One of the two shims `Promise.prototype.finally` installs.
    FinallyShim {
        /// The script's callback, or `None` to pass the outcome straight through.
        callback: Option<Value>,
        /// Whether this is the fulfil side.
        fulfilled: bool,
    },
}

impl Trace for Callable {
    fn trace(&self, tracer: &mut Tracer) {
        match self {
            Callable::Bytecode { code, env } => {
                tracer.visit(code);
                tracer.visit(env);
            }
            Callable::Bound { target, this, args } => {
                target.trace(tracer);
                this.trace(tracer);
                for argument in args {
                    argument.trace(tracer);
                }
            }
            Callable::Native { .. } => {}
            // Nothing to trace, by construction: the closure must not hold a `Gc`. See
            // `HostFn`'s documentation.
            Callable::Host { .. } => {}
            Callable::Revoker(slots) => slots.borrow().trace(tracer),
            Callable::PromiseResolve(promise) | Callable::PromiseReject(promise) => {
                tracer.visit(promise)
            }
            Callable::AwaitResume {
                coroutine, promise, ..
            } => {
                tracer.visit(coroutine);
                tracer.visit(promise);
            }
            Callable::CombinatorSettle { state, .. } => tracer.visit(state),
            Callable::GeneratorResume {
                generator, promise, ..
            } => {
                tracer.visit(generator);
                tracer.visit(promise);
            }
            Callable::YieldResult { promise, .. } => tracer.visit(promise),
            Callable::ModuleBinding { cell } => tracer.visit(cell),
            Callable::DisposeStep {
                remaining,
                promise,
                error,
                ..
            } => {
                tracer.visit(remaining);
                tracer.visit(promise);
                error.trace(tracer);
            }
            Callable::SettlePromise { promise, value, .. } => {
                tracer.visit(promise);
                value.trace(tracer);
            }
            Callable::FinallyShim { callback, .. } => {
                if let Some(callback) = callback {
                    callback.trace(tracer);
                }
            }
        }
    }
}

/// The bytecode of one function, plus the slot-to-cell mapping.
///
/// This is a separate GC object from the closure so that a frame can hold a handle
/// to it and read it while the function object itself is free to be borrowed.
/// Holding a `RefCell` borrow across a call would deadlock the moment a function
/// referred to itself.
pub struct CompiledCode {
    /// The resolved bytecode.
    pub code: CompiledFunction,
    /// For each argument slot, the index of its reference cell, or `-1` when the
    /// slot holds its value directly.
    ///
    /// A captured binding lives in a cell so that closures outlive the frame; every
    /// other slot is a plain value. Precomputing the mapping keeps the hot
    /// `get_arg`/`put_arg` paths to one array read instead of a lookup.
    pub arg_cell: Vec<i32>,
    /// The same, for local slots.
    pub loc_cell: Vec<i32>,
}

impl CompiledCode {
    /// Build the slot-to-cell mapping for `code`.
    ///
    /// Cells are numbered the way [`CompiledFunction::var_ref_count`] counts them:
    /// the captured arguments first, then the captured locals, each in slot order.
    pub fn new(code: CompiledFunction) -> CompiledCode {
        let mut next = 0i32;
        let mut arg_cell = vec![-1; code.args.len()];
        for (slot, arg) in code.args.iter().enumerate() {
            if arg.is_captured {
                arg_cell[slot] = next;
                next += 1;
            }
        }
        let mut loc_cell = vec![-1; code.vars.len()];
        for (slot, local) in code.vars.iter().enumerate() {
            if local.is_captured {
                loc_cell[slot] = next;
                next += 1;
            }
        }
        CompiledCode {
            code,
            arg_cell,
            loc_cell,
        }
    }
}

impl Trace for CompiledCode {
    fn trace(&self, _tracer: &mut Tracer) {}
}

/// The reference cells a closure captured from enclosing scopes.
pub struct ClosureEnv {
    /// Indexed the way the bytecode's closure-variable table indexes them.
    pub cells: Vec<Gc<Cell>>,
    /// The `eval` scope this closure was created in, if any.
    ///
    /// A function built by `eval` has to keep seeing the caller's bindings after the
    /// eval has returned — `for (let i …) eval("push(() => i)")` collects closures that
    /// read `i` later. Those bindings live in the *caller's* frame, which is long gone
    /// by then, so the environment carries them by name instead. Ordinary closures leave
    /// this `None` and resolve globals exactly as before.
    pub eval_scope: Option<Gc<EvalScope>>,
    /// Variables a direct `eval` declared in the frame this closure was created in, and
    /// how much of that frame it may see.
    ///
    /// A closure is created at a *place* in a function, and a direct `eval` can add
    /// variables after that point. Those variables belong to the variable environment
    /// that was current at the closure's creation — the parameter list or the body — which
    /// is what `local_phase` records.
    pub local_vars: Option<Gc<LocalVars>>,
    /// `0` for a closure created in a parameter list, `1` for one in a body. A parameter
    /// list's closure must not see variables a later `eval` in the body declared:
    /// `function f(a, p = () => c) { eval("var c = 1") }` has `p()` resolve `c` outside
    /// the function entirely.
    pub local_phase: u8,
}

/// Variables a direct `eval` declared in one function call.
///
/// A sloppy `eval` declares its `var`s in the caller's variable environment, not on the
/// global object: `function f() { eval("var x = 1") } f(); x` is a `ReferenceError`, and
/// two calls to `f` get two `x`s. Each entry therefore records the phase it was declared
/// in, so a lookup from the parameter list cannot see a body declaration.
pub struct LocalVars {
    names: std::cell::RefCell<Vec<(String, Gc<Cell>, u8)>>,
}

impl Default for LocalVars {
    fn default() -> LocalVars {
        LocalVars::new()
    }
}

impl LocalVars {
    /// An empty set.
    pub fn new() -> LocalVars {
        LocalVars {
            names: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The cell `name` resolves to, when it was declared in `phase` or earlier.
    pub fn get(&self, name: &str, phase: u8) -> Option<Gc<Cell>> {
        self.names
            .borrow()
            .iter()
            .find(|(bound, _, at)| bound == name && *at <= phase)
            .map(|(_, cell, _)| cell.clone())
    }

    /// The cell for `name`, declaring it if this is the first time.
    pub fn declare(&self, name: &str, phase: u8, cell: Gc<Cell>) -> Gc<Cell> {
        let mut names = self.names.borrow_mut();
        if let Some((_, cell, _)) = names.iter().find(|(bound, _, _)| bound == name) {
            return cell.clone();
        }
        names.push((name.to_string(), cell.clone(), phase));
        cell
    }

    /// Whether `name` has been declared at all.
    pub fn contains(&self, name: &str) -> bool {
        self.names
            .borrow()
            .iter()
            .any(|(bound, _, _)| bound == name)
    }
}

impl Trace for LocalVars {
    fn trace(&self, tracer: &mut Tracer) {
        for (_, cell, _) in self.names.borrow().iter() {
            tracer.visit(cell);
        }
    }
}

impl Trace for ClosureEnv {
    fn trace(&self, tracer: &mut Tracer) {
        for cell in &self.cells {
            tracer.visit(cell);
        }
        if let Some(scope) = &self.eval_scope {
            tracer.visit(scope);
        }
        if let Some(local_vars) = &self.local_vars {
            tracer.visit(local_vars);
        }
    }
}

/// The bindings a direct `eval` made visible, as name → cell.
///
/// Only bindings that already live in a cell appear here, because sharing their
/// identity is the point: a closure created by the eval must observe later writes to
/// the same variable, and a copy would not. The compiler puts every binding of a
/// function that contains a direct `eval` into a cell, so in practice that is all of
/// them.
pub struct EvalScope {
    /// In lookup order: the innermost binding for a name first.
    pub names: Vec<(String, Gc<Cell>)>,
}

impl EvalScope {
    /// The cell `name` resolves to, if this scope binds it.
    pub fn get(&self, name: &str) -> Option<&Gc<Cell>> {
        self.names
            .iter()
            .find(|(bound, _)| bound == name)
            .map(|(_, cell)| cell)
    }
}

impl Trace for EvalScope {
    fn trace(&self, tracer: &mut Tracer) {
        for (_, cell) in &self.names {
            tracer.visit(cell);
        }
    }
}

/// What kind of object this is.
pub enum ObjectKind {
    /// An ordinary object: only its property table.
    Ordinary,
    /// An array: a dense element vector plus a property table for everything else.
    ///
    /// `length` is stored **separately** rather than derived from `elements.len()`, so a
    /// sparse array costs nothing to poke at a far index: the reference answers
    /// `a = []; a.length = 4294967295` instantly, while deriving the length from a dense
    /// vector meant allocating one `Value` per slot — 62 GB and 24 seconds for that one
    /// statement. An index past the vector is a hole, and `length` alone records how many
    /// holes there are at the end.
    ///
    /// Invariant: `elements.len() <= length`. A truncation drops both, so the reverse
    /// (`length < elements.len()`) never holds.
    Array {
        /// The dense prefix: everything before the first trailing hole.
        elements: Vec<Value>,
        /// The logical length, which may exceed `elements.len()`.
        length: usize,
    },
    /// A function or a class constructor.
    Function(Callable),
    /// A regular expression: the compiled pattern, plus `lastIndex` as a property.
    RegExp(crate::builtins::regexp::RegExpData),
    /// A `DisposableStack` or `AsyncDisposableStack`: the resources it holds.
    DisposableStack(crate::builtins::disposable::StackState),
    /// An `ArrayBuffer`: a byte range that typed-array views share.
    ///
    /// The bytes sit behind an `Rc<RefCell<…>>` rather than in the object so that every view
    /// of the buffer sees the same storage; the *views* hold the `Gc<JsObject>` of this
    /// buffer, which is what keeps it alive and what the collector traces.
    ArrayBuffer(std::rc::Rc<std::cell::RefCell<BufferStorage>>),
    /// A `Map` or a `Set`: insertion-ordered entries with `SameValueZero` key equality.
    ///
    /// One variant for both, because a `Set` is exactly a `Map` whose value is unused — the
    /// difference is a flag, and `is_set` is what makes `entries` yield `[v, v]` pairs.
    Collection {
        /// The entries, in insertion order.
        entries: Vec<(Value, Value)>,
        /// Whether this is a `Set` rather than a `Map`.
        is_set: bool,
    },
    /// A `DataView`: a byte-oriented window onto an `ArrayBuffer`.
    ///
    /// Separate from `TypedArray` because the two index differently: a typed array has an
    /// element type and counts *elements*, while a `DataView` is untyped and every accessor
    /// names its own width and byte offset. Sharing one variant would mean carrying an
    /// element type that is always "bytes" and a length that means something else.
    DataView {
        /// The buffer whose bytes this view reads.
        buffer: Gc<JsObject>,
        /// Byte offset of the view's first byte.
        offset: usize,
        /// How many bytes the view exposes.
        length: usize,
    },
    /// A typed-array view: a window onto an `ArrayBuffer`.
    TypedArray {
        /// The buffer whose bytes this view reads.
        buffer: Gc<JsObject>,
        /// What one element is.
        element: crate::builtins::typedarray::ElementType,
        /// Byte offset of element zero.
        offset: usize,
        /// How many elements the view exposes.
        length: usize,
    },
    /// A generator object: a detached frame plus how far it got.
    ///
    /// The frame lives here rather than in the interpreter because a suspended generator
    /// has to keep its locals and operand stack alive for as long as the script can reach
    /// the generator object — and the collector has to see them, which it only does for
    /// things hanging off a `Gc`.
    Generator(Box<crate::interp::GeneratorState>),
    /// A `Proxy`: a stand-in that forwards every internal method to a target through a
    /// handler's traps.
    ///
    /// The state sits behind an `Rc` because `Proxy.revocable` hands the script a
    /// `revoke` function that has to reach *this* proxy's two slots later, from a call
    /// site that has no pointer to the object. Revoking clears them, which also drops
    /// the collector's references to the target and the handler.
    Proxy(ProxyRef),
    /// A `Promise`.
    ///
    /// The state sits behind an `Rc` for the same reason a proxy's slots do: the
    /// `resolve`/`reject` pair handed to an executor has to settle *this* promise later,
    /// and those are function objects with no pointer back to it otherwise.
    Promise(PromiseRef),
    /// A `WeakMap` or `WeakSet`: like a collection, but the *keys* are weak.
    ///
    /// The distinction lives entirely in [`Trace`]: a `Collection` traces its keys, a
    /// `WeakCollection` does not. An entry keyed by an otherwise-unreachable object is
    /// therefore reclaimed together with that object instead of keeping it alive, and the
    /// entry simply stops being found — `upgrade` fails and a liveness sweep drops the
    /// pair. That is the "ephemeron" behaviour `WeakMap` promises, and it falls out of
    /// reference counting rather than needing a second collection pass.
    WeakCollection {
        /// The entries, in insertion order, keyed by a non-owning handle.
        entries: Vec<(WeakKey, Value)>,
        /// Whether this is a `WeakSet` rather than a `WeakMap`.
        is_set: bool,
    },
    /// A `WeakRef`: one non-owning handle to a target object.
    WeakRef {
        /// The target. Once `upgrade` fails the reference is dead for good, because a box
        /// whose strong count reached zero never comes back.
        ///
        /// A `WeakKey` rather than a `WeakGc<JsObject>`, because a symbol is a valid target
        /// too — `new WeakRef(Symbol())` works and `deref` has to answer it back.
        target: WeakKey,
    },
    /// A `FinalizationRegistry`: the cleanup callback and its live registrations.
    Finalization(Box<FinalizationState>),
}

/// The mutable half of a `FinalizationRegistry`.
///
/// Boxed inside the [`ObjectKind`] variant because it holds a vector plus a value, and every
/// `ObjectKind` is stored inline in the object header — an unboxed field this wide would
/// make *every* object in the realm that much larger.
pub struct FinalizationState {
    /// One entry per successful `register` call.
    pub registrations: Vec<Registration>,
    /// The cleanup callback, validated as callable when the registry was built.
    pub cleanup: Value,
}

/// A key that can be held weakly: an object, or a symbol.
///
/// Both are heap allocations in this engine (`Value::Symbol` carries a `Gc<JsSymbol>`), so a
/// `WeakGc` holds either one just as well. Symbols became valid keys with the
/// `symbols-as-weakmap-keys` proposal, which is what test262 already tests.
#[derive(Clone)]
pub enum WeakKey {
    /// An object target.
    Object(WeakGc<JsObject>),
    /// A symbol target.
    Symbol(WeakGc<JsSymbol>),
}

impl WeakKey {
    /// Whether the allocation behind this key has been released.
    pub fn is_expired(&self) -> bool {
        match self {
            WeakKey::Object(handle) => handle.is_expired(),
            WeakKey::Symbol(handle) => handle.is_expired(),
        }
    }

    /// Whether two weak keys are the same allocation.
    pub fn same(&self, other: &WeakKey) -> bool {
        match (self, other) {
            (WeakKey::Object(left), WeakKey::Object(right)) => left.ptr_eq(right),
            (WeakKey::Symbol(left), WeakKey::Symbol(right)) => left.ptr_eq(right),
            _ => false,
        }
    }

    /// Whether this key is the same allocation as `value`, without upgrading it.
    ///
    /// Upgrading would resurrect the key, which for a `WeakMap` lookup is exactly what must
    /// not happen — a dead key has to compare equal to nothing.
    pub fn matches(&self, value: &Value) -> bool {
        match (self, value) {
            (WeakKey::Object(handle), Value::Object(object)) => object.upgrades_to(handle),
            (WeakKey::Symbol(handle), Value::Symbol(symbol)) => symbol.upgrades_to(handle),
            _ => false,
        }
    }

    /// The key as a strong value, when it is still alive.
    pub fn upgrade(&self) -> Option<Value> {
        match self {
            WeakKey::Object(handle) => handle.upgrade().map(Value::Object),
            WeakKey::Symbol(handle) => handle.upgrade().map(Value::Symbol),
        }
    }
}

impl Trace for WeakKey {
    fn trace(&self, _tracer: &mut Tracer) {}
}

/// Turn a script value into a weak key, if `CanBeHeldWeakly` allows it.
///
/// The rule is "an object, or a symbol that is not in the registry". A registered symbol —
/// one from `Symbol.for` — is refused on purpose: it lives in the realm's registry for the
/// lifetime of the realm, so it never dies, and a weak entry keyed by it could never be
/// observed to be collected. A plain `Symbol()` is a fresh allocation each time and so has
/// a lifetime worth observing.
pub fn weak_key(value: &Value) -> Option<WeakKey> {
    match value {
        Value::Object(object) => Some(WeakKey::Object(object.downgrade())),
        Value::Symbol(symbol) if symbol.registry_key().is_none() => {
            Some(WeakKey::Symbol(symbol.downgrade()))
        }
        _ => None,
    }
}

/// One `FinalizationRegistry.register` call.
///
/// `token` is compared with `SameValue`, not `SameValueZero`, which is why it is kept as
/// the original `Value` rather than normalised into a key.
pub struct Registration {
    /// The handle whose death fires the callback.
    pub target: WeakKey,
    /// What the callback receives: the script's held value, kept alive until it fires.
    pub held_value: Value,
    /// The cancellation token, if one was given.
    pub token: Option<Value>,
}

/// The two slots of a proxy, shared with its `revoke` function.
///
/// `None` in both slots means revoked: every operation then throws a `TypeError`, and
/// the target and handler are no longer reachable — which is the whole point of
/// `Proxy.revocable`.
#[derive(Default)]
pub struct ProxyState {
    /// The object the traps receive, or `None` once revoked.
    pub target: Option<Gc<JsObject>>,
    /// The trap table, or `None` once revoked.
    pub handler: Option<Value>,
    /// Whether this proxy has a `[[Call]]`, decided once when it is created.
    ///
    /// Revoking drops the target but does not take `[[Call]]` away, so this cannot be
    /// re-derived from `target` being `Some`: `typeof p.proxy` has to stay `"function"`
    /// after `Proxy.revocable(function(){}, {}).revoke()`, and the reference agrees.
    pub callable: bool,
    /// Whether this proxy has a `[[Construct]]`, decided once when it is created.
    ///
    /// Same reasoning as [`ProxyState::callable`], and the two are genuinely independent:
    /// `Proxy.revocable(function(){}, {})` is both callable and constructible, while a
    /// proxy over an arrow function is callable and not constructible.
    pub constructible: bool,
}

/// A `Promise`'s state, shared between the promise object and its resolve/reject
/// functions.
///
/// Behind an `Rc` for the same reason a proxy's slots are: the `resolve` and `reject`
/// functions handed to an executor have to settle *this* promise later, from a call site
/// that only has the function.
pub struct PromiseState {
    /// Pending, or the value it settled with.
    pub status: PromiseStatus,
    /// The `then` reactions waiting on it, in registration order.
    pub reactions: Vec<PromiseReaction>,
    /// Whether a rejection has been seen by a handler.
    ///
    /// A rejection that never is becomes an unhandled-rejection report, which the
    /// reference makes fatal at the end of the job queue.
    pub handled: bool,
}

impl Default for PromiseState {
    fn default() -> Self {
        PromiseState {
            status: PromiseStatus::Pending,
            reactions: Vec::new(),
            handled: false,
        }
    }
}

/// What a `Promise` combinator does when its elements settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CombinatorKind {
    /// The first rejection wins, otherwise an array in input order.
    All,
    /// Never rejects; each entry is `{status, value}` or `{status, reason}`.
    AllSettled,
    /// The first fulfilment wins, otherwise an `AggregateError`.
    Any,
    /// Whichever element settles first.
    Race,
}

/// Which of §27.2.1.2's three states a promise is in.
#[derive(Clone, Default)]
pub enum PromiseStatus {
    /// Neither settled: reactions queue up.
    #[default]
    Pending,
    /// Settled with a value, which no further settle may change.
    Fulfilled(Value),
    /// Settled with a reason.
    Rejected(Value),
}

/// One `then` registration.
#[derive(Clone)]
pub struct PromiseReaction {
    /// Called with the value if the promise fulfils.
    pub on_fulfilled: Option<Value>,
    /// Called with the reason if it rejects.
    pub on_rejected: Option<Value>,
    /// The promise to settle with whatever the handler returns.
    pub promise: Gc<JsObject>,
}

impl Trace for PromiseState {
    fn trace(&self, tracer: &mut Tracer) {
        match &self.status {
            PromiseStatus::Pending => {}
            PromiseStatus::Fulfilled(value) | PromiseStatus::Rejected(value) => value.trace(tracer),
        }
        for reaction in &self.reactions {
            if let Some(on_fulfilled) = &reaction.on_fulfilled {
                on_fulfilled.trace(tracer);
            }
            if let Some(on_rejected) = &reaction.on_rejected {
                on_rejected.trace(tracer);
            }
            tracer.visit(&reaction.promise);
        }
    }
}

/// A promise's shareable state.
pub type PromiseRef = std::rc::Rc<std::cell::RefCell<PromiseState>>;

/// A proxy's mutable state, shared between the proxy object and its `revoke`.
pub type ProxyRef = std::rc::Rc<std::cell::RefCell<ProxyState>>;

impl Trace for ProxyState {
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(target) = &self.target {
            tracer.visit(target);
        }
        if let Some(handler) = &self.handler {
            handler.trace(tracer);
        }
    }
}

impl Trace for ObjectKind {
    fn trace(&self, tracer: &mut Tracer) {
        match self {
            // Nothing in a compiled pattern is a GC value: it holds characters and
            // integers only.
            ObjectKind::RegExp(_) => {}
            ObjectKind::DisposableStack(state) => state.trace(tracer),
            // A buffer's bytes are plain bytes and hold no GC values; a view keeps its
            // buffer alive through the `Gc` it carries.
            ObjectKind::ArrayBuffer(_) => {}
            ObjectKind::TypedArray { buffer, .. } | ObjectKind::DataView { buffer, .. } => {
                tracer.visit(buffer)
            }
            ObjectKind::Collection { entries, .. } => {
                for (key, value) in entries {
                    key.trace(tracer);
                    value.trace(tracer);
                }
            }
            ObjectKind::Ordinary => {}
            ObjectKind::Array { elements, .. } => {
                for element in elements {
                    element.trace(tracer);
                }
            }
            ObjectKind::Function(callable) => callable.trace(tracer),
            ObjectKind::Generator(state) => state.trace(tracer),
            ObjectKind::Proxy(state) => state.borrow().trace(tracer),
            ObjectKind::Promise(state) => state.borrow().trace(tracer),
            // The *keys* are deliberately not visited: that absence is what makes the
            // collection weak, and it is also why a `WeakMap` cannot keep its key alive
            // through a cycle that only the map closes. The values are traced, so
            // `wm.set(k, v)` keeping `v` alive while `k` is reachable is the right half of
            // the ephemeron rule to get right here — the other half (dropping the pair once
            // the key dies) needs no liveness test, because `upgrade` on a dead key fails.
            ObjectKind::WeakCollection { entries, .. } => {
                for (_, value) in entries {
                    value.trace(tracer);
                }
            }
            // A `WeakRef` owns nothing at all: that is its whole purpose.
            ObjectKind::WeakRef { .. } => {}
            // A registry keeps its held values and its callback alive — the held value has
            // to survive until the callback can run — but not the targets, which is the
            // handle type's job rather than the tracer's.
            ObjectKind::Finalization(state) => {
                state.cleanup.trace(tracer);
                for registration in &state.registrations {
                    registration.held_value.trace(tracer);
                    registration.token.trace(tracer);
                }
            }
        }
    }
}

/// The mutable half of an object.
/// What a property is keyed by.
///
/// A string and a symbol are separate key spaces even when they look alike: `o["a"]`
/// and `o[Symbol("a")]` are two properties, and `o[Symbol("a")]` is not reachable by
/// any string. Symbols are also invisible to `for`/`Object.keys`/`JSON.stringify`,
/// which is the whole point of them, so the distinction has to survive into the
/// property table rather than being flattened into a name.
#[derive(Clone)]
pub enum PropertyKey {
    /// An ordinary string key, held as WTF-8 so a key containing an unpaired surrogate
    /// addresses one property rather than colliding with its `U+FFFD` replacement.
    String(crate::value::Wtf8),
    /// A symbol key.
    Symbol(Gc<JsSymbol>),
}

impl PropertyKey {
    /// The key as a string, for the operations that only accept one.
    ///
    /// A symbol has no string form that would address it, so this answers `None` —
    /// which is what makes `get_field` on a symbol key fall through to the general
    /// path instead of silently reading a same-named string property.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyKey::String(text) => text.as_str(),
            PropertyKey::Symbol(_) => None,
        }
    }

    /// The key's bytes, which a string key always has and a symbol never does.
    pub fn as_wtf8(&self) -> Option<&crate::value::Wtf8> {
        match self {
            PropertyKey::String(text) => Some(text),
            PropertyKey::Symbol(_) => None,
        }
    }

    /// Whether this is an ordinary string key.
    pub fn is_symbol(&self) -> bool {
        matches!(self, PropertyKey::Symbol(_))
    }
}

impl fmt::Display for PropertyKey {
    /// How a key reads inside an error message: strings bare, symbols in their
    /// `Symbol(description)` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropertyKey::String(text) => write!(f, "{}", text.to_string_lossy()),
            PropertyKey::Symbol(symbol) => write!(f, "{}", symbol.to_display_string()),
        }
    }
}

impl PartialEq for PropertyKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PropertyKey::String(a), PropertyKey::String(b)) => a == b,
            // Symbols compare by identity, never by description.
            (PropertyKey::Symbol(a), PropertyKey::Symbol(b)) => a.ptr_eq(b),
            _ => false,
        }
    }
}

impl From<&str> for PropertyKey {
    fn from(text: &str) -> PropertyKey {
        PropertyKey::String(crate::value::Wtf8::new(text))
    }
}

impl From<String> for PropertyKey {
    fn from(text: String) -> PropertyKey {
        PropertyKey::String(crate::value::Wtf8::new(text))
    }
}

impl From<crate::value::Wtf8> for PropertyKey {
    fn from(text: crate::value::Wtf8) -> PropertyKey {
        PropertyKey::String(text)
    }
}

impl Trace for PropertyKey {
    fn trace(&self, tracer: &mut Tracer) {
        if let PropertyKey::Symbol(symbol) = self {
            tracer.visit(symbol);
        }
    }
}

/// An object's mutable state: its prototype, property table and internal slots.
pub struct ObjectData {
    /// The prototype, or `None` for `Object.prototype` and for a null-prototype
    /// object created with `Object.create(null)`.
    pub proto: Option<Gc<JsObject>>,
    /// Properties in definition order, string and symbol keys interleaved.
    pub props: Vec<(PropertyKey, Property)>,
    /// What kind of object this is.
    pub kind: ObjectKind,
    /// Whether new properties may be added. `Object.preventExtensions`, `seal` and
    /// `freeze` clear it, and nothing clears it back.
    pub extensible: bool,
    /// Whether array elements may be replaced.
    ///
    /// Elements are not in the property table, so their attributes need somewhere to
    /// live; `Object.freeze` clears this and `Object.isFrozen` reads it. A *single*
    /// non-writable element is recorded in the property table instead, which is what
    /// `Object.defineProperty(a, "3", {configurable: false})` does.
    pub elements_writable: bool,
    /// Whether an array's `length` may be changed.
    ///
    /// `Object.defineProperty(a, "length", {writable: false})` makes it read-only without
    /// touching the elements, so it cannot share `elements_writable`: the two mean
    /// different things and `Object.freeze` sets both.
    pub length_writable: bool,
    /// The object a method was defined on, which is what `super` resolves against.
    ///
    /// Kept here rather than as a property because it is not observable: the spec calls
    /// it `[[HomeObject]]` and no script can read it, so putting it in the property
    /// table would leak it to `Object.getOwnPropertyNames` and `JSON.stringify`.
    pub home_object: Option<Gc<JsObject>>,
    /// Whether this function is a class constructor, which may not be called without
    /// `new`.
    pub class_constructor: bool,
    /// Whether this function has a `[[Construct]]`, i.e. may appear as the target of
    /// `new` or `Reflect.construct`.
    ///
    /// Not derivable from being callable, and that is the whole point: an ordinary
    /// method, an arrow function, a generator, an async function and most built-in
    /// methods are all callable and none of them may be constructed (§10.2.1,
    /// §15.3.1). Deriving it from `is_callable` made `new (Array.prototype.map)` succeed,
    /// which test262 checks for every built-in (`not-a-constructor.js`, 515 files).
    pub constructible: bool,
}

impl Trace for ObjectData {
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(proto) = &self.proto {
            tracer.visit(proto);
        }
        if let Some(home) = &self.home_object {
            tracer.visit(home);
        }
        for (key, property) in &self.props {
            key.trace(tracer);
            property.trace(tracer);
        }
        self.kind.trace(tracer);
    }
}

/// A JavaScript object.
///
/// The interior mutability is what makes `const o = {}; o.x = 1` work: the handle
/// is immutable, the object it points at is not.
pub struct JsObject {
    data: RefCell<ObjectData>,
}

impl JsObject {
    fn new(kind: ObjectKind, proto: Option<Gc<JsObject>>) -> JsObject {
        JsObject {
            data: RefCell::new(ObjectData {
                proto,
                props: Vec::new(),
                kind,
                extensible: true,
                elements_writable: true,
                length_writable: true,
                home_object: None,
                class_constructor: false,
                constructible: false,
            }),
        }
    }

    /// An ordinary object with the given prototype.
    pub fn ordinary(heap: &js_gc::Heap, proto: Option<Gc<JsObject>>) -> Gc<JsObject> {
        Gc::new(heap, JsObject::new(ObjectKind::Ordinary, proto))
    }

    /// An array with the given prototype and initial capacity.
    /// An empty array with room for `capacity` elements.
    ///
    /// The length starts at 0: `capacity` is a `Vec` hint, not a length. Callers that want
    /// holes use [`Self::array_set_length`], which no longer allocates them.
    pub fn array(heap: &js_gc::Heap, proto: Option<Gc<JsObject>>, capacity: usize) -> Gc<JsObject> {
        Gc::new(
            heap,
            JsObject::new(
                ObjectKind::Array {
                    elements: Vec::with_capacity(capacity),
                    length: 0,
                },
                proto,
            ),
        )
    }

    /// A callable object.
    pub fn function(
        heap: &js_gc::Heap,
        proto: Option<Gc<JsObject>>,
        callable: Callable,
    ) -> Gc<JsObject> {
        Gc::new(heap, JsObject::new(ObjectKind::Function(callable), proto))
    }

    /// Run `f` with the object's data borrowed.
    pub fn with<R>(&self, f: impl FnOnce(&ObjectData) -> R) -> R {
        f(&self.data.borrow())
    }

    /// Run `f` with the object's data mutably borrowed.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut ObjectData) -> R) -> R {
        f(&mut self.data.borrow_mut())
    }

    /// Whether this object can be called.
    ///
    /// Covers every [`Callable`] variant, `Bound` included: `typeof bound ===
    /// "function"` and `bound()` works, so from a script's point of view a bound
    /// function is a function.
    pub fn is_callable(&self) -> bool {
        match &self.data.borrow().kind {
            ObjectKind::Function(_) => true,
            // A proxy is callable exactly when its target was when the proxy was built.
            // Not "when its target still is": revoking drops the target but leaves
            // `[[Call]]` in place, so a revoked proxy over a function stays a function.
            // This is what makes `typeof proxyOfFunction === "function"` survive revoke.
            ObjectKind::Proxy(state) => state.borrow().callable,
            _ => false,
        }
    }

    /// Whether this object has a `[[Construct]]`, i.e. may be the target of `new` or
    /// `Reflect.construct`.
    pub fn is_constructor(&self) -> bool {
        let data = self.data.borrow();
        match &data.kind {
            // The flag, not "is a function": a plain method is a function and is not a
            // constructor. Set at creation by whoever knows what was built.
            ObjectKind::Function(_) => data.constructible,
            // Recorded at creation for the same reason as callability: revoking drops the
            // target without taking `[[Construct]]` away.
            ObjectKind::Proxy(state) => state.borrow().constructible,
            _ => false,
        }
    }

    /// Give this function a `[[Construct]]`.
    ///
    /// Only for callables that are constructors by definition: ordinary functions, class
    /// constructors, and the built-in constructors. Everything else — methods, arrows,
    /// generators, async functions — keeps the default `false`.
    pub fn set_constructible(&self, yes: bool) {
        self.data.borrow_mut().constructible = yes;
    }

    /// This object's proxy slots, when it is one (revoked or not).
    pub fn proxy_state(&self) -> Option<ProxyRef> {
        match &self.data.borrow().kind {
            ObjectKind::Proxy(state) => Some(state.clone()),
            _ => None,
        }
    }

    /// A proxy over `target` reading its traps from `handler`.
    ///
    /// The prototype stays empty on purpose: `[[GetPrototypeOf]]` on a proxy forwards to
    /// the target through the `getPrototypeOf` trap, so copying the target's prototype
    /// here would be a second, disagreeing source of truth.
    pub fn proxy(heap: &js_gc::Heap, target: Gc<JsObject>, handler: Value) -> Gc<JsObject> {
        // Read callability from the target *now*: it is the one moment the answer is
        // available, and it outlives revocation.
        let callable = target.is_callable();
        let constructible = target.is_constructor();
        Gc::new(
            heap,
            JsObject::new(
                ObjectKind::Proxy(std::rc::Rc::new(std::cell::RefCell::new(ProxyState {
                    callable,
                    constructible,
                    target: Some(target),
                    handler: Some(handler),
                }))),
                None,
            ),
        )
    }

    /// The prototype.
    pub fn proto(&self) -> Option<Gc<JsObject>> {
        self.data.borrow().proto.clone()
    }

    /// Replace the prototype.
    pub fn set_proto(&self, proto: Option<Gc<JsObject>>) {
        self.data.borrow_mut().proto = proto;
    }

    /// The object a method was defined on, for `super` to resolve against.
    pub fn home_object(&self) -> Option<Gc<JsObject>> {
        self.data.borrow().home_object.clone()
    }

    /// Record the object a method was defined on.
    ///
    /// A method has no `[[Construct]]` (§10.2.1): `new (o.m)` is a `TypeError` even
    /// though `o.m` is a function. Acquiring a home object is exactly the moment a
    /// function becomes a method, so the flag is cleared here rather than left to every
    /// method-defining call site to remember.
    pub fn set_home_object(&self, home: Option<Gc<JsObject>>) {
        let mut data = self.data.borrow_mut();
        data.home_object = home.clone();
        if home.is_some() {
            data.constructible = false;
        }
    }

    /// Whether this is a class constructor.
    pub fn is_class_constructor(&self) -> bool {
        self.data.borrow().class_constructor
    }

    /// Mark this function as a class constructor.
    pub fn set_class_constructor(&self, yes: bool) {
        self.data.borrow_mut().class_constructor = yes;
    }

    /// The elements of an array object, if this is one.
    pub fn array_elements(&self) -> Option<Vec<Value>> {
        match &self.data.borrow().kind {
            ObjectKind::Array { elements, .. } => Some(elements.clone()),
            _ => None,
        }
    }

    /// Every own key with its descriptor, array indices included.
    ///
    /// [`JsObject::own_property_keys`] answers the same question without the
    /// descriptors; a proxy invariant needs both at once, because it has to check the
    /// *value* of a key it is told about and the presence of a key it is not.
    pub fn property_entries(&self) -> Vec<(PropertyKey, Property)> {
        let data = self.data.borrow();
        let mut out: Vec<(PropertyKey, Property)> = data.props.to_vec();
        // An array's elements live in their own vector rather than the property table, so
        // they have to be synthesized — with the attributes an ordinary element has.
        if let ObjectKind::Array { elements, .. } = &data.kind {
            for (index, element) in elements.iter().enumerate() {
                if matches!(element, Value::Hole) {
                    continue;
                }
                let key = PropertyKey::from(index.to_string());
                if !out.iter().any(|(existing, _)| *existing == key) {
                    out.push((key, Property::data(element.clone())));
                }
            }
        }
        // `length` is own on an array without being in the property table either. It is
        // writable, non-enumerable and — the part proxy invariants care about —
        // non-configurable, so leaving it out let a handler hide or redefine it.
        if matches!(data.kind, ObjectKind::Array { .. })
            && let Some(length) = self.array_length()
        {
            let key = PropertyKey::from("length");
            if !out.iter().any(|(existing, _)| *existing == key) {
                out.push((
                    key,
                    Property {
                        value: Some(length_value(length)),
                        writable: self.length_writable(),
                        enumerable: false,
                        configurable: false,
                        getter: None,
                        setter: None,
                    },
                ));
            }
        }
        out
    }

    /// Whether this object has array elements at all.
    pub fn is_array(&self) -> bool {
        match &self.data.borrow().kind {
            ObjectKind::Array { .. } => true,
            // `Array.isArray` is specified to see *through* a proxy: it forwards
            // `[[Get]]`/prototype checks to the target, and the answer for a proxy of an
            // array is yes.
            ObjectKind::Proxy(state) => state
                .borrow()
                .target
                .as_ref()
                .is_some_and(|target| target.is_array()),
            _ => false,
        }
    }

    /// `length` for an array, reading it out of the element vector.
    /// The compiled pattern, when this object is a `RegExp`.
    pub fn regexp(&self) -> Option<crate::builtins::regexp::RegExpData> {
        match &self.data.borrow().kind {
            ObjectKind::RegExp(data) => Some(data.clone()),
            _ => None,
        }
    }

    /// The held resources, when this object is a `DisposableStack`.
    pub fn disposable_stack(&self) -> Option<crate::builtins::disposable::StackState> {
        match &self.data.borrow().kind {
            ObjectKind::DisposableStack(state) => Some(state.clone()),
            _ => None,
        }
    }

    /// Make this object a `DisposableStack` holding `state`.
    pub fn set_disposable_stack(&self, state: crate::builtins::disposable::StackState) {
        self.data.borrow_mut().kind = ObjectKind::DisposableStack(state);
    }

    /// Make this object a `RegExp` carrying `data`.
    pub fn set_regexp(&self, data: crate::builtins::regexp::RegExpData) {
        self.data.borrow_mut().kind = ObjectKind::RegExp(data);
    }

    /// Whether this object is a `RegExp`.
    pub fn is_regexp(&self) -> bool {
        matches!(self.data.borrow().kind, ObjectKind::RegExp(_))
    }

    /// An array's element count, when this object is an array.
    pub fn array_length(&self) -> Option<usize> {
        match &self.data.borrow().kind {
            ObjectKind::Array { length, .. } => Some(*length),
            _ => None,
        }
    }

    /// Read one array element.
    pub fn array_get(&self, index: usize) -> Option<Value> {
        match &self.data.borrow().kind {
            ObjectKind::Array { elements, .. } => elements.get(index).cloned(),
            _ => None,
        }
    }

    /// Append to an array and return the new length.
    pub fn array_push(&self, value: Value) -> usize {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements, length } => {
                elements.push(value);
                *length = elements.len();
                *length
            }
            _ => 0,
        }
    }

    /// Store into an array, and return whether the write happened.
    ///
    /// The gap an out-of-range store leaves behind is filled with [`Value::Hole`], not
    /// `undefined`: `a = [1]; a[3] = 4` has holes at 1 and 2, which `forEach` skips
    /// and the debug printer reports. A store *of* `undefined` is a real element.
    pub fn array_set(&self, index: usize, value: Value) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements, length } => {
                if index >= *length {
                    // Growing has to fill the gap, and filling it means the vector really
                    // does hold `index + 1` slots — a store at `2^32-2` is 68 GB whatever
                    // the length field says. Refuse instead of aborting the process: the
                    // reference throws `RangeError` from the allocation. A caller that
                    // cares converts this `false` into the spec's answer.
                    if index + 1 > ARRAY_ELEMENT_LIMIT {
                        return false;
                    }
                    elements.resize(index + 1, Value::Hole);
                    *length = elements.len();
                } else if index >= elements.len() {
                    // Inside the logical length but past the dense prefix: a hole that is
                    // being filled, so the vector has to catch up first.
                    elements.resize(index + 1, Value::Hole);
                }
                elements[index] = value;
                true
            }
            _ => false,
        }
    }

    /// Store an element, honouring its attributes, reporting whether it happened.
    ///
    /// Array methods that write back (`sort`, `reverse`, `splice`, …) use this rather
    /// than [`Self::array_set`]: the spec has them go through `[[Set]]`, so a frozen
    /// element makes them throw instead of being quietly overwritten.
    pub fn array_store(&self, index: usize, value: Value) -> bool {
        if self.elements_writable() {
            return self.array_set(index, value);
        }
        if self.element_attributes(index as u32).is_some() {
            // A recorded descriptor decides on its own.
            return !self.element_is_locked(index as u32) && self.array_set(index, value);
        }
        false
    }

    /// Store `value` as a plain element without creating holes, used by `push`.
    pub fn array_push_hole(&self) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements, length } => {
                elements.push(Value::Hole);
                *length = elements.len();
                true
            }
            _ => false,
        }
    }

    /// Set the length of an array, truncating or extending with holes.
    ///
    /// `a.length = 5` on a two-element array leaves three holes, so the fill value is
    /// [`Value::Hole`] rather than `undefined`.
    /// Growing only records the new length: the extra slots are holes, and a hole needs
    /// no storage. That is what makes `a = []; a.length = 4294967295` an O(1) operation
    /// instead of a 62 GB allocation.
    pub fn array_set_length(&self, new_length: usize) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements, length } => {
                if new_length < *length {
                    // Shrinking really does drop the trailing elements.
                    elements.truncate(new_length);
                }
                *length = new_length;
                true
            }
            _ => false,
        }
    }

    /// Truncate an array to `length` elements without extending it.
    ///
    /// This is what `arr.length = n` does for `n` smaller than the current length;
    /// it is separate from [`Self::array_set_length`] because growing an array by
    /// assignment produces holes, which are indistinguishable from `undefined`
    /// here.
    pub fn array_truncate(&self, new_length: usize) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements, length } => {
                if new_length < elements.len() {
                    elements.truncate(new_length);
                }
                if new_length < *length {
                    *length = new_length;
                }
                true
            }
            _ => false,
        }
    }

    /// The own data property `key`, if there is one.
    pub fn own_property_key(&self, key: &PropertyKey) -> Option<Property> {
        let data = self.data.borrow();
        data.props
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, property)| property.clone())
    }

    /// The own data property `name`, if there is one.
    pub fn own_property(&self, name: &str) -> Option<Property> {
        self.own_property_key(&PropertyKey::from(name))
    }

    /// Whether the object has an own property `key`.
    pub fn has_own_key(&self, key: &PropertyKey) -> bool {
        let data = self.data.borrow();
        data.props.iter().any(|(existing, _)| existing == key)
    }

    /// Whether the object has an own property `name`.
    pub fn has_own(&self, name: &str) -> bool {
        self.has_own_key(&PropertyKey::from(name))
    }

    /// Define or replace an own property, keeping the original position when the
    /// key already exists.
    pub fn define_key(&self, key: &PropertyKey, property: Property) {
        let mut data = self.data.borrow_mut();
        match data.props.iter_mut().find(|(existing, _)| existing == key) {
            Some((_, slot)) => *slot = property,
            None => data.props.push((key.clone(), property)),
        }
    }

    /// Define or replace an own property under a string key.
    pub fn define(&self, name: &str, property: Property) {
        self.define_key(&PropertyKey::from(name), property);
    }

    /// Define an own data property, with the default attributes.
    ///
    /// This *creates*: on an existing property it overwrites the attributes too, so
    /// it is the wrong call for a plain assignment. Use [`Self::write_value`] for that.
    pub fn define_data(&self, name: &str, value: Value) {
        self.define(name, Property::data(value));
    }

    /// Define an own data property under `key`.
    pub fn define_data_key(&self, key: &PropertyKey, value: Value) {
        self.define_key(key, Property::data(value));
    }

    /// Store a value into an existing own data property, keeping its attributes.
    ///
    /// Returns `false` when the property is absent, is an accessor, or is not
    /// writable. An assignment has to go through this rather than
    /// [`Self::define_data`], because redefining is how a non-configurable global
    /// silently became configurable again and started answering `delete` with `true`.
    pub fn write_value(&self, name: &str, value: Value) -> bool {
        self.write_value_key(&PropertyKey::from(name), value)
    }

    /// Store a value into an existing own data property under `key`.
    pub fn write_value_key(&self, key: &PropertyKey, value: Value) -> bool {
        let mut data = self.data.borrow_mut();
        let Some((_, property)) = data.props.iter_mut().find(|(existing, _)| existing == key)
        else {
            return false;
        };
        if property.is_accessor() || !property.writable {
            return false;
        }
        property.value = Some(value);
        true
    }

    /// Delete an own property, reporting whether it was there and removable.
    pub fn delete(&self, name: &str) -> bool {
        self.delete_key(&PropertyKey::from(name))
    }

    /// Delete an own property under `key`.
    pub fn delete_key(&self, key: &PropertyKey) -> bool {
        let mut data = self.data.borrow_mut();
        match data.props.iter().position(|(existing, _)| existing == key) {
            Some(index) => {
                if data.props[index].1.configurable {
                    data.props.remove(index);
                    true
                } else {
                    false
                }
            }
            None => true,
        }
    }

    /// Whether new properties may be added.
    pub fn is_extensible(&self) -> bool {
        self.data.borrow().extensible
    }

    /// Make the object non-extensible.
    pub fn set_extensible(&self, extensible: bool) {
        self.data.borrow_mut().extensible = extensible;
    }

    /// Whether array elements may be replaced.
    pub fn elements_writable(&self) -> bool {
        self.data.borrow().elements_writable
    }

    /// Whether an array's `length` may be changed.
    pub fn length_writable(&self) -> bool {
        self.data.borrow().length_writable
    }

    /// Record whether `length` may be changed.
    pub fn set_length_writable(&self, writable: bool) {
        self.data.borrow_mut().length_writable = writable;
    }

    /// Make array elements read-only, as `Object.freeze` does.
    pub fn set_elements_writable(&self, writable: bool) {
        self.data.borrow_mut().elements_writable = writable;
    }

    /// The attributes recorded for an array index, if `defineProperty` set any.
    pub fn element_attributes(&self, index: u32) -> Option<Property> {
        self.own_property(&index.to_string())
    }

    /// Whether `delete` at `index` would be rejected.
    ///
    /// Separate from [`JsObject::element_is_locked`]: a store needs the element to be
    /// non-writable, but a *delete* — which is what truncating an array does — only needs
    /// it to be non-configurable. `Object.defineProperty(a, "3", {configurable: false})`
    /// leaves `a[3]` writable and yet undeletable, so `a.length = 2` must fail.
    pub fn element_is_undeletable(&self, index: u32) -> bool {
        if !self.elements_writable() {
            return true;
        }
        self.element_attributes(index)
            .is_some_and(|property| !property.configurable)
    }

    /// Whether a store at `index` would be rejected.
    ///
    /// An element is locked either by `Object.freeze` on the whole object or by a
    /// non-configurable, non-writable descriptor recorded for that one index.
    pub fn element_is_locked(&self, index: u32) -> bool {
        if !self.elements_writable() {
            return true;
        }
        self.element_attributes(index)
            .is_some_and(|property| !property.writable && !property.configurable)
    }

    /// The own property *names*, in definition order. Symbol keys are excluded:
    /// `Object.keys` and `for…in` cannot see them.
    pub fn own_keys(&self) -> Vec<crate::value::Wtf8> {
        self.data
            .borrow()
            .props
            .iter()
            .filter_map(|(key, _)| key.as_wtf8().cloned())
            .collect()
    }

    /// The own *enumerable* property names, in definition order.
    pub fn own_enumerable_keys(&self) -> Vec<crate::value::Wtf8> {
        self.data
            .borrow()
            .props
            .iter()
            .filter(|(_, property)| property.enumerable)
            .filter_map(|(key, _)| key.as_wtf8().cloned())
            .collect()
    }

    /// The own symbol keys, in definition order, for `Object.getOwnPropertySymbols`.
    pub fn own_symbol_keys(&self) -> Vec<Gc<JsSymbol>> {
        self.data
            .borrow()
            .props
            .iter()
            .filter_map(|(key, _)| match key {
                PropertyKey::Symbol(symbol) => Some(symbol.clone()),
                PropertyKey::String(_) => None,
            })
            .collect()
    }

    /// Every own key, strings first then symbols — the order `Reflect.ownKeys` and
    /// `Object.getOwnPropertyDescriptors` report, and the order `JSON.stringify`
    /// walks before it skips the symbols.
    pub fn own_property_keys(&self) -> Vec<PropertyKey> {
        let data = self.data.borrow();
        let mut strings: Vec<PropertyKey> = Vec::new();
        let mut symbols: Vec<PropertyKey> = Vec::new();
        for (key, _) in &data.props {
            if key.is_symbol() {
                symbols.push(key.clone());
            } else {
                strings.push(key.clone());
            }
        }
        strings.extend(symbols);
        strings
    }

    /// Own property keys in **definition order**, with symbols interleaved.
    ///
    /// [`JsObject::own_property_keys`] answers in the spec's order — integer-like keys
    /// ascending, then strings, then symbols — because that is what `Reflect.ownKeys` and
    /// `for...in` must do. A dumper has the opposite requirement: quickjs's
    /// `JS_PrintValue` walks the shape, so `{[Symbol("t")]: 1, y: 2}` prints the symbol
    /// first. That is not observable from a script except through such a dump, which is
    /// why this is a separate accessor rather than a change to `own_property_keys`.
    pub fn own_property_keys_in_definition_order(&self) -> Vec<PropertyKey> {
        self.data
            .borrow()
            .props
            .iter()
            .map(|(key, _)| key.clone())
            .collect()
    }

    /// The prototype chain, starting with the prototypes of this object.
    pub fn chain(&self) -> Vec<Gc<JsObject>> {
        let mut out = Vec::new();
        let mut proto = self.proto();
        while let Some(object) = proto {
            proto = object.proto();
            out.push(object);
        }
        out
    }
}

/// Whether two prototypes are the same one, `None` included.
///
/// A proxy's prototype invariants have to compare "no prototype" against "no
/// prototype" without unwrapping either side first.
pub(crate) fn same_proto(a: Option<&Gc<JsObject>>, b: Option<&Gc<JsObject>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.ptr_eq(b),
        _ => false,
    }
}

/// Walk the prototype chain from `object` looking for `name`.
///
/// Returns the object that owns the property along with the property itself. The
/// owner matters for a store: an inherited accessor is invoked on the receiver,
/// while an inherited *data* property is shadowed by a new own property.
/// The own property `key` names on `object`, arrays included.
///
/// An array keeps its elements in an element vector and derives its `length`, so its
/// property table knows about neither: a hole is a slot in the vector, not a missing
/// property, and `length` is not stored at all. `Array.prototype` is itself an array, so
/// every walk down the chain needs this same care — without it `1 in [1,,3]` stayed false
/// with `Array.prototype[1]` defined, `"length" in []` was false outright, and a hole did
/// not read an inherited element.
pub fn own_property_including_array(object: &Gc<JsObject>, key: &PropertyKey) -> Option<Property> {
    if let Some(property) = object.own_property_key(key) {
        return Some(property);
    }
    // A typed array's indices are its elements in the buffer, which the property table
    // does not hold either. Checked before the `is_array` return below, because a typed
    // array is not one.
    if object.typed_array_parts().is_some() {
        // A detached view exposes no indices at all: `Reflect.has(detached, "0")` is false
        // even though index 0 was in range before the buffer went away.
        let index = key.as_str().and_then(array_index)?;
        let in_range = (index as usize) < object.typed_array_length()?;
        return match object.typed_array_element(index as usize) {
            Some(value) if in_range => Some(Property::data(value)),
            _ => None,
        };
    }
    if !object.is_array() {
        return None;
    }
    let text = key.as_str()?;
    // `length` is non-enumerable and non-configurable; `writable` unless a definition
    // turned it off.
    if text == "length" {
        return Some(Property {
            value: Some(length_value(object.array_length()?)),
            writable: object.length_writable(),
            enumerable: false,
            configurable: false,
            getter: None,
            setter: None,
        });
    }
    // An element is a plain writable/enumerable/configurable data property, and a hole is
    // not one at all.
    let index = array_index(text)?;
    match object.array_get(index as usize) {
        Some(Value::Hole) | None => None,
        Some(value) => Some(Property::data(value)),
    }
}

/// Whether `object` has `key` as an *own* property, arrays included.
pub fn has_own(object: &Gc<JsObject>, key: &PropertyKey) -> bool {
    own_property_including_array(object, key).is_some()
}

/// `key` on `object` or anywhere up its prototype chain, with the owner.
///
/// Array-aware at every step: an array's elements and its derived `length` are not in the
/// property table, and `Array.prototype` is itself an array.
pub fn lookup_key(object: &Gc<JsObject>, key: &PropertyKey) -> Option<(Gc<JsObject>, Property)> {
    if let Some(property) = own_property_including_array(object, key) {
        return Some((object.clone(), property));
    }
    for ancestor in object.chain() {
        if let Some(property) = own_property_including_array(&ancestor, key) {
            return Some((ancestor, property));
        }
    }
    None
}

/// [`lookup_key`] for a string key.
pub fn lookup(object: &Gc<JsObject>, name: &str) -> Option<(Gc<JsObject>, Property)> {
    lookup_key(object, &PropertyKey::from(name))
}

/// The largest array index that may be *stored*, i.e. the largest dense vector this engine
/// will grow to an index for.
///
/// The spec's `length` limit is `2^32-1`, but the elements are a dense `Vec<Value>` here,
/// so a store at index `2^32-2` is a 68 GB reservation. The reference (which keeps
/// elements in a hash map) answers instantly. Refusing past this bound keeps the process
/// alive; the conformance tests that really do store near `2^32` are recorded in
/// `docs/test262-status.md` as a limitation rather than quietly answered wrong.
pub const ARRAY_ELEMENT_LIMIT: usize = 1 << 26;

/// An array's `length` as a JavaScript number.
///
/// `Value::Int` only holds an `i32`, and an array's length may legitimately reach
/// `2^32-1`, so a plain cast is not enough: `a[2**32-2] = 1; a.length` came back as `-1`,
/// and `Object.getOwnPropertyDescriptor(a, "length").value` with it. Every
/// `LengthOfArrayLike` over such an array then ran to `-1`, and the paths without their
/// own `RangeError` check turned into `memory allocation of 68719476720 bytes failed`.
pub fn length_value(length: usize) -> Value {
    if length <= i32::MAX as usize {
        Value::Int(length as i32)
    } else {
        Value::Float(length as f64)
    }
}

/// An array *index* as a JavaScript number.
///
/// Same problem as [`length_value`] and the same fix: an index may be as large as
/// `2^32-2`, which `Value::Int` cannot hold. `a[2**32-2] = 1; a.indexOf(1)` was answering
/// `-2` because the index was truncated on the way out.
pub fn index_value(index: usize) -> Value {
    length_value(index)
}

/// The array index a property key names, if it names one.
///
/// A key is an index when it is the canonical decimal spelling of an integer below
/// `2^32 - 1` — the same rule the atom table uses to decide whether a key is a
/// tagged index. `"01"` and `"-1"` are ordinary string keys.
pub fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 {
        return None;
    }
    let bytes = key.as_bytes();
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() != 1 {
        return None;
    }
    let value: u32 = key.parse().ok()?;
    // `2^32 - 1` is reserved for the array length, so it is not an index.
    (value < u32::MAX).then_some(value)
}

impl Trace for JsObject {
    fn trace(&self, tracer: &mut Tracer) {
        self.data.borrow().trace(tracer);
    }
}

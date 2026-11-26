//! `DisposableStack` and `AsyncDisposableStack`.
//!
//! A stack is an ordered list of resources disposed in reverse order, which is what makes
//! it the imperative counterpart of a `using` declaration: `use`/`adopt`/`defer` push, and
//! `dispose` pops until the list is empty.
//!
//! # Intrinsics, not the global binding
//!
//! `move` returns a *new* stack of the same kind. It builds that object from the recorded
//! intrinsic prototype rather than from `globalThis.DisposableStack`, because a script is
//! allowed to reassign or delete the global — and the disposables proposal is explicit that
//! doing so must not change what `move` returns. The test suite checks exactly that.
//!
//! # Known gap
//!
//! `AsyncDisposableStack` disposes synchronously: this runtime has no `Promise` builtin, so
//! an `asyncDispose` that returns a promise has that promise dropped rather than awaited.
//! The bookkeeping — reverse order, error aggregation, `move` — is the same.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal, WellKnown};
use crate::object::{JsObject, Property, PropertyKey};
use crate::value::Value;
use js_gc::Gc;

/// The resources a stack still holds, plus whether it is the async kind.
///
/// Cloned out of the object on every access because the interpreter may reallocate the
/// object table while a method runs; the authoritative copy is written back afterwards.
#[derive(Clone)]
pub struct StackState {
    /// `(resource, dispose method)` pairs, in the order they were pushed.
    pub resources: Vec<(Value, Value)>,
    /// Whether this is an `AsyncDisposableStack`.
    pub async_form: bool,
    /// Whether `dispose` has already run, which makes it a no-op and `use` a `TypeError`.
    pub disposed: bool,
}

impl StackState {
    /// An empty stack of the given kind.
    pub fn new(async_form: bool) -> StackState {
        StackState {
            resources: Vec::new(),
            async_form,
            disposed: false,
        }
    }
}

impl js_gc::Trace for StackState {
    fn trace(&self, tracer: &mut js_gc::Tracer) {
        for (value, method) in &self.resources {
            js_gc::Trace::trace(value, tracer);
            js_gc::Trace::trace(method, tracer);
        }
    }
}

/// Install both stacks.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let disposable_proto = interp.new_object(Some(object_proto.clone()));
    let constructor = install_one(interp, &disposable_proto, "DisposableStack", false);
    interp.realm.disposable_stack_proto = Some(disposable_proto);

    let async_proto = interp.new_object(Some(object_proto.clone()));
    let async_constructor = install_one(interp, &async_proto, "AsyncDisposableStack", true);
    interp.realm.async_disposable_stack_proto = Some(async_proto);

    let global = interp.realm.global.clone();
    global.define(
        "DisposableStack",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
    global.define(
        "AsyncDisposableStack",
        Property::with_attributes(Value::Object(async_constructor), true, false),
    );
}

fn install_one(
    interp: &mut Interp<'_>,
    prototype: &Gc<JsObject>,
    name: &str,
    async_form: bool,
) -> Gc<JsObject> {
    let constructor = interp.new_native_constructor(name, 0, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(prototype.clone()), false, false),
    );
    prototype.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );

    for (method, length, func) in [
        ("use", 1, use_ as crate::object::NativeFn),
        ("adopt", 2, adopt),
        ("defer", 1, defer),
        ("move", 0, move_),
        ("dispose", 0, dispose),
        ("disposeAsync", 0, dispose_async),
    ] {
        define_method(prototype, method, length, func, interp);
    }
    // `disposed` is an accessor so it cannot be set, and `Symbol.dispose` is what makes
    // `using stack = new DisposableStack()` work.
    let getter = interp.new_native("get disposed", 0, disposed_getter);
    prototype.define(
        "disposed",
        Property {
            getter: Some(Value::Object(getter)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
    for which in [WellKnown::Disposable, WellKnown::AsyncDisposable] {
        let method = interp.new_native("[Symbol.dispose]", 0, dispose);
        prototype.define_key(
            &interp.realm.well_known_key(which),
            Property::with_attributes(Value::Object(method), true, false),
        );
    }
    let symbol_name = interp.new_native("[Symbol.toStringTag]", 0, to_string_tag);
    prototype.define_key(
        &interp.realm.well_known_key(WellKnown::ToStringTag),
        Property::with_attributes(Value::Object(symbol_name), true, false),
    );
    let _ = async_form;
    constructor
}

/// The tag `Object.prototype.toString` reports.
fn to_string_tag(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let stack = state_of(interp, &this)?;
    Ok(interp.string(if stack.async_form {
        "AsyncDisposableStack"
    } else {
        "DisposableStack"
    }))
}

fn state_of(interp: &Interp<'_>, this: &Value) -> JsResult<StackState> {
    match this
        .as_object()
        .and_then(|object| object.disposable_stack())
    {
        Some(state) => Ok(state),
        None => Err(interp.type_error("method called on a non-DisposableStack")),
    }
}

fn write_back(object: &Gc<JsObject>, state: StackState) {
    object.set_disposable_stack(state);
}

/// `new DisposableStack()`.
fn constructor(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // `new` hands us the fresh object, whose prototype came from the constructor; a plain
    // call has no receiver and builds one from the intrinsic prototype.
    let object = match this.as_object() {
        Some(object) => object.clone(),
        None => {
            let proto = interp
                .realm
                .disposable_stack_proto
                .clone()
                .unwrap_or_else(|| interp.realm.object_proto.clone());
            interp.new_object(Some(proto))
        }
    };
    // Which kind this is follows from the prototype the object was built with, so the same
    // native function can serve both constructors.
    let async_form = interp
        .realm
        .async_disposable_stack_proto
        .as_ref()
        .is_some_and(|proto| object.proto().is_some_and(|own| own.ptr_eq(proto)));
    object.set_disposable_stack(StackState::new(async_form));
    Ok(Value::Object(object))
}

/// `stack.use(value)` — validate and push, returning the value.
fn use_(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("use called on a non-object"));
    };
    let value = arg(args, 0);
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return Err(interp.type_error("DisposableStack has already been disposed"));
    }
    let method = interp.using_method_for(&value, state.async_form)?;
    // A nullish resource has a nullish method and is still *counted*: `use(null)` is legal
    // and disposes to nothing.
    if !value.is_nullish() {
        state.resources.push((value.clone(), method));
    }
    write_back(object, state);
    Ok(value)
}

/// `stack.adopt(value, onDispose)` — wrap a resource that has no dispose protocol.
fn adopt(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("adopt called on a non-object"));
    };
    let value = arg(args, 0);
    let on_dispose = arg(args, 1);
    if !on_dispose
        .as_object()
        .is_some_and(|function| function.is_callable())
    {
        return Err(interp.type_error("adopt needs a disposal function"));
    }
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return Err(interp.type_error("DisposableStack has already been disposed"));
    }
    // The callback receives the value: `adopt` is how a resource that does not know
    // `Symbol.dispose` is given a disposer that does. The wrapper *is* the resource as far
    // as disposal is concerned — it is passed as both the receiver and the method — which is
    // how it can carry the callback and the value without capturing them.
    let callable = interp.new_native("onDispose", 1, adopt_callback);
    callable.define(
        "_adopts",
        Property::with_attributes(value.clone(), false, false),
    );
    callable.define(
        "_callback",
        Property::with_attributes(on_dispose, false, false),
    );
    state
        .resources
        .push((Value::Object(callable.clone()), Value::Object(callable)));
    write_back(object, state);
    Ok(value)
}

/// The wrapper `adopt` pushes: calls the script's callback with the adopted value.
fn adopt_callback(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let wrapper = this
        .as_object()
        .ok_or_else(|| interp.type_error("adopt wrapper called without a receiver"))?;
    let value = wrapper
        .own_property("_adopts")
        .and_then(|property| property.value)
        .unwrap_or(Value::Undefined);
    let callback = wrapper
        .own_property("_callback")
        .and_then(|property| property.value)
        .unwrap_or(Value::Undefined);
    interp.call_value(callback, Value::Undefined, vec![value])
}

/// `stack.defer(fn)` — push a plain callback.
fn defer(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("defer called on a non-object"));
    };
    let callback = arg(args, 0);
    if !callback
        .as_object()
        .is_some_and(|function| function.is_callable())
    {
        return Err(interp.type_error("defer needs a callback"));
    }
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return Err(interp.type_error("DisposableStack has already been disposed"));
    }
    // A deferred callback takes no arguments, so the resource is an absent receiver and the
    // method is the callback itself.
    state.resources.push((Value::Undefined, callback));
    write_back(object, state);
    Ok(Value::Undefined)
}

/// `stack.move()` — empty this stack into a fresh one.
fn move_(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("move called on a non-object"));
    };
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return Err(interp.type_error("DisposableStack has already been disposed"));
    }
    let moved = StackState {
        resources: std::mem::take(&mut state.resources),
        async_form: state.async_form,
        disposed: false,
    };
    write_back(object, state);
    // The new stack comes from the *intrinsic* prototype — replacing or deleting the global
    // binding must not redirect `move`, which is what the test suite checks.
    let proto = if moved.async_form {
        interp.realm.async_disposable_stack_proto.clone()
    } else {
        interp.realm.disposable_stack_proto.clone()
    }
    .unwrap_or_else(|| interp.realm.object_proto.clone());
    let fresh = interp.new_object(Some(proto));
    fresh.set_disposable_stack(moved);
    Ok(Value::Object(fresh))
}

/// `stack.dispose()` — dispose everything in reverse order.
fn dispose(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("dispose called on a non-object"));
    };
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return Ok(Value::Undefined);
    }
    state.disposed = true;
    let resources = std::mem::take(&mut state.resources);
    write_back(object, state);
    dispose_all(interp, resources)?;
    Ok(Value::Undefined)
}

/// `stack.disposeAsync()` — dispose everything in reverse order, awaiting each disposer.
///
/// The resources are handed to the interpreter rather than run here: awaiting a disposer
/// means parking between two of them, which is the microtask queue's job.
fn dispose_async(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Err(interp.type_error("disposeAsync called on a non-object"));
    };
    let mut state = state_of(interp, &this)?;
    if state.disposed {
        return interp.promise_resolve_value(Value::Undefined);
    }
    state.disposed = true;
    let resources = std::mem::take(&mut state.resources);
    write_back(object, state);
    interp.dispose_async_all(resources)
}

/// Pop and call every resource, folding failures into one `SuppressedError` chain.
fn dispose_all(interp: &mut Interp<'_>, resources: Vec<(Value, Value)>) -> JsResult<()> {
    let mut error_state = Value::Uninitialized;
    for (value, method) in resources.into_iter().rev() {
        let Some(function) = method.as_object() else {
            continue;
        };
        if !function.is_callable() {
            continue;
        }
        match interp.call_value(Value::Object(function.clone()), value, Vec::new()) {
            Ok(_) => {}
            Err(Signal::Throw(thrown)) => {
                error_state = interp.merge_dispose_error(thrown, error_state);
            }
            Err(other) => return Err(other),
        }
    }
    if matches!(error_state, Value::Uninitialized) {
        return Ok(());
    }
    Err(Signal::Throw(error_state))
}

/// The `disposed` getter.
fn disposed_getter(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(state_of(interp, &this)?.disposed))
}

/// Silence the unused-import warning for `PropertyKey`, which the `define_key` calls above
/// need only through inference.
#[allow(dead_code)]
fn _property_key_is_used(_: PropertyKey) {}

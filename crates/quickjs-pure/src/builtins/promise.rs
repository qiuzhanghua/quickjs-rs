//! `Promise`, its prototype, and the combinator statics.
//!
//! Most of the machinery lives in the interpreter: the state is an
//! `ObjectKind::Promise`, and settling, `then` and the microtask queue are internal
//! operations `await` needs too. What is here is the surface a script touches — the
//! constructor, `then`/`catch`/`finally`, and the combinators, all of which are
//! arrangements of `perform_then` and the capability pair.
//!
//! Two things here are `Callable` variants rather than `Native`s. A `NativeFn` is a
//! function pointer and cannot capture, and both the per-element callbacks a combinator
//! installs and the shims `finally` installs have to remember state — so they carry it in
//! the callable itself, the same way `Proxy.revocable`'s `revoke` does.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::{Callable, CombinatorKind, JsObject, ObjectKind, Property};
use crate::value::Value;
use js_gc::Gc;

/// The `this` of a prototype method, which must be a promise.
fn this_promise(interp: &Interp<'_>, this: &Value) -> Result<Gc<JsObject>, Signal> {
    match this {
        Value::Object(object)
            if object.with(|data| matches!(data.kind, ObjectKind::Promise(_))) =>
        {
            Ok(object.clone())
        }
        _ => Err(interp.type_error("not a promise")),
    }
}

/// An argument as a callable, or `None` when it is not one.
///
/// A non-callable handler is not an error: it is dropped and the settlement passes
/// through, which is what makes `p.then(null, f)` behave as a `catch`.
fn callable(value: Option<&Value>) -> Option<Value> {
    match value {
        Some(value @ Value::Object(object)) if object.is_callable() => Some(value.clone()),
        _ => None,
    }
}

/// `new Promise(executor)`.
///
/// The executor runs **synchronously** — a `resolve` inside it settles the promise before
/// the constructor returns, and only the reactions are deferred.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let executor = arg(args, 0);
    if !matches!(&executor, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("Promise executor is not a function"));
    }
    let (promise, resolve, reject) = interp.new_promise_capability();
    // A throw from the executor rejects the promise rather than escaping. This is the
    // documented way to build a rejected promise: `new Promise(() => { throw e })`.
    if let Err(Signal::Throw(reason)) =
        interp.call_value(executor, Value::Undefined, vec![resolve, reject])
    {
        interp.reject_promise(&promise, reason);
    }
    Ok(Value::Object(promise))
}

/// `Promise.prototype.then(onFulfilled, onRejected)`.
fn then(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let promise = this_promise(interp, &this)?;
    interp.perform_then(&promise, callable(args.first()), callable(args.get(1)))
}

/// `Promise.prototype.catch(onRejected)`.
fn catch(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let promise = this_promise(interp, &this)?;
    interp.perform_then(&promise, None, callable(args.first()))
}

/// `Promise.prototype.finally(onFinally)`.
///
/// Not `then(onFinally, onFinally)`: the callback runs for both outcomes, its return
/// value is *ignored* unless it is a thenable, and the original value or reason continues
/// down the chain. Passing the callback straight to `then` would turn
/// `p.finally(() => 1)` into a promise of `1`.
fn finally(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let promise = this_promise(interp, &this)?;
    let callback = callable(args.first());
    let on_fulfilled = JsObject::function(
        interp.heap,
        Some(interp.realm.function_proto.clone()),
        Callable::FinallyShim {
            callback: callback.clone(),
            fulfilled: true,
        },
    );
    let on_rejected = JsObject::function(
        interp.heap,
        Some(interp.realm.function_proto.clone()),
        Callable::FinallyShim {
            callback,
            fulfilled: false,
        },
    );
    interp.perform_then(
        &promise,
        Some(Value::Object(on_fulfilled)),
        Some(Value::Object(on_rejected)),
    )
}

/// `Promise.resolve(value)`.
fn resolve(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    // A promise is returned as-is, so `Promise.resolve(p) === p`. Only promises of *this*
    // realm can be recognised, which is the only kind this runtime can hold.
    if let Value::Object(object) = &value
        && interp.promise_state(object).is_some()
    {
        return Ok(value);
    }
    interp.promise_resolve_value(value)
}

/// `Promise.reject(reason)`.
fn reject(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let reason = arg(args, 0);
    let promise = interp.new_promise();
    interp.reject_promise(&promise, reason);
    Ok(Value::Object(promise))
}

fn all(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    combinator(interp, args, CombinatorKind::All)
}

fn all_settled(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    combinator(interp, args, CombinatorKind::AllSettled)
}

fn any(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    combinator(interp, args, CombinatorKind::Any)
}

fn race(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    combinator(interp, args, CombinatorKind::Race)
}

/// The shared body of `all`/`allSettled`/`any`/`race`.
///
/// The per-element state lives on a hidden object so the callbacks can reach it: they are
/// function pointers, and `CombinatorSettle` carries the object plus the element's index.
fn combinator(interp: &mut Interp<'_>, args: &[Value], kind: CombinatorKind) -> JsResult<Value> {
    let elements = iterate(interp, &arg(args, 0))?;
    let (promise, resolve, reject) = interp.new_promise_capability();
    let count = elements.len();
    if count == 0 {
        // §27.2.4.1.1 step 6: an empty iteration settles at once, and the three
        // combinators settle differently.
        match kind {
            CombinatorKind::All | CombinatorKind::AllSettled => {
                let empty = interp.new_array(Vec::new());
                interp.fulfill_promise(&promise, Value::Object(empty));
            }
            CombinatorKind::Any => {
                let error = interp.make_error("AggregateError", "all promises were rejected");
                interp.reject_promise(&promise, error);
            }
            // `race([])` never settles.
            CombinatorKind::Race => {}
        }
        return Ok(Value::Object(promise));
    }
    // Results are filled by index so `all` keeps the input order rather than the
    // settlement order.
    let results = interp.new_array(vec![Value::Undefined; count]);
    let state = interp.new_object(Some(interp.realm.object_proto.clone()));
    state.define_data("remaining", Value::Int(count as i32));
    state.define_data("results", Value::Object(results));
    state.define_data("outcome", Value::Object(promise.clone()));
    state.define_data("resolve", resolve);
    state.define_data("reject", reject);
    let kind_value = match kind {
        CombinatorKind::All => 0,
        CombinatorKind::AllSettled => 1,
        CombinatorKind::Any => 2,
        CombinatorKind::Race => 3,
    };
    state.define_data("kind", Value::Int(kind_value));
    for (index, element) in elements.into_iter().enumerate() {
        let resolved = interp.promise_resolve_value(element)?;
        let Value::Object(resolved) = resolved else {
            continue;
        };
        let fulfilled = JsObject::function(
            interp.heap,
            Some(interp.realm.function_proto.clone()),
            Callable::CombinatorSettle {
                state: state.clone(),
                index,
                fulfilled: true,
            },
        );
        let rejected = JsObject::function(
            interp.heap,
            Some(interp.realm.function_proto.clone()),
            Callable::CombinatorSettle {
                state: state.clone(),
                index,
                fulfilled: false,
            },
        );
        interp.perform_then(
            &resolved,
            Some(Value::Object(fulfilled)),
            Some(Value::Object(rejected)),
        )?;
    }
    Ok(Value::Object(promise))
}

/// The elements of the iterable the combinators accept.
///
/// Through `Symbol.iterator`, not as an array-like: `Promise.all(new Set([...]))` has to
/// work, and a string is iterable while having no `length`-indexed promise elements.
fn iterate(interp: &mut Interp<'_>, value: &Value) -> JsResult<Vec<Value>> {
    if matches!(value, Value::Undefined | Value::Null) {
        return Err(interp.type_error("argument is not iterable"));
    }
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Iterator);
    let method = interp.read_field_key(value, &key)?;
    if !matches!(&method, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("argument is not iterable"));
    }
    let iterator = interp.call_value(method, value.clone(), Vec::new())?;
    let Value::Object(iterator_object) = &iterator else {
        return Err(interp.type_error("iterator is not an object"));
    };
    let mut out = Vec::new();
    loop {
        let next = interp
            .get_property(iterator_object, "next")?
            .ok_or_else(|| interp.type_error("iterator has no next"))?;
        let step = interp.call_value(next, iterator.clone(), Vec::new())?;
        let Value::Object(step_object) = &step else {
            return Err(interp.type_error("iterator result is not an object"));
        };
        let done = interp
            .get_property(step_object, "done")?
            .is_some_and(|value| value.to_boolean());
        if done {
            return Ok(out);
        }
        out.push(
            interp
                .get_property(step_object, "value")?
                .unwrap_or(Value::Undefined),
        );
    }
}

/// `Promise.try(callback, ...args)`.
///
/// Calls the callback and settles with its result, turning a synchronous throw into a
/// rejection — the point of it is that the caller need not write
/// `new Promise(resolve => resolve(callback()))`.
fn try_(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let promise = interp.new_promise();
    let callback = arg(args, 0);
    let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
    match interp.call_value(callback, Value::Undefined, rest) {
        Ok(value) => {
            if let Err(Signal::Throw(error)) = interp.resolve_promise_with(&promise, value) {
                interp.reject_promise(&promise, error);
            }
        }
        Err(Signal::Throw(reason)) => interp.reject_promise(&promise, reason),
        Err(other) => return Err(other),
    }
    Ok(Value::Object(promise))
}

/// `Promise.withResolvers()` → `{promise, resolve, reject}`.
fn with_resolvers(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    let (promise, resolve, reject) = interp.new_promise_capability();
    let result = interp.new_object(Some(interp.realm.object_proto.clone()));
    result.define_data("promise", Value::Object(promise));
    result.define_data("resolve", resolve);
    result.define_data("reject", reject);
    Ok(Value::Object(result))
}

/// Install `Promise` and `%Promise.prototype%`.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    interp.realm.promise_proto = Some(proto.clone());
    define_method(&proto, "then", 2, then, interp);
    define_method(&proto, "catch", 1, catch, interp);
    define_method(&proto, "finally", 1, finally, interp);
    // `Object.prototype.toString.call(p)` says `[object Promise]` through this.
    let tag = interp.string("Promise");
    proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property::with_attributes(tag, false, false),
    );

    let constructor = interp.new_native_ctor("Promise", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::data(Value::Object(constructor.clone())),
    );
    for (name, length, func) in [
        ("resolve", 1, resolve as crate::object::NativeFn),
        ("reject", 1, reject),
        ("all", 1, all),
        ("allSettled", 1, all_settled),
        ("any", 1, any),
        ("race", 1, race),
        ("try", 1, try_),
        ("withResolvers", 0, with_resolvers),
    ] {
        define_method(&constructor, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define(
        "Promise",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

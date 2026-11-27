//! The standard library: the globals a script reaches without an import.
//!
//! The set installed here is the one milestone M4 targets — `Object`, `Function`,
//! `Array`, `String`, `Number`, `Boolean`, `Math`, `JSON` and the error classes —
//! which is what a script needs before it can do anything useful at all.
//!
//! Two conventions run through every module:
//!
//! * A method is a native function stored on a prototype, and it receives `this`
//!   exactly as a script method would. Nothing is special-cased by call site.
//! * Argument checks throw the same error *class* the reference does, because the
//!   test suite asserts on `instanceof` and on `e.name`.

pub(crate) mod array;
pub mod bigint;
pub(crate) mod date;
pub mod disposable;
mod error;
mod from_async;
mod function;
mod generator;
mod global;
pub mod iterator;
pub(crate) mod json;
pub mod map;
mod math;
mod number;
mod object;
pub mod promise;
pub mod proxy;
mod reflect;
pub mod regexp;
mod string;
mod symbol;
pub mod typedarray;
pub mod weak;

use crate::interp::Interp;
use crate::object::{JsObject, NativeFn, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install the standard library on `interp`'s realm.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let function_proto = interp.realm.function_proto.clone();
    let array_proto = interp.realm.array_proto.clone();

    // Prototypes for the primitives have to exist before anything is installed on
    // them, and before the constructors name them.
    let string_proto = interp.new_object(Some(object_proto.clone()));
    let number_proto = interp.new_object(Some(object_proto.clone()));
    let boolean_proto = interp.new_object(Some(object_proto.clone()));
    // Each of the three is a wrapper of its own type, carrying the empty value:
    // `Boolean.prototype.valueOf()` is `false`, `Number.prototype.valueOf()` is `0` and
    // `String.prototype.valueOf()` is `""`. Without the slot they were plain objects, so
    // all three threw "not a boolean/number/string" from a method called on its own
    // prototype — which §20.3.3, §21.1.3 and §22.1.3 make legal.
    interp.init_primitive_wrapper(&boolean_proto, Value::Bool(false));
    interp.init_primitive_wrapper(&number_proto, Value::Int(0));
    interp.init_primitive_wrapper(&string_proto, interp.string(""));
    interp.realm.string_proto = Some(string_proto.clone());
    interp.realm.number_proto = Some(number_proto.clone());
    interp.realm.boolean_proto = Some(boolean_proto.clone());

    // `%GeneratorPrototype%` has no constructor of its own, so it is created here and
    // kept on the realm for `new_generator` to use as the prototype of every generator.
    let generator_proto = interp.new_object(Some(object_proto.clone()));
    interp.realm.generator_proto = generator_proto.clone();

    error::install(interp);
    // `Symbol` installs first among the rest because the well-known symbols are property
    // keys the other builtins store their methods under — `Array.prototype[Symbol.iterator]`
    // is defined during `array::install` below.
    symbol::install(interp);
    // `Iterator` right after `Symbol` and before everything that makes an iterable:
    // the array, string, map/set, typed-array and generator protos all inherit from
    // `%IteratorPrototype%`, and they can only do that once it exists.
    iterator::install(interp, &object_proto);
    object::install(interp, &object_proto);
    function::install(interp, &function_proto);
    array::install(interp, &array_proto);
    string::install(interp, &string_proto);
    number::install(interp, &number_proto);
    bigint::install(interp, &object_proto);
    map::install(interp, &object_proto);
    math::install(interp);
    json::install(interp);
    reflect::install(interp);
    // `Promise` before `Proxy`: a proxy handler is a plain object, but the reverse is not
    // true — nothing here reads the other's prototype.
    promise::install(interp, &object_proto);
    proxy::install(interp);
    date::install(interp);
    // `RegExp` after `String` (its methods dispatch to `Symbol.replace` and friends) and
    // after `Symbol`, whose well-known keys it defines its methods under.
    let regexp = regexp::install(interp, &object_proto);
    let global = interp.realm.global.clone();
    global.define(
        "RegExp",
        crate::object::Property::with_attributes(Value::Object(regexp), true, false),
    );
    // The stacks need `Symbol.dispose` (installed by `symbol`) and the error prototypes,
    // and nothing else depends on them.
    disposable::install(interp, &object_proto);
    // The typed arrays need `Array.prototype`-style helpers (`join`) and `Symbol.toStringTag`,
    // both of which are installed by now.
    typedarray::install(interp, &object_proto);
    generator::install(interp, &generator_proto);
    global::install(interp, &boolean_proto);
    // The weak-reference family needs the collection prototypes (it installs beside them),
    // `Symbol.toStringTag`, and the error prototypes; nothing depends on it.
    weak::install(interp, &object_proto);
    // `Array.fromAsync` last: it is written in JavaScript and compiled here, so its
    // source reaches for `Array`, `Promise`, `Reflect`, `Object`, `TypeError` and the
    // well-known symbols by name. Every one of those has to exist by now.
    from_async::install(interp);
}

// ---------------------------------------------------------------------------
// Helpers shared by the modules
// ---------------------------------------------------------------------------

/// A native function object, ready to be stored as a value.
pub fn method(
    interp: &Interp<'_>,
    name: &str,
    length: u32,
    func: NativeFn,
) -> crate::interp::JsResult<Value> {
    Ok(Value::Object(interp.new_native(name, length, func)))
}

/// Define `name` on `target` as a non-enumerable method, the way every built-in
/// method is defined.
///
/// Non-enumerable matters: `for (k in [])` must not list a hundred method names,
/// and `JSON.stringify` must not serialise them.
pub fn define_method(
    target: &JsObject,
    name: &str,
    length: u32,
    func: NativeFn,
    interp: &Interp<'_>,
) {
    let function = interp.new_native(name, length, func);
    target.define(
        name,
        Property::with_attributes(Value::Object(function), true, false),
    );
}

/// Argument `index`, or `undefined` when it was not passed.
pub fn arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or(Value::Undefined)
}

/// Argument `index` as a number, with the `undefined`-is-`NaN` rule of `ToNumber`.
pub fn arg_number(args: &[Value], index: usize) -> f64 {
    arg(args, index).to_number()
}

/// Argument `index` as an integer, or `None` when it is `undefined` — the shape
/// optional numeric arguments need.
pub fn arg_int_or(args: &[Value], index: usize, fallback: i64) -> i64 {
    match args.get(index) {
        None | Some(Value::Undefined) => fallback,
        Some(value) => value.to_number() as i64,
    }
}

/// Whether argument `index` was actually passed.
pub fn has_arg(args: &[Value], index: usize) -> bool {
    args.len() > index
}

/// Give `object` a `Symbol.toStringTag` data property answering `tag`.
///
/// A *data* property, not an accessor: `{value, writable: false, enumerable: false,
/// configurable: true}`, which is what §17 requires of every `@@toStringTag` and what
/// `verifyProperty` reads off the descriptor. An accessor reports `writable: undefined`
/// and fails.
///
/// Namespaced objects (`Math`, `JSON`, `Reflect`, `Intl`, `Atomics`) all need one:
/// without it `Object.prototype.toString.call(Math)` answers `[object Object]` instead of
/// `[object Math]`, and a run of `Array.prototype` tests — `map`/`filter`/`forEach`/
/// `some`/`every`/`reduce`/`reduceRight` applied to `Math` and `JSON` — check exactly
/// that tag from inside the callback.
pub fn install_tag(object: &Gc<JsObject>, tag: &str, interp: &Interp<'_>) {
    object.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property {
            value: Some(interp.string(tag)),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
}

/// The `this` of a method call, as an object.
///
/// A primitive receiver is boxed: `"abc".indexOf(…)` reaches a method on
/// `String.prototype` with a string `this`, and the method behaves as if it had an object.
///
/// This used to be a second, incomplete `ToObject`: it boxed by defining `_value` and
/// nothing else, so a boxed string had no `length` and no index properties, and
/// `Array.prototype.map.call("ab", …)` read nothing at all. Boxing now goes through the
/// interpreter's one implementation, which builds the String exotic object properly.
pub fn this_object(interp: &mut Interp<'_>, this: &Value) -> crate::interp::JsResult<Gc<JsObject>> {
    interp.object_from(this)
}

/// The text a string method operates on, whether `this` is a primitive or a box.
pub fn this_string(interp: &Interp<'_>, this: &Value) -> Option<String> {
    match this {
        Value::String(text) => Some(text.to_string_lossy().into_owned()),
        Value::Object(object) => interp
            .internal_get(object, "_value")
            .map(|value| value.to_js_string()),
        _ => {
            let _ = interp;
            None
        }
    }
}

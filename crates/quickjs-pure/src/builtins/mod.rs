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

mod array;
pub mod bigint;
pub(crate) mod date;
pub mod disposable;
mod error;
mod function;
mod generator;
mod global;
mod json;
pub mod map;
mod math;
mod number;
mod object;
pub mod proxy;
mod reflect;
pub mod regexp;
mod string;
mod symbol;
pub mod typedarray;

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

/// The `this` of a method call, as an object.
///
/// A primitive receiver is boxed on demand: `"abc".indexOf(…)` reaches a method on
/// `String.prototype` with a string `this`, and the method has to behave as if it
/// had an object. The box is only for property access, so the text is read through
/// [`this_string`] instead.
pub fn this_object(interp: &mut Interp<'_>, this: &Value) -> crate::interp::JsResult<Gc<JsObject>> {
    match this {
        Value::Object(object) => Ok(object.clone()),
        Value::Undefined | Value::Null => {
            Err(interp.type_error("cannot convert undefined or null to object"))
        }
        Value::String(_) => {
            let proto = interp.realm.string_proto.clone();
            let boxed = interp.new_object(proto);
            boxed.define(
                "_value",
                Property::with_attributes(this.clone(), false, false),
            );
            Ok(boxed)
        }
        Value::Int(_) | Value::Float(_) => {
            let proto = interp.realm.number_proto.clone();
            let boxed = interp.new_object(proto);
            boxed.define(
                "_value",
                Property::with_attributes(this.clone(), false, false),
            );
            Ok(boxed)
        }
        Value::Bool(_) => {
            let proto = interp.realm.boolean_proto.clone();
            let boxed = interp.new_object(proto);
            boxed.define(
                "_value",
                Property::with_attributes(this.clone(), false, false),
            );
            Ok(boxed)
        }
        // A symbol boxes like any other primitive, which is what makes `Object(sym)` a
        // `Symbol` object whose `valueOf` hands the symbol back.
        Value::Symbol(_) => {
            let proto = interp.realm.symbol_proto.clone();
            let boxed = interp.new_object(proto);
            boxed.define(
                "_value",
                Property::with_attributes(this.clone(), false, false),
            );
            Ok(boxed)
        }
        _ => Err(interp.type_error("cannot convert value to object")),
    }
}

/// The text a string method operates on, whether `this` is a primitive or a box.
pub fn this_string(interp: &Interp<'_>, this: &Value) -> Option<String> {
    match this {
        Value::String(text) => Some(text.to_string_lossy().into_owned()),
        Value::Object(object) => object
            .own_property("_value")
            .and_then(|property| property.value)
            .map(|value| value.to_js_string()),
        _ => {
            let _ = interp;
            None
        }
    }
}

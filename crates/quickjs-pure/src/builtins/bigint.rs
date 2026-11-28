//! The `BigInt` constructor and its prototype.
//!
//! `BigInt` is a conversion function, not a constructor: `new BigInt(1)` is a `TypeError`,
//! because a bigint is a primitive and has no wrapper. The arithmetic itself lives in
//! [`crate::bigint`]; this is the surface a script sees.

use crate::bigint::BigInt;
use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install `BigInt` and `BigInt.prototype`.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    interp.realm.bigint_proto = Some(proto.clone());

    let constructor = interp.new_native_constructor("BigInt", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    define_method(&constructor, "asIntN", 2, as_int_n, interp);
    define_method(&constructor, "asUintN", 2, as_uint_n, interp);

    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    define_method(&proto, "toString", 0, to_string, interp);
    define_method(&proto, "toLocaleString", 0, to_string, interp);
    define_method(&proto, "valueOf", 0, value_of, interp);

    let global = interp.realm.global.clone();
    global.define(
        "BigInt",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `BigInt(value)`, which converts a number, a string or a boolean exactly.
///
/// `new BigInt(1)` is refused here rather than before the call, which is what the
/// reference does: `BigInt` is registered as a `JS_CFUNC_constructor_or_func`, so
/// `[[Construct]]` exists and the native function is the one that says no. The
/// difference is observable in the trace — the frame `at BigInt (native)` is there
/// exactly because the call was entered.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    if interp.is_constructing() {
        return Err(interp.not_a_constructor_error());
    }
    match arg(args, 0) {
        Value::BigInt(big) => Ok(Value::BigInt(big)),
        Value::Bool(flag) => Ok(interp.bigint(BigInt::from_i64(if flag { 1 } else { 0 }))),
        // A fraction has no exact bigint form, so this is a `RangeError` rather than a
        // truncation — silently dropping the fraction would be the wrong kind of lossy.
        Value::Int(value) => Ok(interp.bigint(BigInt::from_i64(value as i64))),
        Value::Float(value) => {
            // Two messages, not one: the reference separates "no exact bigint form"
            // from "no value at all".
            if !value.is_finite() {
                return Err(interp.range_error("cannot convert NaN or Infinity to BigInt"));
            }
            if value.fract() != 0.0 {
                return Err(interp.range_error("cannot convert to BigInt: not an integer"));
            }
            Ok(interp.bigint(BigInt::from_f64_truncated(value)))
        }
        Value::String(text) => {
            let text = text.to_string_lossy();
            let big = string_to_bigint(interp, &text)?;
            Ok(interp.bigint(big))
        }
        // A symbol has no primitive form at all: `to_primitive` hands it straight back, so
        // the recursion below never advanced and `BigInt(Symbol())` overflowed the stack.
        Value::Symbol(_) => Err(interp.type_error("cannot convert a Symbol to a BigInt")),
        Value::Undefined | Value::Null => Err(interp.type_error("cannot convert to BigInt")),
        // An object goes through `ToPrimitive` first; every arm above is primitive, so
        // the recursion terminates.
        other => {
            let primitive = interp.to_primitive(other, crate::interp::Hint::Number)?;
            constructor(interp, Value::Undefined, &[primitive])
        }
    }
}

/// The bigint `this` refers to.
/// `thisBigIntValue`: the bigint a method operates on.
///
/// A `BigInt.prototype` method accepts either a primitive or a wrapper object, so
/// `Object(1n).toString()` and `1n.toString()` both work. Only a wrapper whose
/// `_value` really is a bigint counts — `BigInt.prototype.toString.call({})` is a
/// `TypeError`, and so is calling it on a `Number` wrapper.
fn this_bigint(interp: &Interp<'_>, this: &Value) -> JsResult<Gc<BigInt>> {
    match this {
        Value::BigInt(big) => Ok(big.clone()),
        Value::Object(object) => match interp.internal_get(object, "_value") {
            Some(Value::BigInt(big)) => Ok(big),
            _ => Err(interp.type_error("not a BigInt")),
        },
        _ => Err(interp.type_error("not a BigInt")),
    }
}

/// `bigint.toString(radix)`.
fn to_string(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let big = this_bigint(interp, &this)?;
    let radix = match arg(args, 0) {
        Value::Undefined => 10,
        other => {
            let radix = other.to_number();
            if !(2.0..=36.0).contains(&radix) || radix.fract() != 0.0 {
                return Err(interp.range_error("radix must be between 2 and 36"));
            }
            radix as u32
        }
    };
    Ok(interp.string(big.to_radix_string(radix)))
}

/// `bigint.valueOf()`.
fn value_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // The *primitive*, not the receiver: `Object(1n).valueOf()` answers `1n`, and
    // `typeof` on it has to be `"bigint"` rather than `"object"`.
    Ok(Value::BigInt(this_bigint(interp, &this)?))
}

/// The bit count an `asIntN`/`asUintN` call asks for.
fn bits_of(interp: &mut Interp<'_>, args: &[Value]) -> JsResult<u64> {
    let bits = arg(args, 0).to_number();
    if !bits.is_finite() || bits < 0.0 || bits.fract() != 0.0 {
        return Err(interp.range_error("invalid bit count"));
    }
    Ok(bits as u64)
}

/// `BigInt.asIntN(bits, value)`.
fn as_int_n(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let bits = bits_of(interp, args)?;
    let value = to_bigint(interp, &arg(args, 1))?;
    Ok(interp.bigint(value.as_int_n(bits)))
}

/// `BigInt.asUintN(bits, value)`.
fn as_uint_n(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let bits = bits_of(interp, args)?;
    let value = to_bigint(interp, &arg(args, 1))?;
    Ok(interp.bigint(value.as_uint_n(bits)))
}

/// `ToBigInt`, which the `as*N` pair needs for their second argument.
///
/// This is deliberately *not* `BigInt(value)`. The two differ on a Number, and only on a
/// Number: `BigInt(1)` is `1n`, while `ToBigInt(1)` is a `TypeError` (§7.1.14 step 1 —
/// a BigInt element type accepts a BigInt, a String, a Boolean, or an object, never a
/// Number). Routing `ToBigInt` through the constructor made `BigInt.asIntN(8, 1)` answer
/// `1n` where the reference throws, and it is the same substitution that made
/// `new BigInt64Array([1])` look like it should work.
pub fn to_bigint(interp: &mut Interp<'_>, value: &Value) -> JsResult<BigInt> {
    match value {
        Value::BigInt(big) => Ok((**big).clone()),
        // `StringToBigInt` parses the literal; a malformed one is a `SyntaxError`, which
        // is why this cannot go through `ToNumber`.
        Value::String(text) => string_to_bigint(interp, &text.to_string_lossy()),
        Value::Bool(flag) => Ok(BigInt::from_i64(if *flag { 1 } else { 0 })),
        // A Number — including a boxed one, which `ToPrimitive` unwraps into this arm —
        // has no default bigint conversion. `BigInt(1)` exists because it is explicit.
        // The reference spells this one lowercase for `ToBigInt` (`BigInt.asIntN`), and the
        // exact text is what several test262 assertions compare against.
        Value::Int(_) | Value::Float(_) => Err(interp.type_error("cannot convert to bigint")),
        Value::Undefined | Value::Null => Err(interp.type_error("cannot convert to bigint")),
        Value::Symbol(_) => Err(interp.type_error("cannot convert to bigint")),
        // An object goes through `ToPrimitive` first, then is converted again — and that
        // recursion terminates: every arm above is primitive, and a `Symbol.toPrimitive`
        // that answers an object is already a `TypeError` out of `to_primitive`.
        Value::Object(_) => {
            let primitive = interp.to_primitive(value.clone(), crate::interp::Hint::Number)?;
            to_bigint(interp, &primitive)
        }
        // Not a JavaScript value at all: a bytecode-frame marker that no script can
        // reach. Refusing loudly beats `unreachable!`, because a wrong stack here would
        // otherwise take the process down.
        _ => Err(interp.type_error("cannot convert to bigint")),
    }
}

/// Implement the initializer list of a BigInt-element view: `ToBigInt` on every value.
///
/// Kept next to [`to_bigint`] because the message has to say what went wrong. The write
/// path in `typedarray` reports a Number-into-BigInt error by name, but a bad *string*
/// there would come out as the same `TypeError` — while the spec makes it a
/// `SyntaxError` about the literal.
pub fn bigint_element(interp: &mut Interp<'_>, value: &Value) -> JsResult<Gc<BigInt>> {
    let big = to_bigint(interp, value)?;
    Ok(Gc::new(interp.heap, big))
}

/// `StringToBigInt`, shared by `BigInt(str)` and `ToBigInt(str)`.
pub fn string_to_bigint(interp: &mut Interp<'_>, text: &str) -> JsResult<BigInt> {
    match BigInt::from_string(text) {
        Some(big) => Ok(big),
        None => Err(Signal::Throw(
            interp.make_error("SyntaxError", "invalid bigint literal"),
        )),
    }
}

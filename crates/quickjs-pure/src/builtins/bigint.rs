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
    // `new BigInt()` has no wrapper object to build, so `[[Construct]]` is refused by name.
    constructor.define(
        "_not_a_constructor",
        Property::with_attributes(Value::Bool(true), false, false),
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
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    match arg(args, 0) {
        Value::BigInt(big) => Ok(Value::BigInt(big)),
        Value::Bool(flag) => Ok(interp.bigint(BigInt::from_i64(if flag { 1 } else { 0 }))),
        // A fraction has no exact bigint form, so this is a `RangeError` rather than a
        // truncation — silently dropping the fraction would be the wrong kind of lossy.
        Value::Int(value) => Ok(interp.bigint(BigInt::from_i64(value as i64))),
        Value::Float(value) => {
            if !value.is_finite() || value.fract() != 0.0 {
                return Err(interp.range_error("cannot convert a non-integer to a BigInt"));
            }
            Ok(interp.bigint(BigInt::from_f64_truncated(value)))
        }
        Value::String(text) => {
            let text = text.to_string_lossy().into_owned();
            match BigInt::from_string(&text) {
                Some(big) => Ok(interp.bigint(big)),
                None => Err(Signal::Throw(
                    interp.make_error("SyntaxError", "cannot parse BigInt from this string"),
                )),
            }
        }
        Value::Undefined | Value::Null => Err(interp.type_error("cannot convert to BigInt")),
        // An object goes through `ToPrimitive` first, and a symbol never converts.
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
fn to_bigint(interp: &mut Interp<'_>, value: &Value) -> JsResult<BigInt> {
    match value {
        Value::BigInt(big) => Ok((**big).clone()),
        other => match constructor(interp, Value::Undefined, std::slice::from_ref(other))? {
            Value::BigInt(big) => Ok((*big).clone()),
            _ => Err(interp.type_error("cannot convert to BigInt")),
        },
    }
}

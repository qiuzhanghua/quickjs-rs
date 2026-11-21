//! `Number`, `Boolean`, `parseInt`/`parseFloat`/`isNaN`/`isFinite`, `eval` and the
//! remaining global functions.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::{Value, number_to_string, string_to_number};
use js_gc::Gc;

/// Install `Number` and `Number.prototype`.
pub fn install(interp: &mut Interp<'_>, number_proto: &Gc<JsObject>) {
    define_method(number_proto, "toString", 1, to_string, interp);
    define_method(number_proto, "toFixed", 1, to_fixed, interp);
    define_method(number_proto, "toExponential", 1, to_exponential, interp);
    define_method(number_proto, "toPrecision", 1, to_precision, interp);
    define_method(number_proto, "valueOf", 0, value_of, interp);

    let constructor = interp.new_native("Number", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(number_proto.clone()), false, false),
    );
    number_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, value) in [
        ("MAX_VALUE", f64::MAX),
        ("MIN_VALUE", 5e-324),
        ("MAX_SAFE_INTEGER", 9007199254740991.0),
        ("MIN_SAFE_INTEGER", -9007199254740991.0),
        ("EPSILON", f64::EPSILON),
        ("POSITIVE_INFINITY", f64::INFINITY),
        ("NEGATIVE_INFINITY", f64::NEG_INFINITY),
        ("NaN", f64::NAN),
    ] {
        constructor.define(
            name,
            Property::with_attributes(Value::Float(value), false, false),
        );
    }
    for (name, length, func) in [
        ("isNaN", 1, is_nan as crate::object::NativeFn),
        ("isFinite", 1, is_finite),
        ("isInteger", 1, is_integer),
        ("isSafeInteger", 1, is_safe_integer),
    ] {
        define_method(&constructor, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define(
        "Number",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

fn constructor(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    Ok(match args.first() {
        None => Value::Int(0),
        Some(value) => Value::Float(value.to_number()),
    })
}

/// The receiver as a number, unboxing a wrapper object.
fn number_of(interp: &Interp<'_>, this: &Value) -> Result<f64, crate::interp::Signal> {
    match this {
        Value::Int(_) | Value::Float(_) => Ok(this.to_number()),
        Value::Object(object) => object
            .own_property("_value")
            .and_then(|property| property.value)
            .map(|value| value.to_number())
            .ok_or_else(|| interp.type_error("Number method on a non-number")),
        _ => Err(interp.type_error("Number method on a non-number")),
    }
}

fn to_string(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let value = number_of(interp, &this)?;
    let radix = match args.first() {
        None | Some(Value::Undefined) => return Ok(interp.string(number_to_string(value))),
        Some(other) => other.to_number() as u32,
    };
    if radix == 10 {
        return Ok(interp.string(number_to_string(value)));
    }
    if !(2..=36).contains(&radix) {
        return Err(interp.range_error("toString radix must be between 2 and 36"));
    }
    if !value.is_finite() {
        return Ok(interp.string(number_to_string(value)));
    }
    Ok(interp.string(radix_string(value, radix)))
}

/// `number.toString(radix)` for a radix other than ten.
///
/// The integer part is repeated division. The fraction is the interesting half:
/// emitting digits until the remainder is zero never terminates for most values, so
/// the loop stops once the remaining error is smaller than half an ulp of the
/// original number — which yields the shortest representation that reads back
/// exactly, the same shape V8 and QuickJS produce.
fn radix_string(value: f64, radix: u32) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let integer = magnitude.trunc();
    let mut fraction = magnitude - integer;

    let mut integer_digits = Vec::new();
    let mut remaining = integer;
    if remaining == 0.0 {
        integer_digits.push('0');
    }
    while remaining >= 1.0 {
        let digit = (remaining % radix as f64) as u32;
        integer_digits.push(std::char::from_digit(digit, radix).unwrap_or('0'));
        remaining = (remaining / radix as f64).trunc();
    }
    integer_digits.reverse();

    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.extend(integer_digits);
    if fraction == 0.0 {
        return out;
    }
    out.push('.');

    // Half an ulp at the number's own scale, grown by the radix alongside `fraction`
    // so the two stay comparable after every multiplication.
    let mut delta = 0.5 * (next_after(magnitude) - magnitude);
    delta = delta.max(f64::MIN_POSITIVE);
    loop {
        fraction *= radix as f64;
        delta *= radix as f64;
        let digit = fraction.floor();
        out.push(std::char::from_digit(digit as u32, radix).unwrap_or('0'));
        fraction -= digit;
        // Round the last digit up when the remainder is past the halfway point; a
        // carry can only happen when the fraction is about to end.
        let past_half = fraction > 0.5 || (fraction == 0.5 && (digit as u32) % 2 == 1);
        if past_half && fraction + delta > 1.0 {
            return round_up(&out, radix);
        }
        if fraction < delta {
            break;
        }
    }
    out
}

/// The next representable double above `value`.
fn next_after(value: f64) -> f64 {
    f64::from_bits(value.to_bits() + 1)
}

/// Increment the last digit of `text`, carrying leftwards.
fn round_up(text: &str, radix: u32) -> String {
    let mut chars: Vec<char> = text.chars().collect();
    let mut index = chars.len();
    let max = std::char::from_digit(radix - 1, radix).unwrap_or('z');
    while index > 0 {
        index -= 1;
        if chars[index] == '.' || chars[index] == '-' {
            continue;
        }
        if chars[index] == max {
            chars[index] = '0';
            continue;
        }
        // The digit is below the maximum, so it simply increments.
        let digit = chars[index].to_digit(radix).unwrap_or(0) + 1;
        chars[index] = std::char::from_digit(digit, radix).unwrap_or('0');
        return chars.into_iter().collect();
    }
    chars.into_iter().collect()
}

fn to_fixed(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let value = number_of(interp, &this)?;
    let digits = arg(args, 0).to_number().max(0.0) as usize;
    if digits > 100 {
        return Err(interp.range_error("toFixed argument out of range"));
    }
    if !value.is_finite() {
        return Ok(interp.string(number_to_string(value)));
    }
    // Rust rounds halves to even; the spec rounds them away from zero, so
    // `(1.125).toFixed(2)` is `"1.13"` and not `"1.12"`.
    let factor = 10f64.powi(digits as i32);
    let rounded = round_half_away(value * factor) / factor;
    Ok(interp.string(format!("{rounded:.digits$}")))
}

fn to_exponential(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let value = number_of(interp, &this)?;
    if !value.is_finite() {
        return Ok(interp.string(number_to_string(value)));
    }
    let digits = match args.first() {
        None | Some(Value::Undefined) => {
            // No argument: as many digits as needed, spelling normalised to `e+N`.
            return Ok(interp.string(normalise(&format!("{value:e}"))));
        }
        Some(digits) => digits.to_number() as usize,
    };
    if digits > 100 {
        return Err(interp.range_error("toExponential argument out of range"));
    }
    Ok(interp.string(exponential(value, digits)))
}

/// `toExponential` with an explicit digit count.
///
/// Rust's formatter rounds halves to even while the spec rounds them away from zero,
/// so `(25).toExponential(0)` would be `2e+1` instead of `3e+1`. The rounding is
/// therefore done here on the mantissa, which for a value like `25` is exact.
fn exponential(value: f64, digits: usize) -> String {
    if value == 0.0 {
        let fraction = if digits == 0 {
            String::new()
        } else {
            format!(".{}", "0".repeat(digits))
        };
        return format!("0{fraction}e+0");
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let mut exponent = magnitude.log10().floor() as i32;
    // `log10` can land one below the true exponent at exact powers of ten.
    let scale = 10f64.powi(exponent);
    let mut mantissa = magnitude / scale;
    if mantissa >= 10.0 {
        mantissa /= 10.0;
        exponent += 1;
    } else if mantissa < 1.0 {
        mantissa *= 10.0;
        exponent -= 1;
    }
    let factor = 10f64.powi(digits as i32);
    let mut rounded = round_half_away(mantissa * factor);
    if rounded >= 10.0 * factor {
        rounded /= 10.0;
        exponent += 1;
    }
    let integer = (rounded / factor).floor() as u64;
    let fraction = (rounded - integer as f64 * factor).round() as u64;
    let sign = if negative { "-" } else { "" };
    if digits == 0 {
        format!("{sign}{integer}e{:+}", exponent)
    } else {
        format!(
            "{sign}{integer}.{fraction:0width$}e{:+}",
            exponent,
            width = digits
        )
    }
}

/// Round to the nearest integer, halves away from zero.
fn round_half_away(value: f64) -> f64 {
    if value >= 0.0 {
        (value + 0.5).floor()
    } else {
        (value - 0.5).ceil()
    }
}

fn to_precision(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let value = number_of(interp, &this)?;
    match args.first() {
        None | Some(Value::Undefined) => Ok(interp.string(number_to_string(value))),
        Some(precision) => {
            let precision = precision.to_number() as usize;
            if precision == 0 || precision > 100 {
                return Err(interp.range_error("toPrecision argument out of range"));
            }
            // `toPrecision` chooses the exponential form when the exponent falls
            // outside `[-7, precision)`, which is why `(25).toPrecision(1)` is `3e+1`
            // while `(25).toPrecision(2)` is `25`.
            let exponent = if value == 0.0 {
                0
            } else {
                value.abs().log10().floor() as i32
            };
            if exponent < -7 || exponent >= precision as i32 {
                return Ok(interp.string(exponential(value, precision - 1)));
            }
            let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
            let factor = 10f64.powi(decimals as i32);
            let rounded = round_half_away(value * factor) / factor;
            Ok(interp.string(format!("{rounded:.decimals$}")))
        }
    }
}

/// Rewrite Rust's `1e5` as JavaScript's `1e+5`.
fn normalise(text: &str) -> String {
    match text.split_once('e') {
        Some((mantissa, exponent)) if !exponent.starts_with('-') => {
            format!("{mantissa}e+{}", exponent.trim_start_matches('+'))
        }
        _ => text.to_string(),
    }
}

fn value_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(Value::Float(number_of(interp, &this)?))
}

fn is_nan(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // Unlike the global `isNaN`, this one does not coerce: `Number.isNaN("x")` is
    // `false` because a string is not the number NaN.
    Ok(Value::Bool(match arg(args, 0) {
        Value::Int(_) | Value::Float(_) => arg(args, 0).to_number().is_nan(),
        _ => false,
    }))
}

fn is_finite(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    Ok(Value::Bool(match value {
        Value::Int(_) | Value::Float(_) => value.to_number().is_finite(),
        _ => false,
    }))
}

fn is_integer(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    Ok(Value::Bool(match value {
        Value::Int(_) | Value::Float(_) => {
            let number = value.to_number();
            number.is_finite() && number == number.trunc()
        }
        _ => false,
    }))
}

fn is_safe_integer(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    Ok(Value::Bool(match value {
        Value::Int(_) | Value::Float(_) => {
            let number = value.to_number();
            number.is_finite() && number == number.trunc() && number.abs() <= 9007199254740991.0
        }
        _ => false,
    }))
}

/// Install `Boolean`.
pub fn install_boolean(interp: &mut Interp<'_>, boolean_proto: &Gc<JsObject>) {
    define_method(boolean_proto, "toString", 0, boolean_to_string, interp);
    define_method(boolean_proto, "valueOf", 0, boolean_value_of, interp);
    let constructor = interp.new_native("Boolean", 1, boolean_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(boolean_proto.clone()), false, false),
    );
    boolean_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    let global = interp.realm.global.clone();
    global.define(
        "Boolean",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

fn boolean_constructor(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(arg(args, 0).to_boolean()))
}

fn boolean_receiver(interp: &Interp<'_>, this: &Value) -> Result<bool, crate::interp::Signal> {
    match this {
        Value::Bool(value) => Ok(*value),
        Value::Object(object) => object
            .own_property("_value")
            .and_then(|property| property.value)
            .map(|value| value.to_boolean())
            .ok_or_else(|| interp.type_error("Boolean method on a non-boolean")),
        _ => Err(interp.type_error("Boolean method on a non-boolean")),
    }
}

fn boolean_to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let value = boolean_receiver(interp, &this)?;
    Ok(interp.string(if value { "true" } else { "false" }))
}

fn boolean_value_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(boolean_receiver(interp, &this)?))
}

/// `parseInt(text, radix)`.
///
/// The rules that surprise people and are therefore worth stating: leading
/// whitespace is skipped, an optional sign is allowed, a `0x` prefix implies radix
/// 16, and parsing stops at the first invalid digit rather than failing.
pub fn parse_int(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let text = text.trim_start();
    let (sign, rest) = match text.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, text.strip_prefix('+').unwrap_or(text)),
    };
    let explicit = match args.get(1) {
        None | Some(Value::Undefined) => 0,
        Some(value) => value.to_number() as u32,
    };
    let (radix, rest) = if explicit == 0 {
        match rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
            Some(rest) => (16, rest),
            None => (10, rest),
        }
    } else {
        if !(2..=36).contains(&explicit) {
            return Ok(Value::Float(f64::NAN));
        }
        let rest = if explicit == 16 {
            rest.strip_prefix("0x")
                .or_else(|| rest.strip_prefix("0X"))
                .unwrap_or(rest)
        } else {
            rest
        };
        (explicit, rest)
    };
    let digits: String = rest.chars().take_while(|c| c.is_digit(radix)).collect();
    if digits.is_empty() {
        return Ok(Value::Float(f64::NAN));
    }
    let magnitude = i64::from_str_radix(&digits, radix)
        .map(|value| value as f64)
        .unwrap_or_else(|_| {
            digits.chars().fold(0.0, |acc, c| {
                acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64
            })
        });
    let _ = interp;
    Ok(Value::Float(sign * magnitude))
}

/// `parseFloat(text)`, which also stops at the first invalid character.
pub fn parse_float(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let trimmed = text.trim_start();
    // The longest prefix the number grammar accepts; `string_to_number` handles the
    // validation, so this only has to find where the prefix ends.
    let mut end = 0usize;
    let bytes = trimmed.as_bytes();
    let mut index = 0;
    if matches!(bytes.first(), Some(b'+' | b'-')) {
        index += 1;
    }
    if trimmed[index..].starts_with("Infinity") {
        end = index + "Infinity".len();
    } else {
        let mut digits = 0;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
            digits += 1;
        }
        if index < bytes.len() && bytes[index] == b'.' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() {
                index += 1;
                digits += 1;
            }
        }
        if digits > 0 {
            if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
                let mut probe = index + 1;
                if probe < bytes.len() && (bytes[probe] == b'+' || bytes[probe] == b'-') {
                    probe += 1;
                }
                let start = probe;
                while probe < bytes.len() && bytes[probe].is_ascii_digit() {
                    probe += 1;
                }
                if probe > start {
                    index = probe;
                }
            }
            end = index;
        }
    }
    let _ = interp;
    Ok(Value::Float(if end == 0 {
        f64::NAN
    } else {
        string_to_number(&trimmed[..end])
    }))
}

/// The global `isNaN`, which coerces first.
pub fn global_is_nan(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(arg(args, 0).to_number().is_nan()))
}

/// The global `isFinite`, which coerces first.
pub fn global_is_finite(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(arg(args, 0).to_number().is_finite()))
}

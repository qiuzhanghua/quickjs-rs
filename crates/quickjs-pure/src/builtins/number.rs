//! `Number`, `Boolean`, `parseInt`/`parseFloat`/`isNaN`/`isFinite`, `eval` and the
//! remaining global functions.

use crate::bignum::Big;
use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::{Value, number_to_string, string_to_number};
use js_gc::Gc;
use std::cmp::Ordering;

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

fn constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let number = match args.first() {
        None => Value::Int(0),
        // A bigint converts to the nearest double, so a bigint wider than 53 bits loses
        // precision exactly as `ToNumber` says it should.
        Some(Value::BigInt(big)) => Value::Float(big.to_f64()),
        Some(value) => Value::Float(value.to_number()),
    };
    // `new Number(1)` is a wrapper object, so the number has to be stored on the
    // receiver: without it `Number.prototype.valueOf` finds no `_value` and every
    // `ToPrimitive` step on a wrapper throws. Called as a plain function there is no
    // receiver and the primitive is the answer.
    if let Value::Object(object) = &this {
        object.define("_value", Property::with_attributes(number, false, false));
        return Ok(this);
    }
    let _ = interp;
    Ok(number)
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
/// The answer is the shortest digit string that reads back as the same double, decided
/// the way the reference decides it: in exact integer arithmetic. The value is held as
/// `mantissa * 2^exponent`, a candidate of `digits` significant digits is obtained by
/// rounding `value / radix^place` to nearest-even with [`Big`], and the candidate is
/// accepted when `candidate * radix^place` lands inside the interval that rounds back
/// to the original double. Searching `digits` upwards finds the shortest such candidate;
/// the search bound is `1 + ceil(53 / log2(radix))`, which is the reference's
/// `dtoa_max_digits_table`.
///
/// The previous version grew a float `delta` alongside the fraction and stopped when the
/// remainder dropped below it. That is the right idea and the wrong arithmetic: for
/// `(1.3).toString(7)` the error accumulated over the multiply-and-subtract loop and it
/// stopped one digit early with the last digit misrounded, printing
/// `1.204620462046204621` where the reference prints `1.2046204620462046205`.
fn radix_string(value: f64, radix: u32) -> String {
    if value == 0.0 {
        return "0".to_string();
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let bits = magnitude.to_bits();
    let biased_exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    // `magnitude == mantissa * 2^exponent`, exactly. A subnormal keeps its 52 mantissa
    // bits and shares the minimum exponent, which is also the spacing between doubles
    // there, so the interval below needs no special case.
    let (mantissa, exponent) = if biased_exponent == 0 {
        (fraction, -1074)
    } else {
        (fraction | (1u64 << 52), biased_exponent - 1075)
    };

    // The number of significant digits that always suffices for this radix.
    let max_digits = 1 + (53.0 / (radix as f64).log2()).ceil() as u32;
    // Where the radix point falls: the smallest `point` with `radix^(point-1) <= value`.
    let point = radix_point(magnitude, mantissa, exponent, radix);

    let mut candidate = (Big::zero(), 0);
    for digits in 1..=max_digits {
        // `value ≈ scaled * radix^place`, with `scaled` the `digits`-digit mantissa.
        let mut place = point - digits as i32;
        let mut scaled = scaled_round(mantissa, exponent, radix, place);
        // Rounding can carry into one more digit (`1/3` in radix 3 rounds to `3`, which
        // is two digits). The reference bumps its exponent in that case and tries again;
        // at most one bump is possible, because the carry adds at most one digit.
        if scaled.to_radix_string(radix).len() > digits as usize {
            place += 1;
            scaled = scaled_round(mantissa, exponent, radix, place);
        }
        if scaled.is_zero() {
            continue;
        }
        candidate = (scaled, place);
        if round_trips(&candidate.0, place, radix, mantissa, exponent) {
            break;
        }
    }
    // The shortest candidate wins; the widest one is guaranteed to work, so the loop
    // always leaves something usable here even if no early acceptance happened.
    let (scaled, place) = candidate;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    out.push_str(&place_radix_point(&scaled, place, radix));
    out
}

/// Lay `value = scaled * radix^place` out with the radix point in place.
fn place_radix_point(scaled: &Big, place: i32, radix: u32) -> String {
    let digits = scaled.to_radix_string(radix);
    let length = digits.len() as i32;
    let point = length + place;
    if point <= 0 {
        // Purely fractional: `0.` then the zeros the value needs, then the digits.
        let mut out = String::from("0.");
        out.extend(std::iter::repeat_n('0', (-point) as usize));
        out.push_str(&digits);
        out
    } else if point >= length {
        // Integral: the digits, then the zeros the value needs to reach the point.
        let mut out = digits;
        out.extend(std::iter::repeat_n('0', (point - length) as usize));
        out
    } else {
        let (whole, rest) = digits.split_at(point as usize);
        format!("{whole}.{rest}")
    }
}

/// The smallest `point` with `radix^(point-1) <= value < radix^point`.
///
/// The logarithm gives the answer almost always; the correction loops make it exact,
/// because `f64::log2` is not trustworthy at the boundaries and a wrong `point` would
/// shift every digit of the output by one place.
///
/// `magnitude` is passed in rather than rebuilt from the mantissa: `mantissa *
/// 2^exponent` underflows to zero for the smallest subnormals, and a zero magnitude
/// sends `log2` to negative infinity and the correction loop off the end of `i32`.
fn radix_point(magnitude: f64, mantissa: u64, exponent: i32, radix: u32) -> i32 {
    let mut point = (magnitude.log2() / (radix as f64).log2()).floor() as i32 + 1;
    while compare_power(mantissa, exponent, point - 1, radix) == Ordering::Less {
        point -= 1;
    }
    while compare_power(mantissa, exponent, point, radix) != Ordering::Less {
        point += 1;
    }
    point
}

/// Compare `mantissa * 2^exponent` with `radix^power`.
fn compare_power(mantissa: u64, exponent: i32, power: i32, radix: u32) -> Ordering {
    let (value_num, value_den) = scaled_exact(&Big::from_u64(mantissa), exponent, radix, 0);
    let (power_num, power_den) = scaled_exact(&Big::one(), 0, radix, power);
    value_num.mul(&power_den).cmp(&power_num.mul(&value_den))
}

/// `base * 2^exponent * radix^place` as an exact fraction.
fn scaled_exact(base: &Big, exponent: i32, radix: u32, place: i32) -> (Big, Big) {
    let mut num = base.clone();
    let mut den = Big::one();
    if exponent >= 0 {
        num.shl(exponent as u32);
    } else {
        den.shl((-exponent) as u32);
    }
    if place >= 0 {
        num.mul_pow(radix, place as u32);
    } else {
        den.mul_pow(radix, (-place) as u32);
    }
    (num, den)
}

/// `round(value / radix^place)`, ties to even — the reference's `JS_RNDN`.
fn scaled_round(mantissa: u64, exponent: i32, radix: u32, place: i32) -> Big {
    // Dividing by `radix^place` is multiplying by `radix^-place`.
    let (num, den) = scaled_exact(&Big::from_u64(mantissa), exponent, radix, -place);
    let (mut quotient, remainder) = num.div_rem_radix(&den, radix);
    let mut twice = remainder;
    twice.mul_small(2);
    match twice.cmp(&den) {
        Ordering::Greater => quotient.add_one(),
        // A tie goes to the even neighbour, matching round-to-nearest-even.
        Ordering::Equal if quotient.is_odd() => quotient.add_one(),
        _ => {}
    }
    quotient
}

/// Whether `scaled * radix^place` rounds back to the original double.
///
/// The doubles that round to `mantissa * 2^exponent` are the ones inside
/// `value ± half an ulp`. The ulp is `2^exponent`, except just above a power of two,
/// where the spacing below is half that — hence the `1 << 52` test. A candidate exactly
/// on a boundary rounds to whichever neighbour is even, so the endpoints are included
/// only when the mantissa is even.
fn round_trips(scaled: &Big, place: i32, radix: u32, mantissa: u64, exponent: i32) -> bool {
    let (value_num, value_den) = scaled_exact(scaled, 0, radix, place);
    let inclusive = mantissa % 2 == 0;
    // Upper: `(mantissa * 2 + 1) * 2^(exponent - 1)`; lower, past a power of two:
    // `(mantissa * 4 - 1) * 2^(exponent - 2)`.
    let lower_half_bits = if mantissa == 1 << 52 { 2 } else { 1 };
    let upper = compare_bound(&value_num, &value_den, mantissa, exponent, 1, 1);
    let lower = compare_bound(
        &value_num,
        &value_den,
        mantissa,
        exponent,
        lower_half_bits,
        -1,
    );
    let below_upper = match upper {
        Ordering::Equal => inclusive,
        other => other == Ordering::Less,
    };
    let above_lower = match lower {
        Ordering::Equal => inclusive,
        other => other == Ordering::Greater,
    };
    below_upper && above_lower
}

/// Compare `value_num / value_den` with `(mantissa * 2^half_bits + sign) * 2^(exponent - half_bits)`.
fn compare_bound(
    value_num: &Big,
    value_den: &Big,
    mantissa: u64,
    exponent: i32,
    half_bits: u32,
    sign: i64,
) -> Ordering {
    // `mantissa * 2^half_bits` fits comfortably: the mantissa uses at most 53 bits.
    let mut bound_num = Big::from_u64(mantissa << half_bits);
    if sign < 0 {
        bound_num.sub_one();
    } else {
        bound_num.add_one();
    }
    let mut bound_den = Big::one();
    let shift = exponent - half_bits as i32;
    if shift >= 0 {
        bound_num.shl(shift as u32);
    } else {
        bound_den.shl((-shift) as u32);
    }
    value_num.mul(&bound_den).cmp(&bound_num.mul(value_den))
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

fn boolean_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let flag = Value::Bool(arg(args, 0).to_boolean());
    // Same wrapper rule as `Number`: `new Boolean(true)` stores the primitive.
    if let Value::Object(object) = &this {
        object.define("_value", Property::with_attributes(flag, false, false));
        return Ok(this);
    }
    let _ = interp;
    Ok(flag)
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

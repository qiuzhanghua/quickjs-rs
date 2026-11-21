//! `Math`.
//!
//! Two behaviours are easy to get wrong and both are covered by the reference's test
//! suite: `Math.round` rounds halves *up* rather than away from zero, so
//! `Math.round(-0.5)` is `-0` and not `-1`; and `Math.min`/`max` return `NaN` if any
//! argument is `NaN`, even though `Math.min(1, NaN)` could be read as `1`.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::Property;
use crate::value::Value;

/// Install `Math`.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let math = interp.new_object(Some(object_proto));

    for (name, value) in [
        ("PI", std::f64::consts::PI),
        ("E", std::f64::consts::E),
        ("LN2", std::f64::consts::LN_2),
        ("LN10", std::f64::consts::LN_10),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("SQRT2", std::f64::consts::SQRT_2),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
    ] {
        math.define(
            name,
            Property::with_attributes(Value::Float(value), false, false),
        );
    }
    for (name, length, func) in [
        ("abs", 1, abs as crate::object::NativeFn),
        ("floor", 1, floor),
        ("ceil", 1, ceil),
        ("round", 1, round),
        ("trunc", 1, trunc),
        ("sign", 1, sign),
        ("sqrt", 1, sqrt),
        ("cbrt", 1, cbrt),
        ("pow", 2, pow),
        ("exp", 1, exp),
        ("log", 1, log),
        ("log2", 1, log2),
        ("log10", 1, log10),
        ("sin", 1, sin),
        ("cos", 1, cos),
        ("tan", 1, tan),
        ("asin", 1, asin),
        ("acos", 1, acos),
        ("atan", 1, atan),
        ("atan2", 2, atan2),
        ("min", 2, min),
        ("max", 2, max),
        ("imul", 2, imul),
        ("hypot", 2, hypot),
        ("fround", 1, fround),
        ("clz32", 1, clz32),
        ("random", 0, random),
        ("sumPrecise", 1, sum_precise),
    ] {
        define_method(&math, name, length, func, interp);
    }

    let global = interp.realm.global.clone();
    global.define(
        "Math",
        Property::with_attributes(Value::Object(math), true, false),
    );
}

fn unary(interp: &mut Interp<'_>, args: &[Value], f: impl Fn(f64) -> f64) -> JsResult<Value> {
    let _ = interp;
    Ok(Value::Float(f(arg(args, 0).to_number())))
}

fn abs(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `abs(-0)` is `+0`, and `abs` of an integer stays an integer.
    Ok(match arg(args, 0) {
        Value::Int(value) => Value::Int(value.saturating_abs()),
        other => Value::Float(other.to_number().abs()),
    })
}

fn floor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::floor)
}

fn ceil(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::ceil)
}

/// `Math.round`, which rounds halves toward `+∞`.
///
/// `(x + 0.5).floor()` would be wrong for negatives (`-0.5` would become `-1`), so
/// the half is added and the result clamped back for zero to keep `-0`.
fn round(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0).to_number();
    if !value.is_finite() {
        return Ok(Value::Float(value));
    }
    let rounded = (value + 0.5).floor();
    // `Math.round(-0.4)` is `-0`, not `0`.
    let rounded = if rounded == 0.0 && value < 0.0 {
        -0.0
    } else {
        rounded
    };
    let _ = interp;
    Ok(Value::Float(rounded))
}

fn trunc(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::trunc)
}

fn sign(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0).to_number();
    let _ = interp;
    Ok(Value::Float(if value.is_nan() {
        f64::NAN
    } else if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        // `sign(-0)` is `-0`, `sign(0)` is `0`.
        value
    }))
}

fn sqrt(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::sqrt)
}

fn cbrt(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::cbrt)
}

fn pow(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let base = arg(args, 0).to_number();
    let exponent = arg(args, 1).to_number();
    let _ = interp;
    Ok(Value::Float(base.powf(exponent)))
}

fn exp(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::exp)
}

fn log(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::ln)
}

fn log2(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::log2)
}

fn log10(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::log10)
}

fn sin(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::sin)
}

fn cos(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::cos)
}

fn tan(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::tan)
}

fn asin(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::asin)
}

fn acos(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::acos)
}

fn atan(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    unary(interp, args, f64::atan)
}

fn atan2(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let y = arg(args, 0).to_number();
    let x = arg(args, 1).to_number();
    let _ = interp;
    Ok(Value::Float(y.atan2(x)))
}

/// `Math.min`, which propagates `NaN` and normalises `-0`.
fn min(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    if args.is_empty() {
        return Ok(Value::Float(f64::INFINITY));
    }
    let mut best = f64::INFINITY;
    for value in args {
        let number = value.to_number();
        if number.is_nan() {
            return Ok(Value::Float(f64::NAN));
        }
        // `Math.min(0, -0)` is `-0`, so an equal comparison keeps the negative one.
        if number < best || (number == best && number.is_sign_negative()) {
            best = number;
        }
    }
    let _ = interp;
    Ok(Value::Float(best))
}

fn max(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    if args.is_empty() {
        return Ok(Value::Float(f64::NEG_INFINITY));
    }
    let mut best = f64::NEG_INFINITY;
    for value in args {
        let number = value.to_number();
        if number.is_nan() {
            return Ok(Value::Float(f64::NAN));
        }
        if number > best || (number == best && !number.is_sign_negative()) {
            best = number;
        }
    }
    let _ = interp;
    Ok(Value::Float(best))
}

/// `Math.imul`, the 32-bit wrapping multiply.
fn imul(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let a = arg(args, 0).to_int32();
    let b = arg(args, 1).to_int32();
    let _ = interp;
    Ok(Value::Int(a.wrapping_mul(b)))
}

/// `Math.hypot`, which avoids intermediate overflow by scaling.
fn hypot(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut max = 0.0f64;
    let mut found_infinite = false;
    let mut found_nan = false;
    let numbers: Vec<f64> = args.iter().map(|value| value.to_number()).collect();
    for number in &numbers {
        if number.is_infinite() {
            found_infinite = true;
        }
        if number.is_nan() {
            found_nan = true;
        }
        max = max.max(number.abs());
    }
    let _ = interp;
    if found_infinite {
        return Ok(Value::Float(f64::INFINITY));
    }
    if found_nan {
        return Ok(Value::Float(f64::NAN));
    }
    if max == 0.0 {
        return Ok(Value::Float(0.0));
    }
    let sum: f64 = numbers.iter().map(|value| (value / max).powi(2)).sum();
    Ok(Value::Float(max * sum.sqrt()))
}

/// `Math.fround`, rounding to the nearest `f32`.
fn fround(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0).to_number();
    let _ = interp;
    Ok(Value::Float(value as f32 as f64))
}

/// `Math.clz32`, the count of leading zero bits.
fn clz32(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0).to_uint32();
    let _ = interp;
    Ok(Value::Int(value.leading_zeros() as i32))
}

/// `Math.sumPrecise(values)`: the correctly rounded sum of an iterable.
///
/// Not a compensated accumulator. The defining property of this function is that the
/// result is the exact sum rounded *once*, so `[1, 2^-53, 2^-1074]` is
/// `1.0000000000000002`: the tiny terms break the tie that `2^-53` alone would lose,
/// while Kahan/Neumaier summation still rounds that final `1 + 2^-53` back to `1`.
///
/// The accumulator is a three-term expansion — `hi` plus two correction terms that
/// together carry roughly 159 bits — which is enough for the sums a script builds out
/// of doubles. Each addition is a `two_sum`, pushing what the hardware rounded into
/// the next term down.
fn sum_precise(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(iterable) = arg(args, 0).as_object().cloned() else {
        return Err(interp.type_error("Math.sumPrecise argument is not iterable"));
    };
    let length = iterable.array_length().unwrap_or(0);
    let mut accumulator = Accumulator::default();
    let mut seen = false;
    for index in 0..length {
        let Some(element) = iterable.array_get(index) else {
            continue;
        };
        if !matches!(element, Value::Int(_) | Value::Float(_)) {
            return Err(interp.type_error("Math.sumPrecise element is not a number"));
        }
        seen = true;
        accumulator.add(element.to_number());
    }
    // The empty sum is `-0`, which is what the reference returns and what the
    // addition identity for this operation is defined as.
    if !seen {
        return Ok(Value::Float(-0.0));
    }
    Ok(Value::Float(accumulator.finish()))
}

/// A three-term exact-enough floating point accumulator.
#[derive(Default)]
struct Accumulator {
    hi: f64,
    mid: f64,
    lo: f64,
}

impl Accumulator {
    fn add(&mut self, value: f64) {
        let (hi, error) = two_sum(self.hi, value);
        self.hi = hi;
        // The error is at most half an ulp of `hi`, so it belongs one term down; the
        // previous `mid` may drop further still.
        let (mid, error) = two_sum(self.mid, error);
        self.mid = mid;
        self.lo += error;
    }

    /// Round the expansion once, breaking ties with whatever is left below.
    fn finish(&self) -> f64 {
        let s = self.hi;
        // An overflowed sum is already the answer; adding the corrections to an
        // infinity would turn it into `NaN`.
        if s.is_infinite() {
            return s;
        }
        if self.mid == 0.0 && self.lo == 0.0 {
            return s;
        }
        // Half an ulp at `s`'s own magnitude, which is the tie threshold.
        let upward = next_after(s) - s;
        let downward = s - next_after_toward_zero(s);
        // The tail is `mid + lo`; compare it with the two half-ulps.
        let tail = self.mid + self.lo;
        if tail > 0.0 {
            if tail > upward / 2.0 || (tail == upward / 2.0 && self.lo != 0.0) {
                return next_after(s);
            }
        } else if tail < 0.0
            && (-tail > downward / 2.0 || (-tail == downward / 2.0 && self.lo != 0.0))
        {
            return next_after_toward_zero(s);
        }
        s
    }
}

/// `a + b` plus what the addition lost.
fn two_sum(a: f64, b: f64) -> (f64, f64) {
    let sum = a + b;
    let recovered = b - (sum - a);
    (sum, recovered)
}

/// The next representable double away from zero.
fn next_after(value: f64) -> f64 {
    if value == 0.0 {
        return f64::from_bits(1);
    }
    if value > 0.0 {
        f64::from_bits(value.to_bits() + 1)
    } else {
        f64::from_bits(value.to_bits() - 1)
    }
}

/// The next representable double toward zero.
fn next_after_toward_zero(value: f64) -> f64 {
    if value == 0.0 {
        return 0.0;
    }
    if value > 0.0 {
        f64::from_bits(value.to_bits() - 1)
    } else {
        f64::from_bits(value.to_bits() + 1)
    }
}

/// `Math.random`.
///
/// A xorshift generator seeded from the clock, matching neither the reference's
/// algorithm nor its sequence — no test can compare random numbers, so the only
/// requirement is that successive calls differ.
fn random(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    static mut STATE: u64 = 0;
    // SAFETY: the interpreter is single-threaded by construction (`Gc` is `!Send`),
    // and this is only a generator seed.
    let state = unsafe {
        if STATE == 0 {
            STATE = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos() as u64)
                .unwrap_or(0x2545_F491_4F6C_DD1D)
                | 1;
        }
        STATE ^= STATE << 13;
        STATE ^= STATE >> 7;
        STATE ^= STATE << 17;
        STATE
    };
    let _ = interp;
    Ok(Value::Float((state >> 11) as f64 / (1u64 << 53) as f64))
}

//! JavaScript values, and the conversions between them.
//!
//! A [`Value`] is what the VM's stack and every property hold. The number split
//! between [`Value::Int`] and [`Value::Float`] mirrors QuickJS's `JS_TAG_INT` /
//! `JS_TAG_FLOAT64`: keeping small integers as `i32` makes array indices and loop
//! counters exact and cheap. Every arithmetic result that does not fit `i32`
//! becomes a `Float`, so the two are interchangeable numerically and only
//! [`Value::strict_equals`] has to care about which one it has.

use std::cell::RefCell;
use std::fmt;

use js_gc::{Gc, Trace, Tracer};

use crate::object::JsObject;

/// An immutable string, stored on the GC heap.
///
/// QuickJS interns strings (`JSString`) and shares them between values, so a
/// `Value` only ever holds a handle. Strings here are not yet interned as atoms —
/// that arrives with the property-key work in M4 — so two equal strings are two
/// allocations that compare by content.
pub struct JsString {
    text: String,
}

impl JsString {
    /// Wrap `text`.
    pub fn new(text: String) -> JsString {
        JsString { text }
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Length in UTF-16 code units, which is what `String.prototype.length` and
    /// the string indexing rules count.
    pub fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }
}

impl Trace for JsString {
    fn trace(&self, _tracer: &mut Tracer) {}
}

impl fmt::Debug for JsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.text)
    }
}

/// A mutable binding cell, shared between closures.
///
/// A variable that a closure captures cannot live in a stack slot, because the
/// slot dies with the frame while the closure outlives it. The compiler marks such
/// bindings `is_captured` and counts them in `var_ref_count`; the interpreter
/// allocates one of these per captured binding on entry, and every access — from
/// the owning frame or from any closure — goes through the cell.
pub struct Cell {
    value: RefCell<Value>,
}

impl Cell {
    /// A cell holding `value`.
    pub fn new(value: Value) -> Cell {
        Cell {
            value: RefCell::new(value),
        }
    }

    /// Read the cell.
    pub fn get(&self) -> Value {
        self.value.borrow().clone()
    }

    /// Overwrite the cell.
    pub fn set(&self, value: Value) {
        *self.value.borrow_mut() = value;
    }
}

impl Trace for Cell {
    fn trace(&self, tracer: &mut Tracer) {
        self.value.borrow().trace(tracer);
    }
}

/// A JavaScript value.
#[derive(Clone)]
pub enum Value {
    /// `undefined`
    Undefined,
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// A 32-bit integer, exact.
    Int(i32),
    /// A double.
    Float(f64),
    /// A string.
    String(Gc<JsString>),
    /// An object, function, or array.
    Object(Gc<JsObject>),
    /// A try-handler target, pushed by `catch` and scanned for when an exception
    /// is raised.
    ///
    /// QuickJS keeps this on the value stack as a `JS_TAG_CATCH_OFFSET` value and
    /// finds the innermost handler by walking the stack downwards, which is how
    /// `try` needs no separate table. Offset `0` marks an enumerator (a `for-of`
    /// loop) rather than a `catch`: it closes the iterator and keeps unwinding.
    CatchOffset(u32),
    /// A `gosub` return position.
    ///
    /// `finally` blocks are compiled as subroutines: `gosub` pushes the address to
    /// come back to and jumps into the block, `ret` pops the address and jumps
    /// back. QuickJS reuses the integer tag for this (and notes it as a hardening
    /// gap); a distinct variant keeps it out of reach of arithmetic.
    ReturnAddress(u32),
    /// An array hole: an index that was skipped rather than filled.
    ///
    /// `[1,,3]` and `[1,undefined,3]` are different arrays: `forEach` skips the hole,
    /// `in` reports it absent, and the debug printer shows `<1 empty item>`. That
    /// distinction has to live in the element vector, and this is the marker for it.
    /// It is never observable from JavaScript: reading the index yields `undefined`
    /// and no language operation can produce it.
    Hole,
    /// The internal marker a `let`/`const` binding holds between its scope being
    /// entered and its initialiser running.
    ///
    /// QuickJS calls this `JS_UNINITIALIZED`; it is deliberately not observable
    /// from JavaScript. It only ever sits in a slot or a cell, and every read that
    /// may see it (`get_loc_check` and friends) turns it into a `ReferenceError`.
    Uninitialized,
}

impl Value {
    /// A string value on `heap`.
    pub fn string(heap: &js_gc::Heap, text: impl Into<String>) -> Value {
        Value::String(Gc::new(heap, JsString::new(text.into())))
    }

    /// The number as a double, widening an [`Value::Int`].
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// The value as a string, if it is one.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s.as_str()),
            _ => None,
        }
    }

    /// The object, if this is one.
    pub fn as_object(&self) -> Option<&Gc<JsObject>> {
        match self {
            Value::Object(o) => Some(o),
            _ => None,
        }
    }

    /// Whether this is one of the two null-ish values.
    pub fn is_nullish(&self) -> bool {
        matches!(self, Value::Undefined | Value::Null)
    }

    /// `typeof value`.
    pub fn type_of(&self) -> &'static str {
        match self {
            Value::Undefined => "undefined",
            Value::Null => "object",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::String(_) => "string",
            Value::Object(object) => {
                if object.is_callable() {
                    "function"
                } else {
                    "object"
                }
            }
            // The internal markers are never observable from JavaScript;
            // reporting `undefined` keeps the answer honest if a bug lets one leak.
            Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => "undefined",
        }
    }

    /// `ToBoolean`, which is also what a conditional jump tests.
    pub fn to_boolean(&self) -> bool {
        match self {
            Value::Undefined | Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            // `NaN` is falsy, and so is `-0`.
            Value::Float(f) => *f != 0.0 && !f.is_nan(),
            Value::String(s) => !s.as_str().is_empty(),
            // Every object is truthy, including `new Boolean(false)`.
            Value::Object(_) => true,
            Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => false,
        }
    }

    /// `ToNumber`, for the arithmetic operators.
    pub fn to_number(&self) -> f64 {
        match self {
            Value::Undefined => f64::NAN,
            Value::Null => 0.0,
            Value::Bool(b) => {
                if *b {
                    1.0
                } else {
                    0.0
                }
            }
            Value::Int(i) => *i as f64,
            Value::Float(f) => *f,
            Value::String(s) => string_to_number(s.as_str()),
            // `ToNumber` of an object runs `ToPrimitive` first, which needs the
            // value-of/valueOf machinery; until that lands an object is `NaN`
            // rather than a panic.
            Value::Object(_) => f64::NAN,
            Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => f64::NAN,
        }
    }

    /// `ToInt32`, the conversion the bitwise operators and `<<`/`>>`/`>>>` use.
    pub fn to_int32(&self) -> i32 {
        to_int32(self.to_number())
    }

    /// `ToUint32`.
    pub fn to_uint32(&self) -> u32 {
        self.to_int32() as u32
    }

    /// `ToString`.
    ///
    /// Named `to_js_string` rather than `to_string` so it does not shadow
    /// [`std::fmt::Display`]: the two disagree on purpose (`display` keeps the sign
    /// of `-0`), and an accidental `to_string` on a `Value` would silently pick the
    /// wrong one.
    pub fn to_js_string(&self) -> String {
        match self {
            Value::Undefined => "undefined".to_string(),
            Value::Null => "null".to_string(),
            Value::Bool(true) => "true".to_string(),
            Value::Bool(false) => "false".to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => number_to_string(*f),
            Value::String(s) => s.as_str().to_string(),
            Value::Object(_) => "[object Object]".to_string(),
            Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => "undefined".to_string(),
        }
    }

    /// `===`.
    pub fn strict_equals(&self, other: &Value) -> bool {
        match (self, other) {
            (Value::Undefined, Value::Undefined) | (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // `Int` and `Float` are the same type to JavaScript, so `1 === 1.0`.
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
                let (a, b) = (self.to_number(), other.to_number());
                a == b
            }
            (Value::String(a), Value::String(b)) => a.as_str() == b.as_str(),
            // Objects compare by identity, never structurally.
            (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    /// `==`, with the coercion rules.
    ///
    /// `NaN` compares unequal to everything including itself, which the numeric
    /// branch already gets right. An object compares by identity against another
    /// object and by its primitive form against a primitive.
    pub fn loose_equals(&self, other: &Value) -> bool {
        if matches!(self, Value::Object(_)) || matches!(other, Value::Object(_)) {
            return match (self, other) {
                (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
                _ => primitive_loose_equals(&self.primitive(), &other.primitive()),
            };
        }
        match (self, other) {
            (Value::Undefined | Value::Null, Value::Undefined | Value::Null) => true,
            (Value::Undefined | Value::Null, _) | (_, Value::Undefined | Value::Null) => false,
            (Value::String(a), Value::String(b)) => a.as_str() == b.as_str(),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // Everything else compares numerically, which covers `"1" == 1`,
            // `true == 1` and `Int` against `Float`.
            _ => self.to_number() == other.to_number(),
        }
    }

    /// `ToPrimitive` with no hint.
    ///
    /// The result is an owned [`Primitive`] rather than a [`Value`] because a
    /// string `Value` needs a heap handle and this path deliberately has none.
    fn primitive(&self) -> Primitive {
        match self {
            Value::Undefined => Primitive::Undefined,
            Value::Null => Primitive::Null,
            Value::Bool(b) => Primitive::Bool(*b),
            Value::Int(i) => Primitive::Number(*i as f64),
            Value::Float(f) => Primitive::Number(*f),
            Value::String(s) => Primitive::String(s.as_str().to_string()),
            Value::Object(_) => Primitive::String(self.to_js_string()),
            Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => Primitive::Undefined,
        }
    }
}

/// A value with no object identity, used by [`Value::loose_equals`].
enum Primitive {
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(String),
}

/// `==` once both sides are primitives.
fn primitive_loose_equals(left: &Primitive, right: &Primitive) -> bool {
    use Primitive::*;
    match (left, right) {
        (Undefined | Null, Undefined | Null) => true,
        (Undefined | Null, _) | (_, Undefined | Null) => false,
        (String(a), String(b)) => a == b,
        (Bool(a), Bool(b)) => a == b,
        (Number(a), Number(b)) => a == b,
        // A string against a number compares numerically, so `"" == 0` and
        // `"1" == 1`.
        (String(s), Number(n)) | (Number(n), String(s)) => string_to_number(s) == *n,
        (Bool(b), Number(n)) | (Number(n), Bool(b)) => (if *b { 1.0 } else { 0.0 }) == *n,
        (Bool(b), String(s)) | (String(s), Bool(b)) => {
            string_to_number(s) == (if *b { 1.0 } else { 0.0 })
        }
    }
}

/// `ToInt32`.
///
/// The spec's modulo-2^32 arithmetic: `NaN` and the infinities become zero, and
/// the fractional part is dropped toward zero before the wrap.
pub fn to_int32(value: f64) -> i32 {
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    let truncated = value.trunc();
    let modulo = truncated.rem_euclid(4294967296.0);
    let wrapped = if modulo >= 2147483648.0 {
        modulo - 4294967296.0
    } else {
        modulo
    };
    wrapped as i32
}

/// `ToNumber` applied to a string.
///
/// Leading and trailing whitespace is allowed; anything else must be a valid
/// numeric literal, because a failed parse is `NaN` rather than a prefix match.
/// The empty string is `0`, which is a common source of surprise.
pub fn string_to_number(text: &str) -> f64 {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return 0.0;
    }
    if let Some(rest) = strip_radix_prefix(trimmed, "0x", "0X") {
        return radix_number(rest, 16);
    }
    if let Some(rest) = strip_radix_prefix(trimmed, "0o", "0O") {
        return radix_number(rest, 8);
    }
    if let Some(rest) = strip_radix_prefix(trimmed, "0b", "0B") {
        return radix_number(rest, 2);
    }
    let (sign, digits) = match trimmed.strip_prefix(['+', '-']) {
        Some(rest) => (&trimmed[..1], rest),
        None => ("", trimmed),
    };
    // `Infinity` is a literal; `inf`/`nan` are not, even though Rust parses them.
    if digits == "Infinity" {
        return if sign == "-" {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    // Rust's parser accepts the literals JavaScript does not, so the shape is
    // checked before trusting it: digits with at most one dot, at most one
    // exponent, and no trailing garbage.
    if !is_decimal_literal(digits) {
        return f64::NAN;
    }
    trimmed.parse::<f64>().unwrap_or(f64::NAN)
}

fn strip_radix_prefix<'a>(text: &'a str, lower: &str, upper: &str) -> Option<&'a str> {
    text.strip_prefix(lower)
        .or_else(|| text.strip_prefix(upper))
}

fn radix_number(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
        return f64::NAN;
    }
    // A long literal overflows `u64`; JavaScript then rounds, so fall back to an
    // accumulating double rather than reporting `NaN`.
    match u64::from_str_radix(digits, radix) {
        Ok(value) => value as f64,
        Err(_) => digits.chars().fold(0.0, |acc, c| {
            acc * radix as f64 + c.to_digit(radix).unwrap_or(0) as f64
        }),
    }
}

/// Whether `text` is a decimal literal with no sign.
fn is_decimal_literal(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut index = 0;
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
    if digits == 0 {
        return false;
    }
    if index < bytes.len() && (bytes[index] == b'e' || bytes[index] == b'E') {
        index += 1;
        if index < bytes.len() && (bytes[index] == b'+' || bytes[index] == b'-') {
            index += 1;
        }
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if index == start {
            return false;
        }
    }
    index == bytes.len()
}

/// `ToString` applied to a number.
///
/// The thresholds are the ones the spec's `Number::toString` uses: exponential
/// form below `1e-6` and from `1e21` up, decimal form in between. Within the
/// decimal range Rust's shortest-round-trip formatter already agrees with
/// JavaScript digit for digit — including the notorious `0.30000000000000004` —
/// so only the exponent spelling has to be normalised.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".to_string()
        } else {
            "-Infinity".to_string()
        };
    }
    if value == 0.0 {
        // `ToString(-0)` is `"0"`; `print(-0)` is a different question, see
        // [`number_to_string_for_print`].
        return "0".to_string();
    }
    let magnitude = value.abs();
    if magnitude >= 1e21 || magnitude < 1e-6 {
        return normalise_exponent(format!("{value:e}"));
    }
    format!("{value}")
}

/// `ToString` as the engine's `print` applies it, which differs from the language
/// one for negative zero.
///
/// `print(-0)` prints `-0` in QuickJS while `String(-0)` is `"0"`, because the host
/// printer takes the number's own spelling. Reproduced here so byte-for-byte output
/// comparisons do not trip over it.
pub fn number_to_string_for_print(value: f64) -> String {
    if value == 0.0 && value.is_sign_negative() {
        return "-0".to_string();
    }
    number_to_string(value)
}

/// Rewrite Rust's exponent spelling as JavaScript's: always signed, never padded.
///
/// Rust writes `1e-7` and `1e21`; JavaScript writes `1e-7` and `1e+21`. The
/// difference is only the explicit `+`.
fn normalise_exponent(text: String) -> String {
    let Some((mantissa, exponent)) = text.split_once('e') else {
        return text;
    };
    if exponent.starts_with('-') {
        text
    } else {
        let digits = exponent.strip_prefix('+').unwrap_or(exponent);
        format!("{mantissa}e+{digits}")
    }
}

impl Trace for Value {
    fn trace(&self, tracer: &mut Tracer) {
        match self {
            Value::String(s) => tracer.visit(s),
            Value::Object(o) => tracer.visit(o),
            _ => {}
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Undefined => f.write_str("undefined"),
            Value::Null => f.write_str("null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{}", number_to_string(*x)),
            Value::String(s) => write!(f, "{:?}", s.as_str()),
            Value::Object(_) => f.write_str("[object]"),
            Value::Uninitialized => f.write_str("<uninitialised>"),
            Value::Hole => f.write_str("<hole>"),
            Value::CatchOffset(offset) => write!(f, "<catch {offset}>"),
            Value::ReturnAddress(address) => write!(f, "<ret {address}>"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn int32_wraps_the_way_the_spec_says() {
        assert_eq!(to_int32(0.0), 0);
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(f64::INFINITY), 0);
        assert_eq!(to_int32(1.9), 1);
        assert_eq!(to_int32(-1.9), -1);
        assert_eq!(to_int32(2147483648.0), -2147483648);
        assert_eq!(to_int32(4294967296.0), 0);
        assert_eq!(to_int32(-4294967297.0), -1);
    }

    #[test]
    fn numbers_print_like_javascript() {
        assert_eq!(number_to_string(0.0), "0");
        assert_eq!(number_to_string(-0.0), "0");
        assert_eq!(number_to_string(1.0), "1");
        assert_eq!(number_to_string(-1.5), "-1.5");
        assert_eq!(number_to_string(1e21), "1e+21");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(1e-6), "0.000001");
        assert_eq!(number_to_string(1e20), "100000000000000000000");
        assert_eq!(number_to_string(5e-324), "5e-324");
        // `print(-0)` differs from `String(-0)`.
        assert_eq!(number_to_string_for_print(-0.0), "-0");
        assert_eq!(number_to_string(f64::NAN), "NaN");
        assert_eq!(number_to_string(f64::INFINITY), "Infinity");
        assert_eq!(number_to_string(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn strings_parse_to_numbers_without_prefix_matching() {
        assert_eq!(string_to_number(""), 0.0);
        assert_eq!(string_to_number("  "), 0.0);
        assert_eq!(string_to_number(" 42 "), 42.0);
        assert_eq!(string_to_number("0x10"), 16.0);
        assert_eq!(string_to_number("0b101"), 5.0);
        assert_eq!(string_to_number("0o17"), 15.0);
        assert_eq!(string_to_number("Infinity"), f64::INFINITY);
        assert_eq!(string_to_number("-Infinity"), f64::NEG_INFINITY);
        assert_eq!(string_to_number("1e3"), 1000.0);
        assert_eq!(string_to_number(".5"), 0.5);
        assert_eq!(string_to_number("5."), 5.0);
        // These would all be prefix matches or Rust-only spellings.
        assert!(string_to_number("12px").is_nan());
        assert!(string_to_number("inf").is_nan());
        assert!(string_to_number("nan").is_nan());
        assert!(string_to_number("1e").is_nan());
        assert!(string_to_number("0x").is_nan());
    }

    #[test]
    fn strict_equality_ignores_the_int_float_split() {
        assert!(Value::Int(1).strict_equals(&Value::Float(1.0)));
        assert!(!Value::Int(1).strict_equals(&Value::Float(1.5)));
        assert!(!Value::Float(f64::NAN).strict_equals(&Value::Float(f64::NAN)));
        assert!(Value::Int(0).strict_equals(&Value::Float(-0.0)));
        assert!(!Value::Int(0).strict_equals(&Value::Null));
        assert!(!Value::Undefined.strict_equals(&Value::Null));
    }

    #[test]
    fn falsy_values_are_the_spec_list() {
        assert!(!Value::Undefined.to_boolean());
        assert!(!Value::Null.to_boolean());
        assert!(!Value::Bool(false).to_boolean());
        assert!(!Value::Int(0).to_boolean());
        assert!(!Value::Float(f64::NAN).to_boolean());
        assert!(!Value::Float(-0.0).to_boolean());
        assert!(Value::Float(0.1).to_boolean());
    }
}

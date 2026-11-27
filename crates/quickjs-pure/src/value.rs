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

/// A JavaScript string's bytes, in **WTF-8**.
///
/// A JS string is a sequence of UTF-16 *code units*, and that sequence may contain an
/// unpaired surrogate: `String.fromCharCode(0xD800)` produces exactly one, and
/// `"\uD800".length` is 1. Rust's `String` cannot represent that, so the bytes here are
/// WTF-8 — ordinary UTF-8, except that a code unit with no partner is written as the
/// three-byte sequence UTF-8 would use for U+D800..U+DFFF.
///
/// # The encoding is canonical, and that is load-bearing
///
/// A *surrogate pair* is written as the single four-byte sequence of the astral code point
/// it spells, never as two three-byte sequences. Without that rule the bytes of `"\uD800" +
/// "\uDC00"` would differ from the bytes of `"\u{10000}"` while the two strings are
/// observably identical — same length, same `charCodeAt` at every index — and every
/// equality, property key and `switch` arm in the engine would disagree with the language.
///
/// The canonical form makes **byte equality exactly code-unit equality**, which is what lets
/// a property key be compared without decoding it.
#[derive(Clone)]
pub struct Wtf8 {
    /// The WTF-8 bytes.
    bytes: Box<[u8]>,
    /// Whether `bytes` is valid UTF-8, meaning the string has no unpaired surrogate.
    ///
    /// A `Wtf8` is immutable, so this stays correct for its whole life.
    valid: bool,
}

impl Wtf8 {
    /// Wrap a Rust string, which is valid UTF-8 by construction.
    pub fn new(text: impl Into<String>) -> Wtf8 {
        Wtf8 {
            bytes: text.into().into_bytes().into_boxed_slice(),
            valid: true,
        }
    }

    /// Build from UTF-16 code units, which is the form a JS string really has.
    ///
    /// An unpaired surrogate survives; a pair is folded into its astral code point.
    pub fn from_units(units: &[u16]) -> Wtf8 {
        let mut bytes = Vec::with_capacity(units.len());
        let mut valid = true;
        let mut index = 0;
        while index < units.len() {
            let unit = units[index];
            match units.get(index + 1) {
                Some(&low) if is_high(unit) && is_low(low) => {
                    push_code_point(&mut bytes, combine(unit, low));
                    index += 2;
                }
                _ if is_surrogate(unit) => {
                    // A lone surrogate keeps its own three-byte form, so it round-trips and
                    // still counts as one code unit.
                    valid = false;
                    push_surrogate(&mut bytes, unit);
                    index += 1;
                }
                _ => {
                    push_code_point(&mut bytes, unit as u32);
                    index += 1;
                }
            }
        }
        Wtf8 {
            bytes: bytes.into_boxed_slice(),
            valid,
        }
    }

    /// The text, when it is valid UTF-8.
    ///
    /// `None` only for a string containing an unpaired surrogate — there is no `&str` for
    /// one. Callers that only need to display or compare use [`Wtf8::to_string_lossy`].
    pub fn as_str(&self) -> Option<&str> {
        if self.valid {
            // SAFETY: `valid` is only set by the encoder above, which produces valid UTF-8
            // exactly when no surrogate was written on its own, and the bytes never change.
            Some(unsafe { std::str::from_utf8_unchecked(&self.bytes) })
        } else {
            None
        }
    }

    /// The text with unpaired surrogates replaced by U+FFFD.
    ///
    /// Borrowed, and therefore free, for the ordinary string that has no surrogate.
    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        match self.as_str() {
            Some(text) => std::borrow::Cow::Borrowed(text),
            None => String::from_utf8_lossy(&self.bytes),
        }
    }

    /// The WTF-8 bytes. Two `Wtf8`s are equal exactly when these are.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The UTF-16 code units, which is what indexing and `charCodeAt` count.
    pub fn to_units(&self) -> Vec<u16> {
        let mut units = Vec::with_capacity(self.utf16_len());
        self.push_units(&mut units);
        units
    }

    /// Length in UTF-16 code units, which is what `String.prototype.length` reports.
    ///
    /// Counted from the encoding rather than by decoding, so it does not allocate: a
    /// four-byte sequence is a surrogate pair and therefore two units, everything else is
    /// one.
    pub fn utf16_len(&self) -> usize {
        self.bytes
            .iter()
            .filter(|byte| (**byte & 0xC0) != 0x80)
            .map(|byte| if *byte >= 0xF0 { 2 } else { 1 })
            .sum()
    }

    /// The code unit at `index`, or `None` past the end.
    pub fn code_unit_at(&self, index: usize) -> Option<u16> {
        // Walking instead of decoding keeps `charCodeAt` in a loop linear rather than
        // quadratic, which matters because that is exactly how a script inspects a string.
        let mut position = 0;
        let mut at = 0;
        while at < self.bytes.len() {
            let (units, count, width) = self.sequence_at(at);
            if position + count > index {
                return Some(units[index - position]);
            }
            position += count;
            at += width;
        }
        None
    }

    /// Whether the string is empty.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// One code unit as a string, which is what `s[i]` yields.
    ///
    /// A single unit, not a code point: `"\u{1F600}"[0]` is the lone high surrogate, because
    /// indexing a string in JS works on code units.
    pub fn unit_at(&self, index: usize) -> Option<Wtf8> {
        self.code_unit_at(index)
            .map(|unit| Wtf8::from_units(&[unit]))
    }

    /// The code units `start..end`, re-encoded canonically.
    pub fn slice_units(&self, start: usize, end: usize) -> Wtf8 {
        let units = self.to_units();
        let start = start.min(units.len());
        let end = end.clamp(start, units.len());
        Wtf8::from_units(&units[start..end])
    }

    /// One element per code *point*, which is how a string is iterated.
    ///
    /// Iteration and indexing disagree on purpose: `[...str]` splits an astral character
    /// into one element while `str[0]` splits it into half a surrogate pair.
    pub fn code_points(&self) -> Vec<Wtf8> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.bytes.len() {
            let (_, _, width) = self.sequence_at(at);
            let mut bytes = Vec::with_capacity(width);
            bytes.extend_from_slice(&self.bytes[at..at + width]);
            // A single sequence is always canonical on its own: a four-byte one is already
            // an astral code point and a three-byte one is a BMP code point or a lone
            // surrogate.
            out.push(Wtf8 {
                valid: std::str::from_utf8(&bytes).is_ok(),
                bytes: bytes.into_boxed_slice(),
            });
            at += width;
        }
        out
    }

    /// Concatenate, keeping the encoding canonical.
    ///
    /// Byte concatenation is already canonical *except* where a high surrogate at the end of
    /// `self` meets a low surrogate at the start of `other`: those two must fold into one
    /// astral sequence, or the result would be a different string from the one the language
    /// says `+` produces.
    pub fn concat(&self, other: &Wtf8) -> Wtf8 {
        let bridges =
            ends_with_high_surrogate(&self.bytes) && starts_with_low_surrogate(&other.bytes);
        if !bridges {
            let mut bytes = Vec::with_capacity(self.bytes.len() + other.bytes.len());
            bytes.extend_from_slice(&self.bytes);
            bytes.extend_from_slice(&other.bytes);
            return Wtf8 {
                bytes: bytes.into_boxed_slice(),
                valid: self.valid && other.valid,
            };
        }
        let mut units = self.to_units();
        units.extend(other.to_units());
        Wtf8::from_units(&units)
    }

    /// Append the code units to `out`.
    fn push_units(&self, out: &mut Vec<u16>) {
        let mut at = 0;
        while at < self.bytes.len() {
            let (units, count, width) = self.sequence_at(at);
            out.extend_from_slice(&units[..count]);
            at += width;
        }
    }

    /// The code points the regular expression engine matches against.
    ///
    /// This is [`Wtf8::code_points`] flattened to `u32`, with a lone surrogate kept as its
    /// own value rather than becoming U+FFFD: `\p{Cs}` has to match it, and no `char` can
    /// hold it. A valid pair is one code point, so an offset into this list is still a code
    /// point boundary.
    pub fn code_point_values(&self) -> Vec<u32> {
        self.code_points()
            .into_iter()
            .map(|point| match point.as_str() {
                Some(text) => text.chars().next().map_or(0xFFFD, |c| c as u32),
                // A lone surrogate: three bytes, one code unit, which *is* the value.
                None => point.to_units().first().copied().map_or(0xFFFD, u32::from),
            })
            .collect()
    }

    /// The code units the sequence starting at `at` stands for, how many of them there are,
    /// and its byte width.
    fn sequence_at(&self, at: usize) -> ([u16; 2], usize, usize) {
        let byte = self.bytes[at];
        let byte_at = |offset: usize| self.bytes.get(at + offset).copied().unwrap_or(0) as u32;
        if byte < 0x80 {
            ([byte as u16, 0], 1, 1)
        } else if byte >> 5 == 0b110 {
            (
                [
                    (((byte as u32 & 0x1F) << 6) | (byte_at(1) & 0x3F)) as u16,
                    0,
                ],
                1,
                2,
            )
        } else if byte >> 4 == 0b1110 {
            // A three-byte sequence is either a BMP code point or a lone surrogate; either
            // way it is exactly one code unit.
            (
                [
                    ((byte as u32 & 0x0F) << 12 | ((byte_at(1) & 0x3F) << 6) | (byte_at(2) & 0x3F))
                        as u16,
                    0,
                ],
                1,
                3,
            )
        } else {
            let code = (byte as u32 & 0x07) << 18
                | ((byte_at(1) & 0x3F) << 12)
                | ((byte_at(2) & 0x3F) << 6)
                | (byte_at(3) & 0x3F);
            let code = code.wrapping_sub(0x10000);
            (
                [
                    0xD800u16.wrapping_add((code >> 10) as u16),
                    0xDC00u16.wrapping_add((code & 0x3FF) as u16),
                ],
                2,
                4,
            )
        }
    }
}

fn is_surrogate(unit: u16) -> bool {
    (0xD800..0xE000).contains(&unit)
}

fn is_high(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

/// The astral code point a surrogate pair spells.
fn combine(high: u16, low: u16) -> u32 {
    0x10000 + ((high as u32 - 0xD800) << 10) + (low as u32 - 0xDC00)
}

/// Write a code point as UTF-8.
fn push_code_point(out: &mut Vec<u8>, code: u32) {
    if code < 0x80 {
        out.push(code as u8);
    } else if code < 0x800 {
        out.push(0xC0 | (code >> 6) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else if code < 0x10000 {
        out.push(0xE0 | (code >> 12) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else {
        out.push(0xF0 | (code >> 18) as u8);
        out.push(0x80 | ((code >> 12) & 0x3F) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    }
}

/// Write an unpaired surrogate as the three-byte sequence WTF-8 gives it.
fn push_surrogate(out: &mut Vec<u8>, unit: u16) {
    out.push(0xE0 | (unit >> 12) as u8);
    out.push(0x80 | ((unit >> 6) & 0x3F) as u8);
    out.push(0x80 | (unit & 0x3F) as u8);
}

/// Whether the bytes end with an unpaired high surrogate.
///
/// High surrogates are `ED A0..AF 80..BF` and low ones `ED B0..BF 80..BF`, so the test is on
/// the second byte of the trailing three-byte sequence.
fn ends_with_high_surrogate(bytes: &[u8]) -> bool {
    let len = bytes.len();
    len >= 3 && bytes[len - 3] == 0xED && (0xA0..0xB0).contains(&bytes[len - 2])
}

/// Whether the bytes start with an unpaired low surrogate.
fn starts_with_low_surrogate(bytes: &[u8]) -> bool {
    bytes.len() >= 3 && bytes[0] == 0xED && (0xB0..0xC0).contains(&bytes[1])
}

impl PartialEq for Wtf8 {
    /// Byte equality, which the canonical encoding makes equal to code-unit equality.
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for Wtf8 {}

impl fmt::Debug for Wtf8 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.to_string_lossy())
    }
}

/// An immutable string, stored on the GC heap.
///
/// The content is a [`Wtf8`]; the wrapper exists so a `Value` can hold a `Gc` handle rather
/// than the bytes.
pub struct JsString {
    text: Wtf8,
    /// The code units, built on first use.
    ///
    /// Indexing is the one operation a script performs in a loop over a whole string —
    /// `for (i = 0; i < s.length; i++) s.charCodeAt(i)` — and decoding from the start each
    /// time would make that quadratic. Built once, it makes every later access `O(1)`.
    /// Nothing here is a GC value, so the cache needs no tracing.
    units: RefCell<Option<std::rc::Rc<Vec<u16>>>>,
}

impl JsString {
    /// Wrap `text`.
    pub fn new(text: impl Into<String>) -> JsString {
        JsString {
            text: Wtf8::new(text),
            units: RefCell::new(None),
        }
    }

    /// Wrap WTF-8 that is already encoded.
    pub fn from_wtf8(text: Wtf8) -> JsString {
        JsString {
            text,
            units: RefCell::new(None),
        }
    }

    /// The code units, decoding once and remembering the result.
    pub fn units_cached(&self) -> std::rc::Rc<Vec<u16>> {
        if let Some(units) = self.units.borrow().as_ref() {
            return units.clone();
        }
        let units = std::rc::Rc::new(self.text.to_units());
        *self.units.borrow_mut() = Some(units.clone());
        units
    }

    /// The code unit at `index`, in `O(1)` once the cache is warm.
    pub fn code_unit_at(&self, index: usize) -> Option<u16> {
        self.units_cached().get(index).copied()
    }

    /// The string's bytes.
    pub fn wtf8(&self) -> &Wtf8 {
        &self.text
    }

    /// The text, when it has no unpaired surrogate.
    pub fn as_str(&self) -> Option<&str> {
        self.text.as_str()
    }

    /// The text with unpaired surrogates replaced by U+FFFD.
    pub fn to_string_lossy(&self) -> std::borrow::Cow<'_, str> {
        self.text.to_string_lossy()
    }

    /// The UTF-16 code units.
    pub fn to_units(&self) -> Vec<u16> {
        self.units_cached().as_ref().clone()
    }

    /// Length in UTF-16 code units, which is what `String.prototype.length` and
    /// the string indexing rules count.
    pub fn utf16_len(&self) -> usize {
        // Cheap from the encoding, and the common case never builds the cache.
        self.text.utf16_len()
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

/// A JavaScript symbol, stored on the GC heap.
///
/// A symbol is a *unique* property key that is not a string. Its identity is the
/// allocation, not the description: `Symbol("a") !== Symbol("a")`, and two symbols with
/// the same description still key two different properties. Only `Symbol.for` produces
/// the same symbol twice, and it does so by looking the key up in the realm's registry
/// rather than by comparing descriptions.
pub struct JsSymbol {
    /// The description shown by `Symbol.prototype.toString` and `description`.
    ///
    /// `Symbol()` and the well-known symbols have none, which is why this is an
    /// `Option` and not an empty string — `Symbol().description` is `undefined`.
    description: Option<String>,
    /// The registry key, when the symbol came from `Symbol.for`.
    ///
    /// `Symbol.keyFor` answers `undefined` for every symbol without one, including the
    /// well-known symbols, so the presence of this field is exactly what it tests.
    registry_key: Option<String>,
}

impl JsSymbol {
    /// A unique symbol with `description`.
    pub fn new(description: Option<String>) -> JsSymbol {
        JsSymbol {
            description,
            registry_key: None,
        }
    }

    /// The registry entry for `key`, as `Symbol.for` returns it.
    pub fn registered(key: String) -> JsSymbol {
        JsSymbol {
            description: Some(key.clone()),
            registry_key: Some(key),
        }
    }

    /// The description, if there is one.
    pub fn description(&self) -> Option<&str> {
        self.description.as_deref()
    }

    /// The registry key, if this symbol came from `Symbol.for`.
    pub fn registry_key(&self) -> Option<&str> {
        self.registry_key.as_deref()
    }

    /// `Symbol.prototype.toString`: `Symbol(description)`.
    ///
    /// A description is inserted verbatim, so `Symbol("a)b").toString()` is
    /// `Symbol(a)b)` — the spec does not escape it.
    pub fn to_display_string(&self) -> String {
        match &self.description {
            Some(text) => format!("Symbol({text})"),
            None => "Symbol()".to_string(),
        }
    }
}

impl Trace for JsSymbol {
    fn trace(&self, _tracer: &mut Tracer) {}
}

impl fmt::Debug for JsSymbol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_display_string())
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
    /// A symbol: a unique property key that is not a string.
    Symbol(Gc<JsSymbol>),
    /// A `BigInt`: an integer of any size, stored on the GC heap.
    ///
    /// A separate variant rather than a number because the two never mix: `1n + 1` is a
    /// `TypeError`, and `1n === 1` is false.
    BigInt(Gc<crate::bigint::BigInt>),
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
        Value::String(Gc::new(heap, JsString::new(text)))
    }

    /// A string from bytes that are already WTF-8, which is how a string containing an
    /// unpaired surrogate reaches a `Value`.
    pub fn string_wtf8(heap: &js_gc::Heap, text: Wtf8) -> Value {
        Value::String(Gc::new(heap, JsString::from_wtf8(text)))
    }

    /// The number as a double, widening an [`Value::Int`].
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }

    /// The value as a string, if it is one with no unpaired surrogate.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => s.as_str(),
            _ => None,
        }
    }

    /// The value's string content as WTF-8, which covers every string including one with an
    /// unpaired surrogate.
    pub fn as_wtf8(&self) -> Option<&Wtf8> {
        match self {
            Value::String(s) => Some(s.wtf8()),
            _ => None,
        }
    }

    /// The symbol, if this is one.
    pub fn as_symbol(&self) -> Option<&Gc<JsSymbol>> {
        match self {
            Value::Symbol(symbol) => Some(symbol),
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
            Value::Symbol(_) => "symbol",
            Value::BigInt(_) => "bigint",
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
            Value::String(s) => !s.wtf8().is_empty(),
            // Every symbol is truthy, even the one with no description.
            Value::Symbol(_) => true,
            // A bigint follows the numbers: `0n` is falsy and every other width is truthy.
            // Saying "every bigint is truthy" turned `if (b & 1n)` into an unconditional
            // branch, which quietly corrupted every bigint algorithm that tests a bit.
            Value::BigInt(big) => !big.is_zero(),
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
            Value::String(s) => string_to_number(&s.to_string_lossy()),
            // `ToNumber` of a symbol or a bigint is a `TypeError`; `NaN` keeps this
            // infallible helper total, and the operators reject them first.
            Value::Symbol(_) => f64::NAN,
            Value::BigInt(_) => f64::NAN,
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
            // Lossy only for a string with an unpaired surrogate, which has no `&str`
            // form at all; every other string borrows and copies nothing extra.
            Value::String(s) => s.to_string_lossy().into_owned(),
            Value::BigInt(big) => big.to_decimal_string(),
            // `ToString` of a symbol is a `TypeError`; this descriptive form is what
            // `String(sym)` and the error messages use. Callers that must throw go
            // through `Interp::checked_to_string`.
            Value::Symbol(s) => s.to_display_string(),
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
            // Two bigints compare by value, because the representation is canonical —
            // `2n ** 1000n` computed twice is one value, not two handles.
            (Value::BigInt(a), Value::BigInt(b)) => **a == **b,
            // A symbol is equal only to itself: the same description means nothing.
            (Value::Symbol(a), Value::Symbol(b)) => a.ptr_eq(b),
            // Objects compare by identity, never structurally.
            (Value::Object(a), Value::Object(b)) => a.ptr_eq(b),
            _ => false,
        }
    }

    /// `SameValue`, the equality `Object.is` uses and a proxy invariant needs.
    ///
    /// It is [`Value::same_value_zero`] plus the one extra distinction: `+0` and `-0`
    /// are different values here, which is what makes `Object.is(0, -0)` false.
    pub fn same_value(&self, other: &Value) -> bool {
        if let (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) = (self, other) {
            let (a, b) = (self.to_number(), other.to_number());
            if a.is_nan() && b.is_nan() {
                return true;
            }
            if a == 0.0 && b == 0.0 {
                return a.is_sign_negative() == b.is_sign_negative();
            }
            return a == b;
        }
        self.strict_equals(other)
    }

    /// `SameValueZero`, the equality a `Map` key and a `Set` element use.
    ///
    /// It differs from `===` in exactly one place: `NaN` equals itself. That is what makes
    /// `map.set(NaN, 1); map.get(NaN)` work, and it is why a `Map` cannot use `strict_equals`
    /// directly. A bigint compares by value, since its representation is canonical.
    pub fn same_value_zero(&self, other: &Value) -> bool {
        if let (Value::Float(a), Value::Float(b)) = (self, other)
            && a.is_nan()
            && b.is_nan()
        {
            return true;
        }
        self.strict_equals(other)
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
            (Value::Symbol(a), Value::Symbol(b)) => a.ptr_eq(b),
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // A bigint equals another bigint by value, a string that spells it exactly, or a
            // number with the same mathematical value — that last one is why `1n == 1` is
            // true while `1n === 1` is false.
            (Value::BigInt(a), Value::BigInt(b)) => **a == **b,
            (Value::BigInt(a), Value::String(b)) | (Value::String(b), Value::BigInt(a)) => {
                crate::bigint::BigInt::from_string(&b.to_string_lossy()).as_ref() == Some(&**a)
            }
            (Value::BigInt(a), other) => match other {
                Value::Int(_) | Value::Float(_) => bigint_equals_number(a, other.to_number()),
                // A boolean is `ToNumber`ed first, so `true == 1n` is true.
                Value::Bool(flag) => bigint_equals_number(a, if *flag { 1.0 } else { 0.0 }),
                _ => false,
            },
            (other, Value::BigInt(b)) => match other {
                Value::Int(_) | Value::Float(_) => bigint_equals_number(b, other.to_number()),
                Value::Bool(flag) => bigint_equals_number(b, if *flag { 1.0 } else { 0.0 }),
                _ => false,
            },
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
            Value::String(s) => Primitive::String(s.to_string_lossy().into_owned()),
            // A symbol stays distinct from a string here so `==` does not treat it as
            // one; only an object's `ToPrimitive` can produce a symbol from an object.
            Value::Symbol(s) => Primitive::Symbol(s.clone()),
            // A bigint reaches `Primitive` as its decimal text, which is enough for the
            // loose comparisons that go through here; the operators reject it first.
            Value::BigInt(big) => Primitive::String(big.to_decimal_string()),
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
    /// A symbol, which is a primitive but compares by identity.
    Symbol(Gc<JsSymbol>),
}

/// `==` once both sides are primitives.
fn primitive_loose_equals(left: &Primitive, right: &Primitive) -> bool {
    use Primitive::*;
    match (left, right) {
        (Undefined | Null, Undefined | Null) => true,
        (Undefined | Null, _) | (_, Undefined | Null) => false,
        // A symbol is a primitive, so `==` on two symbols is identity. Against
        // anything else it is `false` — the only coercion that could apply is boxing,
        // and `Object(Symbol())` is an object, not a `Primitive`.
        (Symbol(a), Symbol(b)) => a.ptr_eq(b),
        (Symbol(_), _) | (_, Symbol(_)) => false,
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

/// `bigint == number`, compared as whole numbers rather than through the double.
///
/// Going through `to_f64` made `100000000000000000001n == 1e20` true, because the double
/// rounds both sides onto the same value; the spec compares the exact integers, and a
/// double that is already an integer converts to one exactly.
fn bigint_equals_number(big: &crate::bigint::BigInt, number: f64) -> bool {
    number.is_finite()
        && number.fract() == 0.0
        && crate::bigint::BigInt::from_f64_truncated(number) == *big
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
            Value::Symbol(s) => tracer.visit(s),
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
            Value::Symbol(s) => f.write_str(&s.to_display_string()),
            Value::BigInt(big) => write!(f, "{}n", big.to_decimal_string()),
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

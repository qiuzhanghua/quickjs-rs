//! `String` and `String.prototype`.
//!
//! String indexing and `length` count UTF-16 code units, not characters, because
//! that is what a script observes: `"🐱".length` is `2`. The methods here work on
//! `char`s where the spec is character-based and on code units where it is not, and
//! the two places that difference shows up (`charAt`/`charCodeAt` versus
//! `codePointAt`) are called out where they occur.

use crate::builtins::{arg, arg_int_or, define_method, this_string};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install `String` and its prototype methods.
pub fn install(interp: &mut Interp<'_>, string_proto: &Gc<JsObject>) {
    for (name, length, func) in [
        ("charAt", 1, char_at as crate::object::NativeFn),
        ("charCodeAt", 1, char_code_at),
        ("codePointAt", 1, code_point_at),
        ("indexOf", 1, index_of),
        ("lastIndexOf", 1, last_index_of),
        ("includes", 1, includes),
        ("startsWith", 1, starts_with),
        ("endsWith", 1, ends_with),
        ("slice", 2, slice),
        ("substring", 2, substring),
        ("substr", 2, substr),
        ("split", 2, split),
        ("concat", 1, concat),
        ("toUpperCase", 0, to_upper_case),
        ("toLowerCase", 0, to_lower_case),
        ("trim", 0, trim),
        ("trimStart", 0, trim_start),
        ("trimEnd", 0, trim_end),
        ("repeat", 1, repeat),
        ("padStart", 2, pad_start),
        ("padEnd", 2, pad_end),
        ("toString", 0, to_string),
        ("valueOf", 0, to_string),
        ("at", 1, at),
        ("replace", 2, replace),
        ("localeCompare", 1, locale_compare),
    ] {
        define_method(string_proto, name, length, func, interp);
    }

    let constructor = interp.new_native("String", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(string_proto.clone()), false, false),
    );
    string_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("fromCharCode", 1, from_char_code as crate::object::NativeFn),
        ("fromCodePoint", 1, from_code_point),
    ] {
        define_method(&constructor, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define(
        "String",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

fn constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // `String(x)` converts; `new String(x)` boxes. The constructor is reached both
    // ways, and the difference is whether `this` is an object.
    //
    // The conversion has to be `ToString`, not the built-in's own `to_js_string`:
    // an object argument goes through `ToPrimitive`, so `String(err)` gives
    // `"Error: m"` — `Error.prototype.toString` — instead of `[object Object]`.
    let text = match args.first() {
        Some(value) => interp.to_js_string(value)?,
        None => String::new(),
    };
    let value = interp.string(text);
    // `new String(x)` boxes, so the text is stored on the receiver for
    // `String.prototype.valueOf` and the boxing path in `this_object` to find.
    if let Value::Object(object) = &this {
        object.define("_value", Property::with_attributes(value, false, false));
        return Ok(this);
    }
    Ok(value)
}

fn from_char_code(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `fromCharCode` takes UTF-16 code units, so surrogates can be assembled by hand.
    let units: Vec<u16> = args
        .iter()
        .map(|value| (value.to_number() as i64 & 0xffff) as u16)
        .collect();
    Ok(interp.string(String::from_utf16_lossy(&units)))
}

fn from_code_point(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut text = String::new();
    for value in args {
        let code = value.to_number();
        if !(0.0..=0x10ffff as f64).contains(&code) {
            return Err(interp.range_error("invalid code point"));
        }
        text.push(char::from_u32(code as u32).unwrap_or('\u{fffd}'));
    }
    Ok(interp.string(text))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The receiver text, or a `TypeError` when `this` is neither a string nor a box.
fn text_of(interp: &Interp<'_>, this: &Value) -> Result<String, crate::interp::Signal> {
    this_string(interp, this).ok_or_else(|| interp.type_error("String method on a non-string"))
}

/// The UTF-16 code units of `text`, which is what index arithmetic counts.
fn units(text: &str) -> Vec<u16> {
    text.encode_utf16().collect()
}

/// Rebuild a string from `units`.
fn from_units(units: &[u16]) -> String {
    String::from_utf16_lossy(units)
}

/// Normalise a possibly negative start/end index for a code-unit range.
fn clamp(index: f64, length: usize) -> usize {
    if index.is_nan() {
        return 0;
    }
    if index < 0.0 {
        let from_end = length as f64 + index;
        if from_end < 0.0 { 0 } else { from_end as usize }
    } else if index > length as f64 {
        length
    } else {
        index as usize
    }
}

// ---------------------------------------------------------------------------
// Character access
// ---------------------------------------------------------------------------

fn char_at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    // A negative index is out of range, not clamped to zero: `"a".charAt(-1)` is
    // `""`, which is different from `"a".charAt(0)`.
    let Some(index) = unit_index(arg_int_or(args, 0, 0), text.len()) else {
        return Ok(interp.string(String::new()));
    };
    Ok(interp.string(from_units(&text[index..index + 1])))
}

/// A non-negative index into a string of `length` code units, or `None`.
///
/// The "at" family all treat a negative index as absent rather than wrapping, which
/// is what separates them from `slice` and `at`.
fn unit_index(index: i64, length: usize) -> Option<usize> {
    if index < 0 || index as usize >= length {
        None
    } else {
        Some(index as usize)
    }
}

fn char_code_at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    let index = unit_index(arg_int_or(args, 0, 0), text.len());
    Ok(match index.and_then(|index| text.get(index)) {
        Some(unit) => Value::Int(*unit as i32),
        None => Value::Float(f64::NAN),
    })
}

fn code_point_at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    let Some(index) = unit_index(arg_int_or(args, 0, 0), text.len()) else {
        return Ok(Value::Undefined);
    };
    let first = text[index];
    // A high surrogate followed by a low one is a single code point; anything else
    // is the unit itself.
    if (0xd800..0xdc00).contains(&first)
        && let Some(second) = text.get(index + 1).copied()
        && (0xdc00..0xe000).contains(&second)
    {
        let combined = 0x10000 + ((first as u32 - 0xd800) << 10) + (second as u32 - 0xdc00);
        return Ok(Value::Int(combined as i32));
    }
    Ok(Value::Int(first as i32))
}

fn at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    let raw = arg_int_or(args, 0, 0);
    let index = if raw < 0 {
        text.len() as i64 + raw
    } else {
        raw
    };
    if index < 0 || index as usize >= text.len() {
        return Ok(Value::Undefined);
    }
    Ok(interp.string(from_units(&text[index as usize..index as usize + 1])))
}

// ---------------------------------------------------------------------------
// Searching
// ---------------------------------------------------------------------------

fn index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let needle = arg(args, 0).to_js_string();
    // A negative start does *not* count from the end here, unlike `slice`: it clamps
    // to zero, so `"aaa".indexOf("a", -1)` is `0`.
    let start = (arg_int_or(args, 1, 0).max(0) as usize).min(text.len());
    // Searching from `start` on the *byte* offset is safe because a UTF-8 boundary
    // that is not a character boundary cannot match a whole needle.
    let found = text
        .get(start..)
        .and_then(|tail| tail.find(&needle))
        .map(|offset| text[..start].len() + offset);
    Ok(Value::Int(match found {
        Some(offset) => text[..offset].chars().map(char::len_utf16).sum::<usize>() as i32,
        None => -1,
    }))
}

fn last_index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let needle = arg(args, 0).to_js_string();
    // The second argument caps where a match may *start*, not where it may end: a
    // negative position clamps to zero, so `"aaa".lastIndexOf("a", -Infinity)` still
    // finds the match at 0.
    let limit = match args.get(1) {
        None | Some(Value::Undefined) => text.len(),
        Some(value) => {
            let position = value.to_number();
            if position.is_nan() {
                text.len()
            } else if position < 0.0 {
                0
            } else {
                (position as usize).min(text.len())
            }
        }
    };
    // Search the candidate start positions from `limit` downwards.
    let found = (0..=limit)
        .rev()
        .find(|start| text[*start..].starts_with(&needle));
    Ok(Value::Int(match found {
        Some(offset) => text[..offset].chars().map(char::len_utf16).sum::<usize>() as i32,
        None => -1,
    }))
}

fn includes(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let needle = arg(args, 0).to_js_string();
    Ok(Value::Bool(text.contains(&needle)))
}

fn starts_with(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let needle = arg(args, 0).to_js_string();
    let start = (arg_int_or(args, 1, 0).max(0) as usize).min(text.len());
    Ok(Value::Bool(
        text.get(start..)
            .is_some_and(|tail| tail.starts_with(&needle)),
    ))
}

fn ends_with(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let needle = arg(args, 0).to_js_string();
    let end = match args.get(1) {
        None | Some(Value::Undefined) => text.len(),
        Some(value) => clamp(value.to_number(), text.len()),
    };
    Ok(Value::Bool(
        text.get(..end).is_some_and(|head| head.ends_with(&needle)),
    ))
}

fn locale_compare(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let other = arg(args, 0).to_js_string();
    Ok(Value::Int(match text.cmp(&other) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

// ---------------------------------------------------------------------------
// Slicing
// ---------------------------------------------------------------------------

fn slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    let start = clamp(arg_int_or(args, 0, 0) as f64, text.len());
    let end = match args.get(1) {
        None | Some(Value::Undefined) => text.len(),
        Some(value) => clamp(value.to_number(), text.len()),
    };
    Ok(interp.string(from_units(if start < end {
        &text[start..end]
    } else {
        &[]
    })))
}

fn substring(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    // `substring` clamps to `[0, length]` and swaps its arguments when start > end;
    // it does not accept negative indices, which is the whole difference from `slice`.
    let length = text.len() as f64;
    let a = arg_int_or(args, 0, 0).max(0) as f64;
    let b = match args.get(1) {
        None | Some(Value::Undefined) => length,
        Some(value) => value.to_number().max(0.0),
    };
    let (start, end) = if a.is_nan() || b.is_nan() {
        (0, 0)
    } else {
        let a = a.min(length);
        let b = b.min(length);
        if a <= b {
            (a as usize, b as usize)
        } else {
            (b as usize, a as usize)
        }
    };
    Ok(interp.string(from_units(&text[start..end])))
}

fn substr(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&text_of(interp, &this)?);
    let length = text.len();
    let start = clamp(arg_int_or(args, 0, 0) as f64, length);
    let count = match args.get(1) {
        None | Some(Value::Undefined) => length - start,
        Some(value) => value.to_number().max(0.0) as usize,
    };
    let end = (start + count).min(length);
    Ok(interp.string(from_units(&text[start..end])))
}

fn split(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let limit = match args.get(1) {
        None | Some(Value::Undefined) => usize::MAX,
        Some(value) => value.to_number().max(0.0) as usize,
    };
    let parts: Vec<Value> = match arg(args, 0) {
        // `"abc".split()` is `["abc"]`; `"".split()` is `[]`.
        Value::Undefined => {
            if text.is_empty() {
                Vec::new()
            } else {
                vec![interp.string(text)]
            }
        }
        Value::String(separator) => {
            let separator = separator.as_str();
            if separator.is_empty() {
                text.chars()
                    .map(|character| interp.string(character.to_string()))
                    .collect()
            } else {
                text.split(separator)
                    .map(|part| interp.string(part.to_string()))
                    .collect()
            }
        }
        _ => vec![interp.string(text)],
    };
    let limited: Vec<Value> = parts.into_iter().take(limit).collect();
    let array = interp.new_array(limited);
    Ok(Value::Object(array))
}

fn concat(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let mut text = text_of(interp, &this)?;
    for value in args {
        text.push_str(&value.to_js_string());
    }
    Ok(interp.string(text))
}

fn replace(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let pattern = arg(args, 0).to_js_string();
    let replacement = arg(args, 1);
    let Some(offset) = text.find(&pattern) else {
        return Ok(interp.string(text));
    };
    let before = &text[..offset];
    let after = &text[offset + pattern.len()..];
    // A function replacement receives the match and its offset; a string one has its
    // `$&` and `$1`-style sequences expanded (there are no groups without regex).
    let substituted = match &replacement {
        Value::Object(object) if object.is_callable() => interp
            .call_value(
                replacement.clone(),
                Value::Undefined,
                vec![interp.string(pattern.clone()), Value::Int(offset as i32)],
            )?
            .to_js_string(),
        other => other.to_js_string().replace("$&", &pattern),
    };
    Ok(interp.string(format!("{before}{substituted}{after}")))
}

// ---------------------------------------------------------------------------
// Case, whitespace, repetition
// ---------------------------------------------------------------------------

fn to_upper_case(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?.to_uppercase()))
}

fn to_lower_case(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?.to_lowercase()))
}

fn trim(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?.trim().to_string()))
}

fn trim_start(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?.trim_start().to_string()))
}

fn trim_end(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?.trim_end().to_string()))
}

fn repeat(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let count = arg_number_int(args, 0);
    if count < 0 {
        return Err(interp.range_error("repeat count must not be negative"));
    }
    Ok(interp.string(text.repeat(count as usize)))
}

fn arg_number_int(args: &[Value], index: usize) -> i64 {
    arg(args, index).to_number() as i64
}

fn pad_start(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    pad(interp, this, args, true)
}

fn pad_end(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    pad(interp, this, args, false)
}

fn pad(interp: &mut Interp<'_>, this: Value, args: &[Value], at_start: bool) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    let target = match args.first() {
        None | Some(Value::Undefined) => 0usize,
        Some(value) => value.to_number().max(0.0) as usize,
    };
    let filler = match args.get(1) {
        None | Some(Value::Undefined) => " ".to_string(),
        Some(value) => value.to_js_string(),
    };
    let current = text.encode_utf16().count();
    if target <= current || filler.is_empty() {
        return Ok(interp.string(text));
    }
    let mut padding = String::new();
    while padding.encode_utf16().count() < target - current {
        padding.push_str(&filler);
    }
    let mut padding: Vec<u16> = padding.encode_utf16().collect();
    padding.truncate(target - current);
    let padding = String::from_utf16_lossy(&padding);
    Ok(interp.string(if at_start {
        format!("{padding}{text}")
    } else {
        format!("{text}{padding}")
    }))
}

fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(interp.string(text_of(interp, &this)?))
}

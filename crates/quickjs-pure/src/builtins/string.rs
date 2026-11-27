//! `String` and `String.prototype`.
//!
//! String indexing and `length` count UTF-16 code units, not characters, because
//! that is what a script observes: `"🐱".length` is `2`. The methods here work on
//! `char`s where the spec is character-based and on code units where it is not, and
//! the two places that difference shows up (`charAt`/`charCodeAt` versus
//! `codePointAt`) are called out where they occur.

use crate::builtins::{arg, arg_int_or, define_method};
use crate::interp::{Interp, JsResult, WellKnown};
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
        ("match", 1, match_method),
        ("matchAll", 1, match_all),
        ("search", 1, search),
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

    // `String.prototype[Symbol.iterator]` walks the UTF-16 code units. `array::values`
    // already does exactly that when handed a string primitive — see `element_at` — so
    // the function is shared rather than reimplemented.
    if let Some(values_fn) = interp
        .realm
        .array_proto
        .own_property("values")
        .and_then(|property| property.value)
    {
        let key = interp
            .realm
            .well_known_key(crate::interp::WellKnown::Iterator);
        string_proto.define_key(&key, Property::data(values_fn));
    }

    let constructor = interp.new_native_constructor("String", 1, constructor);
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
        ("raw", 1, raw as crate::object::NativeFn),
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
    // `new String(x)` boxes: the receiver is a String *exotic* object, so it needs the
    // text itself plus the `length` and index properties described in §10.4.3.4.
    // Filling only `_value` left `new String("ab").length` `undefined` and made every
    // `Array.prototype` method called on a box see an array-like of length 0.
    if let (Value::Object(object), Value::String(text)) = (&this, &value) {
        interp.init_string_wrapper(object, text);
        return Ok(this);
    }
    Ok(value)
}

/// `String.raw(template, …values)`.
///
/// The leading argument is the call site's template object, and the *raw* chunks are the
/// source text before escape processing — so `String.raw`a\tb`` keeps the backslash and
/// the `t`. A substitution is inserted verbatim, with no `ToString` beyond the one the
/// concatenation does.
fn raw(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let template = arg(args, 0);
    // `ToObject` refuses `undefined`/`null`, which is what makes `String.raw()` throw.
    let chunks = interp.read_field(&template, "raw")?;
    let length = interp.read_field(&chunks, "length")?.to_number().max(0.0) as usize;
    let mut out = String::new();
    for index in 0..length {
        let chunk = interp.read_field(&chunks, &index.to_string())?;
        out.push_str(&interp.to_js_string(&chunk)?);
        if index + 1 < length {
            if let Some(value) = args.get(index + 1) {
                out.push_str(&interp.to_js_string(value)?);
            }
        }
    }
    Ok(interp.string(out))
}

fn from_char_code(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `fromCharCode` takes UTF-16 code units, so surrogates can be assembled by hand.
    let units: Vec<u16> = args
        .iter()
        .map(|value| (value.to_number() as i64 & 0xffff) as u16)
        .collect();
    // Not `String::from_utf16_lossy`: a unit with no partner is a legal JS string, and
    // replacing it with U+FFFD is exactly the bug this representation exists to avoid.
    Ok(interp.string_wtf8(crate::value::Wtf8::from_units(&units)))
}

fn from_code_point(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut units = Vec::with_capacity(args.len());
    for value in args {
        let code = value.to_number();
        if !(0.0..=0x10ffff as f64).contains(&code) {
            return Err(interp.range_error("invalid code point"));
        }
        let code = code as u32;
        // A code point in the surrogate range is not a `char`, but it is a legal argument
        // here: the spec's only rejection is the range check above.
        if (0xd800..0xe000).contains(&code) {
            units.push(code as u16);
        } else {
            let mut buffer = [0u16; 2];
            units.extend_from_slice(
                char::from_u32(code)
                    .unwrap_or('\u{fffd}')
                    .encode_utf16(&mut buffer),
            );
        }
    }
    Ok(interp.string_wtf8(crate::value::Wtf8::from_units(&units)))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The receiver text, or a `TypeError` when `this` is neither a string nor a box.
/// The text a string method operates on: `RequireObjectCoercible` then `ToString`.
///
/// This is **not** a brand check. §22.1's methods all say
///
/// ```text
/// 1. Let O be ? RequireObjectCoercible(this value).
/// 2. Let S be ? ToString(O).
/// ```
///
/// so `String.prototype.trim.call(123)` is `"123"` and `.call({toString(){return " a "}})`
/// trims that object. Only `toString` and `valueOf` do `thisStringValue`, and they are the
/// two methods that do *not* come through here. Treating every method as a brand check made
/// 116 test262 cases fail with `TypeError: String method on a non-string`.
fn text_of(interp: &mut Interp<'_>, this: &Value) -> JsResult<String> {
    Ok(require_string(interp, this)?.to_string_lossy().into_owned())
}

/// The receiver's code units without flattening an unpaired surrogate.
///
/// The "at" family indexes code units, so it must not read through a lossy `String`: a
/// `0xFFFD` where the string holds `0xD800` would give wrong indices.
fn wtf8_of(interp: &mut Interp<'_>, this: &Value) -> JsResult<crate::value::Wtf8> {
    Ok(require_string(interp, this)?.wtf8().clone())
}

/// `RequireObjectCoercible(this)` followed by `ToString`, as the WTF-8 form.
///
/// `null` and `undefined` are refused; everything else converts, including an object whose
/// `toString`/`valueOf` answers something else. A Symbol has no string form, so it is a
/// `TypeError` too — that is `ToString`'s own rule, not a brand check.
fn require_string(
    interp: &mut Interp<'_>,
    this: &Value,
) -> JsResult<js_gc::Gc<crate::value::JsString>> {
    if matches!(this, Value::Undefined | Value::Null) {
        // `RequireObjectCoercible`'s message names the receiver, and both spellings are
        // compared verbatim by test262.
        let what = if matches!(this, Value::Null) {
            "null"
        } else {
            "undefined"
        };
        return Err(interp.type_error(format!("cannot convert {what} to a string")));
    }
    // A primitive string needs no conversion, so keep the object and its cached units.
    if let Value::String(text) = this {
        return Ok(text.clone());
    }
    // A boxed string converts through `ToPrimitive`, which finds `_value` through
    // `toString` — the same path `String(Object("ab"))` takes — so no special case.
    let text = interp.checked_to_wtf8(this)?;
    Ok(js_gc::Gc::new(
        interp.heap,
        crate::value::JsString::from_wtf8(text),
    ))
}

/// The receiver's string object, so a caller can use its cached code units.
///
/// Only the two brand-checking methods use this: `String.prototype.toString` and
/// `valueOf` both run `thisStringValue`, which refuses anything whose `_value` is not
/// already a string. `String.prototype.toString.call(123)` is a `TypeError`, unlike
/// `String.prototype.trim.call(123)`.
fn string_object(
    interp: &Interp<'_>,
    this: &Value,
) -> Result<js_gc::Gc<crate::value::JsString>, crate::interp::Signal> {
    match this {
        Value::String(text) => Ok(text.clone()),
        // A boxed string carries the primitive in `_value`.
        Value::Object(object) => match interp.internal_get(object, "_value") {
            Some(Value::String(text)) => Ok(text),
            _ => Err(interp.type_error("String method on a non-string")),
        },
        _ => Err(interp.type_error("String method on a non-string")),
    }
}

/// The UTF-16 code units of `text`, which is what index arithmetic counts.
fn units(text: &crate::value::Wtf8) -> Vec<u16> {
    text.to_units()
}

/// Rebuild a string from code units, keeping unpaired surrogates intact.
fn from_units(units: &[u16]) -> crate::value::Wtf8 {
    crate::value::Wtf8::from_units(units)
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
    let text = units(&wtf8_of(interp, &this)?);
    // A negative index is out of range, not clamped to zero: `"a".charAt(-1)` is
    // `""`, which is different from `"a".charAt(0)`.
    let Some(index) = unit_index(arg_int_or(args, 0, 0), text.len()) else {
        return Ok(interp.string(String::new()));
    };
    Ok(interp.string_wtf8(from_units(&text[index..index + 1])))
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
    // Straight at the receiver's cached units rather than through a decoded copy: this is
    // the operation a `for` loop over a string repeats, so it has to be `O(1)`.
    let object = string_object(interp, &this)?;
    let index = arg_int_or(args, 0, 0);
    if index < 0 {
        return Ok(Value::Float(f64::NAN));
    }
    Ok(match object.code_unit_at(index as usize) {
        Some(unit) => Value::Int(unit as i32),
        None => Value::Float(f64::NAN),
    })
}

fn code_point_at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&wtf8_of(interp, &this)?);
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
    let text = units(&wtf8_of(interp, &this)?);
    let raw = arg_int_or(args, 0, 0);
    let index = if raw < 0 {
        text.len() as i64 + raw
    } else {
        raw
    };
    if index < 0 || index as usize >= text.len() {
        return Ok(Value::Undefined);
    }
    Ok(interp.string_wtf8(from_units(&text[index as usize..index as usize + 1])))
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
    let text = units(&wtf8_of(interp, &this)?);
    let start = clamp(arg_int_or(args, 0, 0) as f64, text.len());
    let end = match args.get(1) {
        None | Some(Value::Undefined) => text.len(),
        Some(value) => clamp(value.to_number(), text.len()),
    };
    Ok(interp.string_wtf8(from_units(if start < end {
        &text[start..end]
    } else {
        &[]
    })))
}

fn substring(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&wtf8_of(interp, &this)?);
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
    Ok(interp.string_wtf8(from_units(&text[start..end])))
}

fn substr(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = units(&wtf8_of(interp, &this)?);
    let length = text.len();
    let start = clamp(arg_int_or(args, 0, 0) as f64, length);
    let count = match args.get(1) {
        None | Some(Value::Undefined) => length - start,
        Some(value) => value.to_number().max(0.0) as usize,
    };
    let end = (start + count).min(length);
    Ok(interp.string_wtf8(from_units(&text[start..end])))
}

fn split(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?;
    if let Some(result) = symbol_dispatch(interp, &this, WellKnown::Split, args)? {
        return Ok(result);
    }
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
        // `ToString(separator)`, not "leave any other kind alone". An object separator runs
        // its `toString`, and if that answers an object `ToPrimitive` throws — which is what
        // `transferred-to-number-separator-override-tostring-returns-regexp` asserts. The
        // `Symbol.split` dispatch above already handled a real RegExp.
        separator => {
            let separator = interp.to_js_string(&separator)?;
            if separator.is_empty() {
                text.chars()
                    .map(|character| interp.string(character.to_string()))
                    .collect()
            } else {
                text.split(&*separator)
                    .map(|part| interp.string(part.to_string()))
                    .collect()
            }
        }
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
    if let Some(result) = symbol_dispatch(interp, &this, WellKnown::Replace, args)? {
        return Ok(result);
    }
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

/// Hand `args` to the pattern's `Symbol.replace`-style method, when it has one.
///
/// This is how a `RegExp` (or any object that opts in) gets to define what `replace` and
/// friends mean, and it is why `String.prototype.split` needs no regexp knowledge of its
/// own.
fn symbol_dispatch(
    interp: &mut Interp<'_>,
    this: &Value,
    which: WellKnown,
    args: &[Value],
) -> JsResult<Option<Value>> {
    let pattern = arg(args, 0);
    let Some(object) = pattern.as_object() else {
        return Ok(None);
    };
    let method = interp.get_property_key(object, &interp.realm.well_known_key(which))?;
    let Some(method) = method else {
        return Ok(None);
    };
    if matches!(method, Value::Undefined | Value::Null) {
        return Ok(None);
    }
    let mut call_args: Vec<Value> = vec![this.clone()];
    call_args.extend(args.iter().skip(1).cloned());
    if call_args.len() < 2 && which == WellKnown::Replace {
        call_args.push(Value::Undefined);
    }
    let result = interp.call_value(method, pattern, call_args)?;
    Ok(Some(result))
}

/// `String.prototype.match(pattern)`.
fn match_method(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    if let Some(result) = symbol_dispatch(interp, &this, WellKnown::Match, args)? {
        return Ok(result);
    }
    // A plain string pattern matches itself once, and reports the index.
    let text = text_of(interp, &this)?;
    let pattern = arg(args, 0).to_js_string();
    let Some(offset) = text.find(&pattern) else {
        return Ok(Value::Null);
    };
    let array = interp.new_array(vec![interp.string(&pattern)]);
    interp.set_property(&array, "index", Value::Int(offset as i32))?;
    interp.set_property(&array, "input", interp.string(&text))?;
    Ok(Value::Object(array))
}

/// `String.prototype.matchAll(pattern)`.
fn match_all(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    if let Some(result) = symbol_dispatch(interp, &this, WellKnown::MatchAll, args)? {
        return Ok(result);
    }
    Err(interp.type_error("matchAll needs a global RegExp"))
}

/// `String.prototype.search(pattern)`.
fn search(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    if let Some(result) = symbol_dispatch(interp, &this, WellKnown::Search, args)? {
        return Ok(result);
    }
    let text = text_of(interp, &this)?;
    let pattern = arg(args, 0).to_js_string();
    Ok(match text.find(&pattern) {
        Some(offset) => Value::Int(offset as i32),
        None => Value::Int(-1),
    })
}

// ---------------------------------------------------------------------------
// Case, whitespace, repetition
// ---------------------------------------------------------------------------

fn to_upper_case(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?.to_uppercase();
    Ok(interp.string(text))
}

fn to_lower_case(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?.to_lowercase();
    Ok(interp.string(text))
}

fn trim(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?.trim().to_string();
    Ok(interp.string(text))
}

fn trim_start(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?.trim_start().to_string();
    Ok(interp.string(text))
}

fn trim_end(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = text_of(interp, &this)?.trim_end().to_string();
    Ok(interp.string(text))
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

/// `String.prototype.toString` and `valueOf`: `thisStringValue`, a real brand check.
///
/// These two are the *only* `String.prototype` methods that refuse a non-string receiver
/// (§22.1.3.28): `String.prototype.toString.call(123)` is a `TypeError` while
/// `String.prototype.trim.call(123)` is `"123"`.
///
/// They also have to be the brand check for a mechanical reason: they are what
/// `ToPrimitive` calls on a boxed string, so routing them through `ToString` would ask
/// `ToPrimitive` for the string while already inside `ToPrimitive` — unbounded recursion,
/// which is how the process aborted before this was split apart.
fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let text = string_object(interp, &this)?.wtf8().clone();
    Ok(interp.string_wtf8(text))
}

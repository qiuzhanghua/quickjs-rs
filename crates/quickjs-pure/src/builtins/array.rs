//! `Array` and `Array.prototype`.
//!
//! Arrays are the type a script touches most, so this is the largest module. Two
//! behaviours are worth calling out because they are easy to get subtly wrong:
//!
//! * **`length` is derived, not stored.** It is the element count, so every method
//!   that changes the length only has to change the element vector and the property
//!   reads the right answer. `a.length = n` is handled in the interpreter's
//!   property setter for the same reason.
//! * **Method names are non-enumerable.** `for (k in [])` must not list them, which
//!   is why every install goes through [`define_method`].

use crate::builtins::{arg, arg_int_or, define_method, this_object};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install `Array` and its prototype methods.
pub fn install(interp: &mut Interp<'_>, array_proto: &Gc<JsObject>) {
    for (name, length, func) in [
        ("push", 1, push as crate::object::NativeFn),
        ("pop", 0, pop),
        ("shift", 0, shift),
        ("unshift", 1, unshift),
        ("slice", 2, slice),
        ("splice", 2, splice),
        ("concat", 1, concat),
        ("join", 1, join),
        ("reverse", 0, reverse),
        ("indexOf", 1, index_of),
        ("lastIndexOf", 1, last_index_of),
        ("includes", 1, includes),
        ("forEach", 1, for_each),
        ("map", 1, map),
        ("filter", 1, filter),
        ("reduce", 1, reduce),
        ("reduceRight", 1, reduce_right),
        ("some", 1, some),
        ("every", 1, every),
        ("find", 1, find),
        ("findIndex", 1, find_index),
        ("fill", 1, fill),
        ("sort", 1, sort),
        ("flat", 0, flat),
        ("flatMap", 1, flat_map),
        ("toString", 0, to_string),
        ("at", 1, at),
        ("values", 0, values),
        ("keys", 0, keys),
        ("entries", 0, entries),
    ] {
        define_method(array_proto, name, length, func, interp);
    }

    // `Array.prototype[Symbol.iterator]` is `values`: the spec defines iteration over an
    // array as iteration over its values, and sharing the function means a script
    // calling `a[Symbol.iterator]()` and a `for…of` get the same kind of object.
    if let Some(values_fn) = array_proto
        .own_property("values")
        .and_then(|property| property.value)
    {
        let key = interp
            .realm
            .well_known_key(crate::interp::WellKnown::Iterator);
        array_proto.define_key(&key, Property::data(values_fn));
    }

    let constructor = interp.new_native("Array", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(array_proto.clone()), false, false),
    );
    array_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("isArray", 1, is_array as crate::object::NativeFn),
        ("from", 1, from),
        ("of", 0, of),
    ] {
        define_method(&constructor, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define(
        "Array",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `Array(n)` makes `n` holes; `Array(a, b)` makes a two-element array.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // One numeric argument is a *length*, so it has to be a whole number in range:
    // `new Array(-1)` and `new Array(1.5)` are both `RangeError`, and the limit is the
    // spec's `2^32 - 1`.
    if args.len() == 1
        && let Some(length) = args[0].as_f64()
    {
        if length < 0.0 || length.fract() != 0.0 || length > 4294967295.0 {
            return Err(interp.range_error("invalid array length"));
        }
        let array = interp.new_array(Vec::new());
        // Holes, not `undefined`: `new Array(3)` has three empty slots, so
        // `2 in new Array(3)` is false while `new Array(3).length` is 3.
        array.array_set_length(length as usize);
        return Ok(Value::Object(array));
    }
    let array = interp.new_array(args.to_vec());
    Ok(Value::Object(array))
}

fn is_array(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    Ok(Value::Bool(
        arg(args, 0)
            .as_object()
            .is_some_and(|object| object.is_array()),
    ))
}

fn from(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let source = arg(args, 0);
    let mut out = Vec::new();
    // A string is iterable and yields its characters; an array-like has a length.
    if let Value::String(text) = &source {
        for point in text.wtf8().code_points() {
            out.push(interp.string_wtf8(point));
        }
    } else if let Some(object) = source.as_object() {
        let length = interp
            .get_property(object, "length")?
            .map(|value| value.to_number() as usize)
            .unwrap_or(0);
        for index in 0..length {
            if let Some(value) = interp.get_property(object, &index.to_string())? {
                out.push(value);
            }
        }
    }
    // A mapping function turns it into `Array.from(src, fn)`, i.e. `map`.
    if let Some(mapper) = args
        .get(1)
        .cloned()
        .filter(|v| !matches!(v, Value::Undefined))
    {
        let mut mapped = Vec::with_capacity(out.len());
        for (index, value) in out.into_iter().enumerate() {
            mapped.push(interp.call_value(
                mapper.clone(),
                Value::Undefined,
                vec![value, Value::Int(index as i32)],
            )?);
        }
        out = mapped;
    }
    let array = interp.new_array(out);
    Ok(Value::Object(array))
}

fn of(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let array = interp.new_array(args.to_vec());
    Ok(Value::Object(array))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The receiver as an array, or as an array-like copied into one.
///
/// Array methods are generic in the spec: `Array.prototype.join.call({length:2,0:'a',1:'b'})`
/// has to work. Rather than thread "maybe not an array" through every method, a
/// non-array receiver is copied up front, which is correct for every method that only
/// reads, and harmless for the ones that also write.
fn receiver(interp: &mut Interp<'_>, this: &Value) -> JsResult<Gc<JsObject>> {
    // A string receiver is the common generic case — `Array.prototype.join.call("abc")`
    // — and its elements are its characters, not properties on the box.
    if let Value::String(text) = this {
        let elements: Vec<Value> = text
            .wtf8()
            .code_points()
            .into_iter()
            .map(|point| interp.string_wtf8(point))
            .collect();
        let copy = interp.new_array(elements);
        // A box's indices are read-only, which is why `sort.call("aa")` throws.
        copy.set_elements_writable(false);
        return Ok(copy);
    }
    let object = this_object(interp, this)?;
    if object.is_array() {
        return Ok(object);
    }
    let length = interp
        .get_property(&object, "length")?
        .map(|value| value.to_number().max(0.0) as usize)
        .unwrap_or(0);
    // These methods copy the receiver, so an absurd `length` would be a hang rather
    // than a slow call. The iterators are lazy and do not have this limit; a genuine
    // array of this size would already be out of memory.
    const MAX_ARRAY_LIKE: usize = 1 << 26;
    if length > MAX_ARRAY_LIKE {
        return Err(interp.range_error("array-like length is too large to copy"));
    }
    let mut elements = Vec::with_capacity(length);
    for index in 0..length {
        elements.push(
            interp
                .get_property(&object, &index.to_string())?
                .unwrap_or(Value::Undefined),
        );
    }
    let copy = interp.new_array(elements);
    // Any other primitive receiver is boxed, and a box's indices are read-only too.
    if !matches!(this, Value::Object(_)) {
        copy.set_elements_writable(false);
    }
    Ok(copy)
}

fn elements(object: &Gc<JsObject>) -> Vec<Value> {
    object.array_elements().unwrap_or_default()
}

/// Normalise a possibly negative index the way every array method does.
fn normalise(index: f64, length: usize) -> usize {
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
// Basic mutation
// ---------------------------------------------------------------------------

fn push(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    for value in args {
        let index = array.array_length().unwrap_or(0);
        // `push` adds an index past the end, which a non-extensible array refuses.
        if !array.is_extensible() {
            return Err(interp.type_error("object is not extensible"));
        }
        store_or_throw(interp, &array, index, value.clone())?;
    }
    Ok(Value::Int(array.array_length().unwrap_or(0) as i32))
}

fn pop(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut current = elements(&array);
    let value = current.pop().unwrap_or(Value::Undefined);
    array.array_set_length(current.len());
    Ok(value)
}

fn shift(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut current = elements(&array);
    if current.is_empty() {
        return Ok(Value::Undefined);
    }
    let value = current.remove(0);
    array.array_set_length(current.len());
    for (index, element) in current.into_iter().enumerate() {
        store_or_throw(interp, &array, index, element)?;
    }
    Ok(value)
}

fn unshift(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current = elements(&array);
    let mut combined: Vec<Value> = args.to_vec();
    combined.extend(current);
    let length = combined.len();
    for (index, element) in combined.into_iter().enumerate() {
        store_or_throw(interp, &array, index, element)?;
    }
    Ok(Value::Int(length as i32))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current = elements(&array);
    let start = normalise(arg_int_or(args, 0, 0) as f64, current.len());
    let end = normalise(
        match args.get(1) {
            None | Some(Value::Undefined) => current.len() as f64,
            Some(value) => value.to_number(),
        },
        current.len(),
    );
    let out = if start >= end {
        Vec::new()
    } else {
        current[start..end].to_vec()
    };
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn concat(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut out = elements(&array);
    for value in args {
        // An array argument is flattened one level; anything else is appended.
        match value.as_object() {
            Some(object) if object.is_array() => out.extend(elements(object)),
            _ => out.push(value.clone()),
        }
    }
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn join(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let separator = match args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(value) => interp.to_js_string(value)?,
    };
    // A hole joins as an empty field, exactly as `undefined` does; the difference is
    // only visible to `forEach` and the debug printer. An element that is an object
    // goes through `ToString`, so a boxed number joins as `"1"`.
    let mut parts = Vec::with_capacity(elements(&array).len());
    for value in elements(&array) {
        let part = match value {
            Value::Undefined | Value::Null | Value::Hole => String::new(),
            other => interp.to_js_string(&other)?,
        };
        parts.push(part);
    }
    Ok(interp.string(parts.join(&separator)))
}

fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    join(interp, this, &[])
}

fn index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current = elements(&array);
    let needle = arg(args, 0);
    let start = normalise(arg_int_or(args, 1, 0) as f64, current.len());
    let found = current
        .iter()
        .enumerate()
        .skip(start)
        .find(|(_, value)| value.strict_equals(&needle))
        .map(|(index, _)| index);
    Ok(Value::Int(found.map_or(-1, |index| index as i32)))
}

fn last_index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current = elements(&array);
    let needle = arg(args, 0);
    let found = current
        .iter()
        .enumerate()
        .rev()
        .find(|(_, value)| value.strict_equals(&needle))
        .map(|(index, _)| index);
    Ok(Value::Int(found.map_or(-1, |index| index as i32)))
}

fn includes(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let needle = arg(args, 0);
    let start = normalise(arg_int_or(args, 1, 0) as f64, elements(&array).len());
    // `includes` uses SameValueZero, so it finds `NaN` where `indexOf` cannot.
    let found = elements(&array).iter().skip(start).any(|value| {
        value.strict_equals(&needle)
            || (matches!(value, Value::Float(f) if f.is_nan())
                && matches!(needle, Value::Float(f) if f.is_nan()))
    });
    Ok(Value::Bool(found))
}

fn at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current = elements(&array);
    let length = current.len() as f64;
    let raw = arg_int_or(args, 0, 0) as f64;
    let index = if raw < 0.0 { length + raw } else { raw };
    if index < 0.0 || index >= length {
        return Ok(Value::Undefined);
    }
    Ok(current[index as usize].clone())
}

// ---------------------------------------------------------------------------
// Iterators
// ---------------------------------------------------------------------------

/// `arr.values()`, `arr.keys()`, `arr.entries()`.
///
/// The result is an ordinary object with a `next` method, which is all a `for-of`
/// needs once the interpreter accepts a `next`-carrying object as an iterator. The
/// `ArrayIterator` class itself (and `Symbol.iterator`) arrives with symbols.
fn values(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    iterator(interp, this, IteratorKind::Value)
}

fn keys(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    iterator(interp, this, IteratorKind::Key)
}

fn entries(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    iterator(interp, this, IteratorKind::Entry)
}

/// What an array iterator yields.
///
/// Shared with `Map` and `Set`, which hand their own snapshot to the same machinery rather
/// than growing a second iterator implementation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IteratorKind {
    Value,
    Key,
    Entry,
}

pub(crate) fn iterator(
    interp: &mut Interp<'_>,
    this: Value,
    kind: IteratorKind,
) -> JsResult<Value> {
    // The source is kept as given, **not** materialised: the array methods copy an
    // array-like up front, but an iterator must not, because
    // `Array.prototype.keys.call({length: 2**32 + 1})` is legal and only advances a
    // few steps before `break`.
    let proto = interp.realm.object_proto.clone();
    let object = interp.new_object(Some(proto));
    object.define_data("_source", this);
    object.define_data("_index", Value::Int(0));
    object.define_data(
        "_kind",
        Value::Int(match kind {
            IteratorKind::Value => 0,
            IteratorKind::Key => 1,
            IteratorKind::Entry => 2,
        }),
    );
    let next = crate::builtins::method(interp, "next", 0, array_iterator_next)?;
    object.define_data("next", next);
    // An iterator is itself iterable — `for (var k of a.keys())` needs it — and the way
    // it says so is a `Symbol.iterator` method answering `this`.
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Iterator);
    let identity = crate::builtins::method(interp, "[Symbol.iterator]", 0, self_iterator)?;
    object.define_key(&key, Property::data(identity));
    Ok(Value::Object(object))
}

/// `%ArrayIteratorPrototype%[Symbol.iterator]`, which answers the iterator itself.
fn self_iterator(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(this)
}

/// The shared `next` of the three array iterators.
fn array_iterator_next(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object().cloned() else {
        return Err(interp.type_error("next called on a non-object"));
    };
    let source = interp
        .get_property(&object, "_source")?
        .unwrap_or(Value::Undefined);
    let index = interp
        .get_property(&object, "_index")?
        .map(|value| value.to_number() as usize)
        .unwrap_or(0);
    let kind = interp
        .get_property(&object, "_kind")?
        .map(|value| value.to_number() as i32)
        .unwrap_or(0);
    let length = length_of(interp, &source)?;
    let proto = interp.realm.object_proto.clone();
    let result = interp.new_object(Some(proto));
    if index >= length {
        result.define_data("value", Value::Undefined);
        result.define_data("done", Value::Bool(true));
        return Ok(Value::Object(result));
    }
    interp.set_property(&object, "_index", Value::Int(index as i32 + 1))?;
    let element = element_at(interp, &source, index)?;
    let value = match kind {
        1 => Value::Int(index as i32),
        2 => Value::Object(interp.new_array(vec![Value::Int(index as i32), element])),
        _ => element,
    };
    result.define_data("value", value);
    result.define_data("done", Value::Bool(false));
    Ok(Value::Object(result))
}

/// The clamped length of an array-like, read lazily.
fn length_of(interp: &mut Interp<'_>, source: &Value) -> JsResult<usize> {
    if let Some(array) = source.as_object().cloned()
        && array.is_array()
    {
        return Ok(array.array_length().unwrap_or(0));
    }
    if let Value::String(text) = source {
        return Ok(text.utf16_len());
    }
    let Some(object) = source.as_object().cloned() else {
        return Ok(0);
    };
    let length = interp
        .get_property(&object, "length")?
        .map(|value| value.to_number())
        .unwrap_or(0.0);
    // A negative or `NaN` length is zero; a fractional one truncates.
    Ok(if length.is_finite() && length > 0.0 {
        length as usize
    } else {
        0
    })
}

/// One element of an array-like, read lazily.
fn element_at(interp: &mut Interp<'_>, source: &Value, index: usize) -> JsResult<Value> {
    if let Value::String(text) = source {
        return Ok(match text.wtf8().unit_at(index) {
            Some(unit) => interp.string_wtf8(unit),
            None => Value::Undefined,
        });
    }
    let Some(object) = source.as_object().cloned() else {
        return Ok(Value::Undefined);
    };
    Ok(interp
        .get_property(&object, &index.to_string())?
        .unwrap_or(Value::Undefined))
}

// ---------------------------------------------------------------------------
// Higher-order methods
// ---------------------------------------------------------------------------

/// Run `callback` over the elements, the shape every iteration method needs.
fn iterate(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Vec<(usize, Value)>> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let mut out = Vec::new();
    for (index, value) in elements(&array).into_iter().enumerate() {
        // A hole is not an element: the iteration methods skip it entirely, so
        // `[1,,3].forEach(f)` calls `f` twice and the index passed is the real one.
        if matches!(value, Value::Hole) {
            continue;
        }
        interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value.clone(),
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        out.push((index, value));
    }
    Ok(out)
}

fn for_each(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    iterate(interp, this, args)?;
    Ok(Value::Undefined)
}

fn map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let mut out = Vec::new();
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        out.push(interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value,
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?);
    }
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn filter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let mut out = Vec::new();
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        let keep = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value.clone(),
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        if keep.to_boolean() {
            out.push(value);
        }
    }
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn reduce(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    reduce_with(interp, this, args, false)
}

fn reduce_right(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    reduce_with(interp, this, args, true)
}

fn reduce_with(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    right_to_left: bool,
) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let current: Vec<(usize, Value)> = elements(&array)
        .into_iter()
        .enumerate()
        .filter(|(_, value)| !matches!(value, Value::Hole))
        .collect();
    let ordered: Vec<(usize, Value)> = if right_to_left {
        current.into_iter().rev().collect()
    } else {
        current
    };
    let mut ordered = ordered.into_iter();
    let mut accumulator = match args.get(1) {
        Some(value) if !matches!(value, Value::Undefined) => value.clone(),
        _ => match ordered.next() {
            Some((_, value)) => value,
            None => {
                return Err(interp.type_error("Reduce of empty array with no initial value"));
            }
        },
    };
    for (index, value) in ordered {
        accumulator = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                accumulator,
                value,
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
    }
    Ok(accumulator)
}

fn some(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value,
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn every(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value,
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        if !answer.to_boolean() {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

fn find(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value.clone(),
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(value);
        }
    }
    Ok(Value::Undefined)
}

fn find_index(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let callback = arg(args, 0);
    for (index, value) in elements(&array).into_iter().enumerate() {
        if matches!(value, Value::Hole) {
            continue;
        }
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![
                value,
                Value::Int(index as i32),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(Value::Int(index as i32));
        }
    }
    Ok(Value::Int(-1))
}

fn fill(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let current_len = elements(&array).len();
    let value = arg(args, 0);
    let start = normalise(arg_int_or(args, 1, 0) as f64, current_len);
    let end = normalise(
        match args.get(2) {
            None | Some(Value::Undefined) => current_len as f64,
            Some(value) => value.to_number(),
        },
        current_len,
    );
    for index in start..end {
        store_or_throw(interp, &array, index, value.clone())?;
    }
    Ok(Value::Object(array))
}

// ---------------------------------------------------------------------------
// splice / sort / flat
// ---------------------------------------------------------------------------

fn splice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut current = elements(&array);
    let length = current.len();
    let start = normalise(arg_int_or(args, 0, 0) as f64, length);
    let remove = if args.len() > 1 {
        (arg_int_or(args, 1, 0).max(0) as usize).min(length - start)
    } else if args.is_empty() {
        0
    } else {
        // `splice(start)` removes everything from `start` on.
        length - start
    };
    let removed: Vec<Value> = current
        .splice(start..start + remove, args.iter().skip(2).cloned())
        .collect();
    array.array_set_length(current.len());
    for (index, value) in current.into_iter().enumerate() {
        store_or_throw(interp, &array, index, value)?;
    }
    let result = interp.new_array(removed);
    Ok(Value::Object(result))
}

fn reverse(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut current = elements(&array);
    current.reverse();
    for (index, value) in current.into_iter().enumerate() {
        store_or_throw(interp, &array, index, value)?;
    }
    Ok(Value::Object(array))
}

/// `sort`, with the default order being the *string* order of the elements.
///
/// That default is the surprising part of the spec: `[10, 9].sort()` is `[10, 9]`
/// because `"10" < "9"`. Kept as-is because the reference does the same.
fn sort(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut current = elements(&array);
    let comparator = args
        .first()
        .cloned()
        .filter(|value| !matches!(value, Value::Undefined));
    match &comparator {
        Some(function) if matches!(function, Value::Object(object) if object.is_callable()) => {
            // A merge sort, because a comparator is an arbitrary JavaScript call and
            // Rust's sort needs a total order it can trust; this keeps the comparison
            // count deterministic instead of depending on the pivot strategy.
            let mut items = std::mem::take(&mut current);
            merge_sort(interp, &mut items, function)?;
            current = items;
        }
        _ => {
            current.sort_by_key(|value| SortKey(value.to_js_string()));
        }
    }
    for (index, value) in current.into_iter().enumerate() {
        store_or_throw(interp, &array, index, value)?;
    }
    Ok(Value::Object(array))
}

/// Store one element of a write-back, raising when the array refuses.
///
/// The array methods all go through `[[Set]]` in the spec, so a frozen element makes
/// `sort`/`reverse`/`splice` throw rather than silently keep the old value.
fn store_or_throw(
    interp: &Interp<'_>,
    array: &Gc<JsObject>,
    index: usize,
    value: Value,
) -> JsResult<()> {
    if array.array_store(index, value) {
        return Ok(());
    }
    Err(interp.type_error(format!("'{index}' is read-only")))
}

/// A key that sorts like JavaScript's default string comparison.
struct SortKey(String);

impl PartialEq for SortKey {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for SortKey {}

impl PartialOrd for SortKey {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortKey {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // JavaScript compares UTF-16 code units, which for the BMP is the same as
        // comparing UTF-8 bytes only within a plane; comparing `char`s is exact.
        self.0.chars().cmp(other.0.chars())
    }
}

fn merge_sort(interp: &mut Interp<'_>, items: &mut Vec<Value>, comparator: &Value) -> JsResult<()> {
    if items.len() <= 1 {
        return Ok(());
    }
    let mut right = items.split_off(items.len() / 2);
    merge_sort(interp, items, comparator)?;
    merge_sort(interp, &mut right, comparator)?;
    let mut merged = Vec::with_capacity(items.len() + right.len());
    let mut left_iter = std::mem::take(items).into_iter().peekable();
    let mut right_iter = right.into_iter().peekable();
    loop {
        match (left_iter.peek(), right_iter.peek()) {
            (Some(a), Some(b)) => {
                let answer = interp.call_value(
                    comparator.clone(),
                    Value::Undefined,
                    vec![a.clone(), b.clone()],
                )?;
                if answer.to_number() > 0.0 {
                    merged.push(right_iter.next().expect("peeked"));
                } else {
                    merged.push(left_iter.next().expect("peeked"));
                }
            }
            (Some(_), None) => merged.push(left_iter.next().expect("peeked")),
            (None, Some(_)) => merged.push(right_iter.next().expect("peeked")),
            (None, None) => break,
        }
    }
    *items = merged;
    Ok(())
}

fn flat(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let depth = match args.first() {
        None | Some(Value::Undefined) => 1,
        Some(value) => value.to_number() as i64,
    };
    let mut out = Vec::new();
    flatten(&elements(&array), depth, &mut out);
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn flatten(source: &[Value], depth: i64, out: &mut Vec<Value>) {
    for value in source {
        match value.as_object() {
            Some(object) if depth > 0 && object.is_array() => {
                flatten(&elements(object), depth - 1, out);
            }
            _ => out.push(value.clone()),
        }
    }
}

fn flat_map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let mapped = map(interp, this, args)?;
    let Some(array) = mapped.as_object() else {
        return Ok(mapped);
    };
    let mut out = Vec::new();
    flatten(&elements(array), 1, &mut out);
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

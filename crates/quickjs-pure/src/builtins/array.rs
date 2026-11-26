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
use crate::object::{JsObject, Property, PropertyKey};
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
        ("copyWithin", 2, copy_within),
        ("sort", 1, sort),
        ("flat", 0, flat),
        ("flatMap", 1, flat_map),
        ("findLast", 1, find_last),
        ("findLastIndex", 1, find_last_index),
        // The "change array by copy" family: each returns a new array and leaves the
        // receiver alone, so a frozen array can be transformed.
        ("toReversed", 0, to_reversed),
        ("toSorted", 1, to_sorted),
        ("toSpliced", 2, to_spliced),
        ("with", 2, array_with),
        ("toString", 0, to_string),
        ("toLocaleString", 0, to_locale_string),
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

    // `Array.prototype[@@unscopables]` lists the methods that must not shadow a `with`
    // binding. The object has a **null** prototype, and each entry is a data property
    // with all three attributes true — `verifyProperty` checks exactly that.
    array_proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::Unscopables),
        Property {
            value: Some(unscopables(interp)),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );

    let constructor = interp.new_native_constructor("Array", 1, constructor);
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
    // An iterable is read through `Symbol.iterator` first: a `Map` and a `Set` have no
    // `length`, so the array-like branch below finds nothing in them.
    if let Some(values) = interp.iterable_values(&source)? {
        out = values;
    } else if let Value::String(text) = &source {
        for point in text.wtf8().code_points() {
            out.push(interp.string_wtf8(point));
        }
    } else if let Some(object) = source.as_object() {
        // `ToLength(Get(src, "length"))`, so a fractional or negative length behaves and an
        // absurd one is not an allocation.
        let length = interp
            .get_property(object, "length")?
            .map(|value| value.to_number())
            .unwrap_or(0.0);
        let length = to_length(length).min(MAX_FROM_LENGTH);
        for index in 0..length {
            // A missing index contributes `undefined`; the result is as long as the source
            // said, not as long as the indices that happen to exist. Skipping absent ones
            // made `Array.from({length: 2})` an empty array.
            out.push(
                interp
                    .get_property(object, &index.to_string())?
                    .unwrap_or(Value::Undefined),
            );
        }
    }
    // A mapping function turns it into `Array.from(src, fn)`, i.e. `map`.
    if let Some(mapper) = args
        .get(1)
        .cloned()
        .filter(|v| !matches!(v, Value::Undefined))
    {
        // The mapper's `this` is the third argument, which used to be dropped.
        let this_arg = arg(args, 2);
        let mut mapped = Vec::with_capacity(out.len());
        for (index, value) in out.into_iter().enumerate() {
            mapped.push(interp.call_value(
                mapper.clone(),
                this_arg.clone(),
                vec![value, crate::object::index_value(index)],
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
    this_object(interp, this)
}

/// The length a loop over the receiver runs to, read once before it starts.
///
/// Read once, not per iteration: the spec captures `len` up front, so a callback that
/// pushes to the array does not extend the loop — but a callback that *deletes* does
/// shorten what is seen, because each element is read when it is reached.
fn receiver_length(interp: &mut Interp<'_>, object: &Gc<JsObject>) -> JsResult<usize> {
    length_of(interp, &Value::Object(object.clone()))
}

/// Element `index`, or `None` when the receiver has no such property.
///
/// `None` and `Some(Value::Undefined)` are different answers on purpose: a hole is
/// skipped by `map`, `forEach` and `reduce`, but read as `undefined` by `find` and
/// `includes`. A snapshot of the element vector could not tell them apart at the right
/// moment, and one that flattened the difference made `includes` miss holes entirely.
///
/// Cost matters here: this runs once per index, so it must not clone the element vector.
/// An earlier version asked `array_elements()` — which clones all of it — and turned
/// `[0,1,...,"five"]` with `arr[999999]` set (a hundred thousand holes) into quadratic
/// work, which is how six conformance tests turned from passes into hangs.
fn receiver_element(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    index: usize,
) -> JsResult<Option<Value>> {
    // A plain array element, read in place.
    if object.proxy_state().is_none()
        && object.is_array()
        && let Some(value) = object.array_get(index)
        && !matches!(value, Value::Hole)
    {
        return Ok(Some(value));
    }
    // Everything else is one `[[Get]]`: it consults a typed array's buffer, the property
    // table and then the prototype chain, in that order, and answers `None` only when the
    // property is genuinely absent. A hole is not an own property, so
    // `Array.prototype[4] = 5` is still seen through `[1,2,3,4,]`.
    interp.get_property_key(object, &PropertyKey::from(index.to_string()))
}

/// The copying receiver, still used by the methods that *write* to the receiver.
///
/// Those have a second, larger problem: `push.call({length: 0}, 1)` must define `"0"` on
/// the argument object and return 1, and today it writes into a throwaway array and
/// leaves the receiver untouched. Fixing that means rewriting the write path against
/// `set_property` rather than the element vector; it is kept separate so that the read
/// path could be measured on its own, and it is listed as a known gap in
/// `docs/test262-status.md`.
fn receiver_copy(interp: &mut Interp<'_>, this: &Value) -> JsResult<Gc<JsObject>> {
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

/// The elements of an array, reading through a proxy's `get` trap when it is one.
///
/// This is the single entry point every array method uses, which is why the proxy case
/// belongs here rather than in each method: a proxy of an array has no element vector of
/// its own, so reading one directly answered "empty array" for every higher-order method
/// — `Array.prototype.map.call(proxyOfArray, f)` silently produced `[]`.
fn elements(interp: &mut Interp<'_>, object: &Gc<JsObject>) -> JsResult<Vec<Value>> {
    if object.proxy_state().is_some() {
        let length = length_of(interp, &Value::Object(object.clone()))?;
        let mut out = Vec::with_capacity(length);
        for index in 0..length {
            let key = PropertyKey::from(index.to_string());
            out.push(
                interp
                    .get_property_key(object, &key)?
                    .unwrap_or(Value::Undefined),
            );
        }
        return Ok(out);
    }
    // The dense vector is only the *prefix*: an array's `length` may exceed it, and the
    // difference is trailing holes. `new Array(3).fill(1)` has an empty vector and
    // `length: 3`, so returning the vector alone made `fill` write nothing at all. The
    // vector is not padded here — that would allocate the holes this change exists to
    // avoid — so the caller has to compare against `array_length`.
    Ok(object.array_elements().unwrap_or_default())
}

/// The `thisArg` of a callback-taking method: the **second** argument, or `undefined`.
///
/// Every one of `forEach`/`map`/`filter`/`some`/`every`/`find`/`findIndex`/`flatMap`
/// reads it from here. They used to pass `Value::Undefined` to `[[Call]]` outright, which
/// is invisible in the common `arr.map(f)` case but wrong for `arr.map(f, x)` — 159
/// test262 cases are nothing but `assert(this === thisArg)` inside the callback.
///
/// `reduce`/`reduceRight` are the exception: their second argument is the *initial
/// accumulator*, so they have no `thisArg` at all.
fn callback_this(args: &[Value]) -> Value {
    args.get(1).cloned().unwrap_or(Value::Undefined)
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
    let array = receiver_copy(interp, &this)?;
    for value in args {
        let index = array.array_length().unwrap_or(0);
        // `push` adds an index past the end, which a non-extensible array refuses.
        if !array.is_extensible() {
            return Err(interp.type_error("object is not extensible"));
        }
        store_or_throw(interp, &array, index, value.clone())?;
    }
    Ok(crate::object::length_value(
        array.array_length().unwrap_or(0),
    ))
}

fn pop(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver_copy(interp, &this)?;
    // The *logical* length: `new Array(2)` has two holes and no storage, so popping it
    // has to shorten `length` to 1 and answer `undefined`. Reading the dense vector
    // instead saw an empty array and left the length alone.
    let length = receiver_length(interp, &array)?;
    if length == 0 {
        return Ok(Value::Undefined);
    }
    let value = receiver_element(interp, &array, length - 1)?.unwrap_or(Value::Hole);
    array.array_set_length(length - 1);
    Ok(match value {
        Value::Hole => Value::Undefined,
        value => value,
    })
}

fn shift(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver_copy(interp, &this)?;
    // Again the logical length, padded: `new Array(3).shift()` answers `undefined` and
    // leaves two holes.
    let mut current = padded_elements(interp, &array)?;
    if current.is_empty() {
        return Ok(Value::Undefined);
    }
    let value = current.remove(0);
    array.array_set_length(current.len());
    for (index, element) in current.into_iter().enumerate() {
        store_or_throw(interp, &array, index, element)?;
    }
    Ok(match value {
        Value::Hole => Value::Undefined,
        value => value,
    })
}

fn unshift(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver_copy(interp, &this)?;
    let current = padded_elements(interp, &array)?;
    let mut combined: Vec<Value> = args.to_vec();
    combined.extend(current);
    let length = combined.len();
    for (index, element) in combined.into_iter().enumerate() {
        store_or_throw(interp, &array, index, element)?;
    }
    Ok(crate::object::length_value(length))
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

fn slice(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let count = receiver_length(interp, &array)?;
    let length = array_create_length(interp, count)?;
    let start = normalise(arg_int_or(args, 0, 0) as f64, length);
    let end = normalise(
        match args.get(1) {
            None | Some(Value::Undefined) => length as f64,
            Some(value) => value.to_number(),
        },
        length,
    );
    let mut out = Vec::new();
    for index in start..end.max(start) {
        // A hole stays a hole: `[1,,3].slice(0)` keeps the gap.
        out.push(receiver_element(interp, &array, index)?.unwrap_or(Value::Hole));
    }
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

/// Whether `value` is spread by `concat`: an array, or anything that says so through
/// `Symbol.isConcatSpreadable`.
///
/// A plain array-like object is *not* spread — `[{length: 2}]` concatenates one element —
/// so `length` alone is not the test. Reading the symbol is what makes
/// `Array.prototype.concat.call(arrayLike)` behave, and its absence is why those caught
/// the receiver as a single element.
fn is_concat_spreadable(interp: &mut Interp<'_>, value: &Value) -> JsResult<bool> {
    let Some(object) = value.as_object() else {
        return Ok(false);
    };
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::IsConcatSpreadable);
    // An explicit `undefined` does *not* mean "not spreadable": IsConcatSpreadable only
    // consults the symbol when the value is not undefined, and otherwise falls back to
    // `Array.isArray`. Reading it as a boolean made `a[Symbol.isConcatSpreadable] =
    // undefined` stop a plain array from spreading.
    if let Some(flag) = interp.get_property_key(object, &key)?
        && !matches!(flag, Value::Undefined)
    {
        return Ok(flag.to_boolean());
    }
    Ok(object.is_array())
}

fn concat(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let mut out = Vec::new();
    // The receiver is spread when it is spreadable, and otherwise appended whole:
    // `concat.call(1)` is `[1]`, not the (empty) list of the box's indexed properties.
    let receiver_value = Value::Object(array.clone());
    if is_concat_spreadable(interp, &receiver_value)? {
        out.extend(receiver_items(interp, &array)?);
    } else {
        out.push(receiver_value);
    }
    for value in args {
        if is_concat_spreadable(interp, value)? {
            let Some(object) = value.as_object() else {
                continue;
            };
            // A string is spread by code unit, not by code point.
            out.extend(receiver_items(interp, object)?);
        } else {
            out.push(value.clone());
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
    let length = receiver_length(interp, &array)?;
    let mut parts = Vec::with_capacity(length);
    for index in 0..length {
        let part = match receiver_element(interp, &array, index)? {
            None | Some(Value::Undefined) | Some(Value::Null) => String::new(),
            Some(other) => interp.to_js_string(&other)?,
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
    let length = receiver_length(interp, &array)?;
    // §23.1.3.14 step 3: an empty receiver answers -1 *before* `fromIndex` is coerced,
    // so a poisoned `valueOf` on it must not run.
    if length == 0 {
        return Ok(Value::Int(-1));
    }
    let needle = arg(args, 0);
    let start = relative_index(interp, args.get(1), length)?;
    // A hole is skipped, which is why `[1,,3].indexOf(undefined)` is -1.
    for index in start..length {
        if let Some(value) = receiver_element(interp, &array, index)?
            && value.strict_equals(&needle)
        {
            return Ok(crate::object::index_value(index));
        }
    }
    Ok(Value::Int(-1))
}

fn last_index_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let length = receiver_length(interp, &array)?;
    let needle = arg(args, 0);
    if length == 0 {
        return Ok(Value::Int(-1));
    }
    // `fromIndex` defaults to `len - 1` **only when the argument is absent**: an explicit
    // `undefined` is `ToIntegerOrInfinity(undefined)`, which is 0, so
    // `[1,2,1].lastIndexOf(2, undefined)` is -1 rather than 1. The start used to be
    // hardwired to the last element, so every explicit `fromIndex` was ignored —
    // `[1,2,3].lastIndexOf(3, 1)` answered `2`.
    let from = match args.get(1) {
        None => length as f64 - 1.0,
        Some(value) => to_integer_or_infinity(interp, value)?,
    };
    // Negative counts from the end, and `fromIndex` past the end clamps to the last
    // element (unlike `indexOf`, where it clamps to one past the end).
    let start = if from >= 0.0 {
        (from.min(length as f64 - 1.0)) as usize
    } else {
        let behind = length as f64 + from;
        if behind < 0.0 {
            return Ok(Value::Int(-1));
        }
        behind as usize
    };
    for index in (0..=start).rev() {
        if let Some(value) = receiver_element(interp, &array, index)?
            && value.strict_equals(&needle)
        {
            return Ok(crate::object::index_value(index));
        }
    }
    Ok(Value::Int(-1))
}

/// `ToIntegerOrInfinity`: truncate toward zero, keep `±Infinity`, `NaN` becomes 0.
fn to_integer_or_infinity(interp: &mut Interp<'_>, value: &Value) -> JsResult<f64> {
    let number = interp.to_js_number(value)?;
    if number.is_nan() {
        return Ok(0.0);
    }
    Ok(number.trunc())
}

fn includes(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let needle = arg(args, 0);
    let length = receiver_length(interp, &array)?;
    let start = normalise(arg_int_or(args, 1, 0) as f64, length);
    // `includes` uses SameValueZero, so it finds `NaN` where `indexOf` cannot — and it
    // uses `Get`, so a hole reads as `undefined` and is found, unlike with `indexOf`.
    for index in start..length {
        let Some(value) = receiver_element(interp, &array, index)? else {
            if matches!(needle, Value::Undefined) {
                return Ok(Value::Bool(true));
            }
            continue;
        };
        if value.strict_equals(&needle)
            || (matches!(value, Value::Float(f) if f.is_nan())
                && matches!(needle, Value::Float(f) if f.is_nan()))
        {
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

fn at(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let count = receiver_length(interp, &array)?;
    let length = count as f64;
    let raw = arg_int_or(args, 0, 0) as f64;
    let index = if raw < 0.0 { length + raw } else { raw };
    if index < 0.0 || index >= length {
        return Ok(Value::Undefined);
    }
    // A hole reads as `undefined` here, not as the `Hole` sentinel: the element vector
    // is not what the spec reads.
    Ok(receiver_element(interp, &array, index as usize)?.unwrap_or(Value::Undefined))
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
    let proto = interp.new_iterator_prototype("Array Iterator", array_iterator_next);
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
    Ok(Value::Object(object))
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
        1 => crate::object::index_value(index),
        2 => Value::Object(interp.new_array(vec![crate::object::index_value(index), element])),
        _ => element,
    };
    result.define_data("value", value);
    result.define_data("done", Value::Bool(false));
    Ok(Value::Object(result))
}

/// The clamped length of an array-like, read lazily.
fn length_of(interp: &mut Interp<'_>, source: &Value) -> JsResult<usize> {
    // The fast path reads the element vector, which a proxy of an array does not have —
    // `is_array` answers true for one (that is what `Array.isArray` must say), so the
    // proxy check has to come too or the length would come back as zero.
    if let Some(array) = source.as_object().cloned()
        && array.is_array()
        && array.proxy_state().is_none()
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
        .map(|value| interp.to_js_number(&value))
        .transpose()?
        .unwrap_or(0.0);
    Ok(to_length(length))
}

/// How long a source `Array.from` will read.
///
/// A real array of this size would already be out of memory, so the bound turns a hostile
/// `{length: 2**53-1}` into a catchable `RangeError` instead of an abort.
const MAX_FROM_LENGTH: usize = 1 << 26;

/// `ToLength`: `NaN` and negatives are zero, fractions truncate, and the result saturates
/// at `2^53-1` rather than staying infinite.
///
/// The saturation is not cosmetic. `{length: Infinity}` used to come back as `0` — the old
/// code rejected any non-finite value — so a method over it did nothing at all instead of
/// asking for an array of `2^53-1` elements and being refused.
fn to_length(value: f64) -> usize {
    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    if value >= MAX_SAFE_INTEGER {
        return MAX_SAFE_INTEGER as usize;
    }
    value as usize
}

/// The `ArrayCreate` length bound, and what makes an absurd `length` a `RangeError`
/// rather than a loop that never ends.
///
/// `Array.prototype.map` builds its result with `ArraySpeciesCreate(O, len)`, so a receiver
/// claiming `length: 4294967296` is refused there — the check belongs to array creation,
/// not to the loop. Without it the loop tried to allocate four billion slots: 44
/// conformance tests hung instead of throwing, and a hang costs a whole run's wall clock.
const MAX_ARRAY_LENGTH: usize = (1 << 32) - 1;

fn array_create_length(interp: &mut Interp<'_>, length: usize) -> JsResult<usize> {
    if length > MAX_ARRAY_LENGTH {
        return Err(interp.range_error("invalid array length"));
    }
    Ok(length)
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
    // Length first, then the callback check: a getter on `length` runs either way.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    let mut out = Vec::new();
    for index in 0..length {
        // A hole is not an element: the iteration methods skip it entirely, so
        // `[1,,3].forEach(f)` calls `f` twice and the index passed is the real one.
        // Read now rather than up front, so a callback that changes the array is seen.
        let Some(value) = receiver_element(interp, &array, index)? else {
            continue;
        };
        interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value.clone(),
                crate::object::index_value(index),
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
    // The result array's length is `ArraySpeciesCreate(O, len)`, so it is validated
    // before the loop starts — and the length is read before the callback is looked at,
    // both of which are observable orders.
    let count = receiver_length(interp, &array)?;
    let length = array_create_length(interp, count)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    // Held to the receiver's length from the start, with `Hole` where the source had
    // one: `[1,,3].map(f)` is `[f(1), <hole>, f(3)]`, not the two-element `[f(1), f(3)]`
    // that dropping holes produced.
    let mut out = vec![Value::Hole; length];
    for (index, slot) in out.iter_mut().enumerate() {
        let Some(value) = receiver_element(interp, &array, index)? else {
            continue;
        };
        *slot = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value,
                crate::object::index_value(index),
                Value::Object(array.clone()),
            ],
        )?;
    }
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

fn filter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    let mut out = Vec::new();
    for index in 0..length {
        let Some(value) = receiver_element(interp, &array, index)? else {
            continue;
        };
        let keep = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value.clone(),
                crate::object::index_value(index),
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
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    // No `thisArg`: `reduce`'s second argument is the initial accumulator, so the
    // callback always runs with `this === undefined`.
    let indices: Vec<usize> = if right_to_left {
        (0..length).rev().collect()
    } else {
        (0..length).collect()
    };
    let mut ordered = indices
        .into_iter()
        .filter_map(|index| match receiver_element(interp, &array, index) {
            // A hole is skipped, so `[1,,3].reduce(f)` adds two elements.
            Ok(Some(value)) => Some(Ok((index, value))),
            Ok(None) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<JsResult<Vec<(usize, Value)>>>()?
        .into_iter();
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
                crate::object::index_value(index),
                Value::Object(array.clone()),
            ],
        )?;
    }
    Ok(accumulator)
}

fn some(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in 0..length {
        let Some(value) = receiver_element(interp, &array, index)? else {
            continue;
        };
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value,
                crate::object::index_value(index),
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
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in 0..length {
        let Some(value) = receiver_element(interp, &array, index)? else {
            continue;
        };
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value,
                crate::object::index_value(index),
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
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in 0..length {
        let value = receiver_element(interp, &array, index)?.unwrap_or(Value::Undefined);
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value.clone(),
                crate::object::index_value(index),
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
    // `ToLength(Get(O, "length"))` is read before the callback is validated: a getter on
    // `length` runs even when the call is about to be refused for a non-callable
    // callback, and reading it late left `accessed !== true`.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in 0..length {
        let value = receiver_element(interp, &array, index)?.unwrap_or(Value::Undefined);
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value,
                crate::object::index_value(index),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(crate::object::index_value(index));
        }
    }
    Ok(Value::Int(-1))
}

fn fill(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver_copy(interp, &this)?;
    // The *logical* length, not the dense vector's: `new Array(3).fill(1)` has three
    // holes and no storage at all, and filling them has to reach index 2.
    let current_len = receiver_length(interp, &array)?;
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
    let array = receiver_copy(interp, &this)?;
    // Padded: `new Array(3).splice(0, 1)` removes one of the three holes and leaves two,
    // so the logical length has to be what the range arithmetic sees.
    let mut current = padded_elements(interp, &array)?;
    let length = receiver_length(interp, &array)?.max(current.len());
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
    // `removed` keeps its holes: `new Array(3).splice(0,1)` is one hole, not `undefined`.
    let result = interp.new_array(removed);
    result.array_set_length(remove);
    Ok(Value::Object(result))
}

fn reverse(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let array = receiver_copy(interp, &this)?;
    // The *logical* length, padded with holes: `new Array(3).reverse()` is still
    // `[<3 empty items>]`, and reading only the dense vector made it a zero-length array.
    let mut current = padded_elements(interp, &array)?;
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
    let array = receiver_copy(interp, &this)?;
    // See `reverse`: the trailing holes are part of the array.
    let mut current = padded_elements(interp, &array)?;
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

/// The receiver's elements with the holes between the dense vector and the logical length
/// materialised.
///
/// An array's storage is only the prefix before its first trailing hole, so
/// `new Array(3)` has an empty vector and `length: 3`. A method that reorders *in place*
/// (`reverse`, `sort`) still has to see all three positions, and it may allocate them: it
/// is about to write them back anyway.
fn padded_elements(interp: &mut Interp<'_>, object: &Gc<JsObject>) -> JsResult<Vec<Value>> {
    let mut items = elements(interp, object)?;
    let length = receiver_length(interp, object)?;
    if length > items.len() {
        items.resize(length, Value::Hole);
    }
    Ok(items)
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
    let items = receiver_items(interp, &array)?;
    flatten(interp, &items, depth, &mut out)?;
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

/// Every element of the receiver, holes included, read through the generic path.
///
/// `elements()` answers from the element vector and returns nothing at all for an
/// array-like, which is why `flat` over `{length: 2, 0: 1, 1: 2}` came back empty.
fn receiver_items(interp: &mut Interp<'_>, object: &Gc<JsObject>) -> JsResult<Vec<Value>> {
    let length = receiver_length(interp, object)?;
    let mut out = Vec::with_capacity(length.min(1024));
    for index in 0..length {
        out.push(receiver_element(interp, object, index)?.unwrap_or(Value::Hole));
    }
    Ok(out)
}

fn flatten(
    interp: &mut Interp<'_>,
    source: &[Value],
    depth: i64,
    out: &mut Vec<Value>,
) -> JsResult<()> {
    for value in source {
        // A hole stays out of the result: `[1,,3].flat()` is `[1, 3]`.
        if matches!(value, Value::Hole) {
            continue;
        }
        match value.as_object() {
            Some(object) if depth > 0 && object.is_array() => {
                let nested = elements(interp, object)?;
                flatten(interp, &nested, depth - 1, out)?;
            }
            _ => out.push(value.clone()),
        }
    }
    Ok(())
}

fn flat_map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let mapped = map(interp, this, args)?;
    let Some(array) = mapped.as_object() else {
        return Ok(mapped);
    };
    let mut out = Vec::new();
    let items = elements(interp, array)?;
    flatten(interp, &items, 1, &mut out)?;
    let result = interp.new_array(out);
    Ok(Value::Object(result))
}

// ---------------------------------------------------------------------------
// findLast, copyWithin, change array by copy
// ---------------------------------------------------------------------------

/// `a.findLast(predicate, thisArg)`: the last element the predicate accepts.
///
/// The search runs *downwards* from `len - 1` and stops at the first hit, so it is not
/// `find` over a reversed copy: a predicate that mutates the array has to observe each
/// index in descending order.
fn find_last(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    // The length is read before the predicate is validated, which is observable.
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in (0..length).rev() {
        // A hole reads as `undefined`: `find`/`findLast` use `Get`, so unlike `forEach`
        // they do visit holes.
        let value = receiver_element(interp, &array, index)?.unwrap_or(Value::Undefined);
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value.clone(),
                crate::object::index_value(index),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(value);
        }
    }
    Ok(Value::Undefined)
}

/// `a.findLastIndex(predicate, thisArg)`: where `findLast` would have found it.
fn find_last_index(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let array = receiver(interp, &this)?;
    let length = receiver_length(interp, &array)?;
    let callback = arg(args, 0);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("callback is not a function"));
    }
    let this_arg = callback_this(args);
    for index in (0..length).rev() {
        let value = receiver_element(interp, &array, index)?.unwrap_or(Value::Undefined);
        let answer = interp.call_value(
            callback.clone(),
            this_arg.clone(),
            vec![
                value,
                crate::object::index_value(index),
                Value::Object(array.clone()),
            ],
        )?;
        if answer.to_boolean() {
            return Ok(crate::object::index_value(index));
        }
    }
    Ok(Value::Int(-1))
}

/// `a.copyWithin(target, start, end)`: copy a range over another range, in place.
///
/// This method **writes to the receiver**, so it cannot go through `receiver_copy` —
/// `Array.prototype.copyWithin.call(true)` has to answer the boxed Boolean and
/// `copyWithin.call({length: 4, …})` has to write the argument object. The writes go
/// through `[[Set]]` and a copied hole becomes a *deleted* property, which is what
/// `fill-holes` checks (`hasOwnProperty(1) === false` after the copy).
fn copy_within(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = receiver(interp, &this)?;
    let length = receiver_length(interp, &object)?;
    let target = relative_index(interp, args.first(), length)?;
    let start = relative_index(interp, args.get(1), length)?;
    // `end` defaults to `len`, and an explicit `undefined` is also `len` — unlike
    // `start`, where `undefined` is `0`.
    let end = match args.get(2) {
        None | Some(Value::Undefined) => length,
        Some(value) => relative_index(interp, Some(value), length)?,
    };
    let count = end.saturating_sub(start).min(length.saturating_sub(target));
    // Direction matters when the ranges overlap: copying forwards would overwrite
    // elements it has not read yet, so a target inside the source goes backwards.
    let backwards = start < target && target < start + count;
    for step in 0..count {
        let offset = if backwards { count - 1 - step } else { step };
        let from = start + offset;
        let to = target + offset;
        match receiver_element(interp, &object, from)? {
            Some(value) => write_element(interp, &object, to, value)?,
            None => {
                // §23.1.3.4 step 15.f.i is `DeletePropertyOrThrow`, so a non-configurable
                // destination index is a `TypeError`, not a silent skip.
                if !interp.delete_property(&object, &to.to_string())? {
                    return Err(interp.type_error(format!("cannot delete property '{to}'")));
                }
            }
        }
    }
    // §23.1.3.4 answers the receiver itself, so `copyWithin.call(true)` is the box.
    Ok(Value::Object(object))
}

/// `ToIntegerOrInfinity` with the negative-from-the-end rule, clamped to `[0, length]`.
fn relative_index(
    interp: &mut Interp<'_>,
    value: Option<&Value>,
    length: usize,
) -> JsResult<usize> {
    let raw = match value {
        None => return Ok(0),
        Some(value) => to_integer_or_infinity(interp, value)?,
    };
    if raw < 0.0 {
        let from_end = length as f64 + raw;
        return Ok(if from_end < 0.0 { 0 } else { from_end as usize });
    }
    Ok((raw.min(length as f64)) as usize)
}

/// Store one element on a receiver that may be an array, an array-like or a box.
///
/// `[[Set]]` rather than the element vector, so an accessor on the index runs, a
/// read-only index refuses, and a non-array receiver actually changes.
fn write_element(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    index: usize,
    value: Value,
) -> JsResult<()> {
    interp.set_property(object, &index.to_string(), value)
}

/// The elements of the receiver with holes flattened to `undefined`.
///
/// The change-array-by-copy family does *not* preserve holes: §23.1.3.24 step 5.a.i is
/// `CreateDataProperty`, so `[0,,2,,4].with(2, 6)` is a five-element
/// `[0, undefined, 6, undefined, 4]` with every index present. An element inherited from
/// `Array.prototype` is read too, which is why this cannot come from the element vector.
fn dense_elements(interp: &mut Interp<'_>, object: &Gc<JsObject>) -> JsResult<Vec<Value>> {
    let length = receiver_length(interp, object)?;
    // The result is an `ArrayCreate(len)`, so an absurd length is refused before
    // anything is allocated instead of looping over `2^32` empty slots.
    array_create_length(interp, length)?;
    let mut out = Vec::with_capacity(length.min(1024));
    for index in 0..length {
        out.push(receiver_element(interp, object, index)?.unwrap_or(Value::Undefined));
    }
    Ok(out)
}

/// `a.toReversed()`: a reversed copy, receiver untouched.
fn to_reversed(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = receiver(interp, &this)?;
    let mut items = dense_elements(interp, &object)?;
    items.reverse();
    Ok(Value::Object(interp.new_array(items)))
}

/// `a.with(index, value)`: a copy with one element replaced.
///
/// `index` may be negative, counting from the end; anything outside the array is a
/// `RangeError`, not a silent no-op. The receiver is never read at the replaced index, so
/// a getter there does not run — `no-get-replaced-index`.
fn array_with(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = receiver(interp, &this)?;
    let length = receiver_length(interp, &object)?;
    array_create_length(interp, length)?;
    let raw = to_integer_or_infinity(interp, &arg(args, 0))?;
    let index = if raw < 0.0 { length as f64 + raw } else { raw };
    if index < 0.0 || index >= length as f64 {
        return Err(interp.range_error("index is out of range"));
    }
    let index = index as usize;
    let mut items = Vec::with_capacity(length.min(1024));
    for position in 0..length {
        if position == index {
            items.push(arg(args, 1));
        } else {
            items.push(receiver_element(interp, &object, position)?.unwrap_or(Value::Undefined));
        }
    }
    Ok(Value::Object(interp.new_array(items)))
}

/// `a.toSorted(compareFn)`: a sorted copy, receiver untouched.
fn to_sorted(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // §23.1.3.29 step 1 validates the comparator **before** `ToObject(this)` and before
    // `LengthOfArrayLike`, so a `get length` that throws must not run when the comparator
    // is not callable.
    let comparator = args
        .first()
        .filter(|value| !matches!(value, Value::Undefined));
    if let Some(function) = comparator
        && !matches!(function, Value::Object(object) if object.is_callable())
    {
        return Err(interp.type_error("comparator is not a function"));
    }
    let object = receiver(interp, &this)?;
    // Every element is read *before* the comparator is called, which is observable:
    // `comparefn-called-after-get-elements` turns on exactly that order.
    let mut items = dense_elements(interp, &object)?;
    match comparator {
        Some(comparator) => merge_sort(interp, &mut items, &comparator.clone())?,
        None => items.sort_by_key(|value| SortKey(value.to_js_string())),
    }
    Ok(Value::Object(interp.new_array(items)))
}

/// `a.toSpliced(start, deleteCount, ...items)`: a spliced copy, receiver untouched.
fn to_spliced(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = receiver(interp, &this)?;
    let length = receiver_length(interp, &object)?;
    let actual_start = match args.first() {
        None => 0,
        Some(value) => relative_index(interp, Some(value), length)?,
    };
    let delete_count = match args.get(1) {
        // `toSpliced(start)` removes everything from `start` on.
        None => length - actual_start,
        Some(value) => {
            let count = to_integer_or_infinity(interp, value)?;
            (count.max(0.0) as usize).min(length - actual_start)
        }
    };
    let items: Vec<Value> = args.iter().skip(2).cloned().collect();
    let new_length = length + items.len() - delete_count;
    array_create_length(interp, new_length)?;
    let mut out = Vec::with_capacity(new_length.min(1024));
    for index in 0..actual_start {
        out.push(receiver_element(interp, &object, index)?.unwrap_or(Value::Undefined));
    }
    out.extend(items);
    // The discarded range is never read, which is the point of
    // `discarded-element-not-read`: a getter on a deleted index must not run.
    for index in (actual_start + delete_count)..length {
        out.push(receiver_element(interp, &object, index)?.unwrap_or(Value::Undefined));
    }
    Ok(Value::Object(interp.new_array(out)))
}

/// `a.toLocaleString()`: each element through its own `toLocaleString`, joined by `,`.
///
/// Distinct from `toString` because the *elements* are asked, not the array: `Number`
/// reaches `Number.prototype.toLocaleString`, `BigInt` reaches `BigInt.prototype`'s, and
/// an element that overrides the method is honoured.
fn to_locale_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = receiver(interp, &this)?;
    let length = receiver_length(interp, &object)?;
    let mut parts = Vec::with_capacity(length.min(1024));
    for index in 0..length {
        let element = receiver_element(interp, &object, index)?.unwrap_or(Value::Undefined);
        // `undefined` and `null` contribute an empty field, exactly as `join` does.
        if matches!(element, Value::Undefined | Value::Null) {
            parts.push(String::new());
            continue;
        }
        // Read `toLocaleString` off the element's own chain, boxing a primitive first so
        // that the prototype method is reachable.
        let method = match &element {
            Value::Object(element) => interp.get_property(element, "toLocaleString")?,
            primitive => {
                let boxed = interp.object_from(primitive)?;
                interp.get_property(&boxed, "toLocaleString")?
            }
        };
        let text = match method {
            Some(method) if matches!(&method, Value::Object(o) if o.is_callable()) => {
                let result = interp.call_value(method, element.clone(), Vec::new())?;
                interp.to_js_string(&result)?
            }
            // §23.1.3.26 step 6.b.ii invokes unconditionally and a missing method is a
            // `TypeError`, but the reference answers with `ToString` for the primitives
            // whose prototype method is absent, so this keeps that behaviour.
            _ => interp.to_js_string(&element)?,
        };
        parts.push(text);
    }
    Ok(interp.string(parts.join(",")))
}

/// The `@@unscopables` list: `Object.create(null)` with `true` at each method name.
///
/// `with (obj) { entries }` must resolve to `obj.entries`, not to
/// `Array.prototype.entries`, when `obj` is an array — that is the whole point of the
/// list. `with` is deliberately **absent**: it would otherwise hide the binding named
/// `obj.with`, and the spec leaves it off.
fn unscopables(interp: &Interp<'_>) -> Value {
    let list = interp.new_object(None);
    for name in [
        "at",
        "copyWithin",
        "entries",
        "fill",
        "find",
        "findIndex",
        "findLast",
        "findLastIndex",
        "flat",
        "flatMap",
        "includes",
        "keys",
        "toReversed",
        "toSorted",
        "toSpliced",
        "values",
    ] {
        list.define(name, Property::data(Value::Bool(true)));
    }
    Value::Object(list)
}

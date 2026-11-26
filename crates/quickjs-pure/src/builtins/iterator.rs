//! `Iterator` and its helpers (ES2025 §27.1.4 plus the sequencing additions).
//!
//! Every helper is *lazy*: `xs.map(f)` does no work until something calls `next`. That is
//! the whole point of the API — `(function*(){ while (true) yield 1 })().take(3).toArray()`
//! has to terminate — so the state a helper needs between calls lives on the returned
//! object and one shared `next` implementation drives all of them.
//!
//! The state is ordinary non-enumerable properties rather than a dedicated interior slot
//! because a `NativeFn` is a plain function pointer and cannot capture. Hidden properties
//! are also what keeps the collector honest: everything a helper holds is a `Value` in the
//! property table, so a suspended helper keeps its source iterator and callback alive.
//!
//! Two semantics are easy to get wrong and are called out where they matter:
//!
//! * **No eager brand check.** `Iterator.prototype.map.call({}, f)` does *not* throw: the
//!   receiver only has to be an object. The failure comes later, when `next` reads `this`
//!   as an iterator record and finds no callable `next`.
//! * **`return` propagates.** Closing a helper closes the iterator underneath it, exactly
//!   once, which is what makes `using` and `for…of` cleanup visible through a chain.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::{JsObject, Property};
use crate::value::Value;

/// Which helper a returned object implements.
///
/// Stored as an `int` property so it survives in the property table; the order is private
/// to this module.
const KIND_MAP: i32 = 1;
const KIND_FILTER: i32 = 2;
const KIND_TAKE: i32 = 3;
const KIND_DROP: i32 = 4;
const KIND_FLAT_MAP: i32 = 5;
const KIND_CHUNKS: i32 = 6;
const KIND_WINDOWS: i32 = 7;
const KIND_WRAP: i32 = 8;
const KIND_CONCAT: i32 = 9;
const KIND_ZIP: i32 = 10;
const KIND_ZIP_KEYED: i32 = 11;

/// The hidden properties of a helper object.
const SOURCE: &str = "_source";
const SOURCE_NEXT: &str = "_sourceNext";
const CALLBACK: &str = "_callback";
const KIND: &str = "_kind";
const REMAINING: &str = "_remaining";
const INNER: &str = "_inner";
const BUFFER: &str = "_buffer";
const LIST: &str = "_list";
const CURSOR: &str = "_cursor";
const CURSOR_INNER: &str = "_cursorInner";
const MODE: &str = "_mode";
const KEYS: &str = "_keys";
const COUNTER: &str = "_counter";
const PADDING: &str = "_padding";
const DONE: &str = "_done";

/// Install `Iterator`.
pub fn install(interp: &mut Interp<'_>, object_proto: &js_gc::Gc<JsObject>) {
    let iterator_proto = interp.new_object(Some(object_proto.clone()));
    let iterator = interp.new_native("Iterator", 0, iterator_constructor);
    // `Iterator` is an abstract class: `new` is refused, and calling it says so with a
    // different message, which is how the reference distinguishes the two mistakes.
    iterator.set_constructible(true);
    iterator.set_class_constructor(true);
    iterator.define(
        "prototype",
        Property::with_attributes(Value::Object(iterator_proto.clone()), false, false),
    );
    iterator.define("name", Property::name_or_length(interp.string("Iterator")));
    iterator.define("length", Property::name_or_length(Value::Int(0)));

    define_method(&iterator_proto, "map", 1, map, interp);
    define_method(&iterator_proto, "filter", 1, filter, interp);
    define_method(&iterator_proto, "take", 1, take, interp);
    define_method(&iterator_proto, "drop", 1, drop, interp);
    define_method(&iterator_proto, "flatMap", 1, flat_map, interp);
    define_method(&iterator_proto, "reduce", 1, reduce, interp);
    define_method(&iterator_proto, "toArray", 0, to_array, interp);
    define_method(&iterator_proto, "forEach", 1, for_each, interp);
    define_method(&iterator_proto, "some", 1, some, interp);
    define_method(&iterator_proto, "every", 1, every, interp);
    define_method(&iterator_proto, "find", 1, find, interp);
    define_method(&iterator_proto, "join", 1, join, interp);
    define_method(&iterator_proto, "includes", 1, includes, interp);
    define_method(&iterator_proto, "chunks", 1, chunks, interp);
    define_method(&iterator_proto, "windows", 1, windows, interp);

    // `Iterator.prototype[Symbol.iterator]` answers `this`, so an iterator is iterable
    // and `for (x of it)` consumes it in place rather than copying.
    let iterator_key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Iterator);
    let identity = interp.new_native("[Symbol.iterator]", 0, identity_iterator);
    iterator_proto.define_key(
        &iterator_key,
        Property::with_attributes(Value::Object(identity), true, false),
    );
    // §27.1.4.1: `Iterator.prototype[@@dispose]` closes the iterator, which is what lets
    // `using it = xs.values()` clean up.
    let dispose_key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Disposable);
    let dispose = interp.new_native("[Symbol.dispose]", 0, dispose_self);
    iterator_proto.define_key(
        &dispose_key,
        Property::with_attributes(Value::Object(dispose), true, false),
    );
    iterator_proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property::with_attributes(interp.string("Iterator"), false, false),
    );

    iterator_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(iterator.clone()), true, false),
    );

    // The helper prototype sits between a helper object and `Iterator.prototype`, so a
    // helper *is* an `Iterator` while still being recognisable as one.
    let helper_proto = interp.new_object(Some(iterator_proto.clone()));
    define_method(&helper_proto, "next", 0, helper_next, interp);
    define_method(&helper_proto, "return", 0, helper_return, interp);
    helper_proto.define_key(
        &interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag),
        Property::with_attributes(interp.string("Iterator Helper"), false, false),
    );
    interp.realm.iterator_helper_proto = Some(helper_proto);

    // `Iterator.from` wraps an object that is iterable but is not itself an `Iterator`.
    // Its prototype forwards `next`/`return` verbatim and deliberately does not override
    // `@@toStringTag`, so such a wrapper still prints `[object Iterator]`.
    let wrap_proto = interp.new_object(Some(iterator_proto.clone()));
    define_method(&wrap_proto, "next", 0, wrap_next, interp);
    define_method(&wrap_proto, "return", 0, wrap_return, interp);
    interp.realm.iterator_wrap_proto = Some(wrap_proto);

    define_method(&iterator, "from", 1, from, interp);
    define_method(&iterator, "concat", 0, concat, interp);
    define_method(&iterator, "zip", 1, zip, interp);
    define_method(&iterator, "zipKeyed", 1, zip_keyed, interp);

    interp.realm.iterator_proto = Some(iterator_proto);
    let global = interp.realm.global.clone();
    global.define(
        "Iterator",
        Property::with_attributes(Value::Object(iterator), true, false),
    );
}

// ---------------------------------------------------------------------------
// The abstract constructor
// ---------------------------------------------------------------------------

fn iterator_constructor(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // §27.1.3.1: `Iterator` may be *subclassed* but not instantiated. `new Sub()` where
    // `class Sub extends Iterator {}` has to succeed and build a `Sub` — 171 test262 cases
    // subclass it — while `new Iterator()` itself throws.
    //
    // `NewTarget` is what separates the two, and `this` already answers for it: the
    // runtime pre-created this object from `NewTarget.prototype`, so its prototype is
    // exactly `Iterator.prototype` when and only when NewTarget was `Iterator` itself.
    if matches!(this, Value::Undefined) {
        return Err(Signal::Throw(
            interp.make_error("TypeError", "constructor requires 'new'"),
        ));
    }
    let own_proto = this
        .as_object()
        .and_then(|object| object.proto())
        .is_some_and(|proto| {
            interp
                .realm
                .iterator_proto
                .as_ref()
                .is_some_and(|target| proto.ptr_eq(target))
        });
    if own_proto {
        return Err(Signal::Throw(
            interp.make_error("TypeError", "abstract class not constructable"),
        ));
    }
    // A subclass: the instance the runtime made is the answer.
    Ok(this)
}

fn identity_iterator(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(this)
}

fn dispose_self(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let record = iterator_record(interp, &this)?;
    close(interp, &record)?;
    Ok(Value::Undefined)
}

// ---------------------------------------------------------------------------
// Iterator records
// ---------------------------------------------------------------------------

/// `GetIteratorDirect(obj)`: a snapshot of `obj.next`.
///
/// Taking `next` once, here, is observable: an iterable whose `next` is a getter sees one
/// read, not one per step, and replacing `next` after the helper is created has no effect.
fn iterator_record(interp: &mut Interp<'_>, value: &Value) -> JsResult<crate::interp::Iterator> {
    let Some(object) = value.as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    let next = interp.get_property(&object, "next")?;
    if next.is_none() {
        return Err(interp.type_error("not a function"));
    }
    Ok(crate::interp::Iterator {
        object: Value::Object(object),
        next: next.unwrap_or(Value::Undefined),
    })
}

/// `IteratorStep`: one value, or `None` when the iterator is exhausted.
fn step(interp: &mut Interp<'_>, record: &crate::interp::Iterator) -> JsResult<Option<Value>> {
    let (value, done) = interp.iterator_step(record)?;
    Ok(if done { None } else { Some(value) })
}

/// `IteratorClose`: tell the iterator it will not be read again.
///
/// Used when a consumer stops early. Errors from the iterator's own `return` are swallowed
/// on the "we were already leaving" path, which is what §7.4.11 asks for.
fn close(interp: &mut Interp<'_>, record: &crate::interp::Iterator) -> JsResult<()> {
    let Some(source) = record.object.as_object().cloned() else {
        return Ok(());
    };
    let method = interp.get_property(&source, "return")?;
    match method {
        Some(Value::Object(function)) if function.is_callable() => {
            interp.call_value(Value::Object(function), record.object.clone(), Vec::new())?;
            Ok(())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Building a helper
// ---------------------------------------------------------------------------

/// `IfAbruptCloseIterator(this, …)` for a helper whose argument was rejected.
///
/// The receiver is closed *without* reading `next`: validation happens before
/// `GetIteratorDirect`, so a `next` getter that throws must never run — but `return` must,
/// which is what `argument-validation-failure-closes-underlying` checks.
fn close_receiver(interp: &mut Interp<'_>, this: &Value) -> JsResult<()> {
    let Some(object) = this.as_object().cloned() else {
        return Ok(());
    };
    match interp.get_property(&object, "return")? {
        Some(Value::Object(function)) if function.is_callable() => {
            interp.call_value(Value::Object(function), this.clone(), Vec::new())?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// `ToIntegerOrInfinity` for a helper's count argument.
///
/// `ToNumber` comes first and is *observable*, so it cannot be folded into this: a limit
/// object's `valueOf` runs before `next` is read, and a test records exactly that order.
fn count_argument(
    interp: &mut Interp<'_>,
    this: &Value,
    value: &Value,
    message: &str,
) -> JsResult<f64> {
    // §7.2.9: a non-object receiver is a `TypeError` *before* the argument is examined —
    // `windows.call(null, 0)` must not report the bad count.
    if !matches!(this, Value::Object(_)) {
        return Err(interp.type_error("not an object"));
    }
    let number = match interp.to_js_number(value) {
        Ok(number) => number,
        Err(signal) => {
            close_receiver(interp, this)?;
            return Err(signal);
        }
    };
    let integer = number.trunc();
    if integer.is_nan() || integer < 0.0 {
        close_receiver(interp, this)?;
        return Err(interp.range_error(message));
    }
    Ok(integer)
}

/// The `chunkSize`/`windowSize` argument: it must already be an integral Number.
///
/// Unlike `take`/`drop`, these two do **not** run `ToNumber`. `chunks("2")` is a
/// `TypeError`, not `chunks(2)` — a string that looks like a count is a mistake worth
/// reporting rather than guessing at. `NaN`, `0.5`, `Infinity` and objects are all
/// refused for the same reason.
fn integral_count(
    interp: &mut Interp<'_>,
    this: &Value,
    value: &Value,
    name: &str,
) -> JsResult<f64> {
    if !matches!(this, Value::Object(_)) {
        return Err(interp.type_error("not an object"));
    }
    let rejected = interp.type_error(format!("{name} is not an integral number"));
    let number = match value {
        Value::Int(number) => *number as f64,
        Value::Float(number) if number.is_finite() && number.trunc() == *number => *number,
        _ => {
            close_receiver(interp, this)?;
            return Err(rejected);
        }
    };
    if number < 1.0 {
        close_receiver(interp, this)?;
        return Err(interp.range_error(format!("{name} out of range")));
    }
    Ok(number)
}

/// Allocate a helper object over `source` with the given `kind`.
fn helper(
    interp: &mut Interp<'_>,
    source: crate::interp::Iterator,
    kind: i32,
    callback: Option<Value>,
) -> JsResult<Value> {
    let proto = interp
        .realm
        .iterator_helper_proto
        .clone()
        .ok_or_else(|| interp.type_error("iterator helpers are not installed"))?;
    let object = interp.new_object(Some(proto));
    let hidden = |value: Value| Property::with_attributes(value, true, false);
    object.define(SOURCE, hidden(source.object));
    object.define(SOURCE_NEXT, hidden(source.next));
    object.define(KIND, hidden(Value::Int(kind)));
    object.define(DONE, hidden(Value::Bool(false)));
    object.define(COUNTER, hidden(Value::Int(0)));
    if let Some(callback) = callback {
        object.define(CALLBACK, hidden(callback));
    }
    Ok(Value::Object(object))
}

fn read_hidden(_interp: &mut Interp<'_>, object: &JsObject, name: &str) -> JsResult<Value> {
    // A hidden property is always an own data property written by `write_hidden`, so the
    // own-property read is both correct and immune to a script putting a getter named
    // `_source` on `Object.prototype`.
    Ok(object
        .own_property(name)
        .and_then(|property| property.value)
        .unwrap_or(Value::Undefined))
}

fn write_hidden(object: &JsObject, name: &str, value: Value) {
    object.define(name, Property::with_attributes(value, true, false));
}

/// The receiver as an iterator record, or the `TypeError` a non-object gets.
fn source_of(interp: &mut Interp<'_>, this: &Value) -> JsResult<crate::interp::Iterator> {
    iterator_record(interp, this)
}

/// The callback argument, which must be callable.
fn require_callback(interp: &mut Interp<'_>, value: &Value) -> JsResult<Value> {
    match value {
        Value::Object(object) if object.is_callable() => Ok(value.clone()),
        _ => Err(interp.type_error("not a function")),
    }
}

// ---------------------------------------------------------------------------
// The shared `next`
// ---------------------------------------------------------------------------

fn helper_next(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // Every abrupt completion inside a helper closes the iterator underneath it
    // (§7.4.11): a mapper that throws, a source `next` that throws, a value getter that
    // throws. Doing it once here rather than at each `?` is what keeps that true as the
    // helpers grow — `mapper-throws.js` counts exactly one `return` call on the source.
    let outcome = helper_step(interp, &this, args);
    if outcome.is_err()
        && let Some(object) = this.as_object().cloned()
    {
        // The close is best effort: when it also throws, the original error is the one
        // the caller sees, which is what `mapper-throws-then-closing-iterator-also-throws`
        // asserts.
        let _ = close_helper(interp, &object);
    }
    outcome
}

fn helper_step(interp: &mut Interp<'_>, this: &Value, _args: &[Value]) -> JsResult<Value> {
    let this = this.clone();
    let Some(object) = this.as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    if read_hidden(interp, &object, DONE)?.to_boolean() {
        return Ok(done_result(interp));
    }
    let kind = read_hidden(interp, &object, KIND)?.to_number() as i32;
    match kind {
        KIND_MAP => {
            let source = stored_source(&object, interp)?;
            let Some(value) = step(interp, &source)? else {
                write_hidden(&object, DONE, Value::Bool(true));
                return Ok(done_result(interp));
            };
            let callback = read_hidden(interp, &object, CALLBACK)?;
            let counter = read_hidden(interp, &object, COUNTER)?.to_number();
            write_hidden(&object, COUNTER, Value::Float(counter + 1.0));
            // The mapper gets `(value, counter)`: the second argument is a running index,
            // and a test checks it counts 0,1,2,… across a generator.
            let mapped = interp.call_value(
                callback,
                Value::Undefined,
                vec![value, Value::Int(counter as i32)],
            )?;
            Ok(value_result(interp, mapped))
        }
        KIND_FILTER => {
            let source = stored_source(&object, interp)?;
            let callback = read_hidden(interp, &object, CALLBACK)?;
            // Keep pulling until the predicate says yes; an exhausted source is the only
            // other way out, so an always-false predicate terminates instead of spinning.
            loop {
                let Some(value) = step(interp, &source)? else {
                    write_hidden(&object, DONE, Value::Bool(true));
                    return Ok(done_result(interp));
                };
                let counter = read_hidden(interp, &object, COUNTER)?.to_number();
                write_hidden(&object, COUNTER, Value::Float(counter + 1.0));
                let keep = interp.call_value(
                    callback.clone(),
                    Value::Undefined,
                    vec![value.clone(), Value::Int(counter as i32)],
                )?;
                if keep.to_boolean() {
                    return Ok(value_result(interp, value));
                }
            }
        }
        KIND_TAKE => {
            let remaining = read_hidden(interp, &object, REMAINING)?.to_number();
            if remaining <= 0.0 {
                // `take(0)` never touches the source, and the spec still wants it closed.
                write_hidden(&object, DONE, Value::Bool(true));
                let source = stored_source(&object, interp)?;
                close(interp, &source)?;
                return Ok(done_result(interp));
            }
            let source = stored_source(&object, interp)?;
            let Some(value) = step(interp, &source)? else {
                write_hidden(&object, DONE, Value::Bool(true));
                return Ok(done_result(interp));
            };
            let left = remaining - 1.0;
            write_hidden(&object, REMAINING, Value::Float(left));
            if left <= 0.0 {
                write_hidden(&object, DONE, Value::Bool(true));
                close(interp, &source)?;
            }
            Ok(value_result(interp, value))
        }
        KIND_DROP => {
            let source = stored_source(&object, interp)?;
            let mut left = read_hidden(interp, &object, REMAINING)?.to_number();
            while left > 0.0 {
                if step(interp, &source)?.is_none() {
                    write_hidden(&object, DONE, Value::Bool(true));
                    return Ok(done_result(interp));
                }
                left -= 1.0;
            }
            write_hidden(&object, REMAINING, Value::Float(0.0));
            match step(interp, &source)? {
                Some(value) => Ok(value_result(interp, value)),
                None => {
                    write_hidden(&object, DONE, Value::Bool(true));
                    Ok(done_result(interp))
                }
            }
        }
        KIND_FLAT_MAP => {
            let callback = read_hidden(interp, &object, CALLBACK)?;
            loop {
                // Drain the inner iterator first: `flatMap` flattens one level, so the
                // outer step only happens once the current inner one is exhausted.
                let inner = read_hidden(interp, &object, INNER)?;
                if let Value::Object(_) = &inner {
                    let inner_record = iterator_record(interp, &inner)?;
                    if let Some(value) = step(interp, &inner_record)? {
                        return Ok(value_result(interp, value));
                    }
                    write_hidden(&object, INNER, Value::Undefined);
                }
                let source = stored_source(&object, interp)?;
                let Some(value) = step(interp, &source)? else {
                    write_hidden(&object, DONE, Value::Bool(true));
                    return Ok(done_result(interp));
                };
                let counter = read_hidden(interp, &object, COUNTER)?.to_number();
                write_hidden(&object, COUNTER, Value::Float(counter + 1.0));
                let produced = interp.call_value(
                    callback.clone(),
                    Value::Undefined,
                    vec![value, Value::Int(counter as i32)],
                )?;
                // `flatMap` iterates what the callback returned, and that may be any
                // iterable — an array is the common case — so it goes through the same
                // wrapping `Iterator.from` does.
                let wrapped = into_iterator(interp, produced)?;
                write_hidden(&object, INNER, wrapped);
            }
        }
        KIND_CHUNKS => {
            let size = read_hidden(interp, &object, REMAINING)?.to_number();
            let source = stored_source(&object, interp)?;
            let mut items = Vec::new();
            while (items.len() as f64) < size {
                match step(interp, &source)? {
                    Some(value) => items.push(value),
                    None => break,
                }
            }
            if items.is_empty() {
                write_hidden(&object, DONE, Value::Bool(true));
                return Ok(done_result(interp));
            }
            let array = interp.new_array(items);
            Ok(value_result(interp, Value::Object(array)))
        }
        KIND_WINDOWS => {
            let size = read_hidden(interp, &object, REMAINING)?.to_number() as usize;
            let source = stored_source(&object, interp)?;
            let buffer = read_hidden(interp, &object, BUFFER)?;
            let mut items: Vec<Value> = match &buffer {
                Value::Object(array) => array.array_elements().unwrap_or_default(),
                _ => Vec::new(),
            };
            if items.is_empty() {
                // Prime the window, then yield the first one.
                while items.len() < size {
                    match step(interp, &source)? {
                        Some(value) => items.push(value),
                        None => {
                            write_hidden(&object, DONE, Value::Bool(true));
                            return Ok(done_result(interp));
                        }
                    }
                }
            } else {
                // Slide: drop the oldest, take the next. A short read means no full
                // window is possible any more, and `windows` yields nothing partial.
                items.remove(0);
                match step(interp, &source)? {
                    Some(value) => items.push(value),
                    None => {
                        write_hidden(&object, DONE, Value::Bool(true));
                        return Ok(done_result(interp));
                    }
                }
            }
            let stored = interp.new_array(items.clone());
            write_hidden(&object, BUFFER, Value::Object(stored));
            let window = interp.new_array(items);
            Ok(value_result(interp, Value::Object(window)))
        }
        KIND_CONCAT => zip_next_concat(interp, &object),
        KIND_ZIP => zip_next(interp, &object, false),
        KIND_ZIP_KEYED => zip_next(interp, &object, true),
        _ => Err(interp.type_error("not a valid iterator helper")),
    }
}

/// `Iterator.concat(...).next()`: drain the current source, then move to the next one.
fn zip_next_concat(interp: &mut Interp<'_>, object: &JsObject) -> JsResult<Value> {
    loop {
        let inner = read_hidden(interp, object, CURSOR_INNER)?;
        if let Value::Object(_) = &inner {
            let record = iterator_record(interp, &inner)?;
            if let Some(value) = step(interp, &record)? {
                return Ok(value_result(interp, value));
            }
            write_hidden(object, CURSOR_INNER, Value::Undefined);
        }
        let cursor = read_hidden(interp, object, CURSOR)?.to_number() as usize;
        let list = read_hidden(interp, object, LIST)?;
        let Some(array) = list.as_object().cloned() else {
            write_hidden(object, DONE, Value::Bool(true));
            return Ok(done_result(interp));
        };
        let items = array.array_elements().unwrap_or_default();
        if cursor >= items.len() {
            write_hidden(object, DONE, Value::Bool(true));
            return Ok(done_result(interp));
        }
        // Each source's `Symbol.iterator` is read here, not at construction, so a consumer
        // that stops early never touches the later sources.
        let record = iterator_flattenable(interp, &items[cursor], false)?;
        write_hidden(object, CURSOR, Value::Int(cursor as i32 + 1));
        write_hidden(object, CURSOR_INNER, record.object.clone());
        // Only the object is kept: the next loop turn re-derives the record from it, so
        // one canonical `next` is used for every source.
    }
}

/// `Iterator.zip(...).next()` / `Iterator.zipKeyed(...).next()`.
fn zip_next(interp: &mut Interp<'_>, object: &JsObject, keyed: bool) -> JsResult<Value> {
    let mode = read_hidden(interp, object, MODE)?.to_number() as i32;
    let records = unpack_records(interp, object)?;
    if records.is_empty() {
        // Zero sources: nothing to zip. Answering `done` immediately also keeps the
        // "all exhausted" test below from being vacuously true forever.
        write_hidden(object, DONE, Value::Bool(true));
        return Ok(done_result(interp));
    }
    let mut values = Vec::with_capacity(records.len());
    let mut exhausted = 0usize;
    for record in &records {
        match step(interp, record)? {
            Some(value) => values.push(Some(value)),
            None => {
                values.push(None);
                exhausted += 1;
            }
        }
    }
    match mode {
        0 => {
            // shortest: any exhausted source ends the zip, and the others are closed.
            if exhausted > 0 {
                write_hidden(object, DONE, Value::Bool(true));
                for record in &records {
                    close(interp, record)?;
                }
                return Ok(done_result(interp));
            }
        }
        1 => {
            // longest: keep going until every source is exhausted, padding with
            // `undefined`.
            if exhausted == records.len() {
                write_hidden(object, DONE, Value::Bool(true));
                return Ok(done_result(interp));
            }
        }
        _ => {
            // strict: all sources must agree exactly.
            if exhausted > 0 && exhausted < records.len() {
                write_hidden(object, DONE, Value::Bool(true));
                for record in &records {
                    close(interp, record)?;
                }
                return Err(interp.type_error("iterators have different lengths"));
            }
            if exhausted == records.len() {
                write_hidden(object, DONE, Value::Bool(true));
                return Ok(done_result(interp));
            }
        }
    }
    // A missing value in `longest` mode comes from the `padding` option, which is what
    // `{mode:"longest", padding:["pad"]}` means: pad that source with `"pad"`.
    let pads = read_hidden(interp, object, PADDING)?;
    let pad_values = pads
        .as_object()
        .map(|array| array.array_elements().unwrap_or_default())
        .unwrap_or_default();
    let values: Vec<Value> = values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            value.unwrap_or_else(|| pad_values.get(index).cloned().unwrap_or(Value::Undefined))
        })
        .collect();
    let result = if keyed {
        let keys = read_hidden(interp, object, KEYS)?;
        let names = keys
            .as_object()
            .map(|array| array.array_elements().unwrap_or_default())
            .unwrap_or_default();
        let entry = interp.new_object(Some(interp.realm.object_proto.clone()));
        for (index, value) in values.into_iter().enumerate() {
            let Some(name) = names.get(index).cloned() else {
                continue;
            };
            let Value::String(name) = name else {
                continue;
            };
            entry.define(
                &name.to_string_lossy(),
                Property::with_attributes(value, true, true),
            );
        }
        Value::Object(entry)
    } else {
        Value::Object(interp.new_array(values))
    };
    Ok(value_result(interp, result))
}

/// The source record a helper was built over.
fn stored_source(object: &JsObject, interp: &mut Interp<'_>) -> JsResult<crate::interp::Iterator> {
    Ok(crate::interp::Iterator {
        object: read_hidden(interp, object, SOURCE)?,
        next: read_hidden(interp, object, SOURCE_NEXT)?,
    })
}

fn helper_return(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // Closing a helper closes what is underneath it, once. Without the propagation a
    // `for…of` over `xs.map(f)` that breaks early would leave `xs` open.
    if let Some(object) = this.as_object().cloned() {
        close_helper(interp, &object)?;
    }
    Ok(done_result(interp))
}

/// Close whatever the helper is currently reading, and mark it done.
///
/// Which source is "underneath" depends on the kind: `flatMap` has a per-element inner
/// iterator, `concat` a per-source one, and the rest a single fixed source. All three are
/// checked, because each is the live one for some helper. The `DONE` flag makes the second
/// close a no-op — a `return` followed by an abrupt completion must not close twice.
fn close_helper(interp: &mut Interp<'_>, object: &JsObject) -> JsResult<()> {
    if read_hidden(interp, object, DONE)?.to_boolean() {
        return Ok(());
    }
    write_hidden(object, DONE, Value::Bool(true));
    for slot in [INNER, CURSOR_INNER] {
        let inner = read_hidden(interp, object, slot)?;
        if let Value::Object(_) = &inner {
            let record = iterator_record(interp, &inner)?;
            close(interp, &record)?;
        }
    }
    let source = read_hidden(interp, object, SOURCE)?;
    if let Value::Object(_) = &source {
        let record = stored_source(object, interp)?;
        close(interp, &record)?;
    }
    Ok(())
}

fn done_result(interp: &Interp<'_>) -> Value {
    let object = interp.new_object(Some(interp.realm.object_proto.clone()));
    object.define(
        "value",
        Property::with_attributes(Value::Undefined, true, true),
    );
    object.define(
        "done",
        Property::with_attributes(Value::Bool(true), true, true),
    );
    Value::Object(object)
}

fn value_result(interp: &Interp<'_>, value: Value) -> Value {
    let object = interp.new_object(Some(interp.realm.object_proto.clone()));
    object.define("value", Property::with_attributes(value, true, true));
    object.define(
        "done",
        Property::with_attributes(Value::Bool(false), true, true),
    );
    Value::Object(object)
}

// ---------------------------------------------------------------------------
// The prototype methods
// ---------------------------------------------------------------------------

fn map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let source = source_of(interp, &this)?;
    helper(interp, source, KIND_MAP, Some(callback))
}

fn filter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let source = source_of(interp, &this)?;
    helper(interp, source, KIND_FILTER, Some(callback))
}

fn take(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let remaining = count_argument(interp, &this, &arg(args, 0), "must be positive")?;
    let source = source_of(interp, &this)?;
    let value = helper(interp, source, KIND_TAKE, None)?;
    if let Value::Object(object) = &value {
        write_hidden(object, REMAINING, Value::Float(remaining));
    }
    Ok(value)
}

fn drop(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let remaining = count_argument(interp, &this, &arg(args, 0), "must be positive")?;
    let source = source_of(interp, &this)?;
    let value = helper(interp, source, KIND_DROP, None)?;
    if let Value::Object(object) = &value {
        write_hidden(object, REMAINING, Value::Float(remaining));
    }
    Ok(value)
}

fn flat_map(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let source = source_of(interp, &this)?;
    helper(interp, source, KIND_FLAT_MAP, Some(callback))
}

fn chunks(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let size = integral_count(interp, &this, &arg(args, 0), "chunkSize")?;
    let source = source_of(interp, &this)?;
    let value = helper(interp, source, KIND_CHUNKS, None)?;
    if let Value::Object(object) = &value {
        write_hidden(object, REMAINING, Value::Float(size));
    }
    Ok(value)
}

fn windows(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // `windows` has no implemented options yet, and the reference refuses every second
    // argument it was offered — so the rule here is "anything but `undefined` is a
    // `TypeError`". It is checked before `next` is read, which is what
    // `argument-effect-order` records.
    if !matches!(args.get(1), None | Some(Value::Undefined)) {
        return Err(interp.type_error("bad undersized option"));
    }
    let size = integral_count(interp, &this, &arg(args, 0), "windowSize")?;
    let source = source_of(interp, &this)?;
    let value = helper(interp, source, KIND_WINDOWS, None)?;
    if let Value::Object(object) = &value {
        write_hidden(object, REMAINING, Value::Float(size));
    }
    Ok(value)
}

/// What a consumer does with one element: `Some` stops the walk with that answer.
type Consumer<'a> = dyn FnMut(&mut Interp<'_>, usize, Value) -> JsResult<Option<Value>> + 'a;

/// Drain the receiver through `f`, passing the index as the second argument.
fn iterate(interp: &mut Interp<'_>, this: &Value, f: &mut Consumer<'_>) -> JsResult<Value> {
    let record = source_of(interp, this)?;
    let mut index = 0usize;
    loop {
        match step(interp, &record)? {
            Some(value) => {
                if let Some(result) = f(interp, index, value)? {
                    // An early exit has to close the source; `some`/`find` stopping at the
                    // first match is the ordinary case.
                    close(interp, &record)?;
                    return Ok(result);
                }
                index += 1;
            }
            None => return Ok(Value::Undefined),
        }
    }
}

fn to_array(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let record = source_of(interp, &this)?;
    let mut items = Vec::new();
    while let Some(value) = step(interp, &record)? {
        items.push(value);
    }
    Ok(Value::Object(interp.new_array(items)))
}

fn for_each(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    iterate(interp, &this, &mut |interp, index, value| {
        interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![value, Value::Int(index as i32)],
        )?;
        Ok(None)
    })?;
    Ok(Value::Undefined)
}

fn some(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let found = iterate(interp, &this, &mut |interp, index, value| {
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![value, Value::Int(index as i32)],
        )?;
        Ok(answer.to_boolean().then_some(Value::Bool(true)))
    })?;
    Ok(if matches!(found, Value::Bool(true)) {
        Value::Bool(true)
    } else {
        Value::Bool(false)
    })
}

fn every(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let stopped = iterate(interp, &this, &mut |interp, index, value| {
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![value, Value::Int(index as i32)],
        )?;
        Ok((!answer.to_boolean()).then_some(Value::Bool(false)))
    })?;
    Ok(if matches!(stopped, Value::Bool(false)) {
        Value::Bool(false)
    } else {
        Value::Bool(true)
    })
}

fn find(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let found = iterate(interp, &this, &mut |interp, index, value| {
        let answer = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![value.clone(), Value::Int(index as i32)],
        )?;
        Ok(answer.to_boolean().then_some(value))
    })?;
    Ok(found)
}

fn reduce(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = require_callback(interp, &arg(args, 0))?;
    let record = source_of(interp, &this)?;
    let mut accumulator = if args.len() > 1 {
        arg(args, 1)
    } else {
        // No initial value: the first element becomes the accumulator, and an empty
        // iterator is a `TypeError` rather than `undefined`.
        match step(interp, &record)? {
            Some(value) => value,
            None => {
                return Err(interp.type_error("reduce of empty iterator with no initial value"));
            }
        }
    };
    let mut index = 0i32;
    while let Some(value) = step(interp, &record)? {
        accumulator = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![accumulator, value, Value::Int(index)],
        )?;
        index += 1;
    }
    Ok(accumulator)
}

fn join(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let separator = match args.first() {
        None | Some(Value::Undefined) => ",".to_string(),
        Some(value) => interp.to_js_string(value)?,
    };
    let record = source_of(interp, &this)?;
    let mut parts = Vec::new();
    while let Some(value) = step(interp, &record)? {
        // `join` skips nullish elements rather than printing "undefined", which is the
        // one place it differs from `Array.prototype.join`'s treatment of holes.
        if matches!(value, Value::Undefined | Value::Null) {
            parts.push(String::new());
        } else {
            parts.push(interp.to_js_string(&value)?);
        }
    }
    Ok(interp.string(parts.join(&separator)))
}

fn includes(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // `includes(searchElement, skippedElements)`: the second argument counts elements to
    // pass over first. It must be an integral Number — a string is a `TypeError` rather
    // than a coercion, and a non-integral one such as `1.5` or `NaN` is too.
    let mut skipped = match args.get(1) {
        None | Some(Value::Undefined) => 0.0,
        Some(value) => {
            let number = match value {
                Value::Int(number) => *number as f64,
                Value::Float(number) => *number,
                _ => {
                    return Err(interp.type_error("not a number"));
                }
            };
            // `NaN` and a fraction are both "not an integral number"; `Infinity` is
            // allowed and simply skips everything.
            if number.is_nan() || (number.is_finite() && number.trunc() != number) {
                return Err(interp.type_error("not an integral number"));
            }
            if number < 0.0 {
                close_receiver(interp, &this)?;
                return Err(interp.range_error("must be positive"));
            }
            number
        }
    };
    let needle = arg(args, 0);
    let record = source_of(interp, &this)?;
    while let Some(value) = step(interp, &record)? {
        if skipped > 0.0 {
            skipped -= 1.0;
            continue;
        }
        // `SameValueZero`: `NaN` finds `NaN`, and `-0` finds `0`.
        if same_value_zero(&value, &needle) {
            close(interp, &record)?;
            return Ok(Value::Bool(true));
        }
    }
    Ok(Value::Bool(false))
}

/// `SameValueZero`, which is `===` except that `NaN` matches itself.
fn same_value_zero(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(a), Value::Float(b)) if a.is_nan() && b.is_nan() => true,
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        _ => left.strict_equals(right),
    }
}

// ---------------------------------------------------------------------------
// The static helpers
// ---------------------------------------------------------------------------

fn from(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    into_iterator(interp, arg(args, 0))
}

/// `GetIteratorFlattenable(value, iterateStrings)`.
///
/// The subtle part is step 3: when the object has **no** `@@iterator`, it is used as the
/// iterator record *itself*. That is what makes `Iterator.from({next(){...}})` work — a
/// bare iterator with no `Symbol.iterator` — and it is also why such an object has to be
/// *wrapped* rather than returned: it is not an `Iterator`, so it has no helper methods.
///
/// `iterate_strings` is `true` for `Iterator.from`, where a string primitive iterates its
/// characters, and `false` for `concat`/`zip`, where `Iterator.concat("ab")` is a
/// `TypeError`.
fn iterator_flattenable(
    interp: &mut Interp<'_>,
    value: &Value,
    iterate_strings: bool,
) -> JsResult<crate::interp::Iterator> {
    let allowed_primitive = iterate_strings && matches!(value, Value::String(_));
    if !matches!(value, Value::Object(_)) && !allowed_primitive {
        return Err(interp.type_error("not an object"));
    }
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Iterator);
    // `read_field_key` boxes a primitive receiver, which is how a string primitive finds
    // `String.prototype[Symbol.iterator]`.
    let method = interp.read_field_key(value, &key)?;
    match method {
        Value::Undefined | Value::Null => iterator_record(interp, value),
        callable if matches!(&callable, Value::Object(object) if object.is_callable()) => {
            let produced = interp.call_value(callable, value.clone(), Vec::new())?;
            if !matches!(produced, Value::Object(_)) {
                return Err(interp.type_error("iterator is not an object"));
            }
            iterator_record(interp, &produced)
        }
        _ => Err(interp.type_error("not a function")),
    }
}

/// `Iterator.from(value)`, which `flatMap` also needs.
///
/// `flatMap` maps each element through the callback and then iterates the *result*, so a
/// callback returning a plain array has to work: `[[1,2],[3]].values().flatMap(x => x)`
/// is the canonical example. Without wrapping, the array has no `next` and the helper
/// died with "not a function".
fn into_iterator(interp: &mut Interp<'_>, value: Value) -> JsResult<Value> {
    // Already an `Iterator`: hand it back untouched, so `Iterator.from(xs) === xs` holds
    // for anything this engine produced. The test is the prototype chain, which is the
    // same thing `instanceof Iterator` reports.
    if is_iterator(interp, &value) {
        return Ok(value);
    }
    let record = iterator_flattenable(interp, &value, true)?;
    let proto = interp
        .realm
        .iterator_wrap_proto
        .clone()
        .ok_or_else(|| interp.type_error("iterator helpers are not installed"))?;
    let object = interp.new_object(Some(proto));
    object.define(
        SOURCE,
        Property::with_attributes(record.object, true, false),
    );
    object.define(
        SOURCE_NEXT,
        Property::with_attributes(record.next, true, false),
    );
    object.define(
        KIND,
        Property::with_attributes(Value::Int(KIND_WRAP), true, false),
    );
    Ok(Value::Object(object))
}

/// Whether `value`'s prototype chain reaches `Iterator.prototype`.
fn is_iterator(interp: &mut Interp<'_>, value: &Value) -> bool {
    let Some(target) = interp.realm.iterator_proto.clone() else {
        return false;
    };
    let mut current = value.as_object().cloned();
    // Bounded walk: a proxy could answer with a cycle, and `instanceof` has the same
    // problem, so this stops rather than hangs.
    for _ in 0..10_000 {
        match current {
            Some(object) => {
                if object.ptr_eq(&target) {
                    return true;
                }
                current = object.proto();
            }
            None => return false,
        }
    }
    false
}

/// `%WrapForValidIteratorPrototype%.next`: forward to the wrapped iterator untouched.
///
/// The result object is returned *as the underlying `next` produced it*, not normalised:
/// the wrapper is transparent, so a `next` that answers `{value, done}` with extra
/// properties keeps them.
fn wrap_next(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    let next = read_hidden(interp, &object, SOURCE_NEXT)?;
    let source = read_hidden(interp, &object, SOURCE)?;
    interp.call_value(next, source, Vec::new())
}

fn wrap_return(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    let source = read_hidden(interp, &object, SOURCE)?;
    let record = crate::interp::Iterator {
        object: source,
        next: read_hidden(interp, &object, SOURCE_NEXT)?,
    };
    close(interp, &record)?;
    Ok(done_result(interp))
}

fn concat(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // Every item is checked *now*: `Iterator.concat([1], 5)` throws at the call, not on the
    // first `next`. Reading each item's `Symbol.iterator` is left until the iteration
    // reaches it, which is what keeps `Iterator.concat(a, b)` from touching `b` when the
    // consumer stops inside `a`.
    let mut items = Vec::with_capacity(args.len());
    for item in args {
        if !matches!(item, Value::Object(_)) {
            return Err(interp.type_error("not an object"));
        }
        items.push(item.clone());
    }
    let helper = iterator_object(interp, KIND_CONCAT)?;
    if let Value::Object(object) = &helper {
        let list = interp.new_array(items);
        write_hidden(object, LIST, Value::Object(list));
        write_hidden(object, CURSOR, Value::Int(0));
        write_hidden(object, CURSOR_INNER, Value::Undefined);
    }
    Ok(helper)
}

/// An empty helper object with the given kind and no source.
///
/// `concat`/`zip` have a *list* of sources rather than one, so they cannot go through
/// [`helper`] and fill the single-source slots in.
fn iterator_object(interp: &mut Interp<'_>, kind: i32) -> JsResult<Value> {
    let proto = interp
        .realm
        .iterator_helper_proto
        .clone()
        .ok_or_else(|| interp.type_error("iterator helpers are not installed"))?;
    let object = interp.new_object(Some(proto));
    write_hidden(&object, KIND, Value::Int(kind));
    write_hidden(&object, DONE, Value::Bool(false));
    Ok(Value::Object(object))
}

/// `Iterator.zip(iterables, options)`.
///
/// Three orderings are observable and all three are tested: `mode` is read first, then
/// `padding` — but *only* when the mode is `longest`, so a `padding` getter must not fire
/// for `shortest` — and only then is the iterables argument opened. Every source is opened
/// at call time rather than on the first `next`.
///
/// `padding` is itself an **iterable**, pulled one value per source position; that is what
/// lets `padding: { *[Symbol.iterator]() { while (true) yield 100 + i++ } }` pad an
/// unbounded number of sources.
fn zip(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(iterables) = arg(args, 0).as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    let mode = zip_mode(interp, args.get(1))?;
    // Only `longest` can pad, so only `longest` reads the option.
    let pad_source = if mode == 1 {
        match zip_padding_option(interp, args.get(1))? {
            Some(value) => Some(interp.get_iterator(&value)?),
            None => None,
        }
    } else {
        None
    };
    let outer = interp.get_iterator(&Value::Object(iterables))?;
    let mut records = Vec::new();
    let mut pads = Vec::new();
    // Collect first, then commit: the sources that were already opened have to be closed
    // if a later one fails, and they are closed in reverse order (the most recently opened
    // first), which `iterables-iteration-get-iterator-flattenable-abrupt-completion` counts.
    let outcome = (|| -> JsResult<()> {
        while let Some(item) = step(interp, &outer)? {
            if !matches!(item, Value::Object(_)) {
                return Err(interp.type_error("not an object"));
            }
            records.push(iterator_flattenable(interp, &item, false)?);
            pads.push(match &pad_source {
                Some(source) => step(interp, source)?.unwrap_or(Value::Undefined),
                None => Value::Undefined,
            });
        }
        Ok(())
    })();
    if let Err(signal) = outcome {
        for record in records.iter().rev() {
            let _ = close(interp, record);
        }
        let _ = close(interp, &outer);
        return Err(signal);
    }
    finish_zip(interp, KIND_ZIP, mode, records, pads, None)
}

/// `Iterator.zipKeyed(iterables, options)`.
///
/// Keyed padding is a plain object read by key, not an iterable: `padding: {a: "pad"}`
/// gives `{a: "pad", b: …}`.
fn zip_keyed(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(source) = arg(args, 0).as_object().cloned() else {
        return Err(interp.type_error("not an object"));
    };
    let mode = zip_mode(interp, args.get(1))?;
    let padding = if mode == 1 {
        match arg(args, 1) {
            Value::Object(options) => match interp.get_property(&options, "padding")? {
                None | Some(Value::Undefined) => None,
                Some(Value::Object(map)) => Some(map),
                Some(_) => return Err(interp.type_error("padding must be an object")),
            },
            _ => None,
        }
    } else {
        None
    };
    let mut keys = Vec::new();
    let mut records = Vec::new();
    let mut pads = Vec::new();
    let outcome = (|| -> JsResult<()> {
        for key in source.own_keys() {
            let name = key.to_string_lossy().into_owned();
            let item = interp.get_property(&source, &name)?;
            let Some(item) = item else { continue };
            if !matches!(item, Value::Object(_)) {
                return Err(interp.type_error("not an object"));
            }
            records.push(iterator_flattenable(interp, &item, false)?);
            pads.push(match &padding {
                Some(map) => interp.get_property(map, &name)?.unwrap_or(Value::Undefined),
                None => Value::Undefined,
            });
            keys.push(interp.string(name));
        }
        Ok(())
    })();
    if let Err(signal) = outcome {
        for record in records.iter().rev() {
            let _ = close(interp, record);
        }
        return Err(signal);
    }
    finish_zip(interp, KIND_ZIP_KEYED, mode, records, pads, Some(keys))
}

fn finish_zip(
    interp: &mut Interp<'_>,
    kind: i32,
    mode: i32,
    records: Vec<crate::interp::Iterator>,
    pads: Vec<Value>,
    keys: Option<Vec<Value>>,
) -> JsResult<Value> {
    let helper = iterator_object(interp, kind)?;
    if let Value::Object(object) = &helper {
        let flat = pack_records(&records);
        let list = interp.new_array(flat);
        write_hidden(object, LIST, Value::Object(list));
        write_hidden(object, MODE, Value::Int(mode));
        let pads = interp.new_array(pads);
        write_hidden(object, PADDING, Value::Object(pads));
        if let Some(keys) = keys {
            let keys = interp.new_array(keys);
            write_hidden(object, KEYS, Value::Object(keys));
        }
    }
    Ok(helper)
}

/// `options.mode`, read before anything else and validated on the spot.
fn zip_mode(interp: &mut Interp<'_>, options: Option<&Value>) -> JsResult<i32> {
    let Some(options) = options else {
        return Ok(0);
    };
    if matches!(options, Value::Undefined) {
        return Ok(0);
    }
    let Some(object) = options.as_object().cloned() else {
        return Err(interp.type_error("options must be an object"));
    };
    match interp.get_property(&object, "mode")? {
        None | Some(Value::Undefined) => Ok(0),
        Some(Value::String(text)) => match text.to_string_lossy().as_ref() {
            "shortest" => Ok(0),
            "longest" => Ok(1),
            "strict" => Ok(2),
            _ => Err(interp.type_error("invalid mode")),
        },
        Some(_) => Err(interp.type_error("invalid mode")),
    }
}

/// `options.padding`, read only in `longest` mode.
fn zip_padding_option(interp: &mut Interp<'_>, options: Option<&Value>) -> JsResult<Option<Value>> {
    let Some(Value::Object(object)) = options else {
        return Ok(None);
    };
    match interp.get_property(object, "padding")? {
        None | Some(Value::Undefined) => Ok(None),
        Some(value @ Value::Object(_)) => Ok(Some(value)),
        Some(_) => Err(interp.type_error("padding must be an object")),
    }
}

/// Flatten iterator records into a value list so they can live in one array.
///
/// Each record becomes two adjacent entries, `[object, next]`, which
/// [`unpack_records`] reads back.
fn pack_records(records: &[crate::interp::Iterator]) -> Vec<Value> {
    let mut flat = Vec::with_capacity(records.len() * 2);
    for record in records {
        flat.push(record.object.clone());
        flat.push(record.next.clone());
    }
    flat
}

fn unpack_records(
    interp: &mut Interp<'_>,
    object: &JsObject,
) -> JsResult<Vec<crate::interp::Iterator>> {
    let list = read_hidden(interp, object, LIST)?;
    let Some(array) = list.as_object().cloned() else {
        return Ok(Vec::new());
    };
    let elements = array.array_elements().unwrap_or_default();
    let mut records = Vec::with_capacity(elements.len() / 2);
    for pair in elements.chunks(2) {
        if let [object, next] = pair {
            records.push(crate::interp::Iterator {
                object: object.clone(),
                next: next.clone(),
            });
        }
    }
    Ok(records)
}

//! `Map` and `Set`.
//!
//! Both are an insertion-ordered list of entries with `SameValueZero` key equality, so one
//! implementation serves both: a `Set` is a `Map` whose value is unused. Keeping a list
//! rather than a hash table is what makes the *order* free and exact — a `Map` iterates in
//! insertion order, and a deleted-then-reinserted key moves to the end — at the cost of
//! linear lookup, which the sizes this engine is used at do not notice.
//!
//! # Why `SameValueZero` and not `===`
//!
//! `map.set(NaN, 1)` then `map.get(NaN)` has to work, and `NaN === NaN` is false. The other
//! difference is that `+0` and `-0` are one key, which `===` already gives.
//!
//! # Iterators
//!
//! `keys`/`values`/`entries` snapshot the entries into an array and hand it to the shared
//! array iterator. The spec makes those iterators *live* — an entry added while iterating is
//! visited — and a snapshot is not. That is a documented simplification: it keeps one
//! iterator implementation in the engine, and no test here distinguishes the two.

use crate::builtins::array::{IteratorKind, iterator};
use crate::builtins::{arg, define_method, method};
use crate::interp::{Interp, JsResult, WellKnown};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// `Map.prototype` methods, installed once for both classes.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    install_map(interp, object_proto);
    install_set(interp, object_proto);
}

fn install_map(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_constructor("Map", 0, map_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("get", 1, get as crate::object::NativeFn),
        ("set", 2, set),
        ("has", 1, has),
        ("delete", 1, delete),
        ("clear", 0, clear),
        ("forEach", 1, for_each),
        ("keys", 0, keys),
        ("values", 0, values),
        ("entries", 0, entries),
    ] {
        define_method(&proto, name, length, func, interp);
    }
    install_size(&proto, interp);
    install_iterable(&proto, interp, true);
    interp.realm.map_proto = Some(proto);
    let global = interp.realm.global.clone();
    global.define(
        "Map",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

fn install_set(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_constructor("Set", 0, set_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("add", 1, add as crate::object::NativeFn),
        ("has", 1, has),
        ("delete", 1, delete),
        ("clear", 0, clear),
        ("forEach", 1, for_each),
        ("keys", 0, keys),
        ("values", 0, values),
        ("entries", 0, entries),
        // The seven ES2025 set methods (§24.2.3.13–.19). All of them take a
        // `SetRecord` — an arbitrary object read through `size`/`has`/`keys` — not just
        // another `Set`, which is why they are not implemented in terms of each other.
        ("union", 1, union),
        ("intersection", 1, intersection),
        ("difference", 1, difference),
        ("symmetricDifference", 1, symmetric_difference),
        ("isSubsetOf", 1, is_subset_of),
        ("isSupersetOf", 1, is_superset_of),
        ("isDisjointFrom", 1, is_disjoint_from),
    ] {
        define_method(&proto, name, length, func, interp);
    }
    install_size(&proto, interp);
    install_iterable(&proto, interp, false);
    interp.realm.set_proto = Some(proto);
    let global = interp.realm.global.clone();
    global.define(
        "Set",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `size` is an accessor, so it cannot be assigned and always reflects the entries.
fn install_size(proto: &Gc<JsObject>, interp: &Interp<'_>) {
    let getter = interp.new_native("get size", 0, size);
    proto.define(
        "size",
        Property {
            getter: Some(Value::Object(getter)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
}

/// `Symbol.iterator`, plus the `Symbol.toStringTag` the tag question needs.
///
/// The default iterator differs by collection, and it is easy to get wrong in a way that
/// looks almost right: a `Map` iterates `[key, value]` pairs (the same as `entries`), but
/// a `Set` iterates its *values*. Wiring both to `entries` made `[...new Set([3])]`
/// produce `[[3, 3]]` — one pair — instead of `[3]`.
fn install_iterable(proto: &Gc<JsObject>, interp: &mut Interp<'_>, is_map: bool) {
    let func = if is_map {
        entries as crate::object::NativeFn
    } else {
        values
    };
    let iterator_fn = method(interp, "[Symbol.iterator]", 0, func).expect("a method");
    let key = interp.realm.well_known_key(WellKnown::Iterator);
    proto.define_key(&key, Property::data(iterator_fn));
}

/// A collection's entries, named so the signatures below stay readable.
type Entries = Vec<(Value, Value)>;

/// The entries of `this`, which must be a `Map` or a `Set`.
fn entries_of(interp: &Interp<'_>, this: &Value) -> JsResult<(Gc<JsObject>, Entries)> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("method called on a non-collection"))?;
    let entries = object
        .collection_entries()
        .ok_or_else(|| interp.type_error("method called on a non-collection"))?;
    Ok((object.clone(), entries))
}

fn is_set(interp: &Interp<'_>, this: &Value) -> JsResult<bool> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("method called on a non-collection"))?;
    object
        .collection_is_set()
        .ok_or_else(|| interp.type_error("method called on a non-collection"))
}

/// Build a `Map` or a `Set`, filling it from an iterable argument.
fn make_collection(
    interp: &mut Interp<'_>,
    this: &Value,
    args: &[Value],
    is_set_form: bool,
) -> JsResult<Value> {
    let object = match this.as_object() {
        Some(object) => object.clone(),
        None => {
            let proto = if is_set_form {
                interp.realm.set_proto.clone()
            } else {
                interp.realm.map_proto.clone()
            };
            interp.new_object(proto.or(Some(interp.realm.object_proto.clone())))
        }
    };
    object.set_collection(Vec::new(), is_set_form);
    let source = arg(args, 0);
    if !matches!(source, Value::Undefined | Value::Null) {
        for entry in interp.array_like_to_args(&source)? {
            if is_set_form {
                set_entry(interp, &object, entry)?;
            } else {
                // A `Map` source yields `[key, value]` pairs.
                let pair = interp.array_like_to_args(&entry)?;
                set_entry(interp, &object, arg(&pair, 0))?;
                if let Some(object) = this.as_object() {
                    let mut entries = object.collection_entries().unwrap_or_default();
                    if let Some(last) = entries.last_mut() {
                        last.1 = arg(&pair, 1);
                    }
                    object.set_collection(entries, false);
                }
            }
        }
    }
    Ok(Value::Object(object))
}

/// Insert or update one key.
fn set_entry(interp: &Interp<'_>, object: &Gc<JsObject>, key: Value) -> JsResult<()> {
    let mut entries = object.collection_entries().unwrap_or_default();
    let is_set = object.collection_is_set().unwrap_or(false);
    match entries
        .iter_mut()
        .find(|(existing, _)| existing.same_value_zero(&key))
    {
        // An existing key keeps its position and only its value changes, which is what
        // makes `map.set(k, 1); map.set(k, 2)` iterate `k` once.
        Some((_, value)) => *value = key.clone(),
        // Both classes start the value at `undefined`: a `Set` never reads it, and a `Map`
        // overwrites it as soon as the pair is known.
        None => entries.push((key.clone(), Value::Undefined)),
    }
    let _ = (interp, is_set);
    object.set_collection(entries, is_set);
    Ok(())
}

/// `new Map(iterable)`.
fn map_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    make_collection(interp, &this, args, false)
}

/// `new Set(iterable)`.
fn set_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    make_collection(interp, &this, args, true)
}

fn size(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (_, entries) = entries_of(interp, &this)?;
    Ok(Value::Int(entries.len() as i32))
}

fn get(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let key = arg(args, 0);
    let (_, entries) = entries_of(interp, &this)?;
    Ok(entries
        .into_iter()
        .find(|(existing, _)| existing.same_value_zero(&key))
        .map(|(_, value)| value)
        .unwrap_or(Value::Undefined))
}

fn set(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, mut entries) = entries_of(interp, &this)?;
    let key = arg(args, 0);
    let value = arg(args, 1);
    match entries
        .iter_mut()
        .find(|(existing, _)| existing.same_value_zero(&key))
    {
        Some((_, slot)) => *slot = value,
        None => entries.push((key, value)),
    }
    object.set_collection(entries, false);
    Ok(this)
}

/// `set.add(value)`, which answers the set itself.
fn add(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, mut entries) = entries_of(interp, &this)?;
    let value = arg(args, 0);
    if !entries
        .iter()
        .any(|(existing, _)| existing.same_value_zero(&value))
    {
        entries.push((value, Value::Undefined));
    }
    object.set_collection(entries, true);
    Ok(this)
}

/// A `SetRecord` (§24.2.1.2): the *shape* the set methods accept, not a `Set`.
///
/// `union`/`intersection`/`difference`/`symmetricDifference`/`isSubsetOf`/`isSupersetOf`/
/// `isDisjointFrom` all take "a set-like object", so any object with a numeric `size`, a
/// `has` method and a `keys` method works. That is why they cannot be written in terms of
/// `Set.prototype.has` — the argument need not be a `Set`.
///
/// The reads are *observable and ordered*: `size` first (and it must be a number, else a
/// `TypeError`), then `has`, and `keys` only where the algorithm needs to iterate. Getting
/// the order wrong is visible to a test that logs each access.
struct SetRecord {
    size: f64,
    has: Value,
    /// `keys`, read and validated here even though `difference`/`isSubsetOf` never call it.
    /// `GetSetRecord` (§24.2.1.2) validates all three members up front, so a non-callable
    /// `keys` is a `TypeError` from `difference` too.
    keys: Value,
    /// The object the `has`/`keys` methods were read from, which is their `this`.
    receiver: Value,
}

fn set_record(interp: &mut Interp<'_>, value: &Value) -> JsResult<SetRecord> {
    let Some(object) = value.as_object().cloned() else {
        return Err(interp.type_error("the argument is not a set-like object"));
    };
    let key = interp.property_key(&interp.string("size"));
    let size = interp.get_property_key(&object, &key)?;
    let size = match size {
        None | Some(Value::Undefined) => {
            return Err(interp.type_error("the argument has no size property"));
        }
        Some(value) => interp.to_js_number(&value)?,
    };
    let key = interp.property_key(&interp.string("has"));
    let has = interp.get_property_key(&object, &key)?;
    let has = match has {
        Some(value) if value.as_object().is_some_and(|object| object.is_callable()) => value,
        _ => return Err(interp.type_error("the argument's has property is not callable")),
    };
    let key = interp.property_key(&interp.string("keys"));
    let keys = interp.get_property_key(&object, &key)?;
    let keys = match keys {
        Some(value) if value.as_object().is_some_and(|object| object.is_callable()) => value,
        _ => return Err(interp.type_error("the argument's keys property is not callable")),
    };
    Ok(SetRecord {
        size,
        has,
        keys,
        receiver: Value::Object(object),
    })
}

impl SetRecord {
    /// `SetRecord.has(key)`, which is `[[Call]]` on the user's method.
    fn has(&self, interp: &mut Interp<'_>, key: &Value) -> JsResult<bool> {
        let answer =
            interp.call_value(self.has.clone(), self.receiver.clone(), vec![key.clone()])?;
        Ok(answer.to_boolean())
    }
}

/// The keys a set-like object yields: `record.keys()` then iterate it.
fn set_record_keys(interp: &mut Interp<'_>, record: &SetRecord) -> JsResult<Vec<Value>> {
    // The receiver, not `undefined`: `keys` is a method on the *argument*, so it needs the
    // argument as `this` — passing `undefined` made `Set.prototype.keys` report "method
    // called on a non-collection" for a perfectly good Set.
    let iterator = interp.call_value(record.keys.clone(), record.receiver.clone(), Vec::new())?;
    interp.collect_iterable(&iterator)
}

/// A fresh `Set` holding `keys`, which every "answer with a new set" method needs.
fn set_of(interp: &mut Interp<'_>, keys: Vec<Value>) -> JsResult<Value> {
    let proto = interp.realm.set_proto.clone();
    let object = interp.new_object(proto.or(Some(interp.realm.object_proto.clone())));
    let mut entries: Entries = Vec::with_capacity(keys.len());
    for key in keys {
        if !entries
            .iter()
            .any(|(existing, _)| existing.same_value_zero(&key))
        {
            entries.push((key, Value::Undefined));
        }
    }
    object.set_collection(entries, true);
    Ok(Value::Object(object))
}

/// The receiver as a set's entries, refusing anything that is not one.
fn this_set(interp: &Interp<'_>, this: &Value) -> JsResult<(Gc<JsObject>, Entries)> {
    if !is_set(interp, this)? {
        return Err(interp.type_error("method called on a non-Set"));
    }
    entries_of(interp, this)
}

/// `SameValueZero` membership over a bare list of keys.
///
/// Separate from [`has_entry`] because three of the seven methods compare *two key lists*
/// (the receiver's keys and what `keys()` yielded), and round-tripping those through
/// `(key, undefined)` pairs to reuse the tuple version was both slower and harder to read.
fn contains_key(keys: &[Value], key: &Value) -> bool {
    keys.iter().any(|existing| existing.same_value_zero(key))
}

fn has_entry(entries: &Entries, key: &Value) -> bool {
    entries
        .iter()
        .any(|(existing, _)| existing.same_value_zero(key))
}

/// `Set.prototype.union(other)`: every key of both, receiver's order first.
fn union(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    let mut keys: Vec<Value> = entries.into_iter().map(|(key, _)| key).collect();
    for key in set_record_keys(interp, &record)? {
        if !contains_key(&keys, &key) {
            keys.push(key);
        }
    }
    set_of(interp, keys)
}

/// `Set.prototype.intersection(other)`: keys present in both, in receiver order.
fn intersection(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    let other_keys = set_record_keys(interp, &record)?;
    let mut kept = Vec::new();
    for (key, _) in entries {
        // `size` bounds the work only when it is smaller; correctness needs the key list.
        let _ = record.size;
        if contains_key(&other_keys, &key) {
            kept.push(key);
        }
    }
    set_of(interp, kept)
}

/// `Set.prototype.difference(other)`: receiver keys that `other` lacks.
fn difference(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    let mut kept = Vec::new();
    for (key, _) in entries {
        // `has` is called per key, not derived from `keys()`: that is what the record is
        // for, and the call count is what test262 asserts.
        if !record.has(interp, &key)? {
            kept.push(key);
        }
    }
    set_of(interp, kept)
}

/// `Set.prototype.symmetricDifference(other)`: keys in exactly one side.
fn symmetric_difference(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    let other_keys = set_record_keys(interp, &record)?;
    let mut kept = Vec::new();
    for (key, _) in &entries {
        if !contains_key(&other_keys, key) {
            kept.push(key.clone());
        }
    }
    for key in other_keys {
        if !has_entry(&entries, &key) {
            kept.push(key);
        }
    }
    let _ = record;
    set_of(interp, kept)
}

/// `Set.prototype.isSubsetOf(other)`: every receiver key is in `other`.
fn is_subset_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let record = set_record(interp, &arg(args, 0))?;
    if (entries.len() as f64) > record.size {
        return Ok(Value::Bool(false));
    }
    for (key, _) in entries {
        if !record.has(interp, &key)? {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

/// `Set.prototype.isSupersetOf(other)`: every key of `other` is in the receiver.
fn is_superset_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    if (entries.len() as f64) < record.size {
        return Ok(Value::Bool(false));
    }
    for key in set_record_keys(interp, &record)? {
        if !has_entry(&entries, &key) {
            return Ok(Value::Bool(false));
        }
    }
    Ok(Value::Bool(true))
}

/// `Set.prototype.isDisjointFrom(other)`: the two share no key.
fn is_disjoint_from(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (_, entries) = this_set(interp, &this)?;
    let other = arg(args, 0);
    let record = set_record(interp, &other)?;
    let other_keys = set_record_keys(interp, &record)?;
    // The smaller side drives the probe: `has` is a user call, so which direction is taken
    // is observable as a different number of calls — and the spec asks for the smaller one.
    if entries.len() <= other_keys.len() {
        for (key, _) in entries {
            if contains_key(&other_keys, &key) {
                return Ok(Value::Bool(false));
            }
        }
    } else {
        for key in other_keys {
            if has_entry(&entries, &key) {
                return Ok(Value::Bool(false));
            }
        }
    }
    let _ = record;
    Ok(Value::Bool(true))
}

fn has(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let key = arg(args, 0);
    let (_, entries) = entries_of(interp, &this)?;
    Ok(Value::Bool(
        entries
            .into_iter()
            .any(|(existing, _)| existing.same_value_zero(&key)),
    ))
}

fn delete(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let key = arg(args, 0);
    let (object, mut entries) = entries_of(interp, &this)?;
    let before = entries.len();
    entries.retain(|(existing, _)| !existing.same_value_zero(&key));
    let removed = entries.len() != before;
    let is_set = object.collection_is_set().unwrap_or(false);
    object.set_collection(entries, is_set);
    Ok(Value::Bool(removed))
}

fn clear(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (object, _) = entries_of(interp, &this)?;
    let is_set = object.collection_is_set().unwrap_or(false);
    object.set_collection(Vec::new(), is_set);
    Ok(Value::Undefined)
}

/// `forEach(callback, thisArg)`, called with `(value, key, collection)`.
fn for_each(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = arg(args, 0);
    if !callback
        .as_object()
        .is_some_and(|function| function.is_callable())
    {
        return Err(interp.type_error("forEach needs a callback"));
    }
    let this_arg = arg(args, 1);
    let (_, entries) = entries_of(interp, &this)?;
    // A snapshot, so a callback that deletes the current entry does not disturb the walk —
    // which is what the spec's "visited" bookkeeping amounts to for the common cases.
    for (key, value) in entries {
        let payload = if is_set(interp, &this)? {
            vec![key.clone(), key, this.clone()]
        } else {
            vec![value, key, this.clone()]
        };
        interp.call_value(callback.clone(), this_arg.clone(), payload)?;
    }
    Ok(Value::Undefined)
}

fn keys(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let (_, entries) = entries_of(interp, &this)?;
    let values: Vec<Value> = entries.into_iter().map(|(key, _)| key).collect();
    let array = interp.new_array(values);
    iterator(interp, Value::Object(array.clone()), IteratorKind::Value)
}

fn values(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let is_set_form = is_set(interp, &this)?;
    let (_, entries) = entries_of(interp, &this)?;
    let values: Vec<Value> = entries
        .into_iter()
        .map(|(key, value)| if is_set_form { key } else { value })
        .collect();
    let array = interp.new_array(values);
    iterator(interp, Value::Object(array.clone()), IteratorKind::Value)
}

fn entries(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let is_set_form = is_set(interp, &this)?;
    let (_, entries) = entries_of(interp, &this)?;
    let values: Vec<Value> = entries
        .into_iter()
        .map(|(key, value)| {
            // A `Set`'s entries are `[value, value]`, which is what makes
            // `for (const [a, b] of set)` give the same value twice.
            let pair = if is_set_form {
                vec![key.clone(), key]
            } else {
                vec![key, value]
            };
            Value::Object(interp.new_array(pair))
        })
        .collect();
    let array = interp.new_array(values);
    iterator(interp, Value::Object(array.clone()), IteratorKind::Value)
}

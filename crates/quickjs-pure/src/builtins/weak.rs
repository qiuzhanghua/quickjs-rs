//! `WeakMap`, `WeakSet`, `WeakRef` and `FinalizationRegistry`.
//!
//! # Why these are cheap here
//!
//! The engine frees memory by reference counting with a cycle collector, so a box is
//! released the instant its last strong handle dies. [`WeakGc`] therefore already has the
//! two properties these classes need:
//!
//! * it does not keep the target alive (it owns no strong reference), and
//! * it does not participate in cycle detection (`Trace for WeakGc` visits nothing), so a
//!   `WeakMap` whose key is only reachable through the map does not keep the cycle alive.
//!
//! That means a `WeakMap` entry disappears exactly when its key dies, with no second
//! "clear the dead entries" pass and no tracing-collector machinery: the map stores a
//! `WeakGc` per key, and looking a key up simply finds nothing once `upgrade` fails.
//!
//! # What is *not* free: scheduling the cleanup callback
//!
//! `FinalizationRegistry`'s cleanup callback must run "some time after" the target dies,
//! at a point when script is not running. Reference counting can notice a death precisely
//! (the strong count hits zero) but the crate gives no hook for it, and running script from
//! inside a `Drop` would be a re-entrancy hazard of the worst kind — the drop can happen
//! while a `RefCell` is borrowed, in the middle of arbitrary engine bookkeeping.
//!
//! So the registry is drained at a *safe point* instead: [`drain_finalizers`] is called
//! from the microtask checkpoint, which is where a host may run script after a job anyway,
//! and it collects every registration whose target has expired by then. The observable
//! difference from a real GC is only *when* the callback fires — "at the next turn of the
//! event loop" rather than "at an unpredictable later moment" — which is within what
//! §9.10 permits and is what test262's own `$262.gc` hook models.
//!
//! # Why `WeakMap` uses `WeakGc<JsObject>` and not a symbol-keyed secret
//!
//! A key must be an object, and two objects are one key exactly when they are the same
//! object. `ptr_eq` on a `WeakGc` answers that in one comparison, with no property written
//! onto the key — which matters, because a `WeakMap` must not add observable properties.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, WellKnown};
use crate::object::{FinalizationState, JsObject, Property, Registration, weak_key};
use crate::value::Value;
use js_gc::Gc;

/// Install the four globals.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    install_weak_collection(interp, object_proto, true);
    install_weak_collection(interp, object_proto, false);
    install_weak_ref(interp, object_proto);
    install_finalization(interp, object_proto);
}

/// The `Symbol.toStringTag` a prototype carries, so `Object.prototype.toString` can name it.
///
/// `{ [[Writable]]: false, [[Enumerable]]: false, [[Configurable]]: true }` — the read-only,
/// non-enumerable shape every builtin tag uses. `Property::data` defaults to writable and
/// enumerable, which `verifyProperty` catches.
fn install_tag(interp: &mut Interp<'_>, proto: &Gc<JsObject>, tag: &str) {
    let key = interp.realm.well_known_key(WellKnown::ToStringTag);
    proto.define_key(
        &key,
        Property {
            value: Some(interp.string(tag)),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );
}

/// `WeakMap` (`is_map`) and `WeakSet`, whose surface differs only in the value slot.
fn install_weak_collection(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>, is_map: bool) {
    let name = if is_map { "WeakMap" } else { "WeakSet" };
    let proto = interp.new_object(Some(object_proto.clone()));
    // `length` is 0: the constructor takes an optional `iterable` and nothing required.
    //
    // Two distinct function pointers rather than one that reads a flag off the callee: a
    // native constructor runs with `this = undefined` and its callee is not reliably
    // reachable from inside the call, so a flag lookup silently fell back to `WeakMap` and
    // made `new WeakSet().add(1)` complain about the receiver's brand.
    let constructor = interp.new_native_constructor(
        name,
        0,
        if is_map {
            weak_map_constructor as crate::object::NativeFn
        } else {
            weak_set_constructor
        },
    );
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    if is_map {
        for (method_name, length, func) in [
            ("get", 1, weak_get as crate::object::NativeFn),
            ("set", 2, weak_set),
            ("has", 1, weak_has),
            ("delete", 1, weak_delete),
            // `getOrInsert`/`getOrInsertComputed` are the ES2026 proposal methods; the
            // reference implementation has them and test262 already tests them.
            ("getOrInsert", 2, weak_get_or_insert),
            ("getOrInsertComputed", 2, weak_get_or_insert_computed),
        ] {
            define_method(&proto, method_name, length, func, interp);
        }
    } else {
        for (method_name, length, func) in [
            ("add", 1, weak_add as crate::object::NativeFn),
            ("has", 1, weak_has),
            ("delete", 1, weak_delete),
        ] {
            define_method(&proto, method_name, length, func, interp);
        }
    }
    install_tag(interp, &proto, name);
    let global = interp.realm.global.clone();
    global.define(
        name,
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// The `prototype` of the global constructor called `name`.
///
/// A native constructor builds its own instance, so it has to name its default prototype
/// itself. Looking it up through the global keeps `WeakMap.prototype = {...}` observable in
/// the constructor path the way it is for a script class.
fn constructor_prototype(interp: &mut Interp<'_>, name: &str) -> Option<Gc<JsObject>> {
    let property = interp.realm.global.own_property(name)?;
    let constructor = property.value?.as_object().cloned()?;
    Some(interp.prototype_of(&constructor))
}

/// `IteratorClose`, ignoring an error from `return` itself.
///
/// The "we are already leaving" path swallows a throwing `return`, which is what §7.4.11
/// asks for: the original abrupt completion is the one the script sees.
fn close_iterator(interp: &mut Interp<'_>, iterator: &crate::interp::Iterator) -> JsResult<()> {
    let Some(target) = iterator.object.as_object().cloned() else {
        return Ok(());
    };
    match interp.get_property(&target, "return")? {
        Some(Value::Object(function)) if function.is_callable() => {
            interp.call_value(Value::Object(function), iterator.object.clone(), Vec::new())?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The receiver, which must be a weak collection of the expected kind.
///
/// Two refusals to keep apart: a receiver that is not a weak collection at all is
/// `TypeError: WeakMap object expected`, and a `WeakSet` passed to a `WeakMap` method (or
/// the reverse) is a `TypeError` as well — the spec brands each prototype separately, which
/// is why one shared implementation still has to check the kind.
fn this_weak(interp: &Interp<'_>, this: &Value, want_map: bool) -> JsResult<(Gc<JsObject>, bool)> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    let is_map = match object.weak_is_set() {
        Some(is_set) => !is_set,
        None => {
            let name = if want_map { "WeakMap" } else { "WeakSet" };
            return Err(interp.type_error(format!("{name} object expected")));
        }
    };
    if is_map != want_map {
        let name = if want_map { "WeakMap" } else { "WeakSet" };
        return Err(interp.type_error(format!("{name} object expected")));
    }
    Ok((object, is_map))
}

/// `new WeakMap(iterable)`.
fn weak_map_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    new_weak_collection(interp, this, args, true)
}

/// `new WeakSet(iterable)`.
fn weak_set_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    new_weak_collection(interp, this, args, false)
}

/// The shared body of the two constructors.
fn new_weak_collection(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
    is_map: bool,
) -> JsResult<Value> {
    if !interp.is_constructing() {
        return Err(interp.type_error("must be called with new"));
    }
    let default_proto =
        match constructor_prototype(interp, if is_map { "WeakMap" } else { "WeakSet" }) {
            Some(proto) => proto,
            None => interp.realm.object_proto.clone(),
        };
    // `new.target.prototype` is read *after* the arguments are validated in the spec, but
    // for these two there is nothing to validate before the instance exists, so building it
    // first is observable only through a `prototype` getter — which the reference also does
    // not hit early.
    let object = interp.new_ctor_instance(default_proto)?;
    object.set_weak_collection(Vec::new(), !is_map);

    // `AddEntriesFromIterable`: a non-iterable, non-undefined argument is a `TypeError` from
    // `GetIterator`, and each entry of a `WeakMap` must itself be a two-element object.
    //
    // Iterated one step at a time rather than through `collect_iterable`, because the body
    // can throw (a poisoned `[[1]]` getter, an unyieldable entry) and an abrupt completion
    // has to close the iterator. Draining first would read an *infinite* iterator to the end
    // before noticing: `iterator-item-second-entry-returns-abrupt` is exactly that shape,
    // and draining it is a hang rather than a failure.
    let source = arg(args, 0);
    if matches!(source, Value::Undefined | Value::Null) {
        return Ok(Value::Object(object));
    }
    // §24.2.1.1 step 7: the *adder* is `Get(instance, "set")` (or `"add"`) and is called for
    // every entry. Going straight to the internal slot instead would hide two things the
    // spec makes observable: a script that replaces `WeakMap.prototype.set` must see the
    // constructor's calls, and a non-callable `set` must make `new WeakMap([])` a
    // `TypeError` even though there is nothing to insert.
    let adder_name = if is_map { "set" } else { "add" };
    let adder = interp
        .get_property(&object, adder_name)?
        .unwrap_or(Value::Undefined);
    if !adder.as_object().is_some_and(|adder| adder.is_callable()) {
        return Err(interp.type_error(format!("{adder_name} is not callable")));
    }
    let iterator = interp.get_iterator(&source)?;
    loop {
        let (entry, done) = interp.iterator_step(&iterator)?;
        if done {
            break;
        }
        // `IfAbruptCloseIterator` around the body: whatever goes wrong here, the iterator
        // is told, and the original error is the one that propagates.
        let body = (|| -> JsResult<()> {
            if is_map {
                // `Get(nextItem, "0")` then `Get(nextItem, "1")`, both observable.
                let Some(pair) = entry.as_object().cloned() else {
                    return Err(interp.type_error("iterator value is not an entry object"));
                };
                let key = interp.get_property(&pair, "0")?.unwrap_or(Value::Undefined);
                let value = interp.get_property(&pair, "1")?.unwrap_or(Value::Undefined);
                interp.call_value(
                    adder.clone(),
                    Value::Object(object.clone()),
                    vec![key, value],
                )?;
            } else {
                interp.call_value(adder.clone(), Value::Object(object.clone()), vec![entry])?;
            }
            Ok(())
        })();
        if let Err(signal) = body {
            let _ = close_iterator(interp, &iterator);
            return Err(signal);
        }
    }
    Ok(Value::Object(object))
}

/// `WeakMap.prototype.set`'s and the constructor's shared "insert one pair" step.
fn set_weak_entry(
    interp: &Interp<'_>,
    object: &Gc<JsObject>,
    key: &Value,
    value: Value,
) -> JsResult<()> {
    // `CanBeHeldWeakly`: an object, or a *registered* symbol. A plain `Symbol()` is a fresh
    // primitive every time it is written, so there is nothing whose lifetime could be
    // observed — but `Symbol.for("k")` answers the same symbol for the same key and can be
    // referenced again, so the proposal allows it.
    let Some(key) = weak_key(key) else {
        return Err(interp.type_error("invalid value used as WeakMap key"));
    };
    object.weak_set(key, value);
    Ok(())
}

/// `WeakSet.prototype.add`'s and the constructor's shared "insert one key" step.
fn add_weak_entry(interp: &Interp<'_>, object: &Gc<JsObject>, key: &Value) -> JsResult<()> {
    let Some(key) = weak_key(key) else {
        return Err(interp.type_error("invalid value used as WeakSet key"));
    };
    object.weak_set(key, Value::Undefined);
    Ok(())
}

/// `WeakMap.prototype.set(key, value)`.
fn weak_set(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, _) = this_weak(interp, &this, true)?;
    set_weak_entry(interp, &object, &arg(args, 0), arg(args, 1))?;
    Ok(this)
}

/// `WeakSet.prototype.add(value)`.
fn weak_add(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, _) = this_weak(interp, &this, false)?;
    add_weak_entry(interp, &object, &arg(args, 0))?;
    Ok(this)
}

/// `WeakMap.prototype.get(key)` and, for a `WeakSet`, the value behind `has`/`delete`.
///
/// A key that is not an object answers `undefined`/`false` rather than throwing: the
/// spec's `CanBeHeldWeakly` check returns early instead of raising.
fn weak_lookup(_interp: &Interp<'_>, object: &Gc<JsObject>, key: &Value) -> Option<Value> {
    object
        .weak_entries()?
        .into_iter()
        .find(|(existing, _)| existing.same_value(key))
        .map(|(_, value)| value)
}

/// `WeakMap.prototype.get(key)`.
fn weak_get(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, _) = this_weak(interp, &this, true)?;
    Ok(weak_lookup(interp, &object, &arg(args, 0)).unwrap_or(Value::Undefined))
}

/// `WeakMap.prototype.has(key)` / `WeakSet.prototype.has(value)`.
fn weak_has(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    if object.weak_is_set().is_none() {
        return Err(interp.type_error("invalid receiver"));
    }
    Ok(Value::Bool(
        weak_lookup(interp, &object, &arg(args, 0)).is_some(),
    ))
}

/// `WeakMap.prototype.delete(key)` / `WeakSet.prototype.delete(value)`.
fn weak_delete(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    if object.weak_is_set().is_none() {
        return Err(interp.type_error("invalid receiver"));
    }
    Ok(Value::Bool(object.weak_delete(&arg(args, 0))))
}

/// `WeakMap.prototype.getOrInsert(key, value)`.
fn weak_get_or_insert(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, _) = this_weak(interp, &this, true)?;
    if let Some(found) = weak_lookup(interp, &object, &arg(args, 0)) {
        return Ok(found);
    }
    let value = arg(args, 1);
    set_weak_entry(interp, &object, &arg(args, 0), value.clone())?;
    Ok(value)
}

/// `WeakMap.prototype.getOrInsertComputed(key, callback)`.
///
/// The callback runs *only* on a miss, and it runs before the entry is stored — so a
/// re-entrant insert from inside the callback wins, which is what `GetOrInsertComputed`
/// specifies and what a test checks.
fn weak_get_or_insert_computed(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let (object, _) = this_weak(interp, &this, true)?;
    let key = arg(args, 0);
    let Some(key_handle) = weak_key(&key) else {
        return Err(interp.type_error("invalid value used as WeakMap key"));
    };
    let callback = arg(args, 1);
    if !callback
        .as_object()
        .is_some_and(|object| object.is_callable())
    {
        return Err(interp.type_error("callback is not callable"));
    }
    if let Some(found) = weak_lookup(interp, &object, &key) {
        return Ok(found);
    }
    let computed = interp.call_value(callback, Value::Undefined, vec![key.clone()])?;
    // No second lookup: the upsert stages say "set the value, append the record" *after* the
    // callback returns, unconditionally. Re-checking here would keep whatever the callback
    // inserted and made `map.getOrInsertComputed(k, () => { map.set(k, 0); return 3 })`
    // answer `0` where the spec settles on `3`. Note that `bar`'s callback returns
    // `undefined`, and that `undefined` does overwrite.
    object.weak_set(key_handle, computed.clone());
    Ok(computed)
}

/// `WeakRef`.
fn install_weak_ref(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor = interp.new_native_constructor("WeakRef", 1, weak_ref_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    define_method(&proto, "deref", 0, deref, interp);
    install_tag(interp, &proto, "WeakRef");
    let global = interp.realm.global.clone();
    global.define(
        "WeakRef",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `new WeakRef(target)`.
fn weak_ref_constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    if !interp.is_constructing() {
        return Err(interp.type_error("constructor requires 'new'"));
    }
    // The target has to be an object: a primitive cannot be weakly referenced because there
    // would be nothing whose lifetime to observe. The error text matches the reference.
    let Some(target) = weak_key(&arg(args, 0)) else {
        return Err(interp.type_error("invalid target"));
    };
    let default_proto = match constructor_prototype(interp, "WeakRef") {
        Some(proto) => proto,
        None => interp.realm.object_proto.clone(),
    };
    let object = interp.new_ctor_instance(default_proto)?;
    object.set_weak_ref(target);
    Ok(Value::Object(object))
}

/// `WeakRef.prototype.deref()`.
///
/// Answers the target while it is alive and `undefined` afterwards. Because the engine
/// frees on the last strong release, this becomes `undefined` *immediately* once nothing
/// else holds the target — which is the strongest form of the guarantee the spec requires
/// ("no earlier than" the end of the current job).
fn deref(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let object = this
        .as_object()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    let target = object
        .weak_ref_target()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    Ok(target.upgrade().unwrap_or(Value::Undefined))
}

/// `FinalizationRegistry`.
fn install_finalization(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(object_proto.clone()));
    let constructor =
        interp.new_native_constructor("FinalizationRegistry", 1, finalization_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    define_method(&proto, "register", 2, register, interp);
    define_method(&proto, "unregister", 1, unregister, interp);
    define_method(&proto, "cleanupSome", 0, cleanup_some, interp);
    install_tag(interp, &proto, "FinalizationRegistry");
    let global = interp.realm.global.clone();
    global.define(
        "FinalizationRegistry",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `new FinalizationRegistry(cleanupCallback)`.
fn finalization_constructor(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    if !interp.is_constructing() {
        return Err(interp.type_error("constructor requires 'new'"));
    }
    // The callback is validated *before* the instance is built, so
    // `Reflect.construct(FinalizationRegistry, [1], badTarget)` throws for the callback
    // rather than for the poisoned `prototype`.
    let cleanup = arg(args, 0);
    if !cleanup
        .as_object()
        .is_some_and(|object| object.is_callable())
    {
        return Err(interp.type_error("argument must be a function"));
    }
    let default_proto = match constructor_prototype(interp, "FinalizationRegistry") {
        Some(proto) => proto,
        None => interp.realm.object_proto.clone(),
    };
    let object = interp.new_ctor_instance(default_proto)?;
    object.set_finalization_state(FinalizationState {
        registrations: Vec::new(),
        cleanup,
    });
    // Tell the safe point about this registry. The handle is weak, so the registry stays
    // collectable and being on this list never keeps it — or its held values — alive.
    interp
        .realm
        .finalization_registries
        .push(object.downgrade());
    Ok(Value::Object(object))
}

/// The registry behind `this`, or a `TypeError`.
fn this_registry(interp: &Interp<'_>, this: &Value) -> JsResult<(Gc<JsObject>, FinalizationState)> {
    let object = this
        .as_object()
        .cloned()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    let state = object
        .finalization_state()
        .ok_or_else(|| interp.type_error("invalid receiver"))?;
    Ok((object, *state))
}

/// `FinalizationRegistry.prototype.register(target, heldValue, unregisterToken)`.
fn register(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, mut state) = this_registry(interp, &this)?;
    let target_value = arg(args, 0);
    let Some(target) = weak_key(&target_value) else {
        return Err(interp.type_error("invalid target"));
    };
    // The held value must not be the target: the registry keeps held values alive, so
    // `fr.register(t, t)` would keep `t` alive through the registry forever and the
    // callback could never fire. The spec refuses it for exactly that reason.
    let held_value = arg(args, 1);
    if held_value.same_value(&target_value) {
        return Err(interp.type_error("the held value must not be the target itself"));
    }
    // The token, on the other hand, *may* be the target: §26.2.3.1 step 5 only requires it
    // to be an object or `undefined`. A registry does not keep tokens alive, so using the
    // target as its own token cannot resurrect it — `unregisterToken-same-as-target` asks
    // for precisely this and expects success.
    let token = match args.get(2) {
        None | Some(Value::Undefined) => None,
        Some(token) => {
            // The token follows `CanBeHeldWeakly`: an object, or an *unregistered* symbol.
            // Unlike the target, a registered symbol is refused here. §26.2.3.1 is about
            // what the registry can hold weakly, and a registered symbol never dies.
            if weak_key(token).is_none() {
                return Err(interp.type_error("invalid unregister token"));
            }
            Some(token.clone())
        }
    };
    state.registrations.push(Registration {
        target,
        held_value,
        token,
    });
    object.set_finalization_state(state);
    Ok(Value::Undefined)
}

/// `FinalizationRegistry.prototype.unregister(token)`, answering whether anything was
/// removed. `SameValue` decides identity, so `0` and `-0` are one token and `NaN` is its
/// own — which is why this compares with `Value::same_value` rather than a hash.
fn unregister(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, mut state) = this_registry(interp, &this)?;
    let token = arg(args, 0);
    // The same `CanBeHeldWeakly` rule as `register`: an object, or a symbol that is not in
    // the registry. A *well-known* symbol qualifies (it is one unique allocation), while
    // `Symbol.for("k")` does not — so `unregister(Symbol.hasInstance)` answers `false`
    // where `unregister(Symbol.for("s"))` is a `TypeError`. A primitive is a `TypeError`
    // rather than `false`.
    if weak_key(&token).is_none() {
        return Err(interp.type_error("invalid unregister token"));
    }
    let before = state.registrations.len();
    state
        .registrations
        .retain(|registration| match &registration.token {
            Some(existing) => !existing.same_value(&token),
            None => true,
        });
    let removed = state.registrations.len() != before;
    object.set_finalization_state(state);
    Ok(Value::Bool(removed))
}

/// `FinalizationRegistry.prototype.cleanupSome(callback)`.
///
/// Runs the registrations whose targets have died *now*, optionally through a one-off
/// callback instead of the one the registry was built with.
fn cleanup_some(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let (object, state) = this_registry(interp, &this)?;
    let callback = match arg(args, 0) {
        Value::Undefined => state.cleanup.clone(),
        other => {
            if !other.as_object().is_some_and(|object| object.is_callable()) {
                return Err(interp.type_error("argument must be a function"));
            }
            other
        }
    };
    let dead: Vec<Value> = state
        .registrations
        .iter()
        .filter(|registration| registration.target.is_expired())
        .map(|registration| registration.held_value.clone())
        .collect();
    // Drop them first: the callback may throw, and a registration whose held value was
    // already handed over must not be handed over twice.
    let mut remaining = state;
    remaining
        .registrations
        .retain(|registration| !registration.target.is_expired());
    object.set_finalization_state(remaining);
    for held in dead {
        interp.call_value(callback.clone(), Value::Undefined, vec![held])?;
    }
    Ok(Value::Undefined)
}

/// Run every expired registration, across every registry on the realm.
///
/// Called from the microtask checkpoint rather than from a drop hook: see this module's
/// header for why. The registries are found by walking the heap's live objects, which is
/// acceptable because this runs once per drain and not per operation.
pub fn drain_finalizers(interp: &mut Interp<'_>) -> JsResult<()> {
    let registries = interp.live_registries();
    for (object, state) in registries {
        let dead: Vec<Value> = state
            .registrations
            .iter()
            .filter(|registration| registration.target.is_expired())
            .map(|registration| registration.held_value.clone())
            .collect();
        if dead.is_empty() {
            continue;
        }
        let mut remaining = state;
        remaining
            .registrations
            .retain(|registration| !registration.target.is_expired());
        let cleanup = remaining.cleanup.clone();
        object.set_finalization_state(remaining);
        for held in dead {
            interp.call_value(cleanup.clone(), Value::Undefined, vec![held])?;
        }
    }
    Ok(())
}

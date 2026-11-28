//! `Object` and `Object.prototype`.
//!
//! This is the most load-bearing module in the library: `hasOwnProperty`,
//! `toString` and `Object.keys` are what test helpers and scripts reach for first,
//! and `Object.defineProperty` is how the rest of the library would describe its own
//! properties.
//!
//! `Object.prototype.toString` is also where the `[object X]` tag comes from, which
//! `assert.js` compares with `==`. The tag is decided the way the spec decides it —
//! arrays are `Array`, functions are `Function`, `null` and `undefined` are their own
//! names — not by inspecting a class field, because a value without a class still has
//! to answer.

use crate::builtins::{arg, define_method, this_object};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, ObjectKind, Property, PropertyKey, array_index};
use crate::value::Value;
use js_gc::Gc;

/// Install `Object` and its prototype methods.
pub fn install(interp: &mut Interp<'_>, object_proto: &Gc<JsObject>) {
    define_method(object_proto, "toString", 0, to_string, interp);
    define_method(object_proto, "toLocaleString", 0, to_string, interp);
    define_method(object_proto, "valueOf", 0, value_of, interp);
    define_method(object_proto, "hasOwnProperty", 1, has_own_property, interp);
    // The `__defineGetter__` family (Annex B.2.2.2). Deprecated but not removable: plenty of
    // existing code uses them, and the reference still ships all four.
    define_method(object_proto, "__lookupGetter__", 1, lookup_getter, interp);
    define_method(object_proto, "__lookupSetter__", 1, lookup_setter, interp);
    define_method(object_proto, "__defineGetter__", 2, define_getter, interp);
    define_method(object_proto, "__defineSetter__", 2, define_setter, interp);
    define_method(object_proto, "isPrototypeOf", 1, is_prototype_of, interp);
    define_method(
        object_proto,
        "propertyIsEnumerable",
        1,
        property_is_enumerable,
        interp,
    );

    // `__proto__` is an accessor on `Object.prototype`, not a magic property of every
    // object: `({}).__proto__` finds it here, and a plain `{__proto__: x}` literal never
    // goes through it (the parser handles that form itself).
    let getter = interp.new_native("get __proto__", 0, proto_getter);
    let setter = interp.new_native("set __proto__", 1, proto_setter);
    object_proto.define(
        "__proto__",
        Property {
            getter: Some(Value::Object(getter)),
            setter: Some(Value::Object(setter)),
            enumerable: false,
            configurable: true,
            ..Property::default()
        },
    );

    let constructor = interp.new_native_constructor("Object", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(object_proto.clone()), false, false),
    );
    object_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    for (name, length, func) in [
        ("keys", 1, keys as crate::object::NativeFn),
        ("values", 1, values),
        ("entries", 1, entries),
        ("assign", 2, assign),
        ("create", 2, create),
        ("getPrototypeOf", 1, get_prototype_of),
        ("setPrototypeOf", 2, set_prototype_of),
        ("defineProperty", 3, define_property),
        ("defineProperties", 2, define_properties),
        ("hasOwn", 2, has_own),
        ("getOwnPropertyDescriptors", 1, get_own_property_descriptors),
        ("groupBy", 2, group_by),
        ("getOwnPropertyNames", 1, get_own_property_names),
        ("getOwnPropertySymbols", 1, get_own_property_symbols),
        ("getOwnPropertyDescriptor", 2, get_own_property_descriptor),
        ("freeze", 1, freeze),
        ("isFrozen", 1, is_frozen),
        ("seal", 1, seal),
        ("isSealed", 1, is_sealed),
        ("preventExtensions", 1, prevent_extensions),
        ("isExtensible", 1, is_extensible),
        ("is", 2, is),
        ("fromEntries", 1, from_entries),
    ] {
        define_method(&constructor, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define(
        "Object",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `Object(value)`: a new object for `null`/`undefined`, the value itself for an
/// object, a box for a primitive.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    match args.first() {
        None | Some(Value::Undefined) | Some(Value::Null) => {
            let proto = interp.realm.object_proto.clone();
            Ok(Value::Object(interp.new_object(Some(proto))))
        }
        Some(value @ Value::Object(_)) => Ok(value.clone()),
        Some(primitive) => {
            let boxed = this_object(interp, primitive)?;
            Ok(Value::Object(boxed))
        }
    }
}

/// The `[object X]` tag, per `Object.prototype.toString`.
fn tag_of(interp: &Interp<'_>, value: &Value) -> String {
    // A proxy reports the tag of what it stands in for, which is what makes
    // `Object.prototype.toString.call(new Proxy([], {}))` say `[object Array]` and a proxy
    // over a generator function say `[object GeneratorFunction]`. Resolved up front by
    // recursing on the target, because the tag depends on the target's *kind* and the
    // match below only sees a `Proxy`.
    if let Value::Object(object) = value
        && let Some(state) = object.proxy_state()
        && let Some(target) = state.borrow().target.clone()
    {
        return tag_of(interp, &Value::Object(target));
    }
    // `Symbol.toStringTag` wins over the built-in tag. A generator proxy inherits
    // `"GeneratorFunction"` from `%GeneratorFunction.prototype%`, and deleting that
    // property has to be observable — so this cannot be a special case for plain objects
    // only.
    if let Value::Object(object) = value
        && let Some(tag) = own_or_inherited_tag(interp, object)
    {
        return tag;
    }
    match value {
        Value::Undefined => "Undefined".to_string(),
        Value::Null => "Null".to_string(),
        Value::Bool(_) => "Boolean".to_string(),
        Value::Int(_) | Value::Float(_) => "Number".to_string(),
        Value::String(_) => "String".to_string(),
        Value::Symbol(_) => "Symbol".to_string(),
        // Missing here made `Object.prototype.toString.call(10n)` say `[object Object]`.
        Value::BigInt(_) => "BigInt".to_string(),
        Value::Object(object) => match object.with(|data| match &data.kind {
            ObjectKind::Array { .. } => "Array",
            // A function's tag names its *kind*: `[object Function]` is only right for a
            // plain one. `async` → `AsyncFunction`, `function*` → `GeneratorFunction`,
            // `async function*` → `AsyncGeneratorFunction`.
            // A function's tag comes from `Symbol.toStringTag` on its prototype, which is
            // resolved above; by the time we get here there was no string tag, so the
            // answer is plain `Function`. Hard-coding the kind here instead made
            // `delete g.constructor.prototype[Symbol.toStringTag]` invisible.
            ObjectKind::Function(_) => "Function",
            ObjectKind::Generator(_) => "Generator",
            ObjectKind::RegExp(_) => "RegExp",
            ObjectKind::DisposableStack(_) => "Object",
            ObjectKind::ArrayBuffer(_) => "ArrayBuffer",
            ObjectKind::Collection { is_set, .. } => {
                if *is_set {
                    "Set"
                } else {
                    "Map"
                }
            }
            ObjectKind::TypedArray { .. } | ObjectKind::DataView { .. } => "Object",
            ObjectKind::Ordinary => "Object",
            ObjectKind::Promise(_) => "Promise",
            // A *revoked* proxy has no target to recurse into, and answers `Object`.
            ObjectKind::Proxy(_) => "Object",
            // These four all name themselves through `Symbol.toStringTag` on their
            // prototype, which is resolved before this function is reached, so a deleted
            // tag leaves plain `Object`.
            ObjectKind::WeakCollection { .. }
            | ObjectKind::WeakRef { .. }
            | ObjectKind::Finalization(_) => "Object",
        }) {
            "Array" => "Array".to_string(),
            "Function" => "Function".to_string(),
            "Generator" => "Generator".to_string(),
            "RegExp" => "RegExp".to_string(),
            other => {
                // A boxed primitive reports the primitive's own tag: `Object(sym)` is
                // `[object Symbol]`, not `[object Object]`. `_value` is precisely the "this
                // object is a box" marker, and nothing else uses it — a Date keeps its
                // timestamp in `_date_value`.
                if let Some(boxed) = interp.internal_get(object, "_value") {
                    let tag = match boxed {
                        Value::Bool(_) => Some("Boolean"),
                        Value::String(_) => Some("String"),
                        Value::Int(_) | Value::Float(_) => Some("Number"),
                        Value::Symbol(_) => Some("Symbol"),
                        _ => None,
                    };
                    if let Some(tag) = tag {
                        return tag.to_string();
                    }
                }
                // An arguments object, a RegExp and so on are distinguished by an
                // internal tag; `_class` carries it for the classes that have one
                // (Date, the error classes), and everything else answers `Object`,
                // which is what a plain `{}` does.
                match interp.internal_get(object, "_class") {
                    Some(class) => class.to_js_string(),
                    None => other.to_string(),
                }
            }
        },
        _ => "Object".to_string(),
    }
}

fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // `Symbol.toStringTag` overrides the built-in tag: `{[Symbol.toStringTag]: "Mine"}`
    // stringifies as `[object Mine]`. It applies to objects only, and only when the
    // property is a string.
    if let Value::Object(object) = &this {
        let key = interp
            .realm
            .well_known_key(crate::interp::WellKnown::ToStringTag);
        if let Some(Value::String(tag)) = interp.get_property_key(object, &key)? {
            let tag = tag.to_string_lossy().into_owned();
            return Ok(interp.string(format!("[object {tag}]")));
        }
    }
    let tag = tag_of(interp, &this);
    Ok(interp.string(format!("[object {tag}]")))
}

fn value_of(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // `Object.prototype.valueOf` returns the object, unboxing a primitive receiver.
    match this {
        Value::Object(object) => Ok(interp
            .internal_get(&object, "_value")
            .unwrap_or(Value::Object(object))),
        other => Ok(other),
    }
}

fn has_own_property(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this_object(interp, &this)?;
    let key = interp.property_key(&arg(args, 0))?;
    // `[[GetOwnProperty]]`, not a peek at the property table: a proxy answers this with
    // its `getOwnPropertyDescriptor` trap, and it may still be holding a `TypeError` for
    // a handler that lied about a fixed property.
    Ok(Value::Bool(
        interp.object_get_own_property(&object, &key)?.is_some(),
    ))
}

/// `Object.hasOwn`: `[[GetOwnProperty]]` on a `ToObject`'d target.
///
/// Not `o.hasOwnProperty(k)`: this one works on a primitive and on an object that
/// shadows or replaces `hasOwnProperty`, which is the whole reason it exists.
fn has_own(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let object = interp.object_from(&arg(args, 0))?;
    let key = interp.property_key(&arg(args, 1))?;
    Ok(Value::Bool(
        interp.object_get_own_property(&object, &key)?.is_some(),
    ))
}

/// `Object.getOwnPropertyDescriptors`: every own descriptor, keyed the same way.
fn get_own_property_descriptors(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let object = interp.object_from(&arg(args, 0))?;
    // `Object.groupBy` and this one both build a result whose keys come from data, so the
    // prototype is `null`: a data key called `toString` must not collide with a method.
    let result = interp.new_object(None);
    for key in interp.object_own_property_keys(&object)? {
        let Some(property) = interp.object_get_own_property(&object, &key)? else {
            continue;
        };
        let descriptor = describe(interp, &property);
        result.define_key(&key, Property::with_attributes(descriptor, true, true));
    }
    Ok(Value::Object(result))
}

/// The string `Symbol.toStringTag` resolves to, following the prototype chain.
fn own_or_inherited_tag(interp: &Interp<'_>, object: &Gc<JsObject>) -> Option<String> {
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::ToStringTag);
    let mut current = Some(object.clone());
    for _ in 0..10_000 {
        let found = current?;
        if let Some(property) = found.own_property_key(&key)
            && let Some(value) = property.value
        {
            return match value {
                Value::String(text) => Some(text.to_string_lossy().into_owned()),
                // A non-string tag is *ignored*: `Object.prototype.toString` only uses a
                // string one, which is what `symbol-tag-non-str-proxy-function` checks.
                _ => None,
            };
        }
        current = found.proto();
    }
    None
}

/// One property as the descriptor object `getOwnPropertyDescriptor` answers with.
fn describe(interp: &Interp<'_>, property: &Property) -> Value {
    let proto = interp.realm.object_proto.clone();
    let descriptor = interp.new_object(Some(proto));
    if property.is_accessor() {
        descriptor.define_data("get", property.getter.clone().unwrap_or(Value::Undefined));
        descriptor.define_data("set", property.setter.clone().unwrap_or(Value::Undefined));
    } else {
        descriptor.define_data("value", property.value.clone().unwrap_or(Value::Undefined));
        descriptor.define_data("writable", Value::Bool(property.writable));
    }
    descriptor.define_data("enumerable", Value::Bool(property.enumerable));
    descriptor.define_data("configurable", Value::Bool(property.configurable));
    Value::Object(descriptor)
}

/// `Object.groupBy(items, callback)`: bucket the elements by `callback`'s answer.
///
/// The result has a **null** prototype and its buckets are ordinary arrays. A key of
/// `"__proto__"` is therefore just a key: with `Object.prototype` in the chain, writing it
/// would have replaced the result's prototype instead of creating a bucket.
fn group_by(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let items = arg(args, 0);
    let callback = arg(args, 1);
    if !matches!(&callback, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("not a function"));
    }
    let result = interp.new_object(None);
    let iterator = interp.get_iterator(&items)?;
    let mut index = 0i32;
    loop {
        let (value, done) = interp.iterator_step(&iterator)?;
        if done {
            break;
        }
        let key = interp.call_value(
            callback.clone(),
            Value::Undefined,
            vec![value.clone(), Value::Int(index)],
        )?;
        // `ToPropertyKey`, so a symbol key stays a symbol and `1` becomes `"1"`.
        let key = interp.property_key(&key)?;
        let bucket = match result.own_property_key(&key) {
            Some(existing) => existing.value.unwrap_or(Value::Undefined),
            None => {
                let bucket = interp.new_array(Vec::new());
                let bucket = Value::Object(bucket);
                result.define_key(&key, Property::with_attributes(bucket.clone(), true, true));
                bucket
            }
        };
        if let Value::Object(array) = &bucket {
            let length = array.array_length().unwrap_or(0);
            array.array_set(length, value);
            array.array_set_length(length + 1);
        }
        index += 1;
    }
    Ok(Value::Object(result))
}

/// `__lookupGetter__`: walk the chain for an accessor with a getter.
fn lookup_getter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    lookup_accessor(interp, this, args, true)
}

/// `__lookupSetter__`.
fn lookup_setter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    lookup_accessor(interp, this, args, false)
}

fn lookup_accessor(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    want_getter: bool,
) -> JsResult<Value> {
    let object = interp.object_from(&this)?;
    let key = interp.property_key(&arg(args, 0))?;
    let mut current = Some(object);
    // Bounded: a proxy could answer `getPrototypeOf` with a cycle, and `for…in` over the
    // same object has the same problem.
    for _ in 0..10_000 {
        let Some(object) = current else {
            return Ok(Value::Undefined);
        };
        if let Some(property) = interp.object_get_own_property(&object, &key)?
            && property.is_accessor()
        {
            let half = if want_getter {
                property.getter
            } else {
                property.setter
            };
            return Ok(half.unwrap_or(Value::Undefined));
        }
        current = interp.object_prototype_of(&object)?;
    }
    Ok(Value::Undefined)
}

/// `__defineGetter__`: an accessor with this getter, keeping any existing setter.
fn define_getter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    define_accessor(interp, this, args, true)
}

/// `__defineSetter__`.
fn define_setter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    define_accessor(interp, this, args, false)
}

fn define_accessor(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
    is_getter: bool,
) -> JsResult<Value> {
    let object = interp.object_from(&this)?;
    let function = arg(args, 1);
    if !matches!(&function, Value::Object(object) if object.is_callable()) {
        return Err(interp.type_error("not a function"));
    }
    let key = interp.property_key(&arg(args, 0))?;
    // Annex B.2.2.2: unspecified attributes default to `true`, unlike
    // `Object.defineProperty`, and the *other* half of an existing accessor is preserved.
    let existing = interp.object_get_own_property(&object, &key)?;
    let mut property = Property {
        value: None,
        getter: None,
        setter: None,
        writable: false,
        enumerable: existing.as_ref().is_none_or(|p| p.enumerable),
        configurable: existing.as_ref().is_none_or(|p| p.configurable),
    };
    if let Some(existing) = &existing
        && existing.is_accessor()
    {
        property.getter = existing.getter.clone();
        property.setter = existing.setter.clone();
    }
    if is_getter {
        property.getter = Some(function);
    } else {
        property.setter = Some(function);
    }
    interp.object_define_own_property(&object, &key, property)?;
    Ok(Value::Undefined)
}

fn is_prototype_of(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let proto = this_object(interp, &this)?;
    let Some(target) = arg(args, 0).as_object().cloned() else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(
        target
            .chain()
            .iter()
            .any(|ancestor| ancestor.ptr_eq(&proto)),
    ))
}

fn property_is_enumerable(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this_object(interp, &this)?;
    let key = arg(args, 0).to_js_string();
    let enumerable = match object.own_property(&key) {
        Some(property) => property.enumerable,
        None => array_index(&key)
            .and_then(|index| object.array_get(index as usize))
            .is_some(),
    };
    Ok(Value::Bool(enumerable))
}

/// The own keys of `value`, in the order the spec requires: integer-like keys
/// ascending, then the rest in insertion order.
pub fn own_keys(value: &Value) -> Vec<crate::value::Wtf8> {
    let Some(object) = value.as_object() else {
        // A string has index keys and `length`.
        if let Value::String(text) = value {
            return (0..text.utf16_len())
                .map(|i| crate::value::Wtf8::new(i.to_string()))
                .collect();
        }
        return Vec::new();
    };
    let mut numeric: Vec<u32> = Vec::new();
    let mut strings: Vec<crate::value::Wtf8> = Vec::new();
    for index in 0..object.array_length().unwrap_or(0) {
        if object.array_get(index).is_some() {
            numeric.push(index as u32);
        }
    }
    for key in object.own_keys() {
        // An array index is ASCII, so it always has a `&str` form; anything else is an
        // ordinary key and keeps its code units.
        match key.as_str().and_then(array_index) {
            Some(index) => {
                if !numeric.contains(&index) {
                    numeric.push(index);
                }
            }
            None => strings.push(key),
        }
    }
    numeric.sort_unstable();
    let mut out: Vec<crate::value::Wtf8> = numeric
        .into_iter()
        .map(|n| crate::value::Wtf8::new(n.to_string()))
        .collect();
    out.extend(strings);
    out
}

fn keys(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    let Some(_object) = value.as_object() else {
        // `Object.keys("ab")` is `["0", "1"]`; `Object.keys(1)` is `[]`.
        return match value {
            Value::String(text) => {
                let keys: Vec<Value> = (0..text.utf16_len())
                    .map(|index| interp.string(index.to_string()))
                    .collect();
                let array = interp.new_array(keys);
                Ok(Value::Object(array))
            }
            _ => {
                let array = interp.new_array(Vec::new());
                Ok(Value::Object(array))
            }
        };
    };
    // A proxy reports its own key list, and asks about each key's enumerability one at
    // a time — `Object.keys(proxyOfFrozen)` must not see a key the handler hides.
    let keys: Vec<Value> = own_enumerable_keys(interp, &value)?
        .iter()
        .map(|key| interp.key_value(key))
        .collect();
    let array = interp.new_array(keys);
    Ok(Value::Object(array))
}

/// The own enumerable **string** keys of `value`, in the order `[[OwnPropertyKeys]]`
/// reports them.
///
/// This goes through the interpreter rather than reading the object directly, which is
/// what makes `Object.keys(proxy)` ask the handler: a proxy's own property table is empty
/// by design, so reading it would answer "no keys" for every proxy.
fn own_enumerable_keys(interp: &mut Interp<'_>, value: &Value) -> JsResult<Vec<PropertyKey>> {
    let Some(object) = value.as_object().cloned() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for key in interp.object_own_property_keys(&object)? {
        if key.is_symbol() {
            continue;
        }
        let enumerable = interp
            .object_get_own_property(&object, &key)?
            .is_some_and(|property| property.enumerable);
        if enumerable {
            out.push(key);
        }
    }
    Ok(out)
}

fn values(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    let mut out = Vec::new();
    if let Value::Object(object) = &value {
        for key in own_enumerable_keys(interp, &value)? {
            if let Some(found) = interp.get_property_key(object, &key)? {
                out.push(found);
            }
        }
    }
    let _ = this;
    let array = interp.new_array(out);
    Ok(Value::Object(array))
}

fn entries(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let value = arg(args, 0);
    let mut out = Vec::new();
    if let Value::Object(object) = &value {
        for key in own_enumerable_keys(interp, &value)? {
            if let Some(found) = interp.get_property_key(object, &key)? {
                let name = interp.key_value(&key);
                let pair = interp.new_array(vec![name, found]);
                out.push(Value::Object(pair));
            }
        }
    }
    let array = interp.new_array(out);
    Ok(Value::Object(array))
}

fn assign(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `ToObject(target)`: a primitive target is boxed and the box is returned, so
    // `Object.assign(1, {a: 1})` answers a Number object. Refusing the primitive
    // outright was wrong, and it is the shape most `Object.assign` tests use.
    let target_object = interp.object_from(&arg(args, 0))?;
    let target = Value::Object(target_object.clone());
    for source in args.iter().skip(1) {
        // `ToObject(source)` too: a string source has index properties, so
        // `Object.assign({}, "ab")` copies `0` and `1`. `null`/`undefined` are skipped
        // rather than converted, which is the one exception the spec makes here.
        if matches!(source, Value::Undefined | Value::Null) {
            continue;
        }
        let source_object = interp.object_from(source)?;
        let source = &Value::Object(source_object);
        // `Object.assign` copies exactly the own enumerable keys, so it goes through
        // `[[OwnPropertyKeys]]` and `[[Get]]` — which is what makes it see a proxy's
        // handler rather than the empty property table behind it.
        for as_key in own_enumerable_keys(interp, source)? {
            if let Some(found) =
                interp.get_property_key(source.as_object().expect("checked above"), &as_key)?
            {
                interp.set_property_with_key(
                    &target_object,
                    &as_key,
                    found,
                    crate::interp::DeleteFailure::Throw,
                )?;
            }
        }
    }
    Ok(target)
}

fn create(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let proto = match arg(args, 0) {
        Value::Object(object) => Some(object),
        Value::Null => None,
        other => {
            return Err(interp.type_error(format!(
                "Object.create prototype must be an object or null, got {}",
                other.type_of()
            )));
        }
    };
    let object = interp.new_object(proto);
    // The second argument, `PropertiesFromDescriptors`, was read and then dropped: the
    // object came out empty and `Object.create(null, {a: {value: 1}})` had no `a` at all.
    // Five test262 cases depend on it just in the Iterator area, because a
    // null-prototype object is how those tests build a clean properties bag.
    let properties = arg(args, 1);
    if !matches!(properties, Value::Undefined) {
        define_properties(
            interp,
            Value::Undefined,
            &[Value::Object(object.clone()), properties],
        )?;
    }
    Ok(Value::Object(object))
}

fn get_prototype_of(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    match arg(args, 0) {
        Value::Object(object) => Ok(interp
            .object_prototype_of(&object)?
            .map_or(Value::Null, Value::Object)),
        Value::Undefined | Value::Null => {
            Err(interp.type_error("cannot convert undefined or null to object"))
        }
        _ => Ok(Value::Object(interp.realm.object_proto.clone())),
    }
}

/// The `__proto__` getter: the prototype of the receiver.
fn proto_getter(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        // A primitive receiver is boxed, so `(1).__proto__` is `Number.prototype`.
        let boxed = crate::builtins::this_object(interp, &this)?;
        return Ok(boxed.proto().map_or(Value::Null, Value::Object));
    };
    Ok(match object.proto() {
        // A null prototype reports `null`, not `undefined`.
        None => Value::Null,
        Some(proto) => Value::Object(proto),
    })
}

/// The `__proto__` setter: `o.__proto__ = p`, which is `[[SetPrototypeOf]]`.
///
/// Setting it on a non-extensible object, or to a value that is neither an object nor
/// `null`, is silently ignored rather than an error — the setter is a plain assignment,
/// and an assignment that cannot take effect is a no-op in sloppy mode.
fn proto_setter(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let Some(object) = this.as_object() else {
        return Ok(Value::Undefined);
    };
    if !object.is_extensible() {
        return Ok(Value::Undefined);
    }
    match arg(args, 0) {
        Value::Object(proto) => {
            // A cycle would make the prototype chain infinite, which the spec refuses.
            if proto.ptr_eq(object) || inherits_from(&proto, object) {
                return Ok(Value::Undefined);
            }
            object.set_proto(Some(proto));
        }
        Value::Null => object.set_proto(None),
        _ => {}
    }
    let _ = interp;
    Ok(Value::Undefined)
}

/// Whether `object`'s prototype chain passes through `target`.
fn inherits_from(object: &Gc<JsObject>, target: &Gc<JsObject>) -> bool {
    let mut current = object.proto();
    while let Some(next) = current {
        if next.ptr_eq(target) {
            return true;
        }
        current = next.proto();
    }
    false
}

fn set_prototype_of(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let proto = match arg(args, 1) {
        Value::Object(object) => Some(object),
        Value::Null => None,
        _ => return Err(interp.type_error("prototype must be an object or null")),
    };
    if let Some(object) = target.as_object() {
        // A proxy answers through `setPrototypeOf` and may refuse; the `Object.` entry
        // point turns a refusal into a `TypeError` rather than reporting `false`.
        if !interp.object_set_prototype_of(object, proto)? {
            return Err(interp.type_error("proxy refused to change its prototype"));
        }
    }
    Ok(target)
}

fn define_property(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Value::Object(object) = &target else {
        return Err(interp.type_error("Object.defineProperty target is not an object"));
    };
    if !define_property_on(interp, object, args)? {
        return Err(interp.type_error("cannot redefine property"));
    }
    Ok(target)
}

/// `Object.defineProperties(target, descriptors)`.
///
/// Missing outright until now, which surfaced as "undefined is not a function" rather than
/// as an absent feature — a missing method and a broken one look identical from a test.
/// The descriptors are collected first and applied second, as the spec does, so one that
/// throws leaves the target untouched.
fn define_properties(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Value::Object(object) = &target else {
        return Err(interp.type_error("Object.defineProperties target is not an object"));
    };
    // `ObjectDefineProperties` runs `ToObject` on the descriptor map, so a primitive is
    // not automatically an error: `Object.defineProperties({}, 5)` is a no-op because a
    // Number wrapper has no own enumerable properties. Only `null`/`undefined` are
    // refused outright, and a *non-empty* string is refused one step later, when its
    // indexed values ("a", "b", …) fail `ToPropertyDescriptor`.
    let descriptors = match arg(args, 1) {
        Value::Object(object) => object,
        Value::Undefined | Value::Null => {
            return Err(interp.type_error("cannot convert to object"));
        }
        Value::String(text) if text.utf16_len() > 0 => {
            return Err(interp.type_error("not an object"));
        }
        _ => return Ok(target),
    };
    // Collect first, apply second: a descriptor that throws must leave the target alone.
    let mut pending: Vec<(Value, Value)> = Vec::new();
    for key in interp.object_own_property_keys(&descriptors)? {
        // Only the *enumerable* own properties of the descriptor map are used, which is
        // what makes `Object.defineProperties(o, {a: {...}})` skip a non-enumerable `a`.
        let enumerable = crate::object::own_property_including_array(&descriptors, &key)
            .is_some_and(|property| property.enumerable);
        if !enumerable {
            continue;
        }
        let Some(descriptor) = interp.get_property_key(&descriptors, &key)? else {
            continue;
        };
        let key_value = match &key {
            PropertyKey::String(text) => interp.string_wtf8(text.clone()),
            PropertyKey::Symbol(symbol) => Value::Symbol(symbol.clone()),
        };
        pending.push((key_value, descriptor));
    }
    for (key_value, descriptor) in pending {
        if !define_property_on(interp, object, &[target.clone(), key_value, descriptor])? {
            return Err(interp.type_error("cannot redefine property"));
        }
    }
    Ok(target)
}

/// The shared body of `Object.defineProperty` and `Reflect.defineProperty`.
///
/// `args` is `[target, key, descriptor]`; the target object is passed separately
/// because the two entry points disagree only in how they report failure.
pub fn define_property_on(
    interp: &mut Interp<'_>,
    object: &Gc<JsObject>,
    args: &[Value],
) -> JsResult<bool> {
    let key = interp.property_key(&arg(args, 1))?;
    let Value::Object(descriptor) = arg(args, 2) else {
        return Err(interp.type_error("property descriptor must be an object"));
    };
    // Array-aware: an element lives in the element vector, so a descriptor that changes
    // only attributes (`{configurable: false}`) must still see the current value rather
    // than fall back to `undefined` and blank the element.
    let previous = crate::object::own_property_including_array(object, &key);
    let getter = interp.get_property(&descriptor, "get")?;
    let setter = interp.get_property(&descriptor, "set")?;
    // ToPropertyDescriptor: a `get`/`set` that is present must be callable. `undefined`
    // means "absent" and is allowed, but `get: null` is a TypeError — accepting it stored
    // a non-callable getter that no read could run.
    for function in [&getter, &setter].into_iter().flatten() {
        if !matches!(function, Value::Undefined)
            && !matches!(function.as_object(), Some(object) if object.is_callable())
        {
            return Err(interp.type_error("getter or setter is not callable"));
        }
    }
    let has_get = getter
        .as_ref()
        .is_some_and(|v| !matches!(v, Value::Undefined));
    let has_set = setter
        .as_ref()
        .is_some_and(|v| !matches!(v, Value::Undefined));
    // An accessor property *stays* an accessor when the descriptor only touches the
    // generic attributes. `Object.defineProperty(o, "e", {enumerable: true})` on an
    // accessor must keep the getter: ValidateAndApplyPropertyDescriptor step 9 says a
    // descriptor with no `[[Value]]`/`[[Writable]]`/`[[Get]]`/`[[Set]]` only updates
    // `[[Enumerable]]`/`[[Configurable]]`. Deciding `has_accessor` from the descriptor
    // alone converted the property to a data property and blanked its value.
    let descriptor_is_data = interp.get_property(&descriptor, "value")?.is_some()
        || interp.get_property(&descriptor, "writable")?.is_some();
    let has_accessor = has_get
        || has_set
        || (previous.as_ref().is_some_and(|p| p.is_accessor()) && !descriptor_is_data);
    // A descriptor may not be both kinds: `{value: 12, set: f}` is a TypeError, which
    // `ToPropertyDescriptor` refuses rather than silently preferring one half.
    if has_accessor
        && (interp.get_property(&descriptor, "value")?.is_some()
            || interp.get_property(&descriptor, "writable")?.is_some())
    {
        return Err(interp.type_error("cannot have both a value and a getter/setter"));
    }
    // An attribute the descriptor does not mention is *kept* when the property already
    // exists and is of the same kind — `Object.defineProperty({a: 1}, "a", {value: 5})`
    // leaves `a` writable, enumerable and configurable. Only a brand-new property, or a
    // switch between a data and an accessor property, defaults the omitted ones to false
    // (ValidateAndApplyPropertyDescriptor steps 8 and 9). Defaulting everything to false
    // silently locked a property down on every attribute-only redefine.
    // The two *generic* attributes are always inherited when the descriptor omits them,
    // even across a data/accessor conversion: ValidateAndApplyPropertyDescriptor steps 9
    // and 10 convert the property while keeping `[[Configurable]]` and `[[Enumerable]]`
    // from the existing one. `writable` and `value` are not generic — an accessor has
    // neither — so they are only inherited when both sides are data properties.
    let same_kind = previous
        .as_ref()
        .is_some_and(|existing| existing.is_accessor() == has_accessor);
    let inherited = |named: Option<bool>, pick: fn(&Property) -> bool, across: bool| -> bool {
        named
            .or_else(|| previous.as_ref().filter(|_| across || same_kind).map(pick))
            .unwrap_or(false)
    };
    let enumerable = inherited(
        descriptor_bool(interp, &descriptor, "enumerable")?,
        |p| p.enumerable,
        true,
    );
    let configurable = inherited(
        descriptor_bool(interp, &descriptor, "configurable")?,
        |p| p.configurable,
        true,
    );
    let writable = inherited(
        descriptor_bool(interp, &descriptor, "writable")?,
        |p| p.writable,
        false,
    );
    // `defineProperty` defaults every attribute to `false` when the descriptor omits
    // it, but keeps the *value* when the descriptor is a pure attribute change.
    let property = if has_accessor {
        // A half-specified accessor keeps the other half: `{set: f}` on an existing
        // getter/setter pair must not drop the getter.
        let previous_getter = previous.as_ref().and_then(|p| p.getter.clone());
        let previous_setter = previous.as_ref().and_then(|p| p.setter.clone());
        Property {
            getter: getter.or(previous_getter),
            setter: setter.or(previous_setter),
            enumerable,
            configurable,
            writable: false,
            value: None,
        }
    } else {
        let value = match interp.get_property(&descriptor, "value")? {
            Some(value) => value,
            None => previous
                .as_ref()
                .and_then(|property| property.value.clone())
                .unwrap_or(Value::Undefined),
        };
        Property {
            value: Some(value),
            writable,
            enumerable,
            configurable,
            getter: None,
            setter: None,
        }
    };
    // An array's `length` is exotic: it is not in the property table, and it has its own
    // writability. Handled here because only this function knows which descriptor fields
    // were actually present — `object_define_own_property` receives a filled-in `Property`
    // and cannot tell `{writable: false}` from `{value: 3, writable: false}`.
    if object.is_array() && key.as_str() == Some("length") {
        let requested_value = interp.get_property(&descriptor, "value")?;
        let requested_writable = descriptor_bool(interp, &descriptor, "writable")?;
        // `length` may only ever be a non-enumerable data property: it cannot become
        // configurable, enumerable, or an accessor.
        if configurable
            || descriptor_bool(interp, &descriptor, "enumerable")? == Some(true)
            || (has_get || has_set)
        {
            return Ok(false);
        }
        // `ArraySetLength` step 3 runs `ToUint32`, not `ToNumber`: an object whose
        // `valueOf` gives a fraction falls through to `ToString`, so `{toString: () => "2"}`
        // is a valid length while `{valueOf: () => 2.5}` is a `RangeError`. Comparing
        // `ToNumber` against the current length also made `{toString: () => "2"}` look like
        // `NaN` and refused it.
        let requested_uint = match &requested_value {
            Some(value) => Some(interp.array_length_of(value)?),
            None => None,
        };
        if !object.length_writable() {
            // A read-only `length` refuses both a new value and being made writable again.
            if requested_writable == Some(true) {
                return Ok(false);
            }
            if requested_uint
                .is_some_and(|value| value as usize != object.array_length().unwrap_or(0))
            {
                return Ok(false);
            }
            return Ok(true);
        }
        if let Some(requested) = requested_uint {
            let current = object.array_length().unwrap_or(0);
            let requested = requested as usize;
            if requested < current {
                // `ArraySetLength` walks down from the top and, on hitting a non-deletable
                // element, stops with `[[Length]]` set to *that* index plus one — so
                // `[0,1,2]` with a fixed index 1 ends at length 2, neither 3 nor 0.
                let blocked = (requested..current)
                    .rev()
                    .find(|index| object.element_is_undeletable(*index as u32));
                if let Some(blocked) = blocked {
                    object.array_truncate(blocked + 1);
                    return Ok(false);
                }
            }
            object.array_set_length(requested);
        }
        if let Some(writable) = requested_writable {
            object.set_length_writable(writable);
        }
        return Ok(true);
    }
    // A new property may not appear on a non-extensible object.
    if previous.is_none() && !object.is_extensible() {
        return Ok(false);
    }
    // ValidateAndApplyPropertyDescriptor: a non-configurable property may not be
    // redefined, except to the same values. Without this every redefine "succeeded" and
    // silently overwrote a property the script had locked.
    if let Some(existing) = &previous
        && !existing.configurable
    {
        let kind_changed = existing.is_accessor() != property.is_accessor();
        // Compare the *merged* accessor halves, not the descriptor's presence bits: a
        // descriptor that says `set: undefined` presents no setter, yet on a
        // non-configurable property that is still a change from a real setter. Judging by
        // presence let `Object.defineProperties(arr, {1: {set: undefined}})` quietly
        // succeed on a locked accessor.
        let accessor_changed = property.is_accessor()
            && (!same_accessor_half(&property.getter, &existing.getter)
                || !same_accessor_half(&property.setter, &existing.setter));
        let attribute_changed =
            enumerable != existing.enumerable || configurable || kind_changed || accessor_changed;
        let data_changed = !kind_changed
            && existing.is_accessor() == property.is_accessor()
            && !existing.writable
            && (writable
                // `SameValue`, not `===`: redefining a frozen `NaN` property with `NaN`
                // is not a change, because `SameValue(NaN, NaN)` holds.
                || property.value.as_ref().is_some_and(|value| {
                    !existing
                        .value
                        .as_ref()
                        .is_some_and(|old| old.same_value(value))
                }));
        if attribute_changed || data_changed {
            // `false`, not a throw: the two entry points disagree about reporting, and
            // `Reflect.defineProperty` must answer `false` where `Object.defineProperty`
            // raises. The conversion happens in the callers.
            return Ok(false);
        }
    }
    // A proxy decides through `defineProperty`, and a refusal is a `TypeError` here
    // because `Object.defineProperty` reports failure that way.
    interp.object_define_own_property(object, &key, property)
}
/// Whether two getter/setter slots hold the same function.
///
/// An absent half and an explicit `undefined` are the same thing: an accessor with no
/// getter reads as `undefined` either way, so redefining it as `undefined` is not a
/// change and redefining it as a function is.
fn same_accessor_half(left: &Option<Value>, right: &Option<Value>) -> bool {
    let left = left
        .as_ref()
        .filter(|value| !matches!(value, Value::Undefined));
    let right = right
        .as_ref()
        .filter(|value| !matches!(value, Value::Undefined));
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => left.same_value(right),
        _ => false,
    }
}

/// One boolean field of a property descriptor, or `None` when it is absent.
fn descriptor_bool(
    interp: &mut Interp<'_>,
    descriptor: &Gc<JsObject>,
    field: &str,
) -> JsResult<Option<bool>> {
    Ok(interp
        .get_property(descriptor, field)?
        .filter(|value| !matches!(value, Value::Undefined))
        .map(|value| value.to_boolean()))
}

fn get_own_property_names(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let value = arg(args, 0);
    let Some(object) = value.as_object() else {
        // A string's index properties are own and enumerable but non-writable; a
        // primitive has no other own keys.
        let mut names: Vec<Value> = crate::builtins::object::own_keys(&value)
            .into_iter()
            .map(|name| interp.string_wtf8(name))
            .collect();
        if matches!(&value, Value::String(_)) {
            names.push(interp.string("length"));
        }
        let array = interp.new_array(names);
        return Ok(Value::Object(array));
    };
    let mut names: Vec<Value> = interp
        .object_own_property_keys(object)?
        .iter()
        .filter(|key| !key.is_symbol())
        .map(|key| interp.key_value(key))
        .collect();
    // `length` is an own property of an array that is not in its property table, and it
    // is not enumerable, so only this entry point reports it.
    if object.is_array() && !names.iter().any(|name| name.to_js_string() == "length") {
        names.push(interp.string("length"));
    }
    let array = interp.new_array(names);
    Ok(Value::Object(array))
}

/// `Object.getOwnPropertySymbols(obj)`.
///
/// The only way to enumerate symbol keys, and the reason a symbol-keyed property is
/// effectively private: nothing else reports it.
fn get_own_property_symbols(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let value = arg(args, 0);
    let Some(object) = value.as_object() else {
        return Err(interp.type_error("not an object"));
    };
    let symbols: Vec<Value> = interp
        .object_own_property_keys(object)?
        .into_iter()
        .filter(|key| key.is_symbol())
        .map(|key| interp.key_value(&key))
        .collect();
    let array = interp.new_array(symbols);
    Ok(Value::Object(array))
}

fn get_own_property_descriptor(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let target = arg(args, 0);
    let Value::Object(object) = &target else {
        return Err(interp.type_error("target is not an object"));
    };
    let key = interp.property_key(&arg(args, 1))?;
    let Some(property) = interp.object_get_own_property(object, &key)? else {
        return Ok(Value::Undefined);
    };
    let proto = interp.realm.object_proto.clone();
    let descriptor = interp.new_object(Some(proto));
    if property.is_accessor() {
        descriptor.define_data("get", property.getter.unwrap_or(Value::Undefined));
        descriptor.define_data("set", property.setter.unwrap_or(Value::Undefined));
    } else {
        descriptor.define_data("value", property.value.unwrap_or(Value::Undefined));
        descriptor.define_data("writable", Value::Bool(property.writable));
    }
    descriptor.define_data("enumerable", Value::Bool(property.enumerable));
    descriptor.define_data("configurable", Value::Bool(property.configurable));
    Ok(Value::Object(descriptor))
}

/// `Object.freeze`, `seal` and `preventExtensions`.
///
/// The distinctions the spec draws are observable through `isFrozen`/`isSealed`, so
/// they are kept apart: frozen clears `writable` and `configurable`, sealed clears
/// `configurable` only, and both imply non-extensible.
fn freeze(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    if let Some(object) = target.as_object() {
        object.set_extensible(false);
        // Array elements and `length` are not in the property table, so they carry their
        // own flags; freezing sets both.
        object.set_elements_writable(false);
        object.set_length_writable(false);
        let names = object.own_keys();
        for name in names {
            let key = PropertyKey::from(name);
            if let Some(mut property) = object.own_property_key(&key) {
                property.writable = false;
                property.configurable = false;
                object.define_key(&key, property);
            }
        }
    }
    Ok(target)
}

fn seal(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    if let Some(object) = target.as_object() {
        object.set_extensible(false);
        let names = object.own_keys();
        for name in names {
            let key = PropertyKey::from(name);
            if let Some(mut property) = object.own_property_key(&key) {
                property.configurable = false;
                object.define_key(&key, property);
            }
        }
    }
    Ok(target)
}

fn is_frozen(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object() else {
        // A primitive is frozen by definition.
        return Ok(Value::Bool(!matches!(
            target,
            Value::Undefined | Value::Null
        )));
    };
    let anything_writable = object
        .own_keys()
        .into_iter()
        .filter_map(|name| object.own_property_key(&PropertyKey::from(name)))
        .any(|property| property.configurable || property.writable);
    Ok(Value::Bool(
        !object.is_extensible() && !anything_writable && !object.elements_writable(),
    ))
}

fn is_sealed(_interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object() else {
        return Ok(Value::Bool(!matches!(
            target,
            Value::Undefined | Value::Null
        )));
    };
    let anything_configurable = object
        .own_keys()
        .into_iter()
        .filter_map(|name| object.own_property_key(&PropertyKey::from(name)))
        .any(|property| property.configurable);
    Ok(Value::Bool(
        !object.is_extensible() && !anything_configurable,
    ))
}

fn prevent_extensions(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    if let Some(object) = target.as_object() {
        if !interp.object_prevent_extensions(object)? {
            return Err(interp.type_error("proxy refused to become non-extensible"));
        }
    }
    Ok(target)
}

fn is_extensible(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    Ok(Value::Bool(match target.as_object() {
        Some(object) => interp.object_is_extensible(object)?,
        None => false,
    }))
}

/// `Object.is`, which differs from `===` on exactly two values: `NaN` and `-0`.
fn is(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let (a, b) = (arg(args, 0), arg(args, 1));
    // Same-type check first: `0` and `-0` are both numbers and compare equal under
    // `===`, so the sign has to be inspected explicitly.
    let same = match (&a, &b) {
        (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => {
            let (x, y) = (a.to_number(), b.to_number());
            if x.is_nan() && y.is_nan() {
                true
            } else if x == 0.0 && y == 0.0 {
                x.is_sign_negative() == y.is_sign_negative()
            } else {
                x == y
            }
        }
        _ => a.strict_equals(&b),
    };
    let _ = interp;
    Ok(Value::Bool(same))
}

/// `Object.fromEntries(iterable)`: build an object from `[key, value]` entries.
///
/// This used to walk an *array* by index and skip anything that was not an object, which
/// got three things wrong: a non-iterable argument answered `{}` instead of a `TypeError`,
/// an entry that was not an object was skipped instead of refused, and — the reason most of
/// the tests exist — the iterator was never closed when an entry's key or value threw.
fn from_entries(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let proto = interp.realm.object_proto.clone();
    let object = interp.new_object(Some(proto));
    let iterable = arg(args, 0);
    let iterator = interp.get_iterator(&iterable)?;
    loop {
        let (entry, done) = interp.iterator_step(&iterator)?;
        if done {
            break;
        }
        // `AddEntriesFromIterable` wraps the body in `IfAbruptCloseIterator`: a refused
        // entry, a throwing key accessor or a throwing `ToPropertyKey` all close the
        // iterator, and the original error is the one that propagates.
        let outcome = (|| -> JsResult<(crate::object::PropertyKey, Value)> {
            let Some(pair) = entry.as_object().cloned() else {
                return Err(interp.type_error("not an object"));
            };
            let key = interp.get_property(&pair, "0")?.unwrap_or(Value::Undefined);
            let key = interp.property_key(&key)?;
            let value = interp.get_property(&pair, "1")?.unwrap_or(Value::Undefined);
            Ok((key, value))
        })();
        match outcome {
            Ok((key, value)) => {
                // `CreateDataPropertyOrThrow`, so a key of `"__proto__"` becomes an own
                // property rather than replacing the prototype.
                object.define_key(&key, Property::data(value));
            }
            Err(signal) => {
                let _ = close_iterator(interp, &iterator);
                return Err(signal);
            }
        }
    }
    Ok(Value::Object(object))
}

/// `IteratorClose`, ignoring an error from `return` itself.
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

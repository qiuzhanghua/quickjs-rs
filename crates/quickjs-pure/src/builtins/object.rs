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

    let constructor = interp.new_native("Object", 1, constructor);
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
    match value {
        Value::Undefined => "Undefined".to_string(),
        Value::Null => "Null".to_string(),
        Value::Bool(_) => "Boolean".to_string(),
        Value::Int(_) | Value::Float(_) => "Number".to_string(),
        Value::String(_) => "String".to_string(),
        Value::Symbol(_) => "Symbol".to_string(),
        Value::Object(object) => match object.with(|data| match &data.kind {
            ObjectKind::Array { .. } => "Array",
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
            ObjectKind::TypedArray { .. } => "Object",
            ObjectKind::Ordinary => "Object",
            // A proxy reports the tag of what it stands in for, which is what makes
            // `Object.prototype.toString.call(new Proxy([], {}))` say `[object Array]`.
            ObjectKind::Proxy(_) => {
                if object.is_array() {
                    "Array"
                } else if object.is_callable() {
                    "Function"
                } else {
                    "Object"
                }
            }
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
                if let Some(boxed) = object.own_property("_value").and_then(|p| p.value) {
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
                let _ = interp;
                match object.own_property("_class").and_then(|p| p.value) {
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

fn value_of(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    // `Object.prototype.valueOf` returns the object, unboxing a primitive receiver.
    match this {
        Value::Object(object) => Ok(object
            .own_property("_value")
            .and_then(|property| property.value)
            .unwrap_or(Value::Object(object))),
        other => Ok(other),
    }
}

fn has_own_property(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let object = this_object(interp, &this)?;
    let key = interp.property_key(&arg(args, 0));
    // `[[GetOwnProperty]]`, not a peek at the property table: a proxy answers this with
    // its `getOwnPropertyDescriptor` trap, and it may still be holding a `TypeError` for
    // a handler that lied about a fixed property.
    Ok(Value::Bool(
        interp.object_get_own_property(&object, &key)?.is_some(),
    ))
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
    let target = arg(args, 0);
    let Value::Object(target_object) = &target else {
        return Err(interp.type_error("Object.assign target is not an object"));
    };
    for source in args.iter().skip(1) {
        let Value::Object(_) = source else {
            continue;
        };
        // `Object.assign` copies exactly the own enumerable keys, so it goes through
        // `[[OwnPropertyKeys]]` and `[[Get]]` — which is what makes it see a proxy's
        // handler rather than the empty property table behind it.
        for as_key in own_enumerable_keys(interp, source)? {
            if let Some(found) =
                interp.get_property_key(source.as_object().expect("checked above"), &as_key)?
            {
                interp.set_property_with_key(
                    target_object,
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
    define_property_on(interp, object, args)?;
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
) -> JsResult<()> {
    let key = interp.property_key(&arg(args, 1));
    let Value::Object(descriptor) = arg(args, 2) else {
        return Err(interp.type_error("property descriptor must be an object"));
    };
    let previous = object.own_property_key(&key);
    let writable = descriptor_bool(interp, &descriptor, "writable")?.unwrap_or(false);
    let enumerable = descriptor_bool(interp, &descriptor, "enumerable")?.unwrap_or(false);
    let configurable = descriptor_bool(interp, &descriptor, "configurable")?.unwrap_or(false);
    let getter = interp.get_property(&descriptor, "get")?;
    let setter = interp.get_property(&descriptor, "set")?;
    let has_accessor = getter
        .as_ref()
        .is_some_and(|v| !matches!(v, Value::Undefined))
        || setter
            .as_ref()
            .is_some_and(|v| !matches!(v, Value::Undefined));
    // `defineProperty` defaults every attribute to `false` when the descriptor omits
    // it, but keeps the *value* when the descriptor is a pure attribute change.
    let property = if has_accessor {
        Property {
            getter,
            setter,
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
    // A proxy decides through `defineProperty`, and a refusal is a `TypeError` here
    // because `Object.defineProperty` reports failure that way.
    if !interp.object_define_own_property(object, &key, property)? {
        return Err(interp.type_error("proxy 'defineProperty' refused"));
    }
    Ok(())
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
    let key = interp.property_key(&arg(args, 1));
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
        // Array elements are not in the property table, so they carry their own flag.
        object.set_elements_writable(false);
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

fn from_entries(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let proto = interp.realm.object_proto.clone();
    let object = interp.new_object(Some(proto));
    let Some(iterable) = arg(args, 0).as_object().cloned() else {
        return Ok(Value::Object(object));
    };
    // An array of `[key, value]` pairs, or of `{0,1}`/`{key,value}` objects.
    for index in 0..iterable.array_length().unwrap_or(0) {
        let Some(entry) = iterable.array_get(index) else {
            continue;
        };
        let Some(pair) = entry.as_object().cloned() else {
            continue;
        };
        let key = interp
            .get_property(&pair, "0")?
            .unwrap_or(Value::Undefined)
            .to_js_string();
        let value = interp.get_property(&pair, "1")?.unwrap_or(Value::Undefined);
        object.define_data(&key, value);
    }
    Ok(Value::Object(object))
}

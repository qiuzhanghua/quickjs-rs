//! `Reflect`.
//!
//! `Reflect` duplicates operations the language performs implicitly, with one
//! difference that matters: it reports success as a boolean instead of throwing or
//! silently doing nothing. That is why `Reflect.defineProperty` is the safe way to
//! install a property and why the test suite reaches for `Reflect.apply` when it
//! wants to call a function without spreading the arguments.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};

use crate::value::Value;

/// Install `Reflect`.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    let reflect = interp.new_object(Some(object_proto));
    for (name, length, func) in [
        ("apply", 3, apply as crate::object::NativeFn),
        ("construct", 2, construct),
        ("defineProperty", 3, define_property),
        ("deleteProperty", 2, delete_property),
        ("get", 2, get),
        ("set", 3, set),
        ("has", 2, has),
        ("ownKeys", 1, own_keys),
        ("getOwnPropertyDescriptor", 2, get_own_property_descriptor),
        ("getPrototypeOf", 1, get_prototype_of),
        ("setPrototypeOf", 2, set_prototype_of),
        ("isExtensible", 1, is_extensible),
        ("preventExtensions", 1, prevent_extensions),
    ] {
        define_method(&reflect, name, length, func, interp);
    }
    let global = interp.realm.global.clone();
    global.define_data("Reflect", Value::Object(reflect));
}

fn apply(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let this_arg = arg(args, 1);
    // Unlike `Function.prototype.apply`, `Reflect.apply` requires an arguments list;
    // `undefined` is a TypeError rather than an empty call.
    let rest = match arg(args, 2) {
        Value::Object(object) => {
            let length = interp
                .get_property(&object, "length")?
                .map(|value| value.to_number().max(0.0) as usize)
                .unwrap_or(0);
            let mut out = Vec::with_capacity(length);
            for index in 0..length {
                out.push(
                    interp
                        .get_property(&object, &index.to_string())?
                        .unwrap_or(Value::Undefined),
                );
            }
            out
        }
        _ => return Err(interp.type_error("Reflect.apply arguments must be an array-like")),
    };
    interp.call_value(target, this_arg, rest)
}

fn construct(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let rest = array_like(interp, &arg(args, 1))?;
    interp.construct(target.clone(), target, rest)
}

/// Read an array-like argument into a vector.
fn array_like(interp: &mut Interp<'_>, value: &Value) -> JsResult<Vec<Value>> {
    match value {
        Value::Undefined | Value::Null => Ok(Vec::new()),
        Value::Object(object) => {
            let length = interp
                .get_property(object, "length")?
                .map(|value| value.to_number().max(0.0) as usize)
                .unwrap_or(0);
            let mut out = Vec::with_capacity(length);
            for index in 0..length {
                out.push(
                    interp
                        .get_property(object, &index.to_string())?
                        .unwrap_or(Value::Undefined),
                );
            }
            Ok(out)
        }
        _ => Err(interp.type_error("expected an array-like")),
    }
}

/// `Reflect.defineProperty`: like `Object.defineProperty` but returns a boolean.
fn define_property(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.defineProperty target is not an object"));
    };
    if !object.is_extensible() && !object.has_own(&arg(args, 1).to_js_string()) {
        return Ok(Value::Bool(false));
    }
    // The definition itself is shared with `Object.defineProperty`; only the failure
    // reporting differs, and that is the `false` above.
    let accepted = crate::builtins::object::define_property_on(interp, &object, args)?;
    Ok(Value::Bool(accepted))
}

fn delete_property(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.deleteProperty target is not an object"));
    };
    let key = arg(args, 1).to_js_string();
    Ok(Value::Bool(interp.delete_property(&object, &key)?))
}

fn get(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.get target is not an object"));
    };
    let key = interp.to_js_string(&arg(args, 1))?;
    Ok(interp
        .get_property(&object, &key)?
        .unwrap_or(Value::Undefined))
}

fn set(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.set target is not an object"));
    };
    let key = interp.to_js_string(&arg(args, 1))?;
    interp.set_property(&object, &key, arg(args, 2))?;
    Ok(Value::Bool(true))
}

fn has(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.has target is not an object"));
    };
    let key = interp.to_js_string(&arg(args, 1))?;
    if object.own_property(&key).is_some() || object.array_length().is_some() && key == "length" {
        return Ok(Value::Bool(true));
    }
    let found = crate::object::lookup(&object, &key).is_some()
        || crate::object::array_index(&key)
            .and_then(|index| object.array_get(index as usize))
            .is_some();
    Ok(Value::Bool(found))
}

fn own_keys(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.ownKeys target is not an object"));
    };
    // Through the interpreter, not the free function: a proxy reports its own list, and
    // that is the whole reason `Reflect.ownKeys` takes a proxy seriously.
    let keys: Vec<Value> = interp
        .object_own_property_keys(&object)?
        .iter()
        .map(|key| interp.key_value(key))
        .collect();
    let array = interp.new_array(keys);
    Ok(Value::Object(array))
}

/// `Reflect.getOwnPropertyDescriptor(target, key)` — `Object.getOwnPropertyDescriptor`'s
/// non-throwing twin, and the only builtin that reaches `[[GetOwnProperty]]` directly.
fn get_own_property_descriptor(
    interp: &mut Interp<'_>,
    _this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.getOwnPropertyDescriptor target is not an object"));
    };
    let key = interp.property_key(&arg(args, 1));
    let Some(property) = interp.object_get_own_property(&object, &key)? else {
        return Ok(Value::Undefined);
    };
    let descriptor = interp.new_object(Some(interp.realm.object_proto.clone()));
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

fn get_prototype_of(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Err(interp.type_error("Reflect.getPrototypeOf target is not an object"));
    };
    Ok(interp
        .object_prototype_of(&object)?
        .map_or(Value::Null, Value::Object))
}

fn set_prototype_of(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Ok(Value::Bool(false));
    };
    let proto = match arg(args, 1) {
        Value::Object(proto) => Some(proto),
        Value::Null => None,
        _ => return Err(interp.type_error("prototype must be an object or null")),
    };
    // A refusal is a `false` here rather than a `TypeError`: that difference from
    // `Object.setPrototypeOf` is the whole point of the `Reflect` form.
    Ok(Value::Bool(interp.object_set_prototype_of(&object, proto)?))
}

fn is_extensible(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    Ok(Value::Bool(match target.as_object() {
        Some(object) => interp.object_is_extensible(object)?,
        None => false,
    }))
}

fn prevent_extensions(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let target = arg(args, 0);
    let Some(object) = target.as_object().cloned() else {
        return Ok(Value::Bool(false));
    };
    Ok(Value::Bool(interp.object_prevent_extensions(&object)?))
}

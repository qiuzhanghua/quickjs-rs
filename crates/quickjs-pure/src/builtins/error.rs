//! The error classes.
//!
//! `Error` and its subclasses are ordinary constructors with one twist that matters
//! to the test suite: `instanceof` has to work, so each class gets its own prototype
//! in a chain, and `Error.prototype` sits on it. A thrown runtime error has to be an
//! instance of the right class — `try { null.x } catch (e) { e instanceof TypeError }`
//! is how scripts tell failures apart.

use crate::builtins::arg;
use crate::interp::{Interp, JsResult};
use crate::object::Property;
use crate::value::Value;

/// The classes to install, in the order their prototypes have to be chained.
///
/// `Error` is first because every other class's prototype inherits from it.
const CLASSES: [&str; 8] = [
    "Error",
    "TypeError",
    "RangeError",
    "ReferenceError",
    "SyntaxError",
    "EvalError",
    "URIError",
    "InternalError",
];

/// Install `Error` and its subclasses.
pub fn install(interp: &mut Interp<'_>) {
    let object_proto = interp.realm.object_proto.clone();
    // `Error.prototype` is the root of the error chain and an ordinary object, not
    // an error instance: `Error.prototype.message` is `""`, not `undefined`.
    let error_proto = interp.new_object(Some(object_proto));
    error_proto.define(
        "name",
        Property::with_attributes(interp.string("Error"), true, false),
    );
    error_proto.define(
        "message",
        Property::with_attributes(interp.string(""), true, false),
    );
    crate::builtins::define_method(&error_proto, "toString", 0, to_string, interp);
    interp.realm.error_proto = error_proto.clone();

    for &class in &CLASSES {
        let proto = if class == "Error" {
            error_proto.clone()
        } else {
            let proto = interp.new_object(Some(error_proto.clone()));
            proto.define(
                "name",
                Property::with_attributes(interp.string(class), true, false),
            );
            proto.define(
                "message",
                Property::with_attributes(interp.string(""), true, false),
            );
            proto
        };
        // The constructor is a native function whose identity is what `new` uses;
        // which class it belongs to is carried in `_class`, because a `NativeFn` is
        // a plain function pointer and cannot capture.
        let constructor = interp.new_native(class, 1, error_constructor);
        constructor.define(
            "_class",
            Property::with_attributes(interp.string(class), false, false),
        );
        constructor.define(
            "prototype",
            Property::with_attributes(Value::Object(proto.clone()), false, false),
        );
        proto.define(
            "constructor",
            Property::with_attributes(Value::Object(constructor.clone()), true, false),
        );
        interp
            .realm
            .errors
            .insert(leak_class(class), constructor.clone());
        let global = interp.realm.global.clone();
        global.define(
            class,
            Property::with_attributes(Value::Object(constructor), true, false),
        );
    }
}

/// The `&'static str` keys the realm's error table uses.
fn leak_class(class: &str) -> &'static str {
    CLASSES
        .into_iter()
        .find(|known| *known == class)
        .unwrap_or("Error")
}

/// `new Error(message)`, and the same call without `new`.
fn error_constructor(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    // `this` is the fresh object `new` made, or `undefined` for a plain call; the
    // class is read off the constructor so one function serves all eight.
    let class = interp
        .current_callee()
        .and_then(|callee| callee.own_property("_class"))
        .and_then(|property| property.value)
        .map(|value| value.to_js_string())
        .unwrap_or_else(|| "Error".to_string());
    let target = match &this {
        Value::Object(object) => object.clone(),
        // `Error("x")` without `new` has to behave like `new Error("x")`, so the
        // instance is built here and the prototype comes from the class.
        _ => {
            let proto = interp
                .realm
                .errors
                .get(class.as_str())
                .map(|constructor| interp.prototype_of(constructor))
                .unwrap_or_else(|| interp.realm.error_proto.clone());
            interp.new_object(Some(proto))
        }
    };
    if let Some(Value::String(_)) = args.first() {
        target.define(
            "message",
            Property::with_attributes(arg(args, 0), true, false),
        );
    }
    // An explicit `name` on the instance, so `e.name` survives a prototype change.
    if class != "Error" && !target.has_own("name") {
        target.define(
            "name",
            Property::with_attributes(interp.string(class.clone()), true, false),
        );
    }
    Ok(Value::Object(target))
}

/// `Error.prototype.toString` — `"Name: message"`, omitting the colon when either
/// side is empty.
pub fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let Value::Object(object) = &this else {
        return Err(interp.type_error("Error.prototype.toString called on a non-object"));
    };
    let name = interp
        .get_property(object, "name")?
        .filter(|value| !matches!(value, Value::Undefined))
        .map_or_else(|| "Error".to_string(), |value| value.to_js_string());
    let message = interp
        .get_property(object, "message")?
        .filter(|value| !matches!(value, Value::Undefined))
        .map_or_else(String::new, |value| value.to_js_string());
    let text = match (name.is_empty(), message.is_empty()) {
        (true, true) => String::new(),
        (true, false) => message,
        (false, true) => name,
        (false, false) => format!("{name}: {message}"),
    };
    Ok(interp.string(text))
}

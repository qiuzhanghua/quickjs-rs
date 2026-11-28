//! The error classes.
//!
//! `Error` and its subclasses are ordinary constructors with one twist that matters
//! to the test suite: `instanceof` has to work, so each class gets its own prototype
//! in a chain, and `Error.prototype` sits on it. A thrown runtime error has to be an
//! instance of the right class — `try { null.x } catch (e) { e instanceof TypeError }`
//! is how scripts tell failures apart.

use crate::builtins::arg;
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

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

/// Install `AggregateError` (§20.5.7): the one error class with its own constructor.
///
/// It cannot go through [`CLASSES`]' shared `error_constructor` because it has a *third*
/// parameter and an `errors` own property: `AggregateError(errors, message, options)` runs
/// `IterableToList(errors)` **before** the message conversion, so a bad first argument is a
/// `TypeError` even when the message would have thrown too — and the order is observable.
fn install_aggregate_error(interp: &mut Interp<'_>, error_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(error_proto.clone()));
    proto.define(
        "name",
        Property::with_attributes(interp.string("AggregateError"), true, false),
    );
    proto.define(
        "message",
        Property::with_attributes(interp.string(""), true, false),
    );
    let constructor =
        interp.new_native_constructor("AggregateError", 2, aggregate_error_constructor);
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
        .insert("AggregateError", constructor.clone());
    let global = interp.realm.global.clone();
    global.define(
        "AggregateError",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `AggregateError(errors, message, options)`.
fn aggregate_error_constructor(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
) -> JsResult<Value> {
    // Step order is the spec's: `IteratorToList(errors)` first, then `ToString(message)`,
    // then `InstallErrorCause` — so an invalid `errors` wins over an invalid `message`.
    let errors = arg(args, 0);
    let list = interp.collect_iterable(&errors)?;
    let message = match args.get(1) {
        None | Some(Value::Undefined) => None,
        Some(value) => Some(interp.to_js_string(value)?),
    };
    // The instance, from `new.target` when called with `new` and from the class otherwise.
    let target = match &this {
        Value::Object(object) => object.clone(),
        _ => {
            let proto = interp
                .realm
                .errors
                .get("AggregateError")
                .map(|constructor| interp.prototype_of(constructor))
                .unwrap_or_else(|| interp.realm.error_proto.clone());
            interp.new_object(Some(proto))
        }
    };
    if let Some(message) = message {
        target.define(
            "message",
            Property::with_attributes(interp.string(message), true, false),
        );
    }
    if !target.has_own("name") {
        target.define(
            "name",
            Property::with_attributes(interp.string("AggregateError"), true, false),
        );
    }
    // `errors` is an own, non-enumerable, writable and configurable data property: it is a
    // real array built from the iterable, so `e.errors` is a fresh array each time.
    let array = interp.new_array(list);
    target.define(
        "errors",
        Property {
            value: Some(Value::Object(array)),
            enumerable: false,
            configurable: true,
            writable: true,
            ..Property::default()
        },
    );
    install_error_cause(interp, &target, args.get(2))?;
    if interp.internal_get(&target, "_class").is_none() {
        interp.internal_set(&target, "_class", interp.string("Error"));
    }
    interp.add_stack(&target);
    Ok(Value::Object(target))
}

/// `InstallErrorCause(O, options)` (§20.5.8.1): copy `options.cause` when present.
///
/// `in` is deliberately the check, not "is it `undefined`": `{cause: undefined}` installs
/// the property, so `"cause" in e` is `true` while `e.cause` is `undefined`.
fn install_error_cause(
    interp: &mut Interp<'_>,
    target: &Gc<JsObject>,
    options: Option<&Value>,
) -> JsResult<()> {
    let Some(options) = options else {
        return Ok(());
    };
    let Some(options) = options.as_object().cloned() else {
        // A primitive options object installs nothing rather than throwing.
        return Ok(());
    };
    let key = interp.property_key(&interp.string("cause"))?;
    if !interp.has_own_or_inherited(&options, &key)? {
        return Ok(());
    }
    let cause = interp.get_property_key(&options, &key)?;
    target.define(
        "cause",
        Property {
            value: cause,
            enumerable: false,
            configurable: true,
            writable: true,
            ..Property::default()
        },
    );
    Ok(())
}

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
        let constructor = interp.new_native_constructor(class, 1, error_constructor);
        let class_text = interp.string(class);
        interp.internal_set(&constructor, "_class", class_text);
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

    install_aggregate_error(interp, &error_proto);
    install_suppressed_error(interp, &error_proto);
}

/// `SuppressedError`, the aggregate a failed disposal reports.
///
/// It cannot go through the loop above: every other error class takes `(message)`, while
/// this one takes `(error, suppressed, message)`. Its whole purpose is to carry the two
/// errors `using` has to keep when one disposal throws while another error is already
/// propagating.
fn install_suppressed_error(interp: &mut Interp<'_>, error_proto: &Gc<JsObject>) {
    let proto = interp.new_object(Some(error_proto.clone()));
    proto.define(
        "name",
        Property::with_attributes(interp.string("SuppressedError"), true, false),
    );
    proto.define(
        "message",
        Property::with_attributes(interp.string(""), true, false),
    );
    let constructor =
        interp.new_native_constructor("SuppressedError", 3, suppressed_error_constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(proto.clone()), false, false),
    );
    proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    interp.realm.suppressed_error_proto = Some(proto);
    interp
        .realm
        .errors
        .insert("SuppressedError", constructor.clone());
    let global = interp.realm.global.clone();
    global.define(
        "SuppressedError",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// `new SuppressedError(error, suppressed, message)`.
fn suppressed_error_constructor(
    interp: &mut Interp<'_>,
    this: Value,
    args: &[Value],
) -> JsResult<Value> {
    let error = arg(args, 0);
    let suppressed = arg(args, 1);
    let message = arg(args, 2);
    // `new` has already made the object and passes it as `this`; a plain call behaves like
    // `new`, which is what the reference does for every error class.
    let object = match &this {
        Value::Object(object) => object.clone(),
        _ => {
            let proto = interp
                .realm
                .suppressed_error_proto
                .clone()
                .unwrap_or_else(|| interp.realm.error_proto.clone());
            interp.new_object(Some(proto))
        }
    };
    object.define("error", Property::with_attributes(error, true, true));
    object.define(
        "suppressed",
        Property::with_attributes(suppressed, true, true),
    );
    if !matches!(message, Value::Undefined) {
        let text = interp.to_js_string(&message)?;
        object.define(
            "message",
            Property::with_attributes(interp.string(text), true, true),
        );
    }
    interp.internal_set(&object, "_class", interp.string("Error"));
    interp.add_stack(&object);
    Ok(Value::Object(object))
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
        .and_then(|callee| interp.internal_get(&callee, "_class"))
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
    // Every error instance shares the class name `Error`, whatever its constructor:
    // `Object.prototype.toString` answers `[object Error]` even for a `TypeError`,
    // and the print formatter uses it to choose `Name: message` + trace.
    if interp.internal_get(&target, "_class").is_none() {
        interp.internal_set(&target, "_class", interp.string("Error"));
    }
    // The trace is taken here, in the constructor, exactly as the reference does
    // (`JS_NewError` calls `build_backtrace`): an error object carries its stack from
    // the moment it exists, whether or not it is ever thrown.
    // The reference builds this trace with `JS_BACKTRACE_FLAG_SKIP_FIRST_LEVEL`, so
    // the constructor's own frame is not the first line of every error's stack.
    interp.add_stack_skipping(&target, 1);
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

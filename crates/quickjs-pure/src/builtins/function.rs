//! `Function.prototype` and the `Function` constructor.
//!
//! `Function.prototype` is itself callable and returns `undefined`, which is what
//! makes `Object.getPrototypeOf(f)(…)` harmless. `new Function(args, body)` needs to
//! compile source at run time, so it goes back through the parser and compiler.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult, Signal};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install `Function` and its prototype methods.
pub fn install(interp: &mut Interp<'_>, function_proto: &Gc<JsObject>) {
    define_method(function_proto, "call", 1, call, interp);
    define_method(function_proto, "apply", 2, apply, interp);
    define_method(function_proto, "bind", 1, bind, interp);
    define_method(function_proto, "toString", 0, to_string, interp);

    let constructor = interp.new_native("Function", 1, constructor);
    constructor.define(
        "prototype",
        Property::with_attributes(Value::Object(function_proto.clone()), false, false),
    );
    function_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
    let global = interp.realm.global.clone();
    global.define(
        "Function",
        Property::with_attributes(Value::Object(constructor.clone()), true, false),
    );
}

/// `new Function(p1, p2, …, body)`.
///
/// The source is compiled at run time, which is what `eval` does too — the only
/// difference is the scope the result closes over. Parameters here are local to the
/// new function and the body sees only globals, which is why the compiled script is a
/// plain top-level function.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let body = match args.last() {
        Some(value) => value.to_js_string(),
        None => String::new(),
    };
    let params: Vec<String> = args
        .iter()
        .take(args.len().saturating_sub(1))
        .map(|value| value.to_js_string())
        .collect();
    let params = params.join(",");
    let source = format!("(function({params}) {{{body}}})");
    let program = js_parse::parse_script(&source).map_err(|error| {
        Signal::Throw(interp.make_error("SyntaxError", &format!("invalid function body: {error}")))
    })?;
    let compiled = js_compile::compile_script(&program)
        .map_err(|error| Signal::Throw(interp.make_error("SyntaxError", &error.message)))?;
    // The script's own last expression is the function literal, so running it yields
    // the closure. That reuses the whole compile-and-run path instead of duplicating
    // the constant-pool plumbing here.
    let name = interp.string("anonymous");
    let _ = name;
    interp.run_function_source(compiled)
}

fn call(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let new_this = arg(args, 0);
    let rest: Vec<Value> = args.iter().skip(1).cloned().collect();
    interp.call_value(this, new_this, rest)
}

fn apply(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let new_this = arg(args, 0);
    let rest = match arg(args, 1) {
        Value::Undefined | Value::Null => Vec::new(),
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
        _ => return Err(interp.type_error("apply arguments must be an array-like")),
    };
    interp.call_value(this, new_this, rest)
}

/// `Function.prototype.bind`.
///
/// The result is a callable that carries its target, `this` and leading arguments.
/// Those are stored in a dedicated [`Callable`] variant because a native trampoline
/// cannot capture them, and putting them on the object as properties would make them
/// visible to a script.
fn bind(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let bound_this = arg(args, 0);
    let leading: Vec<Value> = args.iter().skip(1).cloned().collect();
    let bound = interp.new_bound(this.clone(), bound_this, leading.clone());
    // `bind` shortens `length` by the number of bound arguments and renames the
    // function, both observable on the result.
    let length = this
        .as_object()
        .and_then(|object| object.own_property("length"))
        .and_then(|property| property.value)
        .map(|value| value.to_number())
        .unwrap_or(0.0);
    let remaining = (length - leading.len() as f64).max(0.0);
    bound.define(
        "length",
        Property::with_attributes(Value::Int(remaining as i32), false, false),
    );
    if let Some(object) = this.as_object()
        && let Some(Value::String(name)) = object.own_property("name").and_then(|p| p.value)
    {
        let text = format!("bound {}", name.as_str());
        bound.define(
            "name",
            Property::with_attributes(interp.string(text), false, false),
        );
    }
    Ok(Value::Object(bound))
}

fn to_string(interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    let name = this
        .as_object()
        .and_then(|object| object.own_property("name"))
        .and_then(|property| property.value)
        .map(|value| value.to_js_string())
        .unwrap_or_default();
    Ok(interp.string(format!("function {name}() {{\n    [native code]\n}}")))
}

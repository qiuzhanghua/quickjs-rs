//! The global functions and `console`.
//!
//! `print` and `console` are host additions (the reference's CLI provides them), so
//! they live here with the language-defined globals rather than in a separate host
//! module.

use crate::builtins::number;
use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, NativeFn, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install the global functions, constants and `console`.
pub fn install(interp: &mut Interp<'_>, boolean_proto: &Gc<JsObject>) {
    number::install_boolean(interp, boolean_proto);
    let global = interp.realm.global.clone();

    for (name, length, func) in [
        ("parseInt", 2, number::parse_int as NativeFn),
        ("parseFloat", 1, number::parse_float),
        ("isNaN", 1, number::global_is_nan),
        ("isFinite", 1, number::global_is_finite),
        ("eval", 1, eval),
        ("encodeURIComponent", 1, encode_uri_component),
        ("decodeURIComponent", 1, decode_uri_component),
    ] {
        define_method(&global, name, length, func, interp);
    }

    // `print` and `console` are what the CLI adds; nothing in the language needs
    // them, but every script in the reference's test suite uses them.
    let print = interp.new_native("print", 0, js_print);
    global.define_data("print", Value::Object(print.clone()));
    let console = interp.new_object(Some(interp.realm.object_proto.clone()));
    for name in ["log", "info", "debug", "warn", "error"] {
        console.define_data(name, Value::Object(print.clone()));
    }
    global.define_data("console", Value::Object(console));
    global.define_data("globalThis", Value::Object(global.clone()));

    for (name, value) in [
        ("undefined", Value::Undefined),
        ("NaN", Value::Float(f64::NAN)),
        ("Infinity", Value::Float(f64::INFINITY)),
    ] {
        global.define(name, Property::with_attributes(value, false, false));
    }
}

/// `print(...)`: join the arguments with a space and write a line.
///
/// `undefined` is the result, so `print(print(1))` prints two lines.
fn js_print(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut line = String::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&crate::interp::debug_display(interp, argument));
    }
    line.push('\n');
    interp.output.push_str(&line);
    Ok(Value::Undefined)
}

/// `eval(source)`.
///
/// Direct `eval` is specified to see the calling scope, which needs the compiler to
/// emit a variable-environment hand-off. This implementation evaluates in global
/// scope only, which is correct for `eval("1+1")` — what the test suite uses — and
/// wrong for a direct eval that closes over local variables.
fn eval(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let source = match args.first() {
        Some(Value::String(text)) => text.as_str().to_string(),
        // Anything that is not a string is returned unchanged, per the spec.
        Some(other) => return Ok(other.clone()),
        None => return Ok(Value::Undefined),
    };
    interp.eval_in_global(&source)
}

/// `encodeURIComponent`, percent-encoding everything outside the unreserved set.
fn encode_uri_component(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let mut out = String::new();
    for byte in text.bytes() {
        let keep = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    Ok(interp.string(out))
}

/// `decodeURIComponent`.
fn decode_uri_component(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &text[index + 1..index + 3];
            match u8::from_str_radix(hex, 16) {
                Ok(byte) => {
                    out.push(byte);
                    index += 3;
                    continue;
                }
                Err(_) => {
                    return Err(crate::interp::Signal::Throw(
                        interp.make_error("URIError", "malformed URI sequence"),
                    ));
                }
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    Ok(interp.string(String::from_utf8_lossy(&out).into_owned()))
}

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
        // The host-facing collector hook. Not a language global: it is installed under a
        // double-underscore name and the `$262` harness object exposes it as `gc`, which is
        // how a test asks for a collection. A script has no other way to request one, and
        // the reference's `$262.gc` is the same arrangement.
        ("__gc", 0, gc),
        ("encodeURIComponent", 1, encode_uri_component),
        ("decodeURIComponent", 1, decode_uri_component),
        ("encodeURI", 1, encode_uri),
        ("decodeURI", 1, decode_uri),
        // `escape`/`unescape` are Annex B, not removed: they are still in every engine and
        // test262 checks them (`test/built-ins/escape`, `…/unescape`).
        ("escape", 1, escape),
        ("unescape", 1, unescape),
    ] {
        define_method(&global, name, length, func, interp);
    }
    // `eval` is special-cased in the reference: a *direct* `eval(...)` compiles to an
    // opcode rather than a call, so it never appears as a frame in a stack trace.
    // Ours reaches the same native through an ordinary call, so the marker tells the
    // interpreter to leave that frame out and keep the trace identical.
    if let Some(property) = global.own_property("eval")
        && let Some(Value::Object(eval_fn)) = property.value
    {
        eval_fn.define(
            "_frame_less",
            Property::with_attributes(Value::Bool(true), false, false),
        );
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
    // Straight through to the host, flushed per line, so output is visible as it happens
    // rather than when the script ends. Dropping the error is deliberate: `print` cannot
    // report a broken pipe to the script, and the reference ignores write failures too.
    if let Some(sink) = interp.output_sink.as_mut() {
        let _ = sink.write_all(line.as_bytes());
        let _ = sink.flush();
    }
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
        Some(Value::String(text)) => text.to_string_lossy().into_owned(),
        // Anything that is not a string is returned unchanged, per the spec.
        Some(other) => return Ok(other.clone()),
        None => return Ok(Value::Undefined),
    };
    interp.eval_in_global(&source)
}

/// Collect unreachable cycles and drain the finalizer queue.
///
/// Reference counting already reclaimed everything with no live handle; this only matters
/// for a *cycle*, and for giving `FinalizationRegistry` a safe point to run its callbacks
/// from. It is what `$262.gc()` in the test harness ends up calling.
fn gc(interp: &mut Interp<'_>, _this: Value, _args: &[Value]) -> JsResult<Value> {
    interp.heap.collect();
    crate::builtins::weak::drain_finalizers(interp)?;
    Ok(Value::Undefined)
}

/// `encodeURIComponent`, percent-encoding everything outside the unreserved set.
fn encode_uri_component(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    Ok(interp.string(percent_encode(&text, false)))
}

/// `encodeURI`, which additionally leaves the URI's own delimiters alone.
///
/// §19.2.6.3's `uriReserved` plus `#`: `;/?:@&=+$,#`. The difference from
/// `encodeURIComponent` is exactly this set, and it is why `encodeURI("a b/c")` is
/// `"a%20b/c"` while the component form gives `"a%20b%2Fc"`.
fn encode_uri(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    Ok(interp.string(percent_encode(&text, true)))
}

/// The shared percent-encoder. `keep_reserved` is the only difference between the two.
fn percent_encode(text: &str, keep_reserved: bool) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        let unreserved = byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')'
            );
        let reserved = keep_reserved
            && matches!(
                byte,
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
            );
        if unreserved || reserved {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `decodeURIComponent`.
fn decode_uri_component(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    decode_percent(interp, &text, false)
}

/// `decodeURI`, which leaves a percent-encoded reserved character encoded.
///
/// §19.2.6.2: a decoded byte that is one of `;/?:@&=+$,#` is put back in its `%XX` form,
/// so `decodeURI("%2F")` is `"%2F"` while `decodeURIComponent("%2F")` is `"/"`.
fn decode_uri(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    decode_percent(interp, &text, true)
}

/// The shared percent-decoder: `keep_reserved` is `decodeURI`'s extra re-encoding step.
fn decode_percent(interp: &mut Interp<'_>, text: &str, keep_reserved: bool) -> JsResult<Value> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            out.push(bytes[index]);
            index += 1;
            continue;
        }
        // A `%` has to be followed by exactly two hex digits. Slicing the *bytes*
        // rather than the `&str` matters: a `%` inside multi-byte text would not sit
        // on a character boundary, and slicing by index would panic.
        let decoded = read_escape(bytes, index).ok_or_else(|| uri_error(interp))?;
        let (byte, next) = decoded;
        if keep_reserved
            && matches!(
                byte,
                b';' | b'/' | b'?' | b':' | b'@' | b'&' | b'=' | b'+' | b'$' | b',' | b'#'
            )
        {
            out.extend_from_slice(&bytes[index..next]);
        } else {
            out.push(byte);
        }
        index = next;
    }
    // The percent-decoded bytes have to spell valid UTF-8; a truncation such as
    // `decodeURIComponent("%E4%B8")` is a URI error, not replacement characters.
    String::from_utf8(out)
        .map(|text| interp.string(text))
        .map_err(|_| uri_error(interp))
}

/// One `%XX` escape at `index`, answering the byte and the index just past it.
fn read_escape(bytes: &[u8], index: usize) -> Option<(u8, usize)> {
    let hex = bytes.get(index + 1..index + 3)?;
    let hex = std::str::from_utf8(hex).ok()?;
    let byte = u8::from_str_radix(hex, 16).ok()?;
    Some((byte, index + 3))
}

/// `escape(string)` (Annex B §B.2.1.1).
///
/// Not a URI function at all, whatever the name suggests: it works on *code units*. A unit
/// that is ASCII alphanumeric or one of `@*_+-./` is kept, one below 256 becomes `%XX`, and
/// anything wider becomes `%uXXXX`. That is why `escape("\u00FF")` is `"%FF"` — a single
/// byte, not the UTF-8 pair `encodeURIComponent` would produce.
fn escape(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let mut out = String::new();
    for unit in text.encode_utf16() {
        let kept = unit < 0x80
            && (char::from(unit as u8).is_ascii_alphanumeric()
                || matches!(unit as u8, b'@' | b'*' | b'_' | b'+' | b'-' | b'.' | b'/'));
        if kept {
            out.push(unit as u8 as char);
        } else if unit < 0x100 {
            out.push_str(&format!("%{unit:02X}"));
        } else {
            out.push_str(&format!("%u{unit:04X}"));
        }
    }
    Ok(interp.string(out))
}

/// `unescape(string)` (Annex B §B.2.1.2): the inverse of [`escape`].
///
/// Only `%uXXXX` and `%XX` are recognised; any other `%` is left as itself rather than
/// being a `URIError`, which is the one way this differs from `decodeURIComponent`.
fn unescape(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = arg(args, 0).to_js_string();
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out: Vec<u16> = Vec::with_capacity(units.len());
    let mut index = 0;
    while index < units.len() {
        if units[index] != b'%' as u16 {
            out.push(units[index]);
            index += 1;
            continue;
        }
        // `%uXXXX` first: its `u` would otherwise make it an invalid `%XX`.
        if units.get(index + 1) == Some(&(b'u' as u16)) {
            if let Some(hex) = units.get(index + 2..index + 6) {
                let digits: String = hex.iter().map(|unit| *unit as u8 as char).collect();
                if let Ok(unit) = u16::from_str_radix(&digits, 16) {
                    out.push(unit);
                    index += 6;
                    continue;
                }
            }
        } else if let Some(hex) = units.get(index + 1..index + 3) {
            let digits: String = hex.iter().map(|unit| *unit as u8 as char).collect();
            if let Ok(byte) = u8::from_str_radix(&digits, 16) {
                out.push(byte as u16);
                index += 3;
                continue;
            }
        }
        // Not an escape: the `%` stands for itself. `unescape("%")` is `"%"`.
        out.push(units[index]);
        index += 1;
    }
    Ok(Value::String(js_gc::Gc::new(
        interp.heap,
        crate::value::JsString::from_wtf8(crate::value::Wtf8::from_units(&out)),
    )))
}

/// The `URIError` a malformed percent-escape raises.
fn uri_error(interp: &Interp<'_>) -> crate::interp::Signal {
    crate::interp::Signal::Throw(interp.make_error("URIError", "malformed URI sequence"))
}

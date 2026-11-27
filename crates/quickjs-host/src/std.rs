//! The `std` module: what the reference's `quickjs-libc.c` installs, minus the parts that
//! need a file object this crate has not built yet.
//!
//! # Order of arrival
//!
//! M9b is being built in pieces, each one verified against `qjs --std` before the next:
//!
//! 1. `printf`/`sprintf` — pure formatting, and the piece with the most surprising details
//! 2. the file object (`open`/`tmpfile`/`popen`/`fdopen` and its 19 methods)
//! 3. environment, `loadFile`/`loadScript`/`evalScript`, `strerror`, `urlGet`, `exit`, `gc`
//! 4. `parseExtJSON`
//! 5. `__sprintObject`/`__printObject`
//!
//! A function that has not arrived yet is **absent** rather than present and broken, so a
//! script that needs it fails at `std.whatever is not a function` — the failure mode this
//! project prefers over a stub that silently does nothing.

use quickjs::{Context, Interp, JsResult, Value};

/// Install the `std` functions that exist into `object`.
pub fn install(ctx: &mut Context<'_>, object: &Value) -> quickjs::Result<()> {
    if object.as_object().is_none() {
        return Err(quickjs::Error::Exception(
            "std: target is not an object".to_string(),
        ));
    }
    // `printf` writes to stdout and returns the byte count; `sprintf` returns the string.
    // The two share every rule about the format string, which is why only the sink differs.
    let sprintf = ctx.new_host_function_value("sprintf", 1, Box::new(js_sprintf))?;
    ctx.set_property(object, "sprintf", sprintf)?;
    let printf = ctx.new_host_function_value("printf", 1, Box::new(js_printf))?;
    ctx.set_property(object, "printf", printf)?;
    let parse_ext = ctx.new_host_function_value("parseExtJSON", 1, Box::new(js_parse_ext_json))?;
    ctx.set_property(object, "parseExtJSON", parse_ext)?;
    Ok(())
}

/// `std.parseExtJSON(text)`.
///
/// The extended dialect lives in the engine's JSON parser behind a flag, exactly as the
/// reference implements it (`JS_ParseJSON2(..., JS_PARSE_JSON_EXT)`), rather than as a
/// pre-processing pass or a second parser. That keeps one grammar with one set of error
/// positions — and keeps `JSON.parse` strict, because it simply does not pass the flag.
fn js_parse_ext_json(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    // `JS_ToCStringLen` on the argument: a non-string is converted, and a missing one is
    // `undefined` → `"undefined"`, which then fails to parse. Same order as the reference.
    let text = interp.to_js_string(args.first().unwrap_or(&Value::Undefined))?;
    interp.parse_ext_json(&text)
}

/// `std.sprintf(format, ...args)`.
fn js_sprintf(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::printf::sprintf(interp, args)?;
    Ok(interp.string(text))
}

/// `std.printf(format, ...args)`.
///
/// C writes the bytes to `stdout` and returns how many it wrote. The host's sink is where
/// script output belongs, so the text goes there, and the count is the byte length.
///
/// Nothing is flushed separately: the sink is line-buffered by contract (see
/// `Context::set_output_sink`), and `printf` has no newline of its own.
fn js_printf(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let text = crate::printf::sprintf(interp, args)?;
    let written = text.len() as i32;
    if let Some(sink) = interp.output_sink.as_mut() {
        use std::io::Write;
        // A write failure is not the script's to handle: C's `fwrite` returns short and the
        // count is what it returns, which is the same shape as a partial write here.
        let _ = sink.write_all(text.as_bytes());
    }
    // `printf` is also part of `output`, so a host reading `Context::output()` sees it.
    interp.output.push_str(&text);
    Ok(Value::Int(written))
}

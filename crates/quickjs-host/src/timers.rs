//! `os.setTimeout` and `os.clearTimeout`, and nothing else.
//!
//! Both the names and the arithmetic here are copied from `js_os_setTimeout` /
//! `js_os_clearTimeout` rather than from browser habits, because the differences are
//! visible to scripts. Checked against `qjs --std`:
//!
//! | call | reference |
//! |---|---|
//! | `typeof os.setInterval` | `"undefined"` — it does not exist |
//! | `typeof os.clearInterval` | `"undefined"` — it does not exist |
//! | `os.setTimeout(42, 1)` | throws `TypeError` **before** scheduling |
//! | `os.setTimeout(f, 0, "x", "y")` | `f` is called with **no** arguments |
//! | `typeof os.setTimeout()` | `"number"`, an `Int32` |
//! | `os.setTimeout.length` | `2` |
//!
//! Every one of those is a place where a plausible-looking implementation would diverge,
//! and `setInterval` in particular is an easy thing to add by reflex. It is left out
//! deliberately: a timer the reference cannot produce is a timer the differential suite
//! cannot check.
//!
//! The bodies are thin on purpose — *when* something is due belongs to the engine
//! ([`quickjs::Interp::schedule_timer`]), *waiting* belongs to [`crate::eventloop`], and
//! what is left is argument marshalling.

use quickjs::{Context, Interp, JsResult, Value};

/// Install `setTimeout` and `clearTimeout` on `object`.
///
/// The reference puts them on the `os` module rather than on `globalThis`, so this installs
/// into an object the caller owns instead of assuming a global name. That keeps the timer
/// machinery testable before `os` exists, and lets M9d fill in the rest of `os` around it.
pub fn install(ctx: &mut Context<'_>, object: &Value) -> quickjs::Result<()> {
    if object.as_object().is_none() {
        return Err(quickjs::Error::Exception(
            "timers: target is not an object".to_string(),
        ));
    }
    let set = ctx.new_host_function_value("setTimeout", 2, Box::new(set_timeout))?;
    ctx.set_property(object, "setTimeout", set)?;
    let clear = ctx.new_host_function_value("clearTimeout", 1, Box::new(clear_timeout))?;
    ctx.set_property(object, "clearTimeout", clear)?;
    Ok(())
}

/// `setTimeout(callback, delay_ms)`.
///
/// `if (!JS_IsFunction(ctx, func)) return JS_ThrowTypeError(ctx, "not a function");` — the
/// reference rejects a non-callable at *schedule* time, not at fire time, so this does too.
/// Getting that wrong turns a clear error into a mysterious one that appears later and
/// somewhere else.
fn set_timeout(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let callback = args.first().cloned().unwrap_or(Value::Undefined);
    if !callback
        .as_object()
        .is_some_and(|object| object.is_callable())
    {
        return Err(interp.type_error("not a function"));
    }
    // `JS_ToInt64` on the delay: `undefined` and `NaN` both become 0, and a negative value
    // becomes negative — which then means "already due".
    let delay = args.get(1).map_or(0.0, Value::to_number);
    let id = interp.schedule_timer(callback, delay);
    // `JS_NewInt32`, not a float: `typeof` would say `"number"` either way, but `String(id)`
    // and `JSON.stringify(id)` would not.
    Ok(Value::Int(id))
}

/// `clearTimeout(id)`.
///
/// `find_timer_by_id` returns `NULL` for `id <= 0`, so a junk argument silently cancels
/// nothing rather than accidentally matching a timer.
fn clear_timeout(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let id = args.first().map_or(0.0, Value::to_number);
    // The `id <= 0` test lives here rather than in the engine, mirroring the reference's
    // split: `clearTimeout(0)` and `clearTimeout(NaN)` are no-ops, and `NaN as i32` is 0.
    if id >= 1.0 && id.is_finite() {
        interp.cancel_timer(id as i32);
    }
    Ok(Value::Undefined)
}

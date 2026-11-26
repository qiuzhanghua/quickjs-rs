//! The `Proxy` constructor and `Proxy.revocable`.
//!
//! Almost nothing lives here: `Proxy` is a constructor whose only job is to build a
//! [`crate::object::ObjectKind::Proxy`], and every trap is dispatched from
//! `Interp`'s internal-method layer. The two things this module does own are the checks
//! that happen *at construction* — a target must be an object and a handler must not be
//! `null` — and `revocable`, which is the only way a script can take a proxy apart.

use crate::builtins::{arg, define_method};
use crate::interp::{Interp, JsResult};
use crate::object::{JsObject, Property};
use crate::value::Value;
use js_gc::Gc;

/// Install `Proxy` on the realm's global object.
pub fn install(interp: &mut Interp<'_>) {
    let constructor = interp.new_native_constructor("Proxy", 2, constructor);
    // `Proxy` is a function you call with `new`, but it has no `prototype` property:
    // proxies inherit from their *target's* prototype, so there is nothing for one to
    // sit on.
    constructor.define(
        "_not_a_constructor",
        Property::with_attributes(Value::Bool(false), false, false),
    );
    define_method(&constructor, "revocable", 2, revocable, interp);

    let global = interp.realm.global.clone();
    global.define(
        "Proxy",
        Property::with_attributes(Value::Object(constructor), true, false),
    );
}

/// The target and handler arguments, checked.
fn target_and_handler(interp: &mut Interp<'_>, args: &[Value]) -> JsResult<(Gc<JsObject>, Value)> {
    let target = arg(args, 0);
    let Value::Object(target) = target else {
        return Err(interp.type_error("proxy target must be an object"));
    };
    let handler = arg(args, 1);
    // A primitive handler would make every trap read fail later with a confusing message;
    // the spec refuses it here, where the script can still tell what it did wrong.
    if matches!(handler, Value::Null) {
        return Err(interp.type_error("proxy handler must be an object"));
    }
    Ok((target, handler))
}

/// `new Proxy(target, handler)`.
fn constructor(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let (target, handler) = target_and_handler(interp, args)?;
    Ok(Value::Object(interp.new_proxy(target, handler)))
}

/// `Proxy.revocable(target, handler)` → `{ proxy, revoke }`.
fn revocable(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let (target, handler) = target_and_handler(interp, args)?;
    let proxy = interp.new_proxy(target, handler);
    // The revoke function holds the *same* slots the proxy reads, so clearing them here is
    // what makes the proxy inert — and drops the collector's edge to the target and the
    // handler, which is the point of revoking at all.
    let slots = proxy.proxy_state().expect("a proxy was just built");
    let revoke = interp.new_revoker(slots);
    let result = interp.new_object(None);
    result.define_data("proxy", Value::Object(proxy));
    result.define_data("revoke", Value::Object(revoke));
    Ok(Value::Object(result))
}

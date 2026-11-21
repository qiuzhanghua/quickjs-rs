//! The built-in globals a script can reach without an import.
//!
//! Only what the VM needs to be observable: `print`, a `console` object, and the
//! prototype skeletons the object model hangs off. The full standard library is the
//! builtins milestone's job; this is the minimum that makes a script's output
//! comparable with the reference.

use crate::interp::{Interp, JsResult, Signal};
use crate::object::{JsObject, Property};
use crate::value::Value;

/// Install the globals on `interp`'s realm.
pub fn install(interp: &mut Interp<'_>) {
    let print = interp.new_native("print", 0, js_print);
    let global = interp.realm.global.clone();
    global.define_data("print", Value::Object(print.clone()));

    let console = interp.new_object(Some(interp.realm.object_proto.clone()));
    console.define_data("log", Value::Object(print.clone()));
    console.define_data("info", Value::Object(print.clone()));
    console.define_data("warn", Value::Object(print));
    global.define_data("console", Value::Object(console));
    global.define_data("globalThis", Value::Object(global.clone()));

    // The three global constants. All non-writable, non-enumerable and
    // non-configurable, which is why a plain assignment to them is silently
    // ignored in sloppy mode.
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
/// The `undefined` result is what the reference returns, so `print(print(1))`
/// prints two lines.
fn js_print(interp: &mut Interp<'_>, _this: Value, args: &[Value]) -> JsResult<Value> {
    let mut line = String::new();
    for (index, argument) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        line.push_str(&crate::interp::display(argument));
    }
    line.push('\n');
    interp.output.push_str(&line);
    Ok(Value::Undefined)
}

/// `js_gc::Heap` is not nameable from here without a handle; keep the import used.
const _: Option<fn(&JsObject)> = None;

/// Raise a `TypeError`, which is what most argument checks end in.
#[allow(dead_code)]
fn type_error(interp: &Interp<'_>, message: &str) -> Signal {
    Signal::Throw(interp.make_error("TypeError", message))
}

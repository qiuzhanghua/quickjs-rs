//! `%GeneratorPrototype%`: `next`, `return` and `throw`.
//!
//! The object itself is an ordinary object — the state that matters lives in its
//! `ObjectKind::Generator` payload — so all three methods do the same thing: ask the
//! interpreter to advance the frame with a different "which method was this" tag.
//!
//! `%GeneratorPrototype%[Symbol.iterator]`, which is what makes a generator usable as an
//! iterable in `for (… of …)` and `[...g]`, arrives with the `Symbol` builtin. Until then
//! `Interp::get_iterator` recognises an object with a callable `next` directly, which is
//! the same answer by a shorter route.

use crate::interp::{Interp, JsResult, ResumeKind, Signal};
use crate::object::{JsObject, ObjectKind};
use crate::value::Value;
use js_gc::Gc;

/// The generator a `this` refers to, or the `TypeError` the spec raises.
fn generator_of(interp: &Interp<'_>, this: &Value) -> Result<Gc<JsObject>, Value> {
    match this {
        Value::Object(object)
            if object.with(|data| matches!(data.kind, ObjectKind::Generator(_))) =>
        {
            Ok(object.clone())
        }
        _ => Err(interp.make_error("TypeError", "not a generator")),
    }
}

fn next(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_generator(&generator, ResumeKind::Next, value)
}

fn generator_return(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_generator(&generator, ResumeKind::Return, value)
}

fn generator_throw(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_generator(&generator, ResumeKind::Throw, value)
}

pub fn install(interp: &mut Interp<'_>, proto: &Gc<JsObject>) {
    let object_proto = interp.realm.object_proto.clone();
    proto.set_proto(Some(object_proto));
    for (name, length, func) in [
        ("next", 1, next as crate::object::NativeFn),
        ("return", 1, generator_return),
        ("throw", 1, generator_throw),
    ] {
        super::define_method(proto, name, length, func, interp);
    }
}

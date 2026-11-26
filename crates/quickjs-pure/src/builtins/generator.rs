//! `%GeneratorPrototype%`: `next`, `return` and `throw`.
//!
//! The object itself is an ordinary object — the state that matters lives in its
//! `ObjectKind::Generator` payload — so all three methods do the same thing: ask the
//! interpreter to advance the frame with a different "which method was this" tag.
//!
//! `%GeneratorPrototype%[Symbol.iterator]` is installed here too: it answers `this`, which
//! is what makes a generator usable directly in `for (… of …)` and `[...g]`.

use crate::interp::{Interp, JsResult, ResumeKind, Signal};
use crate::object::{JsObject, ObjectKind, Property};
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
    // `%GeneratorPrototype%` inherits from `%IteratorPrototype%`, so a generator *is* an
    // `Iterator` and `gen().map(f)` resolves `map` up the chain. Hanging it off
    // `Object.prototype` instead left `.map` undefined.
    let parent = interp.realm.iterator_proto.clone();
    proto.set_proto(parent);
    for (name, length, func) in [
        ("next", 1, next as crate::object::NativeFn),
        ("return", 1, generator_return),
        ("throw", 1, generator_throw),
    ] {
        super::define_method(proto, name, length, func, interp);
    }
    // `%GeneratorPrototype%[Symbol.iterator]` answers `this`: a generator is its own
    // iterator, which is what makes `for (… of gen())` work.
    let key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::Iterator);
    let identity = interp.new_native("[Symbol.iterator]", 0, iterator);
    proto.define_key(&key, Property::data(Value::Object(identity)));

    // `%AsyncGeneratorPrototype%`: same methods, but each answers a promise, and
    // `Symbol.asyncIterator` is what `for await` looks for.
    let object_proto = interp.realm.object_proto.clone();
    let async_proto = interp.new_object(Some(object_proto));
    interp.realm.async_generator_proto = Some(async_proto.clone());
    for (name, length, func) in [
        ("next", 1, async_next as crate::object::NativeFn),
        ("return", 1, async_generator_return),
        ("throw", 1, async_generator_throw),
    ] {
        super::define_method(&async_proto, name, length, func, interp);
    }
    let async_key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::AsyncIterator);
    let identity = interp.new_native("[Symbol.asyncIterator]", 0, iterator);
    async_proto.define_key(&async_key, Property::data(Value::Object(identity)));
}

/// `%AsyncGeneratorPrototype%.next`.
fn async_next(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_async_generator(&generator, ResumeKind::Next, value)
}

fn async_generator_return(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_async_generator(&generator, ResumeKind::Return, value)
}

fn async_generator_throw(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
    let generator = match generator_of(interp, &this) {
        Ok(generator) => generator,
        Err(error) => return Err(Signal::Throw(error)),
    };
    let value = args.first().cloned().unwrap_or(Value::Undefined);
    interp.resume_async_generator(&generator, ResumeKind::Throw, value)
}

/// `%GeneratorPrototype%[Symbol.iterator]`.
fn iterator(_interp: &mut Interp<'_>, this: Value, _args: &[Value]) -> JsResult<Value> {
    Ok(this)
}

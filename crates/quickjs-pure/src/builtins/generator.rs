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

    // `%GeneratorFunction.prototype%` — the object a generator *function* inherits from.
    // It sits on `Function.prototype`, and its one job that is visible from script is
    // `Symbol.toStringTag: "GeneratorFunction"`. Keeping the tag here rather than
    // hard-coding it in `Object.prototype.toString` is what makes
    // `delete g.constructor.prototype[Symbol.toStringTag]` observable.
    let function_proto = interp.realm.function_proto.clone();
    let tag_key = interp
        .realm
        .well_known_key(crate::interp::WellKnown::ToStringTag);
    // `%GeneratorFunction%` is a real (but unreachable) constructor whose `prototype` is
    // `%GeneratorFunction.prototype%`. It has to exist because `g.constructor` resolves
    // through the proto chain and must be *this*, not `Function`: deleting
    // `g.constructor.prototype[Symbol.toStringTag]` is how a test removes the tag, and if
    // `constructor` pointed at `Function` that `delete` would hit the wrong object.
    let generator_function = interp.new_native("GeneratorFunction", 1, {
        // A native fn is a plain pointer, so the kind is baked in by building one closure
        // per kind and boxing the pointer. `GeneratorFunction` is the only one that can
        // reach this path with `new`, and the others get their own below.
        fn trampoline(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
            generator_function_constructor(FunctionFactory::Generator)(interp, this, args)
        }
        trampoline
    });
    generator_function.set_constructible(true);
    let generator_function_proto = interp.new_object(Some(function_proto.clone()));
    generator_function.define(
        "prototype",
        Property::with_attributes(
            Value::Object(generator_function_proto.clone()),
            false,
            false,
        ),
    );
    generator_function_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(generator_function.clone()), true, false),
    );
    generator_function_proto.define_key(
        &tag_key,
        Property {
            value: Some(interp.string("GeneratorFunction")),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::data(Value::Undefined)
        },
    );
    // A generator function's `[[Prototype]]` *is* this object, so `g.constructor` finds
    // the constructor above.
    interp.realm.generator_function_proto = Some(generator_function_proto.clone());
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
    let async_function = interp.new_native("AsyncFunction", 1, {
        fn trampoline(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
            generator_function_constructor(FunctionFactory::Async)(interp, this, args)
        }
        trampoline
    });
    async_function.set_constructible(true);
    let async_function_proto = interp.new_object(Some(function_proto.clone()));
    async_function.define(
        "prototype",
        Property::with_attributes(Value::Object(async_function_proto.clone()), false, false),
    );
    async_function_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(async_function.clone()), true, false),
    );
    async_function_proto.define_key(
        &tag_key,
        Property {
            value: Some(interp.string("AsyncFunction")),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::data(Value::Undefined)
        },
    );
    // `%AsyncGeneratorFunction.prototype%` inherits from `%AsyncFunction.prototype%`, so an
    // async generator function is both.
    let async_generator_function = interp.new_native("AsyncGeneratorFunction", 1, {
        fn trampoline(interp: &mut Interp<'_>, this: Value, args: &[Value]) -> JsResult<Value> {
            generator_function_constructor(FunctionFactory::AsyncGenerator)(interp, this, args)
        }
        trampoline
    });
    async_generator_function.set_constructible(true);
    let async_generator_function_proto = interp.new_object(Some(async_function_proto.clone()));
    async_generator_function.define(
        "prototype",
        Property::with_attributes(
            Value::Object(async_generator_function_proto.clone()),
            false,
            false,
        ),
    );
    async_generator_function_proto.define(
        "constructor",
        Property::with_attributes(Value::Object(async_generator_function.clone()), true, false),
    );
    async_generator_function_proto.define_key(
        &tag_key,
        Property {
            value: Some(interp.string("AsyncGeneratorFunction")),
            writable: false,
            enumerable: false,
            configurable: true,
            ..Property::data(Value::Undefined)
        },
    );
    interp.realm.async_generator_function_proto = Some(async_generator_function_proto.clone());
    interp.realm.async_function_proto = Some(async_function_proto.clone());
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

/// `new %GeneratorFunction%(…)` and its async siblings: compile a function of that kind.
///
/// `%GeneratorFunction%` is reachable — `Object.getPrototypeOf(function*(){}).constructor`
/// — and `new (that)("a", "yield a")` really does produce a generator function. Sharing
/// `Function`'s constructor would give a plain function, so the *kind* is spliced into the
/// source instead of the call being refused.
fn generator_function_constructor(
    kind: FunctionFactory,
) -> impl Fn(&mut Interp<'_>, Value, &[Value]) -> JsResult<Value> {
    move |interp, _this, args| {
        let body = match args.last() {
            Some(value) => interp.to_js_string(value)?,
            None => String::new(),
        };
        let params: Vec<String> = {
            let mut out = Vec::new();
            for value in args.iter().take(args.len().saturating_sub(1)) {
                out.push(interp.to_js_string(value)?);
            }
            out
        };
        let params = params.join(",");
        let prefix = match kind {
            FunctionFactory::Generator => "function*",
            FunctionFactory::Async => "async function",
            FunctionFactory::AsyncGenerator => "async function*",
        };
        let source = format!("({prefix}({params}) {{{body}}})");
        let program = js_parse::parse_script(&source).map_err(|error| {
            crate::interp::Signal::Throw(interp.make_error("SyntaxError", &error.message))
        })?;
        let compiled = js_compile::compile_script(&program).map_err(|error| {
            crate::interp::Signal::Throw(interp.make_error("SyntaxError", &error.message))
        })?;
        // `new` hands us the instance the runtime built from `NewTarget.prototype`; a
        // generator function is a function object, so the compiled closure *is* the answer.
        let function = interp.run_function_source(compiled)?;
        if let Value::Object(object) = &function {
            object.define(
                "name",
                Property::with_attributes(interp.string("anonymous"), false, false),
            );
        }
        Ok(function)
    }
}

/// Which function kind a `%…Function%` constructor builds.
#[derive(Clone, Copy)]
enum FunctionFactory {
    Generator,
    Async,
    AsyncGenerator,
}

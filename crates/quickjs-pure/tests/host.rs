//! The host surface: a Rust program registering functions a script can call.
//!
//! This is the first thing an embedding needs and the last thing the engine could offer.
//! Before it there was no way in at all: `Context::interp()` is private, and `NativeFn` is
//! a bare `fn` pointer, so a host function had nowhere to keep state. Everything here is
//! about state: a call counter, a captured vector, a sink that records the order in which
//! output appeared.

use std::cell::RefCell;
use std::rc::Rc;

use quickjs::{Context, Runtime, Value};

/// Run `source`, returning what `print` produced.
fn run(register: impl FnOnce(&mut Context<'_>), source: &str) -> String {
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    register(&mut context);
    context.eval(source).expect("script runs");
    context.output().to_string()
}

#[test]
fn a_host_function_is_callable_from_script_and_keeps_its_state() {
    // The point of `HostFn` over `NativeFn`: the closure outlives the call, so a counter
    // written in Rust accumulates across invocations. A `fn` pointer could not.
    // The closure is stored on the function object, so it outlives this call: it has to
    // *own* its state, and `Rc<RefCell<…>>` is how the test reads it back afterwards.
    let calls = Rc::new(RefCell::new(0usize));
    let arguments = Rc::new(RefCell::new(Vec::<String>::new()));
    let output = run(
        |context| {
            let calls = calls.clone();
            let arguments = arguments.clone();
            context
                .set_global_function(
                    "record",
                    2,
                    Box::new(move |interp, _this, args| {
                        *calls.borrow_mut() += 1;
                        // `to_js_string` is how a host reads an argument; it is the same
                        // conversion the builtins use, so an object goes through
                        // `ToPrimitive` rather than appearing as `[object Object]`.
                        let joined = args
                            .iter()
                            .map(|value| interp.to_js_string(value).unwrap_or_default())
                            .collect::<Vec<_>>()
                            .join("|");
                        arguments.borrow_mut().push(joined);
                        Ok(Value::Undefined)
                    }),
                )
                .unwrap();
        },
        r#"
        record("a", 1);
        record("b", 2);
        record("c");
        print("done");
        "#,
    );
    assert_eq!(output, "done\n");
    assert_eq!(
        *calls.borrow(),
        3,
        "the closure's counter is shared across calls"
    );
    assert_eq!(*arguments.borrow(), ["a|1", "b|2", "c"]);
}

#[test]
fn a_host_function_can_return_a_value_the_script_uses() {
    // A host function is not just a sink: its return value is an ordinary JS value, so it
    // can be allocated on the heap (here a string) and flow through the script.
    let output = run(
        |context| {
            context
                .set_global_function(
                    "hostAdd",
                    2,
                    Box::new(|interp, _this, args| {
                        let total: f64 = args.iter().map(Value::to_number).sum();
                        Ok(interp.string(format!("{total}")))
                    }),
                )
                .unwrap();
        },
        r#"print(hostAdd(2, 3).length, hostAdd(2, 3));"#,
    );
    assert_eq!(output, "1 5\n");
}

#[test]
fn the_host_can_call_into_script() {
    // The other direction, which a timer callback needs: Rust holds a JS function and
    // calls it. `Context::call` is the only way to do that from outside.
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    let function = context
        .eval("(function (a, b) { return a * b })")
        .expect("function parses");
    let result = context
        .call(
            function,
            Value::Undefined,
            vec![Value::Int(6), Value::Int(7)],
        )
        .expect("call succeeds");
    assert_eq!(result.to_number(), 42.0);

    // A throw travels back as an error rather than unwinding through Rust.
    let thrower = context
        .eval("(function () { throw new TypeError('nope') })")
        .expect("function parses");
    let error = context
        .call(thrower, Value::Undefined, Vec::new())
        .expect_err("the throw must surface");
    assert!(
        error.to_string().contains("nope"),
        "the host sees the script's own error: {error}"
    );
}

#[test]
fn a_host_constructor_builds_an_instance_with_a_prototype() {
    // The shape `std.File` needs: a constructor whose instances carry host-set properties
    // and inherit methods from the prototype the host was given.
    let output = run(
        |context| {
            let constructor = context
                .set_global_constructor(
                    "Point",
                    2,
                    Box::new(|interp, _this, args| {
                        // `own_instance` is true for a host constructor, so the body makes
                        // the object; `this` is `undefined` by the time it runs.
                        let proto = interp
                            .current_native()
                            .and_then(|callee| callee.own_property("prototype"))
                            .and_then(|property| property.value)
                            .and_then(|value| value.as_object().cloned())
                            .expect("host constructor has a prototype");
                        let object = interp.new_object(Some(proto));
                        object.define(
                            "x",
                            quickjs::Property::with_attributes(args[0].clone(), true, true),
                        );
                        object.define(
                            "y",
                            quickjs::Property::with_attributes(args[1].clone(), true, true),
                        );
                        Ok(Value::Object(object))
                    }),
                )
                .unwrap();
            let Some(proto) = constructor
                .as_object()
                .and_then(|object| object.own_property("prototype"))
                .and_then(|property| property.value)
            else {
                panic!("no prototype");
            };
            let sum = context
                .eval("(function () { return this.x + this.y })")
                .expect("method parses");
            context.set_property(&proto, "sum", sum).unwrap();
        },
        r#"
        var p = new Point(2, 3);
        print(p.x, p.y, p.sum(), p instanceof Point, typeof Point);
        "#,
    );
    assert_eq!(output, "2 3 5 true function\n");
}

#[test]
fn a_host_object_can_be_built_property_by_property() {
    // `new_object` + `set_property` + `set_global` is how a host module is assembled
    // without writing a constructor: `std` is one object with a dozen functions on it.
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    let module = context.new_object();
    context
        .set_property(&module, "answer", Value::Int(42))
        .unwrap();
    let name = context.new_string("quickjs");
    context.set_property(&module, "name", name).unwrap();
    let ready = context.new_boolean(true);
    context.set_property(&module, "ready", ready).unwrap();
    context.set_global("std", module).unwrap();

    context
        .eval("print(std.answer, std.name, std.ready, typeof std.missing)")
        .expect("script sees the module");
    assert_eq!(context.output(), "42 quickjs true undefined\n");
}

/// Output must reach the sink in the order it was produced, at the moment it is produced.
///
/// This is the requirement `test_std.js`'s async tests turn on: a timer callback that
/// prints after a `print` in the script body has to be observably later. Buffering the
/// whole script and printing at the end would satisfy "the same text" and fail "the same
/// order", so the check is on the sink's own timeline rather than on `output()`.
#[test]
fn a_sink_sees_output_when_it_happens() {
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl std::io::Write for Recorder {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(buf).into_owned());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let recorder = Recorder::default();
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    context.set_output_sink(Box::new(recorder.clone()));

    // A host function that prints *during* the script, like a timer callback would.
    context
        .set_global_function(
            "duringScript",
            0,
            Box::new(|interp, _this, _args| {
                interp.output.push_str("from host\n");
                if let Some(sink) = interp.output_sink.as_mut() {
                    let _ = std::io::Write::write_all(sink, b"from host\n");
                    let _ = std::io::Write::flush(sink);
                }
                Ok(Value::Undefined)
            }),
        )
        .unwrap();

    context
        .eval(r#"print("first"); duringScript(); print("third");"#)
        .expect("script runs");

    let chunks = recorder.0.lock().unwrap().clone();
    assert_eq!(
        chunks,
        ["first\n", "from host\n", "third\n"],
        "each line arrives as it is printed, not as one blob at the end"
    );
    // The buffer still holds the whole log, which is what `output()` promises.
    assert_eq!(context.output(), "first\nfrom host\nthird\n");
    // And draining it does not disturb the sink or double-print.
    let drained = context.take_output();
    assert_eq!(drained, "first\nfrom host\nthird\n");
    assert_eq!(context.output(), "");
}

/// A host function survives collection, and its state does — the `Trace` contract.
///
/// `Callable::Host` traces nothing, because the closure is opaque Rust and cannot be
/// walked. That is safe only under the rule in [`quickjs::Callable::Host`]'s neighbours:
/// the closure keeps Rust state and never a `Gc`. This test holds the rule to its word by
/// running a collection *explicitly* while a host closure with live state stays reachable,
/// then calling it again to prove both it and its captured `String` are intact.
#[test]
fn a_host_function_and_its_state_survive_collection() {
    let seen = Rc::new(RefCell::new(Vec::<String>::new()));
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    {
        let seen = seen.clone();
        context
            .set_global_function(
                "remember",
                1,
                Box::new(move |interp, _this, args| {
                    // A `String` captured by value: pure Rust state, which is the only
                    // kind this closure is allowed to hold.
                    let text = interp.to_js_string(&args[0]).unwrap_or_default();
                    seen.borrow_mut().push(text.clone());
                    Ok(interp.string(&text))
                }),
            )
            .expect("registration succeeds");
    }

    context.eval("remember(\"before\")").expect("first call");

    // Fill the heap with garbage, then collect for real. `run_gc` is the same entry point
    // `$262.gc()` reaches, so this is not "hope the allocator noticed".
    context
        .eval(
            r#"
            var junk = [];
            for (var i = 0; i < 5000; i++) junk.push({ i: i, s: "x" + i });
            junk = null;
            "#,
        )
        .expect("churn runs");
    let stats = runtime.run_gc();
    assert!(
        stats.collected > 0,
        "the churn must actually free something, or this proves nothing: {stats:?}"
    );

    context.eval("remember(\"after\")").expect("second call");
    assert_eq!(
        *seen.borrow(),
        ["before", "after"],
        "the closure kept its state across a real collection"
    );

    // And it is still callable from the host side, after the script finished.
    let function = context.eval("remember").expect("global is reachable");
    let value = context
        .call(function, Value::Undefined, vec![Value::Int(7)])
        .expect("still callable");
    assert_eq!(value.to_number(), 7.0);
    assert_eq!(*seen.borrow(), ["before", "after", "7"]);
}

/// Two contexts must not see each other's host state.
///
/// This is why `HostFn` captures rather than a `static`: plan 1.1 rejected the global
/// singleton precisely because `Runtime::new()` may be called more than once.
#[test]
fn host_state_is_per_context() {
    let first_runtime = Runtime::new();
    let mut first = Context::new(&first_runtime);
    first
        .set_global_function(
            "tag",
            0,
            Box::new(|interp, _this, _args| Ok(interp.string("one"))),
        )
        .expect("registration succeeds");

    let second_runtime = Runtime::new();
    let mut second = Context::new(&second_runtime);
    second
        .set_global_function(
            "tag",
            0,
            Box::new(|interp, _this, _args| Ok(interp.string("two"))),
        )
        .expect("registration succeeds");

    assert_eq!(
        first.eval("tag()").expect("first runs").to_js_string(),
        "one"
    );
    assert_eq!(
        second.eval("tag()").expect("second runs").to_js_string(),
        "two"
    );
}

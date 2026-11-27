//! `std.__sprintObject` against the reference's `JS_PrintValue`.
//!
//! Every expectation here was produced by `qjs --std`, and the set covers the cases the
//! reference's own `test_print_value` checks plus the limits and odd classes around it.
//!
//! The dumper is not `JSON.stringify` and not `console.log`: it walks quickjs's object
//! *shape*, which is why symbols print where they were defined rather than after the
//! strings, and why `Map`/`Set`/`Date`/`RegExp` print as themselves.

#![cfg(unix)]

use quickjs::{Context, Runtime};

fn run(source: &str) -> String {
    let runtime = Runtime::new();
    let mut ctx = Context::new(&runtime);
    let fds = quickjs_host::fdtable::FdTable::shared();
    let os = ctx.new_object();
    quickjs_host::timers::install(&mut ctx, &os).expect("timers");
    quickjs_host::os::install(&mut ctx, &os, &fds).expect("os");
    ctx.set_global("os", os).expect("mount os");
    let std = ctx.new_object();
    quickjs_host::std::install(&mut ctx, &std, &fds).expect("std");
    ctx.set_global("std", std).expect("mount std");
    ctx.eval(source).expect("script runs");
    ctx.output().to_string()
}

/// Primitives, which is where `-0`, `1e+21` and `Infinity` each need their own rule.
#[test]
fn primitives_print_as_the_reference_does() {
    let output = run(r#"
        var out = [];
        for (var v of [undefined, null, true, false, 0, -0, 1.5, 1e21, 1e-7,
                       NaN, Infinity, -Infinity, 10n, -3n, Symbol("s")])
            out.push(std.__sprintObject(v));
        out.push(std.__sprintObject("tab\there"));
        out.push(std.__sprintObject("back\\slash"));
        out.push(std.__sprintObject("\u0001\u007f"));
        print(out.join("|"));
        "#);
    // `-0` keeps its sign (`JS_DTOA_MINUS_ZERO`) where `String(-0)` would print `0`.
    assert_eq!(
        output.trim_end(),
        "undefined|null|true|false|0|-0|1.5|1e+21|1e-7|NaN|Infinity|-Infinity|10n|-3n|\
         Symbol(\"s\")|\"tab\\there\"|\"back\\\\slash\"|\"\\u0001\\u007f\""
    );
}

/// The array shapes: dense, empty, a trailing hole, a leading hole, and a value past the
/// depth limit.
#[test]
fn arrays_show_their_shape() {
    let output = run(r#"
        print(std.__sprintObject([]));
        print(std.__sprintObject([1, 2, 3]));
        print(std.__sprintObject(new Array(3)));
        print(std.__sprintObject([1, 2, ,]));
        print(std.__sprintObject([1, , 3]));
        print(std.__sprintObject([[1, 2], [3]]));
        print(std.__sprintObject({ a: { b: [1, 2] } }));
        var v = [1, 2, 3]; v.x = 4; print(std.__sprintObject(v));
        "#);
    assert_eq!(
        output,
        "[  ]\n[ 1, 2, 3 ]\n[ <3 empty items> ]\n[ 1, 2, <1 empty item> ]\n\
         [ 0: 1, 2: 3 ]\n[ [ 1, 2 ], [ 3 ] ]\n{ a: { b: [Array] } }\n[ 1, 2, 3, x: 4 ]\n"
    );
}

/// Objects: bare keys for identifiers, quoted keys otherwise, integers bare, symbols where
/// they were defined, and accessors named rather than invoked.
#[test]
fn objects_show_keys_and_definition_order() {
    let output = run(r#"
        print(std.__sprintObject({}));
        print(std.__sprintObject({ a: 1, b: "x" }));
        print(std.__sprintObject({ "a b": 1 }));
        print(std.__sprintObject({ "1": 1 }));
        print(std.__sprintObject({ [Symbol("test")]: 1, y: 2 }));
        print(std.__sprintObject({ get x() { return 1 } }));
        var v = { x: 1, y: 1 };
        Object.defineProperty(v, "z", { value: 3, enumerable: false });
        print(std.__sprintObject(v));
        "#);
    assert_eq!(
        output,
        "{  }\n{ a: 1, b: \"x\" }\n{ \"a b\": 1 }\n{ 1: 1 }\n\
         { [Symbol(\"test\")]: 1, y: 2 }\n{ x: [Getter] }\n{ x: 1, y: 1 }\n"
    );
}

/// Cycles name the depth they were first seen at, so two cycles stay distinguishable.
#[test]
fn cycles_are_marked() {
    let output = run(r#"
        var a = {}, b = {};
        a.b = b; b.a = a;
        print(std.__sprintObject(a));
        var self = []; self.push(self);
        print(std.__sprintObject(self));
        "#);
    assert_eq!(output, "{ b: { a: [circular 0] } }\n[ [circular 0] ]\n");
}

/// The classes that print as themselves rather than as brace bodies.
#[test]
fn built_in_classes_print_as_themselves() {
    let output = run(r#"
        print(std.__sprintObject(new Date(1506098258091)));
        print(std.__sprintObject(new Date(NaN)));
        print(std.__sprintObject(/ab/g));
        print(std.__sprintObject(new RegExp("a/b", "gi")));
        print(std.__sprintObject(new Map([["x", 1], [2, 3]])));
        print(std.__sprintObject(new Set(["x", 2])));
        print(std.__sprintObject(new Int32Array([1, 2, 3])));
        print(std.__sprintObject(new BigInt64Array([1n])));
        print(std.__sprintObject(function f() {}));
        print(std.__sprintObject(function () {}));
        // A user class prints as a plain object: only quickjs's own classes carry a name.
        print(std.__sprintObject(new (class Point { constructor() { this.x = 1 } })()));
        "#);
    assert_eq!(
        output,
        "2017-09-22T16:37:38.091Z\nDate {  }\n/ab/g\n/a\\/b/gi\n\
         Map(2) { \"x\" => 1, 2 => 3 }\nSet(2) { \"x\", 2 }\n\
         Int32Array(3) [ 1, 2, 3 ]\nBigInt64Array(1) [ 1 ]\n\
         [Function f]\n[Function (anonymous)]\n{ x: 1 }\n"
    );
}

/// The limits are visible in the output, so they have to be the reference's numbers:
/// 1000 characters per string and 100 items per collection.
#[test]
fn the_limits_are_reported() {
    let output = run(r#"
        var s = "";
        for (var i = 0; i < 1005; i++) s += "a";
        print(std.__sprintObject(s).length, std.__sprintObject(s).slice(-24));
        var a = [];
        for (var i = 0; i < 105; i++) a.push(i);
        print(std.__sprintObject(a).slice(-20));
        "#);
    // 1000 quoted characters, then the note: `"` + 1000 + `"` + `... 5 more characters`.
    assert_eq!(
        output.trim_end(),
        "1023 aa\"... 5 more characters\n, ... 5 more items ]"
    );
}

/// `__printObject` is `__sprintObject` plus a newline on stdout.
#[test]
fn print_object_adds_a_newline() {
    let output = run(r#"
        std.__printObject({ a: 1 });
        std.__printObject([1]);
        "#);
    assert_eq!(output, "{ a: 1 }\n[ 1 ]\n");
}

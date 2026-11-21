//! The standard library, asserted against fixed expectations.
//!
//! Every expected string was taken from the reference implementation
//! (`qjs -e …`). The file is organised by the object the behaviour belongs to, and
//! states the rules that are easy to get subtly wrong rather than restating the
//! happy path.

fn eval(source: &str) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval(source) {
        Ok(_) => ctx.output().to_string(),
        Err(error) => panic!(
            "{source:?} failed: {error}\noutput so far:\n{}",
            ctx.output()
        ),
    }
}

fn value(source: &str) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval(source) {
        Ok(value) => value.to_js_string(),
        Err(error) => panic!("{source:?} failed: {error}"),
    }
}

fn throws(source: &str) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval(source) {
        Ok(_) => panic!("{source:?} should have thrown"),
        Err(error) => error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Object
// ---------------------------------------------------------------------------

#[test]
fn object_keys_order_integers_first() {
    // The specification's order: integer-like keys ascending, then the rest in
    // insertion order. `for-in` and `Object.keys` share it.
    assert_eq!(
        eval("print(Object.keys({b:1,2:1,1:1}))"),
        "[ \"1\", \"2\", \"b\" ]\n"
    );
    assert_eq!(eval("print(Object.keys({z:1,a:2}))"), "[ \"z\", \"a\" ]\n");
    assert_eq!(eval("print(Object.keys([7,8]))"), "[ \"0\", \"1\" ]\n");
}

#[test]
fn object_tostring_uses_the_tag_not_the_class_field() {
    assert_eq!(
        eval("print(Object.prototype.toString.call([]))"),
        "[object Array]\n"
    );
    assert_eq!(
        eval(
            "print(Object.prototype.toString.call(null), Object.prototype.toString.call(undefined))"
        ),
        "[object Null] [object Undefined]\n"
    );
    assert_eq!(eval("print(String({}))"), "[object Object]\n");
}

#[test]
fn object_is_differs_from_strict_equals_only_on_nan_and_zero() {
    assert_eq!(
        eval("print(Object.is(1,1), Object.is(NaN,NaN), Object.is(0,-0), 0 === -0, NaN === NaN)"),
        "true true false true false\n"
    );
}

#[test]
fn define_property_defaults_every_attribute_to_false() {
    // A descriptor that omits `writable`/`enumerable`/`configurable` gets `false` for
    // each, which is why `defineProperty` can create an invisible property.
    assert_eq!(
        eval("var o={}; Object.defineProperty(o,'x',{value:1}); print(o.x, Object.keys(o).length)"),
        "1 0\n"
    );
    assert_eq!(
        eval(
            "var o={}; Object.defineProperty(o,'x',{value:1,enumerable:true}); print(Object.keys(o))"
        ),
        "[ \"x\" ]\n"
    );
}

#[test]
fn freeze_and_seal_differ_and_are_observable() {
    assert_eq!(
        eval("var o={a:1}; Object.freeze(o); print(Object.isFrozen(o), Object.isSealed(o))"),
        "true true\n"
    );
    assert_eq!(
        eval("var o={a:1}; Object.seal(o); print(Object.isFrozen(o), Object.isSealed(o))"),
        "false true\n"
    );
    assert_eq!(
        eval("var o={a:1}; Object.preventExtensions(o); o.b=2; print(o.b)"),
        "undefined\n"
    );
}

#[test]
fn a_frozen_array_refuses_writes() {
    // The write is silent in sloppy mode and a `TypeError` in strict mode, which is
    // the one place an assignment differs by mode.
    assert_eq!(
        eval("var a=[1,2]; Object.freeze(a); a[0]=9; print(a[0])"),
        "1\n"
    );
    assert_eq!(
        eval("'use strict'; var a=[1,2]; Object.freeze(a); try{a[0]=9}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    // `sort` writes every element back, so it throws rather than returning quietly.
    assert_eq!(
        eval("try{Object.freeze([2,1]).sort()}catch(e){print(e.name)}"),
        "TypeError\n"
    );
}

// ---------------------------------------------------------------------------
// Array
// ---------------------------------------------------------------------------

#[test]
fn array_length_is_derived_and_holes_are_real() {
    assert_eq!(
        eval("var a=[1,2,3]; a.length=2; print(a, a.length)"),
        "[ 1, 2 ] 2\n"
    );
    // Growing leaves holes, which `forEach` skips and the printer reports.
    assert_eq!(eval("var a=[1]; a[3]=4; print(a)"), "[ 0: 1, 3: 4 ]\n");
    assert_eq!(
        eval("var n=0; [1,,3].forEach(function(){n++}); print(n)"),
        "2\n"
    );
    assert_eq!(eval("print([1,,3].length, [1,,3][1])"), "3 undefined\n");
}

#[test]
fn array_sort_defaults_to_string_order() {
    // The surprising part of the spec: without a comparator, elements compare as
    // strings, so `10` sorts before `9`.
    assert_eq!(eval("print([10,9,1].sort())"), "[ 1, 10, 9 ]\n");
    assert_eq!(
        eval("print([10,9,1].sort(function(a,b){return a-b}))"),
        "[ 1, 9, 10 ]\n"
    );
}

#[test]
fn array_iteration_methods_pass_the_receiver() {
    assert_eq!(
        eval("print([1,2,3].map(function(x){return x*2}))"),
        "[ 2, 4, 6 ]\n"
    );
    assert_eq!(
        eval("print([1,2,3].filter(function(x){return x>1}))"),
        "[ 2, 3 ]\n"
    );
    assert_eq!(
        eval("print([1,2,3].reduce(function(a,b){return a+b}))"),
        "6\n"
    );
    assert_eq!(
        eval("print([1,2,3].reduce(function(a,b){return a+b}, 10))"),
        "16\n"
    );
}

#[test]
fn an_array_method_works_on_any_array_like() {
    // The methods are generic in the spec, including a string receiver — whose
    // indices are read-only, which is why the mutating ones throw.
    assert_eq!(
        eval(r#"print(Array.prototype.join.call("abc"))"#),
        "a,b,c\n"
    );
    assert_eq!(
        eval(r#"print(Array.prototype.slice.call("abc",1))"#),
        "[ \"b\", \"c\" ]\n"
    );
    assert_eq!(
        eval(r#"try{Array.prototype.sort.call("aa")}catch(e){print(e.name)}"#),
        "TypeError\n"
    );
}

#[test]
fn array_iterators_are_lazy() {
    // A huge `length` must not be materialised: the loop only takes six steps.
    assert_eq!(
        eval(
            "var n=0; for (var k of Array.prototype.keys.call({length: 2**32+1})) { if (++n>5) break } print(n)"
        ),
        "6\n"
    );
    assert_eq!(eval("for (var x of [10,20].values()) print(x)"), "10\n20\n");
    assert_eq!(
        eval(r#"for (var e of ["a"].entries()) print(e)"#),
        "[ 0, \"a\" ]\n"
    );
}

// ---------------------------------------------------------------------------
// String
// ---------------------------------------------------------------------------

#[test]
fn string_index_family_treats_negatives_as_absent() {
    assert_eq!(
        eval(r#"print("a".charAt(-1) === "", "abc".charAt(1))"#),
        "true b\n"
    );
    assert_eq!(
        eval(r#"print("abc".charCodeAt(-1), "abc".charCodeAt(9))"#),
        "NaN NaN\n"
    );
    assert_eq!(eval(r#"print("abc".codePointAt(-1))"#), "undefined\n");
    // `indexOf` clamps a negative start to zero instead of counting from the end.
    assert_eq!(
        eval(r#"print("aaa".indexOf("a",-1), "aaa".lastIndexOf("a",-Infinity))"#),
        "0 0\n"
    );
}

#[test]
fn string_slicing_family_disagrees_on_negatives() {
    assert_eq!(eval(r#"print("abcdef".slice(-2))"#), "ef\n");
    assert_eq!(eval(r#"print("abcdef".substring(-2))"#), "abcdef\n");
    assert_eq!(eval(r#"print("abcdef".substr(-2))"#), "ef\n");
    // `substring` swaps its arguments; `slice` does not.
    assert_eq!(
        eval(r#"print("abcdef".substring(4,1), "abcdef".slice(4,1))"#),
        "bcd \n"
    );
}

#[test]
fn strings_count_utf16_code_units() {
    assert_eq!(eval(r#"print("😀".length)"#), "2\n");
    // A surrogate pair written as two escapes is the same string as the character.
    assert_eq!(eval(r#"print("\ud83d\ude00" === "😀")"#), "true\n");
    assert_eq!(
        eval(r#"print("\u{dbff}\u{dfff}" === "\u{10ffff}")"#),
        "true\n"
    );
    assert_eq!(
        eval(r#"print("😀".codePointAt(0), "😀".charCodeAt(0))"#),
        "128512 55357\n"
    );
}

#[test]
fn string_search_and_case() {
    assert_eq!(
        eval(r#"print("AbC".toLowerCase(), "abc".toUpperCase())"#),
        "abc ABC\n"
    );
    assert_eq!(eval(r#"print(" a ".trim() === "a")"#), "true\n");
    assert_eq!(
        eval(r#"print("ab".repeat(3), "5".padStart(3,"0"))"#),
        "ababab 005\n"
    );
    assert_eq!(eval(r#"print("a,b".split(","))"#), "[ \"a\", \"b\" ]\n");
    assert_eq!(eval(r#"print("abc".replace("b","X"))"#), "aXc\n");
}

// ---------------------------------------------------------------------------
// Number and Math
// ---------------------------------------------------------------------------

#[test]
fn number_formatting_rounds_halves_away_from_zero() {
    // Rust's formatter rounds halves to even, so these would be `2e+1` and `1.12`.
    assert_eq!(
        eval("print((25).toExponential(0), (-25).toExponential(0))"),
        "3e+1 -3e+1\n"
    );
    assert_eq!(
        eval("print((1.125).toFixed(2), (0.5).toFixed(0))"),
        "1.13 1\n"
    );
    assert_eq!(
        eval("print((25).toPrecision(1), (25).toPrecision(2))"),
        "3e+1 25\n"
    );
}

#[test]
fn number_to_string_in_other_radixes() {
    assert_eq!(
        eval("print((255).toString(16), (0.5).toString(2))"),
        "ff 0.1\n"
    );
    assert_eq!(eval("print((-255).toString(16))"), "-ff\n");
}

#[test]
fn math_round_and_sign_keep_negative_zero() {
    assert_eq!(eval("print(Math.round(-0.5), Math.round(0.5))"), "-0 1\n");
    assert_eq!(eval("print(1/Math.round(-0.4))"), "-Infinity\n");
    assert_eq!(eval("print(Math.min(0,-0), Math.max(0,-0))"), "-0 0\n");
    assert_eq!(eval("print(Math.min(1,NaN), Math.max(1,NaN))"), "NaN NaN\n");
    assert_eq!(
        eval("print(Math.floor(-0.5), Math.ceil(-0.5), Math.trunc(-0.5))"),
        "-1 -0 -0\n"
    );
}

#[test]
fn math_sum_precise_rounds_once() {
    // A compensated accumulator still rounds `1 + 2^-53` back to `1`; only an exact
    // sum rounded once produces the extra ulp.
    assert_eq!(
        eval("print(Math.sumPrecise([1, Number.EPSILON/2, Number.MIN_VALUE]))"),
        "1.0000000000000002\n"
    );
    assert_eq!(
        eval("print(Math.sumPrecise([]), Math.sumPrecise([0.1,0.2]))"),
        "-0 0.30000000000000004\n"
    );
}

#[test]
fn parse_int_stops_at_the_first_invalid_digit() {
    assert_eq!(
        eval("print(parseInt('12px'), parseInt('0x10'), parseInt('  42  '))"),
        "12 16 42\n"
    );
    assert_eq!(
        eval("print(parseInt('11',2), parseInt('z'), parseInt('10',1))"),
        "3 NaN NaN\n"
    );
    assert_eq!(
        eval("print(parseFloat('1.5x'), parseFloat('x'))"),
        "1.5 NaN\n"
    );
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

#[test]
fn json_stringify_matches_the_reference_text() {
    assert_eq!(
        eval("print(JSON.stringify({a:1}))"),
        r#"{"a":1}"#.to_string() + "\n"
    );
    assert_eq!(eval("print(JSON.stringify([1,2]))"), "[1,2]\n");
    // A hole or `undefined` in an array becomes `null`; in an object it is omitted.
    assert_eq!(
        eval("print(JSON.stringify([undefined,null]))"),
        "[null,null]\n"
    );
    assert_eq!(
        eval("print(JSON.stringify({a:undefined,b:1}))"),
        r#"{"b":1}"#.to_string() + "\n"
    );
    assert_eq!(eval("print(JSON.stringify(undefined))"), "undefined\n");
    // `NaN` and the infinities serialise as `null`.
    assert_eq!(
        eval("print(JSON.stringify([NaN,Infinity]))"),
        "[null,null]\n"
    );
}

#[test]
fn json_stringify_indents_with_the_given_string() {
    assert_eq!(
        eval("print(JSON.stringify({a:1,b:[2]}, null, 2))"),
        "{\n  \"a\": 1,\n  \"b\": [\n    2\n  ]\n}\n"
    );
}

#[test]
fn json_parse_round_trips_and_rejects_garbage() {
    assert_eq!(eval(r#"print(JSON.parse('{"a":[1,2]}').a[1])"#), "2\n");
    assert_eq!(eval(r#"print(JSON.parse('"x"'))"#), "x\n");
    assert!(throws(r#"JSON.parse('{')"#).contains("SyntaxError"));
}

// ---------------------------------------------------------------------------
// Function and Reflect
// ---------------------------------------------------------------------------

#[test]
fn bind_carries_this_and_leading_arguments() {
    assert_eq!(
        eval("function f(a,b){return [this,a,b]} var g=f.bind(1,2); print(g(3).join(','))"),
        "1,2,3\n"
    );
    assert_eq!(
        eval("function f(a,b){return a+b} print(f.bind(null,1)(2))"),
        "3\n"
    );
    assert_eq!(
        eval("function f(a,b){} print(f.bind(null,1).length)"),
        "1\n"
    );
    assert_eq!(eval("function f(){} print(f.bind(null).name)"), "bound f\n");
    assert_eq!(
        eval("print(typeof (function(){}).bind(null))"),
        "function\n"
    );
}

#[test]
fn apply_and_call_differ_on_their_argument_list() {
    // `Function.prototype.apply` accepts `undefined` as an empty list;
    // `Reflect.apply` requires one.
    assert_eq!(
        eval("print((function(){return 1}).apply(null, undefined))"),
        "1\n"
    );
    assert_eq!(
        eval("try{Reflect.apply(function(){return 1}, null, undefined)}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    assert_eq!(
        eval("print((function(a,b){return a+b}).call(null,1,2))"),
        "3\n"
    );
}

#[test]
fn sloppy_mode_boxes_a_primitive_receiver() {
    assert_eq!(
        eval("function f(){return typeof this} print(f.call(1))"),
        "object\n"
    );
    assert_eq!(
        eval("'use strict'; function f(){return typeof this} print(f.call(1))"),
        "number\n"
    );
}

// ---------------------------------------------------------------------------
// Builtins as a whole
// ---------------------------------------------------------------------------

#[test]
fn arguments_reports_what_was_passed() {
    assert_eq!(
        eval("function f(){return arguments.length} print(f(1,2,3), f())"),
        "3 0\n"
    );
    // A declared parameter count does not change what `arguments` holds.
    assert_eq!(
        eval("function f(a){return arguments.length} print(f(1,2))"),
        "2\n"
    );
    assert_eq!(
        eval("function f(){return arguments[1]} print(f(7,8))"),
        "8\n"
    );
}

#[test]
fn method_names_are_not_enumerable() {
    // Otherwise `for (k in [])` would list a hundred built-ins and `JSON.stringify`
    // would serialise them.
    assert_eq!(eval("var n=0; for (var k in []) n++; print(n)"), "0\n");
    assert_eq!(eval("var n=0; for (var k in {a:1}) n++; print(n)"), "1\n");
}

#[test]
fn a_script_can_use_the_whole_library_together() {
    let source = r#"
        var data = [{n: "a", v: 3}, {n: "b", v: 1}, {n: "c", v: 2}];
        var sorted = data.slice().sort(function (x, y) { return x.v - y.v });
        var names = sorted.map(function (o) { return o.n });
        print(names.join("-"));
        print(JSON.stringify(sorted.map(function (o) { return o.v })));
        print(Math.max.apply(null, sorted.map(function (o) { return o.v })));
    "#;
    assert_eq!(eval(source), "b-c-a\n[1,2,3]\n3\n");
}

#[test]
fn an_uncaught_error_names_its_class() {
    assert_eq!(
        eval("try{null.x}catch(e){print(e instanceof TypeError, e.name)}"),
        "true TypeError\n"
    );
    assert_eq!(
        eval("try{throw new RangeError('bad')}catch(e){print(e.name, e.message)}"),
        "RangeError bad\n"
    );
    assert_eq!(
        value("try{null.x}catch(e){e.toString()}"),
        "TypeError: cannot read property 'x' of null"
    );
}

// ---------------------------------------------------------------------------
// Language semantics that the builtins exposed
// ---------------------------------------------------------------------------

#[test]
fn block_scoped_bindings_do_not_leak_or_shadow() {
    // A `let` belongs to its block. Regressing this made `{ let x }` shadow the
    // outer `x` for the rest of the function, which is worse than not supporting
    // blocks at all: the program kept running with the wrong answer.
    assert_eq!(eval("var x=1; { let x=2; } print(x)"), "1\n");
    assert_eq!(eval("{ let y=2; } print(typeof y)"), "undefined\n");
    assert_eq!(eval("let z=1; { let z=2; } print(z)"), "1\n");
    assert_eq!(
        eval("for (let i=0;i<1;i++){} print(typeof i)"),
        "undefined\n"
    );
    assert_eq!(
        eval("{ let s=1; { let s=2; print(s) } print(s) }"),
        "2\n1\n"
    );
    // …and an inner block still sees the outer binding shadowed by its own.
    assert_eq!(eval("var x=1; { let x=2; print(x) } print(x)"), "2\n1\n");
    assert_eq!(
        eval("function f(){ { let a=1 } return typeof a } print(f())"),
        "undefined\n"
    );
}

#[test]
fn const_is_read_only_after_initialisation() {
    assert_eq!(
        eval("try{ const c=1; c=2 }catch(e){print(e.name, e.message)}"),
        "TypeError 'c' is read-only\n"
    );
    // A closure writing it is caught too, which needs the realm to remember the
    // declaration: the closure's own entry does not carry the `const` flag.
    assert_eq!(
        eval("const c=1; var f=function(){c=2}; try{f()}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    // The initialiser itself must go through, and so must mutating the value.
    assert_eq!(eval("const o={}; o.x=1; print(o.x)"), "1\n");
}

#[test]
fn var_globals_are_properties_of_the_global_object() {
    // The observable half of the split between `var` (on the global object) and
    // `let` (in the global lexical environment, invisible there).
    assert_eq!(eval("var v=1; print(globalThis.v)"), "1\n");
    assert_eq!(eval("var v=1; globalThis.v=3; print(v)"), "3\n");
    assert_eq!(
        eval("let n=2; print(globalThis.n, 'n' in globalThis)"),
        "undefined false\n"
    );
    // Writable and enumerable, but not configurable.
    assert_eq!(eval("var v=1; print(delete globalThis.v, v)"), "false 1\n");
    // Hoisted: it exists, holding `undefined`, before its initialiser runs.
    assert_eq!(eval("print(v); var v=1"), "undefined\n");
    assert_eq!(
        eval("function f(){} print(typeof globalThis.f)"),
        "function\n"
    );
    // An implicit global is the one that *is* configurable.
    assert_eq!(eval("x=1; print(x, delete globalThis.x)"), "1 true\n");
}

#[test]
fn delete_removes_properties_and_leaves_holes() {
    assert_eq!(
        eval("var o={a:1}; print(delete o.a, o.a)"),
        "true undefined\n"
    );
    // An array index becomes a hole: the length stays and `in` starts answering false.
    assert_eq!(
        eval("var a=[1,2,3]; delete a[1]; print(a.length, 1 in a, a[1])"),
        "3 false undefined\n"
    );
    // A non-configurable property refuses and says so.
    assert_eq!(
        eval("var o={}; Object.defineProperty(o,'x',{value:1}); print(delete o.x)"),
        "false\n"
    );
    // Deleting a binding always fails; nothing to do at run time.
    assert_eq!(
        eval("function f(x){return delete x} print(f(1))"),
        "false\n"
    );
}

#[test]
fn loose_equality_converts_objects_but_not_object_to_object() {
    assert_eq!(
        eval("print([]=='', [1]=='1', [1,2]=='1,2')"),
        "true true true\n"
    );
    assert_eq!(
        eval("var a={},b={}; print(a===b, a==b, a==a)"),
        "false false true\n"
    );
    assert_eq!(
        eval("print(null==undefined, null===undefined)"),
        "true false\n"
    );
    assert_eq!(eval("print([0]==false, '0'==false)"), "true true\n");
}

#[test]
fn unsigned_shift_produces_a_non_negative_number() {
    // `>>>` is the unsigned shift, so its result can exceed `i32::MAX` and must not
    // be squeezed back into a negative int.
    assert_eq!(
        eval("print(1<<31, (1<<31)>>>0)"),
        "-2147483648 2147483648\n"
    );
    assert_eq!(eval("print(-1>>>0, -1>>0)"), "4294967295 -1\n");
    assert_eq!(eval("print(7>>>1, 0xdeadbeef>>>0)"), "3 3735928559\n");
}

#[test]
fn spread_appends_whatever_the_iterable_yields() {
    assert_eq!(eval(r#"print([..."ab"])"#), "[ \"a\", \"b\" ]\n");
    assert_eq!(eval("print([0,...[1,2],3])"), "[ 0, 1, 2, 3 ]\n");
    assert_eq!(eval("print([...[]].length)"), "0\n");
    // A hole is yielded as `undefined`, so the spread fills it in for real.
    assert_eq!(eval("print([...[1,,3]])"), "[ 1, undefined, 3 ]\n");
}

#[test]
fn named_function_expressions_bind_their_own_name() {
    assert_eq!(
        eval("var f=function g(){return typeof g}; print(f(), typeof g)"),
        "function undefined\n"
    );
    // Recursion through the self-binding works even when the outer name is rebound.
    assert_eq!(
        eval("print((function f(n){return n<2?1:n*f(n-1)})(5))"),
        "120\n"
    );
    assert_eq!(
        eval("var f=function g(){return g===f}; print(f())"),
        "true\n"
    );
}

#[test]
fn array_constructor_validates_its_length_argument() {
    assert_eq!(
        eval("try{new Array(-1)}catch(e){print(e.name)}"),
        "RangeError\n"
    );
    assert_eq!(
        eval("try{new Array(1.5)}catch(e){print(e.name)}"),
        "RangeError\n"
    );
    assert_eq!(eval("print(Array(3).length, Array(3,4).length)"), "3 2\n");
    assert_eq!(eval("print(2 in new Array(3))"), "false\n");
}

#[test]
fn string_conversion_runs_to_primitive() {
    assert_eq!(
        eval(r#"print(String({toString:function(){return "T"}}))"#),
        "T\n"
    );
    assert_eq!(eval(r#"print(String(new Error("m")))"#), "Error: m\n");
}

#[test]
fn a_for_let_loop_binds_freshly_each_iteration() {
    // `let` in a `for` head creates a new binding per iteration, so a closure made in
    // the body captures that iteration's value. Sharing one binding — which is what
    // happens with `var` — gives the final value everywhere.
    assert_eq!(
        eval(
            "var a=[];for(let i=0;i<3;i++)a.push(function(){return i});print(a[0](),a[1](),a[2]())"
        ),
        "0 1 2\n"
    );
    assert_eq!(
        eval(
            "var a=[];for(var i=0;i<3;i++)a.push(function(){return i});print(a[0](),a[1](),a[2]())"
        ),
        "3 3 3\n"
    );
    // A block-scoped binding is frozen when its block ends, so a closure made inside
    // keeps the value it had then.
    assert_eq!(
        eval("var f; { let x=1; f=function(){return x} } print(f())"),
        "1\n"
    );
    // A loop variable that no closure captures needs no per-iteration cell at all.
    assert_eq!(eval("var n=0;for(let i=0;i<3;i++)n++;print(n)"), "3\n");
}

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
fn number_to_string_radix_is_the_shortest_that_reads_back() {
    // Taken from the reference. Each of these is one digit short or one digit
    // misrounded by any conversion that decides "close enough" in floating point:
    // the error of the multiply-and-subtract loop compounds before the remainder
    // drops under half an ulp.
    assert_eq!(eval("print((1.3).toString(7))"), "1.2046204620462046205\n");
    assert_eq!(eval("print((1.3).toString(35))"), "1.ahhhhhhhhhm\n");
    assert_eq!(
        eval("print((1 - 2**-53).toString(12))"),
        "0.bbbbbbbbbbbbbba\n"
    );
    // Rounding up to `radix^digits` carries into an extra digit, which has to move the
    // radix point rather than print a trailing zero: `1/3` in radix 3 is `0.1`.
    assert_eq!(eval("print((1/3).toString(3))"), "0.1\n");
    // The smallest subnormal is exactly `2^-1074`; its magnitude underflows to zero
    // when rebuilt as `mantissa * 2^exponent`, which used to send the digit count
    // search off the end of the integer range.
    assert_eq!(eval("print((5e-324).toString(16).length)"), "271\n");
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

// ---------------------------------------------------------------------------
// Date
// ---------------------------------------------------------------------------

#[test]
fn date_parses_both_grammars() {
    // ISO form, then the lenient one the reference also accepts.
    assert_eq!(value("Date.parse('1970-01-01T00:30:00Z')"), "1800000");
    assert_eq!(value("Date.parse('1970-01-01T00:30:00+00:30')"), "0");
    // An explicit zone wins over the local one, whatever this machine's zone is.
    assert_eq!(value("Date.parse('Jan 1 1970 00:30:00 UTC')"), "1800000");
    // Out-of-range fields are not repaired.
    assert_eq!(value("Date.parse('1970-13-01')"), "NaN");
    assert_eq!(value("Date.parse('1970-01-01T24:00:01Z')"), "NaN");
    // The fractional part is truncated to milliseconds, not rounded.
    assert_eq!(
        value("new Date('2020-01-01T01:01:01.9999Z').toISOString()"),
        "2020-01-01T01:01:01.999Z"
    );
}

#[test]
fn date_make_time_keeps_the_specified_operation_order() {
    // test262's fp-evaluation-order case: the products in `MakeTime` may not be
    // fused or reordered, or these two answers change.
    assert_eq!(
        value("Date.UTC(1970, 0, 1, 80063993375, 29, 1, -288230376151711740)"),
        "29312"
    );
    assert_eq!(
        value("Date.UTC(1970, 0, 213503982336, 0, 0, 0, -18446744073709552000)"),
        "34447360"
    );
}

#[test]
fn date_local_and_utc_halves_agree_at_the_epoch() {
    // Whatever this machine's zone is, breaking a time into local fields and putting
    // those fields back through the local-time constructor must return the same
    // instant. This is the round trip the two halves have to agree on.
    assert_eq!(
        eval(
            "var d = new Date(0)\n\
              var e = new Date(d.getFullYear(), d.getMonth(), d.getDate(),\n\
                               d.getHours(), d.getMinutes(), d.getSeconds(),\n\
                               d.getMilliseconds())\n\
              print(e.getTime() === d.getTime())"
        ),
        "true\n"
    );
    // The UTC half is zone-independent.
    assert_eq!(
        value("new Date(0).toISOString()"),
        "1970-01-01T00:00:00.000Z"
    );
}

#[test]
fn date_printing_and_serialisation() {
    assert_eq!(
        value("String(new Date(0).toISOString())"),
        "1970-01-01T00:00:00.000Z"
    );
    // `print` shows a Date as its ISO string, the way the reference's value printer
    // does, and `JSON.stringify` goes through `toJSON`.
    assert_eq!(eval("print(new Date(0))"), "1970-01-01T00:00:00.000Z\n");
    assert_eq!(
        value("JSON.stringify({d: new Date(0)})"),
        "{\"d\":\"1970-01-01T00:00:00.000Z\"}"
    );
    // An invalid Date prints as an object and serialises to `null`, because
    // `toJSON` answers `null` for a non-finite time value.
    assert_eq!(value("new Date(NaN).toJSON()"), "null");
    assert_eq!(value("new Date(NaN).toString()"), "Invalid Date");
    assert_eq!(
        throws("new Date(NaN).toISOString()"),
        "RangeError: Date value is NaN"
    );
}

#[test]
fn date_is_tagged_and_its_default_hint_is_json_safe() {
    assert_eq!(
        value("Object.prototype.toString.call(new Date(0))"),
        "[object Date]"
    );
    assert_eq!(
        value("Object.prototype.toString.call(new Error('x'))"),
        "[object Error]"
    );
    // The default hint converts a Date to a *string*, so `"" + d` is not the number.
    assert_eq!(
        value("'' + new Date(0).toISOString()"),
        "1970-01-01T00:00:00.000Z"
    );
    assert_eq!(value("typeof ('' + new Date(0))"), "string");
    // A Date method on a non-Date is a TypeError, not a silent coercion.
    assert_eq!(
        throws("Date.prototype.getTime.call({})"),
        "TypeError: not a Date object"
    );
}

// ---------------------------------------------------------------------------
// Spread calls and eval
// ---------------------------------------------------------------------------

#[test]
fn spread_arguments_reach_the_callee() {
    assert_eq!(
        eval("function g(a,b){print(a+b)} var v=[1,2]; g(...v)"),
        "3\n"
    );
    // Mixed forms build one argument array, in order.
    assert_eq!(
        eval("function g(){print([].slice.call(arguments).join())} g(0,...[1,2],3)"),
        "0,1,2,3\n"
    );
    // A method keeps its receiver, which is what `apply` has to carry through.
    assert_eq!(
        eval("var o={n:7,f:function(a){print(this.n+a)}}; o.f(...[1])"),
        "8\n"
    );
    // `new` with spread still constructs and still sets the prototype.
    assert_eq!(
        eval("function C(a,b){this.s=a+b} var c=new C(...[3,4]); print(c.s, c instanceof C)"),
        "7 true\n"
    );
}

#[test]
fn update_expressions_work_on_members_and_elements() {
    // `o.x++` has to read the receiver twice, so it compiles to a `get_field2` /
    // `post_inc` / `perm3` / `put_field` sequence; a prefix one swaps `perm3` for
    // `insert2`. Both forms must write back *and* leave the right result behind.
    assert_eq!(
        eval("var o={x:1}; print(o.x++, o.x, ++o.x, o.x--, o.x)"),
        "1 2 3 3 2\n"
    );
    // A computed element keeps the key alive as well, which is the `perm4` /
    // `insert3` shape.
    assert_eq!(
        eval("var a=[5],i=0; print(a[i]++, a[0], ++a[i], --a[i])"),
        "5 6 7 6\n"
    );
    // The receiver of a deep chain is evaluated once, and only the last link is read
    // with the form that keeps it.
    assert_eq!(
        eval("var o={a:{b:2}}; print(o.a.b++, o.a.b, ++o.a.b)"),
        "2 3 4\n"
    );
}

#[test]
fn eval_sees_the_calling_scope() {
    // `this` and `arguments` come from the callee, not from global scope. Strict
    // mode is needed for `this` to stay the primitive: a sloppy function boxes it.
    assert_eq!(
        eval("'use strict'; function f(b){return eval(b)} print(f.call(1,'this'))"),
        "1\n"
    );
    assert_eq!(
        eval("function f(b){return eval(b)} print(f('arguments.length', 1))"),
        "2\n"
    );
    // A local of the calling function is readable *and* writable.
    assert_eq!(
        eval("function f(){var x=1,y=2; eval('x = x + y'); return x} print(f())"),
        "3\n"
    );
    // The scope chain is followed outwards, not just the immediate caller.
    assert_eq!(
        eval(
            "function inner(){return eval('a')}\n\
              function outer(){var a=5; return inner()}\n\
              print(outer())"
        ),
        "5\n"
    );
    // A strict eval's `var` does not leak into the global object.
    assert_eq!(
        eval("'use strict'; eval('var leaked = 1'); print(typeof leaked)"),
        "undefined\n"
    );
    // An *assignment* to an undeclared name is still a ReferenceError in strict mode;
    // only a declaration creates a binding of the eval's own.
    assert_eq!(
        throws("'use strict'; eval('undeclared_thing = 1')"),
        "ReferenceError: undeclared_thing is not defined"
    );
    // A *sloppy* eval has no variable environment of its own, so its `var` lands in
    // the caller's — for a top-level eval, the global object. The split is on
    // strictness alone, not on being called from a function.
    assert_eq!(
        eval("eval('var sloppy = 1'); print(typeof sloppy)"),
        "number\n"
    );
    // A sloppy assignment to a non-writable global is a silent no-op; strict mode
    // reports the same refusal as a TypeError.
    assert_eq!(
        eval(
            "(1, eval)('var ro = 1');\n\
             Object.defineProperty(globalThis, 'ro', { writable: false });\n\
             ro = 2;\n\
             print(ro)"
        ),
        "1\n"
    );
    assert_eq!(
        throws(
            "'use strict'; (1, eval)('var ro = 1');\n\
             Object.defineProperty(globalThis, 'ro', { writable: false });\n\
             ro = 2;"
        ),
        "TypeError: 'ro' is read-only"
    );
}

// ---------------------------------------------------------------------------
// The wrapper constructors
// ---------------------------------------------------------------------------

#[test]
fn new_number_string_boolean_store_their_primitive() {
    // `new Number(1)` is a wrapper object, so every `ToPrimitive` step on it runs
    // the prototype's `valueOf`, which reads the primitive the constructor stored
    // on the receiver. Without that store the whole boxed family throws.
    assert_eq!(eval("print(new Number(1) == 1)"), "true\n");
    assert_eq!(eval("print(new String('ab') == 'ab')"), "true\n");
    assert_eq!(eval("print(new Boolean(true) == true)"), "true\n");
    // Called as a plain function there is no receiver to store into, and the
    // primitive comes back unwrapped.
    assert_eq!(
        eval("print(new Number(1) + 1, Number(5), String(new String('x')))"),
        "2 5 x\n"
    );
}

// ---------------------------------------------------------------------------
// Destructuring
// ---------------------------------------------------------------------------

/// Every expectation here was read off the reference implementation.
#[test]
fn array_patterns_drain_an_iterator() {
    assert_eq!(eval("var [a,b]=[1,2]; print(a,b)"), "1 2\n");
    // A hole skips a value rather than binding it.
    assert_eq!(eval("var [x,,y]=[1,2,3]; print(x,y)"), "1 3\n");
    // A rest element collects what is left, into a real array.
    assert_eq!(
        eval("var [x,,y,...z]=[1,2,3,4]; print(x,y,JSON.stringify(z))"),
        "1 3 [4]\n"
    );
    assert_eq!(
        eval("var [...all]=[1,2]; print(JSON.stringify(all))"),
        "[1,2]\n"
    );
    // Patterns nest, and any iterable works — a string is one.
    assert_eq!(eval("var [[p],[q]]=[[7],[8]]; print(p,q)"), "7 8\n");
    assert_eq!(eval("var [p,q]='ab'; print(p,q)"), "a b\n");
    // A default fills in only `undefined`, never `null`.
    assert_eq!(eval("var [a=1,b=2]=[5]; print(a,b)"), "5 2\n");
    assert_eq!(eval("var [a=1]=[null]; print(a)"), "null\n");
}

#[test]
fn object_patterns_box_their_source() {
    assert_eq!(eval("var {c,d:e=5}={c:3}; print(c,e)"), "3 5\n");
    // A default may itself be a template, and it is evaluated lazily.
    assert_eq!(
        eval("var a='Bar'; var {b=`${a+`a${a}`}baz`}={}; print(b)"),
        "BaraBarbaz\n"
    );
    // A rest element gets the properties the literal part did not take.
    assert_eq!(
        eval("var {a,...r}={a:1,b:2}; print(a,JSON.stringify(r))"),
        "1 {\"b\":2}\n"
    );
    // `ToObject` is what makes a null source a TypeError rather than a silent miss.
    assert_eq!(
        eval("try{var {} = null}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    assert_eq!(eval("var {} = {}; print('ok')"), "ok\n");
}

#[test]
fn destructuring_assignments_keep_the_right_hand_side() {
    // The assigned value is the expression's value, and a member target is legal here
    // even though it is not in a declaration.
    assert_eq!(eval("var o={}; [o.x,o.y]=[5,6]; print(o.x,o.y)"), "5 6\n");
    assert_eq!(
        eval("var a,b; var r=[a,b]=[1,2]; print(JSON.stringify(r),a,b)"),
        "[1,2] 1 2\n"
    );
    assert_eq!(eval("var o={}; var k='z'; [o[k]]=[7]; print(o.z)"), "7\n");
    assert_eq!(eval("var a,b; ({a,b}={a:3,b:4}); print(a,b)"), "3 4\n");
}

#[test]
fn pattern_parameters_bind_locals_and_count_towards_length() {
    assert_eq!(
        eval("function f({b},[c]){return b+c} print(f({b:1},[2]))"),
        "3\n"
    );
    assert_eq!(
        eval("function f(a=1,...r){return a+r.length} print(f(undefined,2,3))"),
        "3\n"
    );
    // `.length` is the arity before the first default or rest parameter, so a
    // destructuring parameter counts as one and a defaulted one ends the count.
    assert_eq!(eval("print((([a,b]) => {}).length)"), "1\n");
    assert_eq!(eval("print(((c,[a,b]=1,d)=>{}).length)"), "1\n");
    assert_eq!(
        eval("print(((a,b=1,c)=>{}).length, ((...r)=>{}).length)"),
        "1 0\n"
    );
}

#[test]
fn a_tagged_template_passes_the_call_site_object() {
    assert_eq!(eval("print(String.raw `a${1}b`)"), "a1b\n");
    // `raw` is the source text, so an escape is not expanded.
    assert_eq!(eval("print(String.raw `x\\ty`)"), "x\\ty\n");
    // The tag receives the cooked chunks and the substitutions, and a member tag keeps
    // its receiver as `this`.
    assert_eq!(
        eval("function t(s,...v){return s[0]+'|'+s.raw[0]+'|'+v.join()} print(t`x\\ty${2}z`)"),
        "x\ty|x\\ty|2\n"
    );
    assert_eq!(
        eval("var o={n:5,m(s){return this.n+s[0]}}; print(o.m`!`)"),
        "5!\n"
    );
}

// ---------------------------------------------------------------------------
// with
// ---------------------------------------------------------------------------

#[test]
fn with_puts_an_object_in_front_of_every_scope() {
    // The object shadows a global and a local alike: the Object Environment Record sits
    // between the block and the function's own bindings.
    assert_eq!(eval("var o={x:1},x=2; with(o){print(x)}"), "1\n");
    assert_eq!(
        eval("function f(){var x=2; with({x:1}) return x} print(f())"),
        "1\n"
    );
    // A store writes through to the object that supplied the name.
    assert_eq!(eval("var o={x:1},x=2; with(o){x=5} print(x,o.x)"), "2 5\n");
    // Nested blocks resolve innermost first.
    assert_eq!(
        eval("var o1={x:1},o2={y:2}; with(o1){with(o2){print(x,y)}}"),
        "1 2\n"
    );
    // A name the object does not have falls through to the ordinary lookup.
    assert_eq!(
        eval("var o={x:1}; with(o){print(typeof nope)}"),
        "undefined\n"
    );
    // A function created inside the block keeps seeing the object, because the boxed
    // object is a captured local.
    assert_eq!(
        eval("var f; with({z:1}){f=function(){return z}} print(f())"),
        "1\n"
    );
}

#[test]
fn eval_inside_with_sees_the_object() {
    assert_eq!(eval("var o={x:'o1'}; with(o){print(eval('x'))}"), "o1\n");
    // A write through an `eval` reaches the object too.
    assert_eq!(
        eval("var o={x:'o1'}; with(o){eval('x=7')} print(o.x)"),
        "7\n"
    );
    // The innermost object wins, and a name it lacks comes from the enclosing one.
    assert_eq!(
        eval("var o1={x:'a',y:'b'},o2={x:'c'}; with(o1){with(o2){print(eval('x'),eval('y'))}}"),
        "c b\n"
    );
}

// ---------------------------------------------------------------------------
// Arrows inherit `this`, `arguments` and `new.target`
// ---------------------------------------------------------------------------

#[test]
fn an_arrow_reads_the_enclosing_functions_bindings() {
    // An arrow has no `this` of its own, so the enclosing function materialises its
    // receiver into a local and the arrow captures it.
    assert_eq!(
        eval("'use strict'; function f(){return (()=>this)()} print(f.call('t'))"),
        "t\n"
    );
    assert_eq!(
        eval("var o={m(){return (()=>this)()}}; print(o.m()===o)"),
        "true\n"
    );
    // Two levels deep is still the outermost function's receiver.
    assert_eq!(
        eval("'use strict'; function f(){return (()=>()=>this)()()} print(f.call('x'))"),
        "x\n"
    );
    // The same holds for `arguments`, which is likewise the enclosing function's.
    assert_eq!(
        eval("function f(){return (()=>arguments.length)()} print(f(1,2))"),
        "2\n"
    );
    // `new.target` is lexical too, and `undefined` for an ordinary call.
    assert_eq!(
        eval("function f(){return new.target} print(f(), new f()===f)"),
        "undefined true\n"
    );
    assert_eq!(
        eval("function f(){return (()=>new.target)()} print(new f()===f)"),
        "true\n"
    );
}

#[test]
fn eval_inherits_the_callers_this_and_new_target() {
    // A direct eval runs in the caller's function context, so both are visible even
    // though the eval'd source is a bare script.
    assert_eq!(
        eval("'use strict'; function f(){return eval('this')} print(f.call('v'))"),
        "v\n"
    );
    assert_eq!(
        eval("function f(){return eval('new.target')} print(new f()===f)"),
        "true\n"
    );
    // Through an arrow the lookup skips the arrow's own frame, which has neither.
    assert_eq!(
        eval("'use strict'; function f(){return (()=>eval('this'))()} print(f.call('v'))"),
        "v\n"
    );
    assert_eq!(
        eval("'use strict'; function f(){return (()=>eval('new.target'))()} print(new f()===f)"),
        "true\n"
    );
}

// ---------------------------------------------------------------------------
// Classes
// ---------------------------------------------------------------------------

#[test]
fn class_members_land_on_the_right_object() {
    // Methods and accessors go on the prototype, `static` ones on the constructor —
    // and none of them is enumerable, which is the whole difference from an object
    // literal.
    assert_eq!(eval("class A{m(){return 1}} print(new A().m())"), "1\n");
    assert_eq!(eval("class A{static s(){return 2}} print(A.s())"), "2\n");
    assert_eq!(eval("class A{get g(){return 3}} print(new A().g)"), "3\n");
    assert_eq!(
        eval("class A{m(){}} print(Object.keys(A.prototype).length)"),
        "0\n"
    );
    // `static` is a legal method *name* as well as a modifier.
    assert_eq!(
        eval(
            "class P{get=()=>'123'; static(){return 42}} var p=new P(); print(p.get(),p.static())"
        ),
        "123 42\n"
    );
    // A class constructor may not be called without `new`.
    assert_eq!(
        eval("class A{} try{A()}catch(e){print(e.name)}"),
        "TypeError\n"
    );
}

#[test]
fn a_class_binds_its_own_name_inside_the_body() {
    assert_eq!(
        eval("var E1=class E{static F(){return E}}; print(E1===E1.F())"),
        "true\n"
    );
    assert_eq!(
        eval("class A{static m(){return A}} print(A.m()===A)"),
        "true\n"
    );
    // The static field initialiser sees the class too — and inside a *function*, where the
    // binding is a closure variable rather than a local.
    assert_eq!(
        eval("class S{static x=42;static y=S.x;static z=this.x} print(S.x,S.y,S.z)"),
        "42 42 42\n"
    );
    assert_eq!(
        eval("function f(){class S{static x=42;static y=S.x} return S.y} print(f())"),
        "42\n"
    );
}

#[test]
fn extends_inherits_both_chains() {
    assert_eq!(
        eval("class A{f(){return 1}} class B extends A{} print(new B().f())"),
        "1\n"
    );
    // The instance must be a `new.target` instance, so a derived constructor's `super()`
    // has to build an object of the *subclass* prototype.
    assert_eq!(
        eval("class A{constructor(){this.v=1}} class B extends A{} print(new B().v)"),
        "1\n"
    );
    assert_eq!(
        eval(
            "class A{constructor(){this.v=1}} class B extends A{} class C extends B{} print(new C().v)"
        ),
        "1\n"
    );
    // `new` on a bound function constructs its target, so the instance is a target one.
    assert_eq!(
        eval(
            "function C(a){this.x=a} var g=C.bind(null,1); var r=new g(); print(r.x, r instanceof C)"
        ),
        "1 true\n"
    );
}

#[test]
fn super_reads_writes_and_calls_through_the_home_object() {
    assert_eq!(
        eval("class A{f(){return 1}} class B extends A{h(){return super.f()}} print(new B().h())"),
        "1\n"
    );
    assert_eq!(
        eval(
            "class A{static F(){return -1}} class B extends A{static H(){return super['F']()}} print(B.H())"
        ),
        "-1\n"
    );
    // A getter found on the parent prototype runs with the *instance* as `this`.
    assert_eq!(
        eval(
            "class A{get g(){return this.v}} class B extends A{constructor(){super();this.v=9} h(){return super.g}} print(new B().h())"
        ),
        "9\n"
    );
    // `delete super.x` is refused rather than deleting the parent's property.
    assert_eq!(
        eval("var a={f(){delete super.a}}; try{a.f()}catch(e){print(e.name)}"),
        "ReferenceError\n"
    );
}

#[test]
fn a_short_circuited_optional_chain_is_still_a_receiver() {
    // `(a?.b)()` keeps `a` as `this`, and a short-circuited one calls `undefined` so the
    // call throws instead of silently yielding `undefined`.
    assert_eq!(
        eval("var a={b(){return this._b},_b:{c:42}}; print((a?.b)().c, (a?.['b'])().c)"),
        "42 42\n"
    );
    assert_eq!(
        eval("var z=null; try{(z?.b)()}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    // `delete` on a skipped chain answers `true`: there was nothing to delete.
    assert_eq!(
        eval("var a={b:{c:2}},z=null; print(delete z?.b.c, delete a?.b.c, JSON.stringify(a))"),
        "true true {\"b\":{}}\n"
    );
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

#[test]
fn a_generator_pauses_and_resumes_its_frame() {
    // The body does not run until the first `next()`, and the value passed to `next`
    // becomes the value of the `yield` expression.
    assert_eq!(
        eval(
            "function* g(){yield 1} var it=g(); print(JSON.stringify(it.next()),JSON.stringify(it.next()))"
        ),
        "{\"value\":1,\"done\":false} {\"done\":true}\n"
    );
    assert_eq!(
        eval(
            "function* g(){var x=yield 1; yield x*2} var it=g(); it.next(); print(JSON.stringify(it.next(5)))"
        ),
        "{\"value\":10,\"done\":false}\n"
    );
    // A `return` leaves the frame's locals and operand stack intact across the pause.
    assert_eq!(
        eval(
            "function* g(){var a=1; yield a; a=2; yield a} var it=g(); it.next(); print(JSON.stringify(it.next()))"
        ),
        "{\"value\":2,\"done\":false}\n"
    );
    // `return(v)` finishes the generator with that value, even before it started.
    assert_eq!(
        eval("function* g(){yield 1} var it=g(); print(JSON.stringify(it.return(9)))"),
        "{\"value\":9,\"done\":true}\n"
    );
    assert_eq!(
        eval("function* g(){} var it=g(); print(JSON.stringify(it.return(5)))"),
        "{\"value\":5,\"done\":true}\n"
    );
    // `throw(e)` is delivered at the `yield`, so the body's own `try` catches it.
    assert_eq!(
        eval(
            "function* g(){try{yield 1}catch(e){yield 'caught:'+e}} var it=g(); it.next(); print(JSON.stringify(it.throw('boom')))"
        ),
        "{\"value\":\"caught:boom\",\"done\":false}\n"
    );
}

#[test]
fn yield_star_forwards_the_inner_iterator() {
    assert_eq!(
        eval(
            "function* o(){yield* [2,3]} var it=o(); print(JSON.stringify(it.next()),JSON.stringify(it.next()),JSON.stringify(it.next()))"
        ),
        "{\"value\":2,\"done\":false} {\"value\":3,\"done\":false} {\"done\":true}\n"
    );
    // The delegate's return value becomes the value of the `yield*` expression.
    assert_eq!(
        eval(
            "function* i(){yield 1; return 'end'} function* o(){var r=yield* i(); yield r} var it=o(); print(JSON.stringify(it.next())); print(JSON.stringify(it.next())); print(JSON.stringify(it.next()))"
        ),
        "{\"value\":1,\"done\":false}\n{\"value\":\"end\",\"done\":false}\n{\"done\":true}\n"
    );
}

#[test]
fn a_generator_is_iterable_and_not_a_constructor() {
    // `for (… of …)` and the array spread both drive it through `next`.
    assert_eq!(
        eval(
            "function* g(){yield 1; yield 2} var t=[]; for (var x of g()) t.push(x); print(t.join())"
        ),
        "1,2\n"
    );
    assert_eq!(
        eval("function* g(){yield 1; yield 2} print([...g()].join())"),
        "1,2\n"
    );
    // A pattern drains it, and a `return` value is not a yielded element.
    assert_eq!(
        eval("function* g(){return 0} var [x]=g(); print(x)"),
        "undefined\n"
    );
    // A generator function has no `[[Construct]]`.
    assert_eq!(
        eval("function* G(){} try{new G()}catch(e){print(e.name, e.message)}"),
        "TypeError G is not a constructor\n"
    );
}

// ---------------------------------------------------------------------------
// Symbols
// ---------------------------------------------------------------------------

#[test]
fn a_symbol_is_a_unique_key_that_is_not_a_string() {
    // Identity, not description: two symbols with the same text are two different keys.
    assert_eq!(
        eval("print(Symbol()===Symbol(), Symbol.for('k')===Symbol.for('k'))"),
        "false true\n"
    );
    // A symbol key and a string key coexist on one object and are invisible to each
    // other. `Object.keys` and `JSON.stringify` skip the symbol; only
    // `getOwnPropertySymbols` and `Reflect.ownKeys` report it.
    assert_eq!(
        eval(
            "var o={}; var s=Symbol('a'); o[s]=1; o.a=2; print(o[s], o.a, Object.keys(o).length, Object.getOwnPropertySymbols(o).length, JSON.stringify(o), s in o)"
        ),
        "1 2 1 1 {\"a\":2} true\n"
    );
    assert_eq!(
        eval("print(Reflect.ownKeys({a:1,[Symbol('s')]:2}).map(String).join())"),
        "a,Symbol(s)\n"
    );
}

#[test]
fn symbol_converts_to_a_string_only_on_request() {
    assert_eq!(
        eval(
            "print(typeof Symbol(), String(Symbol('x')), Symbol().description, Object.prototype.toString.call(Symbol()))"
        ),
        "symbol Symbol(x) undefined [object Symbol]\n"
    );
    // `ToString` of a symbol is a TypeError, in concatenation and in a template alike.
    assert_eq!(
        eval("try{''+Symbol()}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    assert_eq!(
        eval("try{print(`${Symbol()}`)}catch(e){print(e.name)}"),
        "TypeError\n"
    );
    // `new Symbol()` has no `[[Construct]]`.
    assert_eq!(
        eval("try{new Symbol()}catch(e){print(e.name, e.message)}"),
        "TypeError Symbol is not a constructor\n"
    );
}

#[test]
fn well_known_symbols_drive_the_protocols() {
    // `Symbol.toStringTag` overrides the class name in `Object.prototype.toString`.
    assert_eq!(
        eval("var o={[Symbol.toStringTag]:'M'}; print(Object.prototype.toString.call(o))"),
        "[object M]\n"
    );
    // `Symbol.toPrimitive` takes over conversion, and is handed the hint.
    assert_eq!(
        eval("var o={[Symbol.toPrimitive](h){return h}}; print(`${o}`)"),
        "string\n"
    );
    // `Symbol.iterator` makes any object iterable — no special case in the engine.
    assert_eq!(
        eval(
            "var o={[Symbol.iterator](){var i=0;return{next:()=>({value:i++,done:i>2})}}}; print([...o].join())"
        ),
        "0,1\n"
    );
    // The built-in types advertise iteration through the same symbol, so a script can
    // drive them by hand.
    assert_eq!(
        eval(
            "var it=[1,2][Symbol.iterator](); print(JSON.stringify(it.next()), JSON.stringify(it.next()), JSON.stringify(it.next()))"
        ),
        "{\"value\":1,\"done\":false} {\"value\":2,\"done\":false} {\"done\":true}\n"
    );
    assert_eq!(
        eval("print([...'ab'].join(), typeof Symbol.iterator)"),
        "a,b symbol\n"
    );
}

#[test]
fn eval_hands_its_scope_to_the_closures_it_builds() {
    // A closure created by `eval` outlives the eval, so it has to keep reading the
    // caller's binding — and each iteration of `for (let i …)` is a *different*
    // binding, because the loop variable is copied per iteration.
    assert_eq!(
        eval(
            "var tab=[]; for(let i=0;i<3;i++) eval('tab.push(function(){return i})');\n\
             print(tab[0](), tab[1](), tab[2]())"
        ),
        "0 1 2\n"
    );
    // The eval may sit in a nested function, in which case the binding it captures
    // belongs to that function's *caller*.
    assert_eq!(
        eval(
            "var tab=[];\n\
             for(let i=0;i<3;i++){ let f=function(){ eval('tab.push(function(){return i})') }; f() }\n\
             print(tab[0](), tab[1](), tab[2]())"
        ),
        "0 1 2\n"
    );
    // Two loops in a row use the same name, so name lookup alone cannot tell them
    // apart: the compiler records which slot each call site means.
    assert_eq!(
        eval(
            "var a=[], b=[];\n\
             for(let i=0;i<3;i++) eval('a.push(function(){return i})');\n\
             for(let i=0;i<3;i++) eval('b.push(function(){return i})');\n\
             print(a.map(f=>f()).join(), b.map(f=>f()).join())"
        ),
        "0,1,2 0,1,2\n"
    );
}

#[test]
fn eval_respects_a_read_only_binding() {
    // A local `const` is as read-only through `eval` as it is directly.
    assert_eq!(
        eval(
            "function f(){const a=1; try{eval('a = 2'); print('no throw')}catch(e){print(e.name)}} f()"
        ),
        "TypeError\n"
    );
    // The name of a named function expression is read-only too, but only strict mode
    // reports the attempt — a sloppy store is silently dropped, the same as a direct one.
    assert_eq!(
        eval("var f=function n(){eval('n = 1'); return n}; print(f()===f)"),
        "true\n"
    );
    assert_eq!(
        eval(
            "var f=function n(){'use strict'; eval('n = 1')}; try{f(); print('no throw')}catch(e){print(e.name)}"
        ),
        "TypeError\n"
    );
    // …and the same holds when the assignment goes through a nested arrow, which
    // reaches the binding via a closure entry rather than a local slot.
    assert_eq!(
        eval("var f=function n(){ (()=>{n=1})(); return n }; print(f()===f)"),
        "true\n"
    );
    assert_eq!(
        eval(
            "var f=function n(){ 'use strict'; (()=>{n=1})() }; try{f(); print('no throw')}catch(e){print(e.name)}"
        ),
        "TypeError\n"
    );
}

#[test]
fn parameter_expressions_get_their_own_variable_environment() {
    // A default expression makes the parameter list a scope of its own: the body's `var a`
    // is a different binding, so the closure still sees the parameter.
    assert_eq!(
        eval(
            "var f = function f(a = 1, p = () => a) { var a = 2; return [a, p()] };\n\
             print(f().join())"
        ),
        "2,1\n"
    );
    // A destructured parameter behaves the same way.
    assert_eq!(
        eval(
            "var g = ({x} = {x: 7}, p = () => x) => { var x = 9; return [x, p()] }; print(g().join())"
        ),
        "9,7\n"
    );
    // …and so does a body that leaves the parameter alone.
    assert_eq!(
        eval("var g = ({x} = {x: 7}, p = () => x) => [x, p()]; print(g().join())"),
        "7,7\n"
    );
    // `arguments` exists twice as well, and the parameter list's copy is the one its
    // closures see: the arrow returns `f`'s arguments, not its own call's.
    assert_eq!(
        eval("var f = function f(a, b = () => arguments) { return b()[0] }; print(f(9))"),
        "9\n"
    );
    // A parameter list does not see the body's `var`s, so this `c` is the global one.
    assert_eq!(
        eval(
            "var c = 'global';\n\
             var f = (a = 1, p = () => c) => { var c = 2; return [c, p()] };\n\
             print(f().join())"
        ),
        "2,global\n"
    );
}

#[test]
fn eval_in_a_parameter_list_belongs_to_the_caller() {
    // A `var` a sloppy eval declares goes into the caller's variable environment, not onto
    // the global object — and a direct eval inside a parameter list puts it in the
    // parameter environment, which its closures and the body both see.
    assert_eq!(
        eval(
            "var c = 'global';\n\
             var f = function f(a = eval('var c = 1')) { return c };\n\
             print(f(), c)"
        ),
        "1 global\n"
    );
    // A closure born in the parameter list sees it…
    assert_eq!(
        eval(
            "var c = 'global';\n\
             var f = (a = eval('var c = 1'), p = () => c) => { var c = 2; return [c, p()] };\n\
             print(f().join(), c)"
        ),
        "2,1 global\n"
    );
    // …including when the variable shadows an enclosing function's own binding.
    assert_eq!(
        eval(
            "function t() {\n\
                 var c = 'outer';\n\
                 function f(a = eval('var c = 1'), p = () => c) { return [p(), c] }\n\
                 return [f().join(), c]\n\
             }\n\
             print(t().join(' '))"
        ),
        "1,1 outer\n"
    );
    // An *indirect* eval has no caller scope at all, so its `var` is a global.
    assert_eq!(
        eval(
            "function t() { (1, eval)('var indirect = 1') }\n\
             t(); print(typeof indirect)"
        ),
        "number\n"
    );
}

#[test]
fn eval_inherits_the_callers_super() {
    // An eval'd body is not a method, but it runs inside one: `super.x` resolves against
    // the caller's `[[HomeObject]]`, which is what `special_object 4` already walks to.
    assert_eq!(
        eval(
            "var o1 = { x: 1 };\n\
             var o2 = { f() { return eval('super.x') } };\n\
             o2.__proto__ = o1;\n\
             print(o2.f())"
        ),
        "1\n"
    );
    // The receiver is still `this`, so a `super` method call gets the right one.
    assert_eq!(
        eval(
            "var o1 = { f() { return this } };\n\
             var o2 = { f() { return (() => eval('super.f()'))() } };\n\
             o2.__proto__ = o1;\n\
             print(o2.f() === o2)"
        ),
        "true\n"
    );
    // Outside a method there is no home object, so `super.x` stays a syntax error.
    assert_eq!(
        throws("eval('super.x')"),
        "SyntaxError: invalid source: 1:1: invalid use of 'super' outside of a method"
    );
}

#[test]
fn object_prototype_has_a_proto_accessor() {
    // `__proto__` is an ordinary accessor on `Object.prototype`, not a magic property of
    // every object — scripts read and write it constantly.
    assert_eq!(
        eval(
            "var o = {}; o.__proto__ = { x: 1 }; print(o.x, Object.getPrototypeOf(o) === o.__proto__)"
        ),
        "1 true\n"
    );
    assert_eq!(
        eval("var o = Object.create(null); print(o.__proto__ === undefined)"),
        "true\n"
    );
    // A primitive receiver is boxed, so it reports its wrapper's prototype.
    assert_eq!(eval("print((1).__proto__ === Number.prototype)"), "true\n");
    // A cycle is refused rather than creating an infinite chain.
    assert_eq!(
        eval(
            "var a = {}, b = {}; a.__proto__ = b; b.__proto__ = a; print(Object.getPrototypeOf(b) === a)"
        ),
        "false\n"
    );
}

#[test]
fn regexp_literals_and_exec_match_the_reference() {
    // A literal is compiled by the `regexp` opcode, not by a call to the constructor.
    assert_eq!(eval("print(/ab+c/i.source, /ab+c/i.flags)"), "ab+c i\n");
    // `exec` reports the whole match, the groups, and where it found them.
    assert_eq!(
        eval(
            "var m = /(\\d+)-(\\d+)/.exec('x 12-34 y');\n\
             print([m[0], m[1], m[2], m.index, m.input].join(' '))"
        ),
        "12-34 12 34 2 x 12-34 y\n"
    );
    // A group that did not participate is `undefined`, not an empty string.
    assert_eq!(
        eval("print(/a(b)?c/.exec('ac')[1] === undefined)"),
        "true\n"
    );
    // `lastIndex` advances for `g` and resets when the match fails.
    assert_eq!(
        eval(
            "var r = /o/g;\n\
             print([r.test('foo'), r.lastIndex, r.test('foo'), r.lastIndex, r.test('foo'), r.lastIndex].join() )"
        ),
        "true,2,true,3,false,0\n"
    );
    // A quantified group takes its captures from the *last* iteration.
    assert_eq!(
        eval("print(/(z)((a+)?(b+)?(c))*/.exec('zaacbbbcac').join('|'))"),
        "zaacbbbcac|z|ac|a||c\n"
    );
}

#[test]
fn string_methods_dispatch_to_the_regexp_symbol_methods() {
    // `replace`, `split`, `search` and `match` reach the pattern through
    // `Symbol.replace` and friends, so a `RegExp` decides what they mean.
    assert_eq!(eval("print('a1b2c3'.replace(/\\d/g, '#'))"), "a#b#c#\n");
    assert_eq!(
        eval("print('a1b2c3'.replace(/(\\d)/g, '[$1]'))"),
        "a[1]b[2]c[3]\n"
    );
    assert_eq!(
        eval("print('a1b2'.replace(/\\d/g, function (m) { return '<' + m + '>' }))"),
        "a<1>b<2>\n"
    );
    assert_eq!(eval("print('a,b;c'.split(/[,;]/).join('|'))"), "a|b|c\n");
    assert_eq!(eval("print('abcabc'.search(/b/))"), "1\n");
    assert_eq!(eval("print('xabcx'.match(/b/).index)"), "2\n");
    assert_eq!(eval("print('aXbXc'.match(/X/g).join('-'))"), "X-X\n");
    // A dollar in the replacement template refers to a group, and `$&` to the match.
    assert_eq!(eval("print('ab'.replace(/b/, '[$&]'))"), "a[b]\n");
    assert_eq!(eval("print('ab'.replace(/b/, '$1'))"), "a$1\n");
}

#[test]
fn regexp_flags_change_meaning() {
    assert_eq!(eval("print(/^b/m.test('a\\nb'))"), "true\n");
    assert_eq!(eval("print(/a.c/s.test('a\\nc'))"), "true\n");
    assert_eq!(eval("print(/abc/i.test('AbC'))"), "true\n");
    // `y` refuses to search forward from `lastIndex`.
    assert_eq!(
        eval("var r = /b/y; r.lastIndex = 0; print(r.test('abc'))"),
        "false\n"
    );
    // An invalid pattern or flag is a SyntaxError, as it is in the reference.
    assert_eq!(
        throws("new RegExp('a', 'bad')"),
        "SyntaxError: invalid regular expression flags"
    );
    assert_eq!(throws("new RegExp('(')"), "SyntaxError: expecting ')'");
}

#[test]
fn using_disposes_in_reverse_order() {
    // The resources go out in reverse, and the body's own work happens first.
    assert_eq!(
        eval(
            "var log = [];\n\
             function f() {\n\
                 using a = { [Symbol.dispose]() { log.push('a') } };\n\
                 using b = { [Symbol.dispose]() { log.push('b') } };\n\
                 log.push('body');\n\
             }\n\
             f(); print(log.join())"
        ),
        "body,b,a\n"
    );
    // A `return` still disposes, because the region is a cleanup on every exit.
    assert_eq!(
        eval(
            "var log = [];\n\
             function f() { using a = { [Symbol.dispose]() { log.push('a') } }; return 7 }\n\
             print(f(), log.join())"
        ),
        "7 a\n"
    );
    // So does a `break` out of a loop inside the block.
    assert_eq!(
        eval(
            "var log = [];\n\
             function f() {\n\
                 using a = { [Symbol.dispose]() { log.push('a') } };\n\
                 for (;;) { log.push('loop'); break }\n\
                 log.push('after');\n\
             }\n\
             f(); print(log.join())"
        ),
        "loop,after,a\n"
    );
    // `null` and `undefined` are legal and dispose to nothing.
    assert_eq!(
        eval(
            "function f() { using a = null; using b = undefined; return 1 }\n\
             print(f())"
        ),
        "1\n"
    );
}

#[test]
fn using_validates_at_the_declaration() {
    // A value that is not an object, or has no `Symbol.dispose`, fails before the body runs.
    assert_eq!(
        eval(
            "var ran = false;\n\
             try { (function () { using x = {}; ran = true })() } catch (e) {}\n\
             print(ran)"
        ),
        "false\n"
    );
    assert_eq!(
        eval(
            "var name;\n\
             try { (function () { using x = 42 })() } catch (e) { name = e.constructor.name }\n\
             print(name)"
        ),
        "TypeError\n"
    );
    // A non-callable disposer is a `TypeError` too.
    assert_eq!(
        throws("(function () { using x = { [Symbol.dispose]: 1 } })()"),
        "TypeError: dispose method is not a function"
    );
    // The method is snapshotted once, at the declaration: replacing it afterwards does not
    // change what runs.
    assert_eq!(
        eval(
            "var reads = 0, disposed = 0, target = {};\n\
             Object.defineProperty(target, Symbol.dispose, {\n\
                 get() { reads++; return function () { disposed++ } }\n\
             });\n\
             (function () { using x = target })();\n\
             print(reads, disposed)"
        ),
        "1 1\n"
    );
}

#[test]
fn failed_disposals_chain_into_suppressed_errors() {
    // Each failure is wrapped around the one before it, newest outermost:
    // LIFO disposes c, then b, then a, so `a` ends up on the outside.
    assert_eq!(
        eval(
            "var caught;\n\
             try {\n\
                 (function () {\n\
                     using a = { [Symbol.dispose]() { throw new Error('a') } };\n\
                     using b = { [Symbol.dispose]() { throw new Error('b') } };\n\
                     using c = { [Symbol.dispose]() { throw new Error('c') } };\n\
                 })();\n\
             } catch (e) { caught = e }\n\
             print(caught instanceof SuppressedError, caught.error.message,\n\
                   caught.suppressed.error.message,\n\
                   caught.suppressed.suppressed.message)"
        ),
        "true a b c\n"
    );
    // A body that throws still disposes, and the disposal failure wins over the body's.
    assert_eq!(
        eval(
            "var caught;\n\
             try {\n\
                 (function () {\n\
                     using a = { [Symbol.dispose]() { throw new Error('dispose') } };\n\
                     throw new Error('body');\n\
                 })();\n\
             } catch (e) { caught = e }\n\
             print(caught instanceof SuppressedError, caught.error.message,\n\
                   caught.suppressed.message)"
        ),
        "true dispose body\n"
    );
    // Rewriting `SuppressedError.prototype.constructor` must not hijack the engine's own
    // wrapping: the aggregate is built from the prototype, not by calling the constructor.
    assert_eq!(
        eval(
            "var saved = SuppressedError.prototype.constructor, hijacked = false;\n\
             SuppressedError.prototype.constructor = function () { hijacked = true };\n\
             try {\n\
                 (function () {\n\
                     using a = { [Symbol.dispose]() { throw new Error('a') } };\n\
                     using b = { [Symbol.dispose]() { throw new Error('b') } };\n\
                 })();\n\
             } catch (e) {}\n\
             SuppressedError.prototype.constructor = saved;\n\
             print(hijacked)"
        ),
        "false\n"
    );
}

#[test]
fn for_of_using_disposes_each_iteration() {
    // The element is validated per iteration, so a bad one fails on its own turn.
    assert_eq!(
        eval(
            "var iters = 0, name;\n\
             try {\n\
                 (function () {\n\
                     for (using x of [{ [Symbol.dispose]() {} }, 42]) { iters++ }\n\
                 })();\n\
             } catch (e) { name = e.constructor.name }\n\
             print(iters, name)"
        ),
        "1 TypeError\n"
    );
    // …and a good run disposes after each iteration body, not at the end of the loop.
    assert_eq!(
        eval(
            "var log = [];\n\
             (function () {\n\
                 for (using x of [\n\
                     { tag: 'a', [Symbol.dispose]() { log.push(this.tag) } },\n\
                     { tag: 'b', [Symbol.dispose]() { log.push(this.tag) } },\n\
                 ]) { log.push('iter') }\n\
             })();\n\
             print(log.join())"
        ),
        "iter,a,iter,b\n"
    );
}

#[test]
fn disposable_stack_moves_through_the_intrinsic_prototype() {
    assert_eq!(
        eval(
            "var saved = DisposableStack, disposed = 0;\n\
             var ds = new saved();\n\
             ds.use({ [Symbol.dispose]() { disposed++ } });\n\
             DisposableStack = class Other {};\n\
             var moved = ds.move();\n\
             var same = Object.getPrototypeOf(moved) === saved.prototype;\n\
             moved.dispose();\n\
             DisposableStack = saved;\n\
             print(same, disposed, moved.disposed)"
        ),
        "true 1 true\n"
    );
    // `defer` takes a plain callback, `adopt` gives a resource a disposer.
    assert_eq!(
        eval(
            "var log = [];\n\
             var stack = new DisposableStack();\n\
             stack.defer(function () { log.push('deferred') });\n\
             stack.adopt({ n: 3 }, function (value) { log.push('adopted' + value.n) });\n\
             stack.dispose();\n\
             print(log.join())"
        ),
        "adopted3,deferred\n"
    );
    // A stack can itself be a `using` resource.
    assert_eq!(
        eval(
            "var log = [];\n\
             (function () {\n\
                 using stack = new DisposableStack();\n\
                 stack.use({ [Symbol.dispose]() { log.push('inner') } });\n\
                 log.push('body');\n\
             })();\n\
             print(log.join())"
        ),
        "body,inner\n"
    );
}

#[test]
fn boxed_primitives_report_their_own_tag() {
    // `Object(sym)` is a real wrapper: `valueOf` hands the symbol back and the object tags
    // as a Symbol rather than an Object.
    assert_eq!(
        eval(
            "var s = Symbol('aaa'); var b = Object(s);\n\
              print(b.valueOf() === s, b.toString(), typeof b, Object.prototype.toString.call(b))"
        ),
        "true Symbol(aaa) object [object Symbol]\n"
    );
    assert_eq!(
        eval(
            "print(Object.prototype.toString.call('x'),\n\
                   Object.prototype.toString.call(1),\n\
                   Object.prototype.toString.call(true),\n\
                   Object.prototype.toString.call({}))"
        ),
        "[object String] [object Number] [object Boolean] [object Object]\n"
    );
}

#[test]
fn generator_resumes_after_calling_a_function() {
    // A generator body that calls an ordinary function must survive the call: the callee's
    // `return` ends *its* frame, not the generator's. Getting this wrong is silent — the
    // generator reports the callee's value as its own result and marks itself done.
    assert_eq!(
        eval(
            "function *f() { var r = yield 1; return 3 }\n\
             function *g() { var r = yield 1; (function () {}()); return 3 }\n\
             function *h() { var r = yield 1; return r }\n\
             var a = f(); a.next(); var ra = a.next(9);\n\
             var b = g(); b.next(); var rb = b.next(9);\n\
             var c = h(); c.next(); var rc = c.next(9);\n\
             print(ra.value, ra.done, rb.value, rb.done, rc.value, rc.done)"
        ),
        "3 true 3 true 9 true\n"
    );
    // A throw from a called function still reaches the generator's own `catch`.
    assert_eq!(
        eval(
            "function boom() { throw new Error('boom') }\n\
             function *a() { try { yield 1; boom() } catch (e) { yield 'caught:' + e.message } }\n\
             var g = a(); g.next(); var v = g.next();\n\
             print(v.value, v.done)"
        ),
        "caught:boom false\n"
    );
}

#[test]
fn regexp_unicode_mode_and_case_folding() {
    // `u` tightens the grammar: `{`, `}` and `]` must be escaped, unlike Annex B, where
    // they are ordinary literals.
    for pattern in ["aaa]", "a}", "a{"] {
        assert!(
            throws(&format!("new RegExp({pattern:?}, 'u')")).starts_with("SyntaxError"),
            "{pattern} should be a SyntaxError under u"
        );
        assert_eq!(
            eval(&format!(
                "try {{ new RegExp({pattern:?}); print('ok') }} catch (e) {{ print(e.name) }}"
            )),
            "ok
",
            "{pattern} is a literal without u"
        );
    }
    // `\b` is backspace inside a class and a word boundary outside it.
    assert_eq!(
        eval("print(/[\\b]/.test('\\b'), /\\b/.test('a'))"),
        "true true\n"
    );
    // Property escapes, and the case folding that applies to them.
    assert_eq!(
        eval("print('aAbBcC#4'.replace(/\\p{Lower}/gu, 'X'))"),
        "XAXBXC#4\n"
    );
    assert_eq!(
        eval("print('aAbBcC#4'.replace(/\\p{Upper}/gui, 'X'))"),
        "XXXXXX#4\n"
    );
    // Negation applies to the *set*, then the character is canonicalised — so `\P{Lower}`
    // matches `a` under `i`, because `a` canonicalises to the `A` that is in the complement.
    assert_eq!(
        eval("print('aAbBcC#4'.replace(/\\P{Lower}/gui, 'X'))"),
        "XXXXXXXX\n"
    );
    // A case-insensitive range has to fold both sides, not only the input.
    assert_eq!(eval("print(/[A-Z]/i.test('a'))"), "true\n");
    // …without breaking a range whose bounds do not survive folding.
    assert_eq!(
        eval("print(/[A-_]/.test('a'), /[A-_]/.test('^'))"),
        "false true\n"
    );
}

#[test]
fn typed_arrays_are_views_onto_a_buffer() {
    // The whole point of the type: two views of one buffer see each other's writes.
    assert_eq!(
        eval(
            "var b = new ArrayBuffer(8); var u = new Uint8Array(b); var i = new Int32Array(b);\n\
              i[0] = -1; print(u.join(','))"
        ),
        "255,255,255,255,0,0,0,0\n"
    );
    // `subarray` shares storage; `slice` copies.
    assert_eq!(
        eval(
            "var a = new Uint8Array([1, 2, 3, 4]);\n\
              var s = a.subarray(1, 3); s[0] = 9;\n\
              var c = a.slice(1, 3); c[0] = 7;\n\
              print(a.join(','), s.join(','), c.join(','))"
        ),
        "1,9,3,4 9,3 7,3\n"
    );
    // `.set` reads its source before writing, so `a.set(a)` is a no-op rather than a smear.
    assert_eq!(
        eval(
            "var a = new Uint8Array([1, 2, 3, 4]); a.set([10, 11], 2); a.set(a.subarray(0, 2), 2);\n\
              print(a.join(','))"
        ),
        "1,2,1,2\n"
    );
}

#[test]
fn typed_array_stores_wrap_and_clamp() {
    // An integer element type wraps at its own width; `Int8Array` of `255` is `-1`, and
    // `Int32Array` of `2^32 - 1` is `-1` as well.
    assert_eq!(
        eval(
            "var a = new Int8Array(1); a[0] = 255; var b = new Int32Array(1);\n\
              b[0] = Math.pow(2, 32) - 1; print(a[0], b[0])"
        ),
        "-1 -1\n"
    );
    // `Uint8ClampedArray` clamps instead, and rounds half to *even*: `0.5` is `0`, `2.5` is
    // `2`, `1.5` is `2`.
    assert_eq!(
        eval(
            "var a = new Uint8ClampedArray(5);\n\
              a[0] = -100; a[1] = 1.5; a[2] = 0.5; a[3] = 1233.5; a[4] = NaN;\n\
              print(a.join(','))"
        ),
        "0,2,0,255,0\n"
    );
    // …and a float view round-trips through half precision.
    assert_eq!(
        eval("print(new Float16Array([1, 0.5, -2]).join(','))"),
        "1,0.5,-2\n"
    );
}

#[test]
fn typed_array_reads_and_detachment() {
    // Out-of-range reads and writes are silently `undefined`/ignored, not errors.
    assert_eq!(
        eval("var a = new Uint8Array(2); a[5] = 1; print(String(a[5]), a.length)"),
        "undefined 2\n"
    );
    // `transfer()` detaches: the byte count drops to 0 and every index reads `undefined`,
    // which all the views of that buffer see at once.
    assert_eq!(
        eval(
            "var b = new ArrayBuffer(16); var a = new Uint8Array(b); a.fill(42);\n\
              var before = a[0]; b.transfer();\n\
              print(before, String(a[0]), a.length, b.byteLength)"
        ),
        "42 undefined 0 0\n"
    );
    // A misaligned or oversized window is a `RangeError`.
    assert_eq!(
        throws("new Uint32Array(new ArrayBuffer(16), 2, 1)"),
        "RangeError: byte offset is not aligned"
    );
    assert_eq!(
        throws("new Uint32Array(new ArrayBuffer(4), 0, 2)"),
        "RangeError: invalid length"
    );
    // One argument means "the rest of the buffer", so a missing length is not a `NaN` error.
    assert_eq!(
        eval("var b = new ArrayBuffer(16); print(new Uint8Array(b).length)"),
        "16\n"
    );
}

#[test]
fn strings_hold_unpaired_surrogates() {
    // The whole reason for the WTF-8 representation: `String.fromCharCode` takes a code
    // unit, and a unit with no partner is a legal string.
    assert_eq!(
        eval(
            "var s = String.fromCharCode(0xD800);\n\
              print(s.length, s.charCodeAt(0))"
        ),
        "1 55296\n"
    );
    // Building a run of units, which is what a script does when it assembles a string.
    assert_eq!(
        eval(
            "var s = ''; for (var i = 0; i < 5; i++) s += String.fromCharCode(0xD800 + i);\n\
              print(s.length, s.charCodeAt(0), s.charCodeAt(4), String(s.charCodeAt(9)))"
        ),
        "5 55296 55300 NaN\n"
    );
    // `length` and indexing count code *units*, so an astral character is two of them.
    assert_eq!(
        eval("print('\\u{1F600}'.length, String.fromCharCode(0xD800, 0xDC00).length)"),
        "2 2\n"
    );
    // `codePointAt` recombines a pair; `charCodeAt` does not.
    assert_eq!(
        eval(
            "var s = String.fromCharCode(0xD800, 0xDC00);\n\
              print(s.codePointAt(0), s.charCodeAt(0), s.charCodeAt(1))"
        ),
        "65536 55296 56320\n"
    );
}

#[test]
fn concatenation_keeps_the_encoding_canonical() {
    // A surrogate pair split across a `+` must produce the *same* string as the astral
    // character, or equality and property keys would disagree with the language.
    assert_eq!(
        eval("print(String.fromCharCode(0xD800) + String.fromCharCode(0xDC00) === '\\u{10000}')"),
        "true\n"
    );
    assert_eq!(
        eval("print((String.fromCharCode(0xD800) + String.fromCharCode(0xDC00)).length)"),
        "2\n"
    );
    // …and the same through `+=`, which is how a script usually builds one.
    assert_eq!(
        eval(
            "var s = ''; s += String.fromCharCode(0xD83D); s += String.fromCharCode(0xDE00);\n\
              print(s.length, s === '\\u{1F600}')"
        ),
        "2 true\n"
    );
    // Two unpaired surrogates that are *not* a pair stay two separate units.
    assert_eq!(
        eval("print((String.fromCharCode(0xD800) + String.fromCharCode(0xD800)).length)"),
        "2\n"
    );
}

#[test]
fn a_lone_surrogate_key_is_its_own_property() {
    // A key with an unpaired surrogate must not collide with the replacement character,
    // which is what a lossy conversion would do.
    assert_eq!(
        eval(
            "var o = {};\n\
              o[String.fromCharCode(0xD800)] = 1;\n\
              o[String.fromCharCode(0xFFFD)] = 2;\n\
              print(o[String.fromCharCode(0xD800)], o[String.fromCharCode(0xFFFD)],\n\
                    Object.keys(o).length)"
        ),
        "1 2 2\n"
    );
}

#[test]
fn indexing_a_long_string_stays_linear() {
    // `charCodeAt` in a loop must not re-decode from the start each time: it is the one
    // shape a script uses to walk a whole string, so the code units are cached on first
    // use. 60k units is enough that a quadratic implementation would take minutes.
    assert_eq!(
        eval(
            "var s = ''; for (var i = 0; i < 60000; i++) s += String.fromCharCode(i & 0xffff);\n\
              var bad = 0;\n\
              for (var i = 0; i < 60000; i++) if (s.charCodeAt(i) !== (i & 0xffff)) bad++;\n\
              print(s.length, bad)"
        ),
        "60000 0\n"
    );
}

#[test]
fn bigint_is_exact_at_any_width() {
    // The canonical representation is what makes a computed value equal to itself.
    assert_eq!(
        eval("print(2n ** 1000n === 2n ** 1000n, 2n ** 1000n - (2n ** 1000n - 1n))"),
        "true 1n\n"
    );
    assert_eq!(
        eval("print(1n + 2n, 10n - 3n, 6n * 7n, 7n / 2n, 7n % 2n, 1n << 128n)"),
        "3n 7n 42n 3n 1n 340282366920938463463374607431768211456n\n"
    );
    assert_eq!(eval("print(typeof 1n, -5n, ~0n)"), "bigint -5n -1n\n");
    // `>>` is arithmetic: `-1n >> 1n` is `-1n`, not `0n`.
    assert_eq!(
        eval("print(-1n >> 1n, -3n >> 1n, -1n & 3n, -1n | 3n)"),
        "-1n -2n 3n -1n\n"
    );
}

#[test]
fn bigint_and_number_never_mix_in_arithmetic() {
    for expression in ["1n + 1", "1n * 2", "1 - 1n"] {
        assert_eq!(
            throws(expression),
            "TypeError: cannot convert bigint to number",
            "{expression}"
        );
    }
    // Relation is the exception: it compares the mathematical values, not rounded ones.
    assert_eq!(
        eval("print(-1n < 1n, 2n < 2.5, 3n > 2.5)"),
        "true true true\n"
    );
    // …and loose equality crosses the types while strict does not.
    assert_eq!(
        eval("print(1n == 1, 1n === 1, 1n == '1')"),
        "true false true\n"
    );
}

#[test]
fn bigint_conversion_is_exact_or_refuses() {
    assert_eq!(
        eval("print(BigInt(5), BigInt('123'), BigInt(true), BigInt('0x1f'))"),
        "5n 123n 1n 31n\n"
    );
    // `StringToBigInt` reads an empty literal as zero, while a lone sign stays a
    // `SyntaxError` — that pair is why the empty case cannot simply be "no digits".
    assert_eq!(
        eval("print(BigInt(''), BigInt('   '), BigInt('  123  '))"),
        "0n 0n 123n\n"
    );
    for source in ["BigInt('+')", "BigInt('-')", "BigInt('\\x00a')"] {
        assert!(
            throws(source).starts_with("SyntaxError"),
            "{source} should be a SyntaxError"
        );
    }
    // A fraction has no exact bigint form, so it is a `RangeError` rather than a truncation.
    assert!(throws("BigInt(1.5)").starts_with("RangeError"));
    // A bigint is a primitive, so there is no wrapper to construct.
    assert_eq!(
        throws("new BigInt(1)"),
        "TypeError: BigInt is not a constructor"
    );
    assert_eq!(
        eval("print((255n).toString(16), BigInt.asIntN(8, 128n))"),
        "ff -128n\n"
    );
    // `Number()` of a bigint rounds to the nearest double, losing width past 53 bits.
    assert_eq!(
        eval("print(Number(1n), Number(0xffffffffffffffffn))"),
        "1 18446744073709552000\n"
    );
}

#[test]
fn bigint_obeys_the_number_rules_for_truth_and_equality() {
    // `0n` is falsy. Treating every bigint as truthy made `if (b & 1n)` branch
    // unconditionally, which silently corrupted bigint algorithms that test a bit —
    // `compute_pi` drifted from the 29th digit onwards.
    assert_eq!(
        eval("print(0n ? 'T' : 'F', 1n ? 'T' : 'F', !0n, Boolean(-1n))"),
        "F T true true\n"
    );
    // `++`/`--` keep a bigint a bigint. Routing them through `ToNumber` produced `NaN`,
    // so `for (i = 0n; i < n; i++)` ran exactly once.
    assert_eq!(
        eval("var i = 0n; print(i++, i, ++i, typeof i); var j = 3n; print(j--, j)"),
        "0n 1n 2n bigint\n3n 2n\n"
    );
    // Unary `+` asks for a *number*, which a bigint cannot become, so it is a `TypeError`
    // rather than the `NaN` it used to be.
    assert!(throws("+1n").starts_with("TypeError"));
    // Loose equality compares the exact integers, not two rounded doubles:
    // `100000000000000000001n` and `1e20` round to the same double but are not equal.
    // A boolean is `ToNumber`ed first, so `true == 1n` holds.
    assert_eq!(
        eval(
            "print(2 == 3n, 2 == 2n, '' == 0n, 2.5 == 2n, true == 1n, false == 0n,\n\
             100000000000000000000n == 1e20, 100000000000000000001n == 1e20, null == 0n)"
        ),
        "false true true false true true true false false\n"
    );
    // `+` concatenates before it ever considers the numeric types.
    assert_eq!(eval("print('a' + 1n, 1n + 'b')"), "a1 1b\n");
    assert!(throws("1n + 1").starts_with("TypeError"));
}

#[test]
fn map_keys_use_same_value_zero() {
    // `NaN` equals itself as a key, which `===` does not give.
    assert_eq!(
        eval("var m = new Map(); m.set(NaN, 'nan'); print(m.get(NaN), m.size)"),
        "nan 1\n"
    );
    // Numbers, strings and bigints are three different keys even when they look alike.
    assert_eq!(
        eval(
            "var m = new Map(); m.set(1, 'n'); m.set('1', 's'); m.set(1n, 'b');\n\
              print(m.size, m.get(1), m.get('1'), m.get(1n))"
        ),
        "3 n s b\n"
    );
    // Objects key by identity, never structurally.
    assert_eq!(
        eval(
            "var m = new Map(); var a = {}, b = {}; m.set(a, 'a');\n\
              print(m.get(a), m.get(b))"
        ),
        "a undefined\n"
    );
    // Insertion order is iteration order, and a re-set key keeps its position.
    assert_eq!(
        eval(
            "var m = new Map(); m.set(1, 'a'); m.set(2, 'b'); m.set(1, 'c');\n\
              var log = []; m.forEach(function (v, k) { log.push(k + ':' + v) });\n\
              print(m.size, log.join(','))"
        ),
        "2 1:c,2:b\n"
    );
}

#[test]
fn map_and_set_surface() {
    assert_eq!(
        eval(
            "var m = new Map([[1, 'a'], [2, 'b']]);\n\
              print(m.size, m.has(1), m.delete(1), m.has(1), m.size)"
        ),
        "2 true true false 1\n"
    );
    // A `Set` deduplicates and its entries repeat the value.
    assert_eq!(
        eval(
            "var s = new Set([1, 2, 2, 3]);\n\
              print(s.size, [...s.entries()].map(e => e.join('=')).join(','))"
        ),
        "3 1=1,2=2,3=3\n"
    );
    assert_eq!(
        eval(
            "print(Object.prototype.toString.call(new Map()),\n\
                    Object.prototype.toString.call(new Set()))"
        ),
        "[object Map] [object Set]\n"
    );
    assert_eq!(
        eval(
            "var s = new Set(['a']); var out = [];\n\
              s.forEach(function (v, k) { out.push(v === k) });\n\
              print(out.join(','))"
        ),
        "true\n"
    );
}

#[test]
fn regexp_character_class_set_algebra() {
    // `v` mode replaces the flat range list with a real set algebra: nested classes,
    // intersection and subtraction.
    assert_eq!(
        eval("print('aAbBcC'.replace(/[[a-c]&&B]/gvi, 'X'))"),
        "aAXXcC\n"
    );
    assert_eq!(
        eval("print('aAbBcC'.replace(/[[a-c]--B]/gvi, 'X'))"),
        "XXbBXX\n"
    );
    // `\q{…}` makes a set of *strings*: a member may be two characters long, so matching it
    // consumes two and the alternation is tried longest-first.
    assert_eq!(
        eval("print('abcAbC'.replace(/[\\q{BC|A}]/gvi, 'X'))"),
        "XXXX\n"
    );
    assert_eq!(
        eval("print('abcAbC'.replace(/[\\q{BC|A}--a]/gvi, 'X'))"),
        "aXAX\n"
    );
    // `&&` and `--` cannot be mixed at one level, and `-` must be escaped.
    assert!(throws("new RegExp('[[a-c]&&B--a]', 'v')").starts_with("SyntaxError"));
    // A range is still a range, but a lone `-` has to be escaped — that is what makes `--`
    // unambiguous as the subtraction operator.
    assert_eq!(eval("print(/[a-z]/v.test('m'))"), "true\n");
    assert!(throws("new RegExp('[-]', 'v')").starts_with("SyntaxError"));
    assert!(throws("new RegExp('[a-]', 'v')").starts_with("SyntaxError"));
}

#[test]
fn regexp_u_and_v_disagree_about_negated_properties() {
    // Under `i`, `u` negates per case variant — `a` canonicalises to the non-lowercase `A` —
    // while `v` case-closes the set first and only then complements it.
    assert_eq!(
        eval("print(/\\P{Lower}/ui.test('a'), /\\P{Lower}/vi.test('a'))"),
        "true false\n"
    );
    assert_eq!(
        eval(
            "print('aAbBcC#4'.replace(/\\P{Lower}/gui, 'X'), 'aAbBcC#4'.replace(/\\P{Lower}/gvi, 'X'))"
        ),
        "XXXXXXXX aAbBcCXX\n"
    );
    // …so the double negation `[^\P{Lower}]` means "lowercase letters" in `v`.
    assert_eq!(
        eval("print('aAbBcC#4'.replace(/[^\\P{Lower}]/gvi, 'X'))"),
        "XXXXXX#4\n"
    );
    // A property escape inside a class works in `u` as well.
    assert_eq!(
        eval("print(/[\\p{Lower}]/u.test('a'), /[\\p{Lower}]/u.test('A'))"),
        "true false\n"
    );
}

#[test]
fn regexp_code_point_escapes_and_code_unit_offsets() {
    // `\u{…}` is the strict-grammar spelling of a code point; without `u` it is the letter
    // `u` followed by a quantifier, which is the Annex B reading.
    assert_eq!(
        eval("print(/\\u{10000}/u.test('\\u{10000}'), /\\u{2}/.test('uu'))"),
        "true true\n"
    );
    assert_eq!(eval("print(/\\u{2}/.source)"), "\\u{2}\n");
    // `index`, `lastIndex` and the `d` flag's pairs all count UTF-16 code *units*, so an
    // astral match spans two.
    assert_eq!(
        eval("var m = /\\u{10000}/u.exec('a\\u{10000}'); print(m.index, m[0].length)"),
        "1 2\n"
    );
    assert_eq!(
        eval("var m = /\\u{10000}/vgd.exec('\\u{10000}_'); print(m.indices[0].join(','))"),
        "0,2\n"
    );
    // A `lastIndex` inside a surrogate pair belongs to the character containing it, which is
    // the same as rounding down to a code point boundary.
    assert_eq!(
        eval("var a = /(?:)/gu; a.lastIndex = 1; a.exec('\\u{1F600}'); print(a.lastIndex)"),
        "0\n"
    );
}

#[test]
fn a_sets_default_iterator_yields_values_not_entries() {
    // A `Map` iterates `[key, value]` pairs and a `Set` iterates its values. Wiring both
    // to `entries` made `[...new Set([3])]` produce `[[3, 3]]` — one pair — which reads
    // like a working iterator until you look at the elements.
    assert_eq!(
        eval(
            r#"
print([...new Set([3])].join());
print([...new Set([1,2,2,3])].join());
for (const v of new Set(["a"])) print("v:" + v);
print([...new Map([["k", 1]])].join("|"));
"#
        ),
        "3\n1,2,3\nv:a\nk,1\n"
    );
}

#[test]
fn array_from_reads_an_iterable_not_only_an_array_like() {
    // A `Map` and a `Set` have no `length`, so an array-like-only reader answered `[]`
    // for both — the wrong answer rather than an error.
    assert_eq!(
        eval(
            r#"
print(JSON.stringify(Array.from(new Set([1,2]))));
print(JSON.stringify(Array.from(new Map([[1,2]]))));
print(JSON.stringify(Array.from("ab")));
print(JSON.stringify(Array.from([1,2])));
print(JSON.stringify(Array.from({length: 2, 0: "x", 1: "y"})));
print(JSON.stringify(Array.from(new Set([1,2]), v => v * 10)));
"#
        ),
        "[1,2]\n[[1,2]]\n[\"a\",\"b\"]\n[1,2]\n[\"x\",\"y\"]\n[10,20]\n"
    );
}

#[test]
fn function_name_length_and_prototype_have_the_spec_attributes() {
    // Every one of these was `configurable: false` — derived from `writable`, a
    // derivation that is right for a built-in method and wrong for these three shapes.
    // Roughly five hundred conformance tests depended on it, spread over every built-in,
    // because `Object.assign`, `bind` and every `defineProperty(f, "length", …)` need to
    // be able to redefine them.
    let descriptor = |expr: &str| {
        value(&format!(
            r#"var d = Object.getOwnPropertyDescriptor({expr});
               d === undefined ? "absent" : [d.writable, d.enumerable, d.configurable].join(",")"#
        ))
    };
    // `{writable: false, enumerable: false, configurable: true}` for name and length.
    let pairs = [
        ("(function f(a){})", "name"),
        ("(function f(a){})", "length"),
        ("(function(){})", "name"),
        ("(()=>{})", "name"),
        ("(()=>{})", "length"),
        ("(class C{})", "name"),
        ("Object", "name"),
        ("Object", "length"),
        ("Array.prototype.map", "name"),
        ("Array.prototype.map", "length"),
        ("(function f(a){}).bind(null)", "name"),
        ("(function f(a){a}).bind(null)", "length"),
        (
            "Object.getOwnPropertyDescriptor({get x(){}}, 'x').get",
            "name",
        ),
    ];
    for (object, key) in pairs {
        assert_eq!(
            descriptor(&format!("{object}, \"{key}\"")),
            "false,false,true",
            "{object}.{key}"
        );
    }
    // An anonymous function's name is present and empty, not absent.
    assert_eq!(value("(function(){}).name"), "");
    assert_eq!(value("(()=>{}).name"), "");
    assert_eq!(value("(class {}).name"), "");
    // A user function's `prototype` is writable but *not* configurable, and a built-in
    // constructor's is neither.
    assert_eq!(
        descriptor("(function f(){}), \"prototype\""),
        "true,false,false"
    );
    assert_eq!(descriptor("Object, \"prototype\""), "false,false,false");
    assert_eq!(value("delete (function f(){}).prototype"), "false");
    // NamedEvaluation still names an anonymous class after its binding.
    assert_eq!(value("var x = class {}; x.name"), "x");
    assert_eq!(value("var x = class {}; typeof x"), "function");
}

#[test]
fn array_methods_are_generic_over_array_likes_and_read_the_receiver_lazily() {
    // Every line here was wrong: the receiver was copied up front (so the third callback
    // argument was an impostor and a callback that deleted an element still saw it),
    // holes were dropped from `map`'s result, a primitive receiver was not boxed, and an
    // absurd `length` looped instead of being refused.
    assert_eq!(
        eval(
            r#"
var o = {length: 2, 0: 1, 1: 2};
var same = null;
Array.prototype.map.call(o, function (v, i, x) { same = x; });
print(same === o);

var a = [1, 2];
print(JSON.stringify(a.map(function (v, i, x) { if (i === 0) x[1] = 99; return v; })));
print(JSON.stringify([1,,3].map(function (x) { return x; })));
print(1 in [1,,3].map(function (x) { return x; }));
print([1,,3].includes(undefined), [1,,3].indexOf(undefined));

Boolean.prototype[0] = true;
Boolean.prototype.length = 1;
print(Array.prototype.map.call(false, function (v, i, x) {
  return x instanceof Boolean;
})[0]);

print(JSON.stringify(Object.assign({}, "ab")));
print(typeof Object.assign(1, {a: 1}));

Array.prototype[1] = 9;
var visited = [];
[1,,3].forEach(function (v) { visited.push(v); });
print(JSON.stringify(visited));
delete Array.prototype[1];

try {
  Array.prototype.map.call({length: 4294967296}, function () {});
  print("no throw");
} catch (e) { print(e.name); }
"#
        ),
        concat!(
            "true\n",
            "[1,99]\n",
            "[1,null,3]\n",
            "false\n",
            "true -1\n",
            "true\n",
            "{\"0\":\"a\",\"1\":\"b\"}\n",
            "object\n",
            "[1,9,3]\n",
            "RangeError\n",
        )
    );
}

#[test]
fn define_property_follows_validate_and_apply() {
    // `Object.defineProperties` did not exist, so "undefined is not a function" was the
    // answer to everything — including the tests that assert a TypeError, which passed by
    // accident. With it in place the validation rules had to be real.
    assert_eq!(
        eval(
            r#"
function attempt(source) {
  try { return String(eval(source)); } catch (e) { return e.name; }
}
// An omitted attribute is kept, not defaulted to false.
print(attempt("var o={a:1}; Object.defineProperty(o,'a',{value:5});" +
              "JSON.stringify(Object.getOwnPropertyDescriptor(o,'a'))"));
// A non-configurable property cannot be redefined...
print(attempt("var o={}; Object.defineProperty(o,'p',{value:1});" +
              "Object.defineProperty(o,'p',{value:2})"));
// ...but redefining it with the same value is not a change, `NaN` included.
print(attempt("var o={}; Object.defineProperty(o,'n',{value:NaN});" +
              "Object.defineProperty(o,'n',{value:NaN}); 'ok'"));
// A descriptor cannot be both a data and an accessor property.
print(attempt("Object.defineProperty({},'x',{value:1,set:function(){}})"));
// A non-extensible target refuses a new property.
print(attempt("var o=Object.preventExtensions({}); Object.defineProperty(o,'p',{value:1})"));
// An array's `length` is exotic.
var a = [1, 2, 3];
Object.defineProperty(a, "length", {value: 1});
print(a.length, a[2]);
print(attempt("Object.defineProperty([],'length',{enumerable:true})"));
var b = [1, 2, 3];
Object.defineProperty(b, "length", {writable: false});
print(Object.getOwnPropertyDescriptor(b, "length").writable);
print(attempt("Object.defineProperty(b,'3',{value:'x'})"));
print(b.hasOwnProperty("3"));
"#
        ),
        concat!(
            "{\"value\":5,\"writable\":true,\"enumerable\":true,\"configurable\":true}\n",
            "TypeError\n",
            "ok\n",
            "TypeError\n",
            "TypeError\n",
            "1 undefined\n",
            "TypeError\n",
            "false\n",
            "TypeError\n",
            "false\n",
        )
    );
}

#[test]
fn typed_arrays_are_complete_enough_to_iterate_and_transform() {
    // Every one of these was `TypeError: undefined is not a function`: `%TypedArray%` and
    // its whole method family did not exist. `map`/`filter`/`slice` answer a *view of the
    // same type* rather than an array, and `map` over `BigInt64Array` must answer BigInts.
    assert_eq!(
        eval(
            r#"
print(new Int32Array([1,2,3]).map(function (x) { return x * 2; }).join(","));
print(new Int32Array([1,2,3]).map(function (x) { return x; }).constructor.name);
print(new Int32Array([1,2,3,4]).filter(function (x) { return x % 2 === 0; }).join(","));
print([...new Int32Array([1,2,3])].join(","));
print(JSON.stringify([...new Int32Array([9,8]).keys()]));
print(new Int32Array([10,9,1]).sort().join(","));
print(new Int32Array([1,2,3]).reduce(function (a, b) { return a + b; }));
print(new Int32Array([5,6,7]).at(-1), new Int32Array([5,6,7]).findLast(function (x) { return x > 5; }));

// A BigInt view reads and writes BigInts, refuses numbers, and wraps modulo 2^64.
print(typeof new BigInt64Array(1)[0]);
var wrapped = new BigUint64Array(1);
wrapped[0] = -1n;
print(wrapped[0]);
try { new BigInt64Array([1]); print("no throw"); } catch (e) { print(e.name); }
print(new BigInt64Array([1n,-2n]).sort().join(","));

// `%TypedArray%` is the shared parent of every view constructor.
var TA = Object.getPrototypeOf(Int8Array);
print(TA.name, Object.getOwnPropertyNames(TA).join(","));
print(Int8Array.from([1,2]).join(","), Int32Array.of(3,4).join(","));
try { TA(); print("no throw"); } catch (e) { print(e.name); }

// `Symbol.iterator` wins over `length` when constructing from a source.
var iterable = {};
iterable[Symbol.iterator] = function () {
  var i = 0;
  return { next: function () { return i < 2 ? { value: ++i, done: false } : { done: true }; } };
};
print(new Int32Array(iterable).join(","));

// A derived view that is too short is refused rather than written past its end.
var s = new Int8Array(2);
s.constructor = {};
s.constructor[Symbol.species] = function () { return new Int8Array(); };
try { s.map(function () { return 0; }); print("no throw"); } catch (e) { print(e.name); }

// A detached buffer makes every method throw.
var ab = new ArrayBuffer(8);
var detached = new Int32Array(ab);
ab.transfer();
try { detached.map(function () { return 0; }); print("no throw"); } catch (e) { print(e.name); }
"#
        ),
        concat!(
            "2,4,6\n",
            "Int32Array\n",
            "2,4\n",
            "1,2,3\n",
            "[0,1]\n",
            "1,9,10\n",
            "6\n",
            "7 7\n",
            "bigint\n",
            "18446744073709551615n\n",
            "TypeError\n",
            "-2,1\n",
            "TypedArray length,name,from,of,prototype\n",
            "1,2 3,4\n",
            "TypeError\n",
            "1,2\n",
            "TypeError\n",
            "TypeError\n",
        )
    );
}

#[test]
fn array_from_and_array_index_search_honour_their_arguments() {
    // `Array.from({length: 2})` was an *empty* array — a missing index was skipped instead
    // of contributing `undefined` — which quietly shortened every source a test built with
    // `Array.from({length: n}, …)`. And the typed-array searches ignored `fromIndex`
    // entirely, so a Symbol there returned cleanly instead of throwing.
    assert_eq!(
        eval(
            r#"
print(Array.from({length: 2}).length);
print(JSON.stringify(Array.from({length: 2, 0: "x"})));
print(JSON.stringify(Array.from([1], function () { return this.v; }, {v: 9})));
print(JSON.stringify(Array.from(new Set([1,2]))));

var a = new Int32Array([1,2,3,2,1]);
print(a.indexOf(2), a.indexOf(2, 2), a.indexOf(2, -2));
print(a.lastIndexOf(2), a.lastIndexOf(2, 2), a.lastIndexOf(2, -2));
print(a.includes(1, 2), a.includes(1, -1), a.includes(1, Infinity));
print(a.indexOf(1, Infinity), a.lastIndexOf(1, -Infinity));
try { a.includes(1, Symbol()); print("no throw"); } catch (e) { print(e.name); }
"#
        ),
        concat!(
            "2\n",
            "[\"x\",null]\n",
            "[9]\n",
            "[1,2]\n",
            "1 3 3\n",
            "3 1 3\n",
            "true true false\n",
            "-1 -1\n",
            "TypeError\n",
        )
    );
}

#[test]
fn data_view_reads_and_writes_bytes_at_any_offset() {
    // `DataView` did not exist at all — all 520 conformance cases were `ReferenceError`.
    // Its distinguishing feature against a typed array is that the width and byte offset are
    // named per access, so alignment never matters and endianness is per call.
    assert_eq!(
        eval(
            r#"
var ab = new ArrayBuffer(16);
var dv = new DataView(ab);
print(dv.byteOffset, dv.byteLength, dv.buffer === ab);
print(Object.prototype.toString.call(dv));

var partial = new DataView(ab, 4);
print(partial.byteOffset, partial.byteLength);
var sized = new DataView(ab, 4, 8);
print(sized.byteOffset, sized.byteLength);

// Big-endian by default, little-endian on request.
var order = new DataView(new ArrayBuffer(4));
order.setUint32(0, 0x01020304);
print([...new Uint8Array(order.buffer)].join(","));
order.setUint32(0, 0x01020304, true);
print([...new Uint8Array(order.buffer)].join(","));

var nums = new DataView(new ArrayBuffer(8));
nums.setInt16(0, -2);
print(nums.getInt16(0, true), nums.getInt16(0, false));
nums.setFloat64(0, Math.PI);
print(nums.getFloat64(0));
nums.setBigInt64(0, -1n);
print(String(nums.getBigInt64(0)), String(nums.getBigUint64(0)));

// Bounds are enforced per access, not per view.
var small = new DataView(new ArrayBuffer(2));
try { small.getInt32(0); print("no throw"); } catch (e) { print(e.name); }
try { new DataView(ab, 4, 100); print("no throw"); } catch (e) { print(e.name); }
try { new DataView(new ArrayBuffer(8), Symbol()); print("no throw"); } catch (e) { print(e.name); }
// A detached buffer makes reads throw but keeps the properties readable.
var gone = new DataView(new ArrayBuffer(8));
gone.buffer.transfer();
print(gone.byteLength);
try { gone.getInt8(0); print("no throw"); } catch (e) { print(e.name); }
"#
        ),
        concat!(
            "0 16 true\n",
            "[object DataView]\n",
            "4 12\n",
            "4 8\n",
            "1,2,3,4\n",
            "4,3,2,1\n",
            "-257 -2\n",
            "3.141592653589793\n",
            "-1 18446744073709551615\n",
            "RangeError\n",
            "RangeError\n",
            "TypeError\n",
            "0\n",
            "TypeError\n",
        )
    );
}

#[test]
fn reflect_construct_honours_new_target_and_native_constructors_order_their_checks() {
    // `Reflect.construct`'s third argument was dropped entirely, so `new.target.prototype`
    // never decided the prototype. Fixing that exposed the next layer: a native constructor
    // built its instance *before* validating its arguments, so `new Promise(nonCallable)`
    // and `new DataView(detached, …)` read a `prototype` getter they should not have
    // reached. Both halves are asserted here because each hid the other.
    assert_eq!(
        eval(
            r#"
function newTarget() {}
var proto = {};
newTarget.prototype = proto;

function C() {}
print(Object.getPrototypeOf(Reflect.construct(C, [], newTarget)) === proto);
print(Object.getPrototypeOf(Reflect.construct(C, [])) === C.prototype);
print(Object.getPrototypeOf(Reflect.construct(ArrayBuffer, [8], newTarget)) === proto);

// A `prototype` that is not an object falls back to the callee's own.
var bad = function () {};
bad.prototype = 1;
print(Object.getPrototypeOf(Reflect.construct(C, [], bad)) === C.prototype);

// A throwing `prototype` getter must not be reached when an argument is already invalid.
var angry = (function () {}).bind();
Object.defineProperty(angry, "prototype", {
  get: function () { throw new Error("prototype read too early"); },
});
try { Reflect.construct(Promise, [], angry); print("no throw"); }
catch (e) { print(e.name); }
"#
        ),
        "true\ntrue\ntrue\ntrue\nTypeError\n"
    );
}

#[test]
fn only_constructors_have_a_construct_method() {
    // §7.2.4 `IsConstructor`. Being callable is not the same property: an arrow function,
    // a generator, an async function, a class method and a plain object method are all
    // callable and none of them may be used with `new`. Deriving `[[Construct]]` from
    // `is_callable` made `new (Array.prototype.map)` build an object out of nothing.
    assert_eq!(
        eval(
            r#"
function isCtor(f) { try { Reflect.construct(function () {}, [], f); return true } catch (e) { return false } }
print(isCtor(Array.prototype.map), isCtor(Object.keys), isCtor(JSON.stringify), isCtor(Math.max));
print(isCtor(Object), isCtor(Array), isCtor(Map), isCtor(Error), isCtor(Proxy));
print(isCtor(1), isCtor({}), isCtor(null));
function plain() {}
print(isCtor(plain), isCtor(class {}), isCtor(() => 1), isCtor(function* () {}), isCtor(async function () {}));
var object = { method() {} };
class Klass { method() {} }
print(isCtor(object.method), isCtor(Klass.prototype.method), isCtor(Klass));
print(isCtor(plain.bind(null)), isCtor((() => 1).bind(null)));
print(isCtor(Symbol), isCtor(BigInt));
"#
        ),
        "false false false false\n\
         true true true true true\n\
         false false false\n\
         true true false false false\n\
         false false true\n\
         true false\n\
         true true\n"
    );
}

#[test]
fn new_on_a_non_constructor_names_it() {
    // The message matters: test262 asserts on the class, and users read the name.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name + ": " + e.message } }
function named() {}
print(kind(() => new named()));
print(kind(() => new (() => 1)()));
print(kind(() => new (function* G() {})()));
print(kind(() => new (async function A() {})()));
print(kind(() => new Symbol()));
print(kind(() => new BigInt(1)));
print(kind(() => new named()));
"#
        ),
        "no throw\n\
         TypeError:  is not a constructor\n\
         TypeError: G is not a constructor\n\
         TypeError: A is not a constructor\n\
         TypeError: Symbol is not a constructor\n\
         TypeError: BigInt is not a constructor\n\
         no throw\n"
    );
}

#[test]
fn math_refuses_a_bigint_instead_of_answering_nan() {
    // `Value::to_number` is total and answers `NaN`, which is fine for the operators (they
    // check the bigint themselves) and wrong for every `ToNumber` caller: `Math.max(1n)`
    // is a `TypeError`, and answering `NaN` made it silently wrong for a whole milestone.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name } }
print(kind(() => Math.max(1n)), kind(() => Math.min(1n)), kind(() => Math.abs(1n)));
print(kind(() => Math.sqrt(1n)), kind(() => Math.pow(1n, 2)), kind(() => Math.hypot(1n)));
print(kind(() => Math.sign(1n)), kind(() => Math.imul(1n, 2)), kind(() => Math.clz32(1n)));
print(kind(() => Math.fround(1n)), kind(() => Math.sumPrecise([1n])));
print(kind(() => Math.max(Symbol())));
print(Math.max(1, 2), Math.max(), 1 / Math.min(0, -0));
"#
        ),
        "TypeError TypeError TypeError\n\
         TypeError TypeError TypeError\n\
         TypeError TypeError TypeError\n\
         TypeError TypeError\n\
         TypeError\n\
         2 -Infinity -Infinity\n"
    );
}

#[test]
fn number_refuses_a_symbol_but_accepts_a_bigint() {
    // The one asymmetry in `ToNumber`: §21.1.1.1 makes `Number(1n)` return `1`, while
    // `Number(Symbol())` is a `TypeError` — so the BigInt special case cannot be dropped
    // in favour of `ToNumber`, and the Symbol case cannot be left to `to_number`'s `NaN`.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { return String(f()) } catch (e) { return e.constructor.name } }
print(kind(() => Number(1n)), kind(() => Number()), kind(() => Number(true)));
print(kind(() => Number(Symbol())), kind(() => Number(Object(Symbol()))));
"#
        ),
        "1 0 1\nTypeError TypeError\n"
    );
}

#[test]
fn iterator_helpers_are_lazy_and_chainable() {
    // The defining property of the API: `take` on an infinite generator has to terminate,
    // which is only possible if nothing runs until `next` is called.
    assert_eq!(
        eval(
            r#"
function* naturals() { let i = 0; while (true) yield i++; }
print(JSON.stringify(naturals().take(3).toArray()));
print(JSON.stringify([1, 2, 3, 4].values().map(x => x * 2).filter(x => x > 4).toArray()));
print(JSON.stringify([1, 2, 3, 4, 5].values().drop(3).toArray()));
print(JSON.stringify([[1, 2], [3]].values().flatMap(x => x).toArray()));
print(JSON.stringify([1, 2, 3].values().reduce((a, b) => a + b, 10)));
print(JSON.stringify([1, 2, 3, 4].values().chunks(3).toArray()));
print(JSON.stringify([1, 2, 3, 4].values().windows(3).toArray()));
print(JSON.stringify([1, 2].values().join("-")), [1, 2].values().includes(2));
print(JSON.stringify([1, 2, 3].values().map((v, i) => i).toArray()));
"#
        ),
        "[0,1,2]\n[6,8]\n[4,5]\n[1,2,3]\n16\n[[1,2,3],[4]]\n[[1,2,3],[2,3,4]]\n\"1-2\" true\n[0,1,2]\n"
    );
}

#[test]
fn an_iterator_is_not_the_same_as_an_iterable() {
    // `Iterator` may be subclassed but not instantiated, `Iterator.from` returns an
    // existing iterator untouched, and a bare `{next}` counts as an iterator even without
    // `Symbol.iterator`. All three are easy to get backwards.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name + ": " + e.message } }
class Sub extends Iterator {}
print(Object.getPrototypeOf(new Sub()) === Sub.prototype);
print(kind(() => new Iterator()));
print(kind(() => Iterator()));
const existing = [1].values();
print(Iterator.from(existing) === existing);
print(JSON.stringify(Iterator.from("ab").toArray()));
print(JSON.stringify(Iterator.from({ next() { return { done: true }; } }).toArray()));
print(kind(() => Iterator.from(1)));
print(JSON.stringify(Iterator.concat([1], [2]).toArray()));
print(JSON.stringify(Iterator.zip([[1, 2], [3, 4]]).toArray()));
print(JSON.stringify(Iterator.zip([[1, 2, 3], [4, 5]], { mode: "longest" }).toArray()));
print(JSON.stringify(Iterator.zipKeyed({ a: [1], b: [2] }).toArray()));
print(kind(() => Iterator.zip([[1, 2], [3]], { mode: "strict" }).toArray()));
"#
        ),
        "true\nTypeError: abstract class not constructable\nTypeError: constructor requires 'new'\n\
         true\n[\"a\",\"b\"]\n[]\nTypeError: not an object\n[1,2]\n[[1,3],[2,4]]\n\
         [[1,4],[2,5],[3,null]]\n[{\"a\":1,\"b\":2}]\n\
         TypeError: iterators have different lengths\n"
    );
}

#[test]
fn object_create_applies_its_properties_argument() {
    // `Object.create(proto, props)` used to drop `props` on the floor: the object came out
    // empty. Five test262 cases in the Iterator area alone build a null-prototype
    // properties bag that way.
    assert_eq!(
        eval(
            r#"
const o = Object.create(null, {
  a: { value: 1, enumerable: true },
  b: { get() { return 2; }, enumerable: true },
  c: { value: 3, enumerable: false },
});
print(Object.getPrototypeOf(o) === null);
print(Object.keys(o).join(","), Object.getOwnPropertyNames(o).join(","));
print(o.a, o.b);
const plain = Object.create(Object.prototype, { x: { value: 9 } });
print(plain.x, Object.getPrototypeOf(plain) === Object.prototype);
"#
        ),
        "true\na,b a,b,c\n1 2\n9 true\n"
    );
}

#[test]
fn define_property_keeps_an_accessor_whole() {
    // A descriptor that only names `enumerable` must leave an accessor an accessor: the
    // property used to be converted to a data property and its value blanked. And a
    // descriptor that names *one* half must keep the other.
    assert_eq!(
        eval(
            r#"
const o = {};
Object.defineProperty(o, "e", { get() { return "ve"; }, configurable: true });
Object.defineProperty(o, "e", { enumerable: true });
print(o.e, typeof Object.getOwnPropertyDescriptor(o, "e").get, o.propertyIsEnumerable("e"));

let stored = 1;
const p = {};
Object.defineProperty(p, "x", { get() { return stored; }, set(v) { stored = v; }, configurable: true });
Object.defineProperty(p, "x", { set(v) { stored = v * 10; } });
p.x = 3;
print(p.x);
"#
        ),
        "ve function true\n30\n"
    );
}

#[test]
fn a_property_name_is_only_set_on_a_function() {
    // `NamedEvaluation` applies to anonymous *functions*, never to whatever happens to sit
    // on the right-hand side. Renaming plain objects was how a `zipKeyed` result object
    // acquired an extra `name` property, and it happened through `Object.defineProperty`
    // too, which has no `NamedEvaluation` at all.
    assert_eq!(
        eval(
            r#"
const value = {};
Object.defineProperty({}, "value", { value });
print(value.name === undefined);
const other = {};
({ key: other });
print(other.name === undefined);
print(({ m: function () {} }).m.name, ({ n: class {} }).n.name, ({ p: () => 1 }).p.name);
"#
        ),
        "true\ntrue\nm n p\n"
    );
}

#[test]
fn reflect_keys_stay_symbols_and_propagate_errors() {
    // `Reflect.get`/`set`/`has` coerced the key with `ToString`, so every symbol key read
    // as `""` — which made a Proxy's `get` trap look like it returned `undefined`, because
    // `proxy_get` forwards through `Reflect.get`. `has` also ignored the prototype chain.
    assert_eq!(
        eval(
            r#"
const s = Symbol.iterator;
const o = { [s]: 1, a: 2 };
print(Reflect.get(o, s), Reflect.has(o, s), Reflect.has({}, "toString"));
Reflect.set(o, s, 7);
print(o[s]);
const p = new Proxy(o, { get: (t, k, r) => Reflect.get(t, k, r) });
print(typeof p[s]);
function kind(f) { try { f(); return "no throw" } catch (e) { return "threw" } }
const bad = { toString() { throw new Error("boom"); } };
print(kind(() => Reflect.get({}, bad)), kind(() => Reflect.set({}, bad, 1)), kind(() => Reflect.has({}, bad)));
"#
        ),
        "1 true true\n7\nnumber\nthrew threw threw\n"
    );
}

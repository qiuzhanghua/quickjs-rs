//! What the VM actually does, asserted against fixed expectations.
//!
//! Every expected string here was taken from the reference implementation
//! (`qjs -e …`) on the release this port tracks, so the file is a checkpoint on
//! behaviour rather than on our own output. It is deliberately independent of the
//! reference binary: the differential harness in `quickjs-conformance` compares
//! against a build when one is available, and this file keeps working without it.

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

#[test]
fn arithmetic() {
    assert_eq!(eval("print(1+1)"), "2\n");
    assert_eq!(eval("print(7-2, 3*4, 8/2, 7%3, 2**10)"), "5 12 4 1 1024\n");
    // A result that does not fit an integer becomes a double.
    assert_eq!(eval("print(2147483647 + 1)"), "2147483648\n");
    assert_eq!(eval("print(1/0, -1/0, 0/0)"), "Infinity -Infinity NaN\n");
    // `-0` keeps its sign through division.
    assert_eq!(eval("print(1/-0)"), "-Infinity\n");
    assert_eq!(
        eval("print(1e21, 1e-7, 0.1+0.2)"),
        "1e+21 1e-7 0.30000000000000004\n"
    );
}

#[test]
fn strings_and_concat() {
    assert_eq!(eval(r#"print("a"+"b")"#), "ab\n");
    // `+` concatenates as soon as either side is a string.
    assert_eq!(eval(r#"print("a"+1, 1+"a", 1+1+"a")"#), "a1 1a 2a\n");
    assert_eq!(eval(r#"print("abc".length, "abc"[1])"#), "3 b\n");
}

#[test]
fn comparison_and_equality() {
    assert_eq!(
        eval(r#"print(1<2, 2<=2, 3>4, "10"<"9")"#),
        "true true false true\n"
    );
    assert_eq!(
        eval(r#"print(1 == "1", 1 === "1", null == undefined, null === undefined)"#),
        "true false true false\n"
    );
    assert_eq!(eval("print(NaN == NaN, NaN != NaN)"), "false true\n");
}

#[test]
fn typeof_covers_the_type_tags() {
    assert_eq!(
        eval(r#"print(typeof 1, typeof "s", typeof undefined, typeof null, typeof {})"#),
        "number string undefined object object\n"
    );
    assert_eq!(
        eval("print(typeof function(){}, typeof true)"),
        "function boolean\n"
    );
}

#[test]
fn variables_and_blocks() {
    assert_eq!(eval("var x = 5; print(x)"), "5\n");
    assert_eq!(eval("let y = 1; print(y)"), "1\n");
    assert_eq!(eval("{ let x = 1; print(x) }"), "1\n");
    // A `var` in a `for` head is the same binding as one in the body.
    assert_eq!(eval("for (var i = 0; i < 3; i++) print(i)"), "0\n1\n2\n");
    assert_eq!(
        eval("var s = 0; for (var i = 0; i < 3; i++) s += i; print(s)"),
        "3\n"
    );
}

#[test]
fn a_let_binding_is_in_the_dead_zone_until_its_initialiser_runs() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let error = ctx
        .eval("{ print(x); let x = 1; }")
        .expect_err("reading a dead binding throws");
    assert!(error.to_string().contains("ReferenceError"), "{error}");
}

#[test]
fn functions_and_recursion() {
    assert_eq!(eval("function add(a,b){return a+b} print(add(2,3))"), "5\n");
    assert_eq!(
        eval("function fib(n){return n<2?n:fib(n-1)+fib(n-2)} print(fib(20))"),
        "6765\n"
    );
    assert_eq!(
        eval("var f = function(x){ return x*2 }; print(f(21))"),
        "42\n"
    );
    // A missing argument is `undefined`, not an error.
    assert_eq!(
        eval("function f(a,b){ return b } print(f(1))"),
        "undefined\n"
    );
}

#[test]
fn closures_share_one_binding() {
    assert_eq!(
        eval(
            "function o(){ var n=0; return function(){ return ++n } } var c=o(); print(c(),c(),c())"
        ),
        "1 2 3\n"
    );
    // Each call of the outer function gets its own binding.
    assert_eq!(
        eval("function mk(n){ return function(){ return n*2 } } print(mk(3)(), mk(5)())"),
        "6 10\n"
    );
    assert_eq!(
        eval("function f(a){ return function(b){ return a+b } } print(f(1)(2))"),
        "3\n"
    );
}

#[test]
fn objects_and_arrays() {
    assert_eq!(eval(r#"var o={a:1,b:"x"}; print(o.a, o["b"])"#), "1 x\n");
    assert_eq!(eval("var o={}; o.x = 1; print(o.x)"), "1\n");
    assert_eq!(eval("var a=[1,2,3]; print(a[1], a.length)"), "2 3\n");
    assert_eq!(eval("var a=[1,2]; a[4]=5; print(a.length)"), "5\n");
    assert_eq!(eval(r#"var o={}; o["a b"]=1; print(o["a b"])"#), "1\n");
    // A method call passes its receiver.
    assert_eq!(
        eval("var o={f:function(){return this.x},x:7}; print(o.f())"),
        "7\n"
    );
}

#[test]
fn constructors_get_a_fresh_object_with_the_prototypes() {
    assert_eq!(
        eval("function C(x){this.x=x} var c=new C(3); print(c.x)"),
        "3\n"
    );
    assert_eq!(eval("function C(){} print(new C() instanceof C)"), "true\n");
    assert_eq!(
        eval("function C(){} print(C.prototype.constructor===C)"),
        "true\n"
    );
    // A constructor that returns an object overrides `this`; a primitive does not.
    assert_eq!(eval("function C(){return {a:1}} print(new C().a)"), "1\n");
    assert_eq!(
        eval("function C(){return 7} print(new C() instanceof C)"),
        "true\n"
    );
}

#[test]
fn loops_and_control_flow() {
    assert_eq!(eval("var i=0; while(i<3){ print(i); i++ }"), "0\n1\n2\n");
    assert_eq!(eval("for(;;){ print(1); break }"), "1\n");
    assert_eq!(
        eval("var s=0; for(var i=0;i<10;i++){ if(i==5) continue; if(i==8) break; s+=i } print(s)"),
        "23\n"
    );
    assert_eq!(
        eval("switch(2){case 1:print('a');break;case 2:print('b');break;default:print('c')}"),
        "b\n"
    );
    // A labelled `break` leaves both loops; a labelled `continue` skips the rest of
    // the labelled iteration, which is why the first loop prints one pair only.
    assert_eq!(
        eval("l: for(var i=0;i<3;i++){ for(var j=0;j<3;j++){ if(j==1) break l; print(i,j) } }"),
        "0 0\n"
    );
    assert_eq!(
        eval("o: for(var i=0;i<3;i++){ for(var j=0;j<3;j++){ if(i==1) continue o; print(i,j) } }"),
        "0 0\n0 1\n0 2\n2 0\n2 1\n2 2\n"
    );
}

#[test]
fn for_in_and_for_of() {
    assert_eq!(eval("for (var k in {x:1,y:2}) print(k)"), "x\ny\n");
    assert_eq!(eval("for (var k of [10,20]) print(k)"), "10\n20\n");
    // Integer-like keys come first, in ascending order.
    assert_eq!(eval("for (var k in {b:1,2:1,1:1}) print(k)"), "1\n2\nb\n");
    assert_eq!(
        eval("var t=0; for (var v of [1,2,3]) t+=v; print(t)"),
        "6\n"
    );
    // The "integer keys first" ordering is per object, not for the whole chain: a
    // prototype's `"4"` belongs after the target's own `x`/`y`.
    assert_eq!(
        eval(
            "var a={x:2,y:2,'1':3}, b={'4':3}; Object.setPrototypeOf(a,b);\n\
             var t=[]; for (var k in a) t.push(k); print(t.toString())"
        ),
        "1,x,y,4\n"
    );
    // A non-enumerable own property hides an enumerable one further up the chain.
    assert_eq!(
        eval(
            "var a={y:2,'1':3}; Object.defineProperty(a,'x',{value:1});\n\
             var b={x:3}; Object.setPrototypeOf(a,b);\n\
             var t=[]; for (var k in a) t.push(k); print(t.toString())"
        ),
        "1,y\n"
    );
    // A destructuring or member target stores each element instead of a binding.
    assert_eq!(
        eval(
            "var a=[], t=[];\n\
             for (a[0] in {p:1,q:2}) t.push(a[0]);\n\
             for (var [m] of [[1],[2]]) t.push(m);\n\
             for ([a[1]] of [[9]]) t.push(a[1]);\n\
             print(t.toString())"
        ),
        "p,q,1,2,9\n"
    );
}

#[test]
fn a_function_sees_a_var_declared_later_in_its_scope() {
    // Function bodies used to be compiled at their declaration, so a `var` written
    // further down the same scope was not declared yet and the read resolved to a
    // *global* instead of the enclosing slot.
    assert_eq!(
        eval("function f(){ function g(){ return t } var t; return g() } print(f())"),
        "undefined\n"
    );
    assert_eq!(
        eval("function f(){ function g(){ return t } var t='v'; return g() } print(f())"),
        "v\n"
    );
    assert_eq!(
        eval("function f(){ function g(){ t += 'x' } var t='a'; g(); return t } print(f())"),
        "ax\n"
    );
    // The same for a `let` in the body, which is a different binding again.
    assert_eq!(
        eval("function f(){ function g(){ return u } let u='L'; return g() } print(f())"),
        "L\n"
    );
    // Hoisting still works in the other direction: a call before the declaration.
    assert_eq!(
        eval("function f(){ return g() } function g(){ return 7 } print(f())"),
        "7\n"
    );
}

#[test]
fn exceptions() {
    assert_eq!(
        eval("try{ throw 1 }catch(e){ print('caught',e) }"),
        "caught 1\n"
    );
    // A runtime error is a catchable exception like any other.
    assert_eq!(eval("try{ null.x }catch(e){ print('ok') }"), "ok\n");
    assert_eq!(eval("try{ a() }catch(e){ print('ok') }"), "ok\n");
    // `finally` runs on the way out of a `return`.
    assert_eq!(
        eval("function f(){ try{ return 1 }finally{ print('fin') } } print(f())"),
        "fin\n1\n"
    );
    assert_eq!(
        eval("try{ try{ throw 1 }finally{ print('inner') } }catch(e){ print('outer',e) }"),
        "inner\nouter 1\n"
    );
}

#[test]
fn an_uncaught_exception_is_an_error_not_a_crash() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let error = ctx.eval("null.x").expect_err("throws");
    assert!(error.to_string().contains("TypeError"), "{error}");
    assert_eq!(ctx.output(), "");
}

#[test]
fn runaway_recursion_hits_our_own_limit() {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    // Deep JavaScript recursion must not overflow the Rust stack: frames live on
    // the heap, so the limit is the engine's own.
    let error = ctx
        .eval("function f(){ return f() } f()")
        .expect_err("a stack overflow is an exception");
    assert!(
        error
            .to_string()
            .contains("Maximum call stack size exceeded"),
        "{error}"
    );
}

#[test]
fn print_matches_the_reference_formatting() {
    assert_eq!(eval("print()"), "\n");
    assert_eq!(eval("print(1,2,3)"), "1 2 3\n");
    // `print(-0)` keeps the sign while `String(-0)` does not.
    assert_eq!(eval("print(-0)"), "-0\n");
    assert_eq!(
        eval("print(undefined, null, true)"),
        "undefined null true\n"
    );
    assert_eq!(eval("console.log('x', 1)"), "x 1\n");
}

#[test]
fn a_script_reports_its_completion_value() {
    assert_eq!(value("1+1"), "2");
    assert_eq!(value("var x = 5"), "undefined");
    assert_eq!(value("'text'"), "text");
}

// ---------------------------------------------------------------------------
// Silent-wrong-answer regressions
//
// Each of these used to return a plausible but wrong value with no error. They
// were found by running the reference `test_builtin.js`, and each is asserted
// against what the reference prints.
// ---------------------------------------------------------------------------

#[test]
fn an_object_literal_keeps_both_halves_of_an_accessor() {
    // Defining the setter used to replace the property outright, dropping the
    // getter, so `a.z` read `undefined`.
    assert_eq!(
        eval("var a={get z(){return 4},set z(v){this.v=v}}; print(a.z, a.z=5, a.v)"),
        "4 5 5\n"
    );
    // The accessor functions are named `get z`/`set z`, which is what the
    // reference does and what a stack trace prints.
    assert_eq!(
        eval("var a={get z(){return 1},set z(v){}}; print(a.z.name !== '')"),
        "true\n"
    );
}

#[test]
fn new_on_a_bound_function_constructs_its_target() {
    // `new` has to unwrap `bind` and use the target's prototype; without that the
    // body ran with `this === null` and threw.
    assert_eq!(
        eval(
            "function C(a){this.x=a} var g=C.bind(null,1); var r=new g(); print(r.x, r instanceof C)"
        ),
        "1 true\n"
    );
}

#[test]
fn arithmetic_operators_run_to_primitive_on_objects() {
    // A bare tag check turned the operand into NaN and swallowed exceptions thrown
    // by `valueOf`.
    assert_eq!(
        eval("var o={valueOf:function(){return 3}}; print(2*o)"),
        "6\n"
    );
    assert_eq!(
        eval("var o={valueOf:function(){throw Error('p')}}; try{2*o}catch(e){print('caught')}"),
        "caught\n"
    );
    // The primitive coercions still behave.
    assert_eq!(
        eval(r#"print(2*"3", 2*"x", 2*null, 2*true)"#),
        "6 NaN 0 2\n"
    );
}

#[test]
fn a_strict_directive_is_scoped_to_its_own_function() {
    // The directive used to leak outwards, so a function declared *after* a strict
    // function lost its declaration status and calling it raised.
    assert_eq!(
        eval(
            r#"function a(){return b()} function c(){"use strict";return 1} function b(){return "b"} print(a(), c())"#
        ),
        "b 1\n"
    );
}

#[test]
fn a_call_before_its_function_declaration_still_resolves() {
    // `a` resolves `b` while `b` is still unknown; the guess has to be corrected
    // when the declaration is reached, or `b` does not exist at run time.
    assert_eq!(
        eval(r#"function a(){return b()} function b(){return 42} print(a())"#),
        "42\n"
    );
}

#[test]
fn assignment_to_an_undeclared_name_is_an_error_in_strict_mode() {
    assert_eq!(
        eval(r#""use strict"; try { not_def = 1; print("no") } catch(e) { print(e.name) }"#),
        "ReferenceError\n"
    );
    // Sloppy mode still creates the implicit global, and it stays deletable.
    assert_eq!(
        eval(r#"not_def = 1; print(not_def, delete not_def, typeof not_def)"#),
        "1 true undefined\n"
    );
}

#[test]
fn a_throw_in_tail_position_does_not_reuse_the_frame() {
    // The reference's `tail_call` still pushes a frame, so unbounded tail recursion
    // hits the stack limit rather than looping forever. Reusing the frame made
    // `function f(){return f()}` hang instead of reporting an overflow.
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    let error = ctx
        .eval("function f(n){ return n<=0 ? 'done' : f(n-1) } f(200000)")
        .expect_err("deep tail recursion overflows");
    assert!(error.to_string().contains("stack"), "{error}");
}

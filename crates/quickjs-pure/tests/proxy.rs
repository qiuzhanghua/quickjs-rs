//! `Proxy`, and the internal-method plumbing it hangs off.
//!
//! Two things are asserted here and they fail in different ways. The *traps* are the
//! easy part: a handler either fires or it does not. The **invariants** are the part that
//! matters, because a proxy that skips them does not fail loudly — it lets a script
//! rewrite a `Object.freeze`d object by standing in front of it. Every invariant case
//! below was checked against the reference first.

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

fn throws(source: &str) -> String {
    let rt = quickjs::Runtime::new();
    let mut ctx = quickjs::Context::new(&rt);
    match ctx.eval(source) {
        Ok(_) => panic!("{source:?} should have thrown"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn every_internal_method_has_a_trap() {
    // One handler, every trap, and the log is the assertion: the order the traps fire in
    // is part of the observable behaviour, and it is easy to get wrong by routing an
    // operation through the wrong internal method.
    assert_eq!(
        eval(
            r#"
var log = [];
var handler = {
  get: (t, k) => { log.push("get:" + String(k)); return t[k]; },
  set: (t, k, v) => { log.push("set:" + String(k)); t[k] = v; return true; },
  has: (t, k) => { log.push("has:" + String(k)); return k in t; },
  deleteProperty: (t, k) => { log.push("del:" + String(k)); return delete t[k]; },
  ownKeys: (t) => { log.push("ownKeys"); return Reflect.ownKeys(t); },
  getOwnPropertyDescriptor: (t, k) => {
    log.push("gopd:" + String(k));
    return Reflect.getOwnPropertyDescriptor(t, k);
  },
  defineProperty: (t, k, d) => { log.push("def:" + String(k)); return Reflect.defineProperty(t, k, d); },
  getPrototypeOf: (t) => { log.push("gpo"); return Reflect.getPrototypeOf(t); },
  setPrototypeOf: (t, p) => { log.push("spo"); return Reflect.setPrototypeOf(t, p); },
  isExtensible: (t) => { log.push("ext"); return Reflect.isExtensible(t); },
  preventExtensions: (t) => { log.push("prev"); return Reflect.preventExtensions(t); },
};
var p = new Proxy({a: 1}, handler);
p.a;
p.b = 2;
"a" in p;
delete p.b;
Object.keys(p);
Object.getOwnPropertyDescriptor(p, "a");
Object.defineProperty(p, "c", {value: 3, enumerable: true});
Object.getPrototypeOf(p);
Object.setPrototypeOf(p, null);
Object.isExtensible(p);
Object.preventExtensions(p);
print(log.join(","));
"#
        ),
        "get:a,set:b,has:a,del:b,ownKeys,gopd:a,gopd:a,def:c,gpo,spo,ext,prev\n"
    );
}

#[test]
fn a_proxy_of_a_function_is_callable_and_constructible() {
    // `typeof` and `Array.isArray` look through the proxy; a call goes to `apply` and
    // `new` goes to `construct`, each receiving the arguments as one array.
    assert_eq!(
        eval(
            r#"
function add(a, b) { return this.base + a + b }
var called = new Proxy(add, {
  apply: (t, th, args) => { print("apply", args.join("-")); return "from trap" },
});
print(called.call({base: 0}, 1, 2), typeof called, called.length);
var C = function (x) { this.x = x };
var built = new Proxy(C, {construct: (t, args) => ({made: args[0]})});
print(new built(7).made, new (new Proxy(C, {}))(5).x);
print(typeof new Proxy(function () {}, {}) === "function");
print(Array.isArray(new Proxy([], {})), Array.isArray(new Proxy({}, {})));
"#
        ),
        "apply 1-2\nfrom trap function 2\n7 5\ntrue\ntrue false\n"
    );
}

#[test]
fn a_revoked_proxy_refuses_everything() {
    // After `revoke()` the slots are gone, so there is nothing to forward to and no trap
    // to ask: every operation is a `TypeError`, not just the ones that read properties.
    assert_eq!(
        eval(
            r#"
var r = Proxy.revocable({a: 1}, {});
print(r.proxy.a);
r.revoke();
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name } }
print(kind(() => r.proxy.a));
print(kind(() => { r.proxy.a = 2 }));
print(kind(() => "a" in r.proxy));
print(kind(() => Object.keys(r.proxy)));
print(kind(() => delete r.proxy.a));
print(kind(() => Object.getPrototypeOf(r.proxy)));
print(r.revoke());
"#
        ),
        "1\nTypeError\nTypeError\nTypeError\nTypeError\nTypeError\nTypeError\nundefined\n"
    );
}

#[test]
fn a_fixed_property_cannot_be_hidden_or_rewritten() {
    // A non-configurable property is the engine's promise, and no handler may break it:
    // not by reporting another value, not by denying it exists, not by deleting it, and
    // not by claiming to have removed it.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name } }
var fixed = {};
Object.defineProperty(fixed, "x", {value: 1, writable: false, configurable: false});
print(new Proxy(fixed, {get: () => 1}).x);
print(kind(() => new Proxy(fixed, {get: () => 2}).x));
print(kind(() => "x" in new Proxy(fixed, {has: () => false})));
print(kind(() => Object.keys(new Proxy(fixed, {ownKeys: () => []}))));
print(kind(() => delete new Proxy(fixed, {deleteProperty: () => true}).x));
print(kind(() => Object.getOwnPropertyDescriptor(
  new Proxy(fixed, {getOwnPropertyDescriptor: () => undefined}), "x")));
print(kind(() => Object.getOwnPropertyDescriptor(
  new Proxy(fixed, {getOwnPropertyDescriptor: () => ({value: 1, configurable: true})}), "x")));
print(kind(() => { "use strict"; new Proxy(fixed, {set: () => true}).x = 2 }));
"#
        ),
        "1\nTypeError\nTypeError\nTypeError\nTypeError\nTypeError\nTypeError\nTypeError\n"
    );
}

#[test]
fn a_non_extensible_target_pins_the_answers_down() {
    // Once the target is closed, `isExtensible` may not lie, `ownKeys` must be exactly the
    // target's keys, the prototype cannot move, and no new property may appear.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name } }
var closed = {a: 1};
Object.preventExtensions(closed);
print(kind(() => Object.isExtensible(new Proxy(closed, {isExtensible: () => true}))));
print(kind(() => Object.preventExtensions(new Proxy({}, {preventExtensions: () => true}))));
print(kind(() => Object.keys(new Proxy(closed, {ownKeys: () => ["a", "b"]}))));
print(kind(() => Object.keys(new Proxy(closed, {ownKeys: () => []}))));
print(kind(() => Object.getPrototypeOf(new Proxy(closed, {getPrototypeOf: () => null}))));
print(Object.keys(new Proxy(closed, {})));
print(kind(() => Object.keys(new Proxy({}, {ownKeys: () => ["a", "a"]}))));
"#
        ),
        "TypeError\nTypeError\nTypeError\nTypeError\nTypeError\n[ \"a\" ]\nTypeError\n"
    );
}

#[test]
fn a_bad_handler_or_target_is_refused_at_construction() {
    assert_eq!(
        throws("new Proxy(1, {})"),
        "TypeError: proxy target must be an object"
    );
    assert_eq!(
        throws("new Proxy({}, null)"),
        "TypeError: proxy handler must be an object"
    );
    // A trap that is present but not callable is refused when it fires, not treated as
    // "no trap" — silently falling back would hide the bug in the handler.
    assert!(throws("new Proxy({}, {get: 1}).a").contains("not a function"));
}

#[test]
fn for_in_re_reads_each_descriptor_as_it_goes() {
    // `for…in` over a proxy is not "list the keys once and filter": the spec asks
    // `[[GetOwnProperty]]` again for each key as the loop reaches it. A handler that
    // stops reporting a key partway through therefore drops it from the rest of the
    // iteration, which is exactly what this test's handler does.
    assert_eq!(
        eval(
            r#"
var removed = "";
var proxy = new Proxy({}, {
  ownKeys: () => ["a", "b", "c"],
  getOwnPropertyDescriptor: (t, key) => {
    if (removed !== "" && key === removed) return undefined;
    return {enumerable: true, configurable: true, value: 1};
  },
});
var seen = "";
for (var key in proxy) {
  seen += key;
  if (key === "a") removed = "b";
}
print(seen);
"#
        ),
        "ac\n"
    );
}

#[test]
fn a_proxy_reports_the_own_keys_it_is_given() {
    // `Object.keys`, `Object.getOwnPropertyNames`, `Object.values`, `Object.entries` and
    // `JSON.stringify` all start from `[[OwnPropertyKeys]]`, so all of them have to ask the
    // handler. Missing any one of them is silent: it answers as if the proxy were empty.
    assert_eq!(
        eval(
            r#"
var proxy = new Proxy({}, {
  ownKeys: () => ["a", "b"],
  getOwnPropertyDescriptor: (t, k) => ({enumerable: true, configurable: true, value: k}),
  get: (t, k) => k === "a" ? 1 : 2,
});
print(Object.keys(proxy).join());
print(Object.getOwnPropertyNames(proxy).join());
print(Object.values(proxy).join());
print(JSON.stringify(Object.entries(proxy)));
"#
        ),
        "a,b\na,b\n1,2\n[[\"a\",1],[\"b\",2]]\n"
    );
}

#[test]
fn a_proxy_forwards_to_its_target_when_a_trap_is_missing() {
    // The empty handler is the important case: an empty `Proxy` must behave exactly like
    // its target, which means every internal method has to forward rather than answer
    // from the proxy's own (empty) property table.
    assert_eq!(
        eval(
            r#"
var target = {a: 1, b: 2};
var bare = new Proxy(target, {});
print(bare.a, "a" in bare, Object.keys(bare).join());
bare.c = 3;
print(target.c);
print(delete bare.a, "a" in bare, "a" in target);
print(Object.getPrototypeOf(bare) === Object.prototype);
print(Array.isArray(new Proxy([1, 2], {})), new Proxy([1, 2], {}).length);
"#
        ),
        "1 true a,b\n3\ntrue false false\ntrue\ntrue 2\n"
    );
}

#[test]
fn a_far_jump_is_not_truncated_into_the_middle_of_the_bytecode() {
    // A jump further than 127 bytes has to keep a wide encoding. Getting this wrong is
    // invisible until a loop happens to straddle the boundary: the truncated `goto8`
    // lands outside the function, the VM treats that as a return, and the script simply
    // stops — no error, no output past the point of truncation.
    //
    // The numbers below are the smallest shape found that straddles it: a nested loop
    // whose body holds four closures, each textually containing `eval`.
    assert_eq!(
        eval(
            r#"
function s(f) { try { var v = f(); return String(v) } catch (e) { return "E" } }
var bases = ["({a:1})"];
for (var i = 0; i < 2; i++) {
  for (var j = 0; j < bases.length; j++) {
    var mk = bases[j];
    print("p", i, j);
    print(" a:", s(() => eval(mk).a));
    print(" b:", s(() => { eval(mk).n = 9; return 1 }));
    print(" c:", s(() => eval(mk).n));
    print(" d:", s(() => "a" in eval(mk)));
  }
}
print("END");
"#
        ),
        "p 0 0\n a: 1\n b: 1\n c: undefined\n d: true\np 1 0\n a: 1\n b: 1\n c: undefined\n d: true\nEND\n"
    );
}

#[test]
fn an_arrays_length_is_an_own_key_behind_the_property_table() {
    // `length` is own, non-enumerable and non-configurable, and it lives in the element
    // vector rather than the property table. Every view of "own keys" has to synthesize
    // it, or `Reflect.ownKeys` loses it and a proxy invariant stops covering it.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { f(); return "no throw" } catch (e) { return e.constructor.name } }
print(Reflect.ownKeys([1, 2]).join());
print(Object.getOwnPropertyNames([1, 2]).join());
print(Object.keys([1, 2]).join());
print(Object.getOwnPropertyDescriptor([1, 2], "length").enumerable);
var seen = [];
for (var k in [7, 8]) seen.push(k);
print(seen.join());
print(Object.keys(new Proxy([1, 2], {})));
print(kind(() => Object.keys(new Proxy([1, 2], {ownKeys: () => ["0", "1"]}))));
print(kind(() => Object.keys(new Proxy([1, 2], {
  getOwnPropertyDescriptor: (t, k) => ({value: 1, enumerable: true, configurable: true}),
}))));
"#
        ),
        "0,1,length\n0,1,length\n0,1\nfalse\n0,1\n[ \"0\", \"1\" ]\nTypeError\nTypeError\n"
    );
}

#[test]
fn a_builtin_that_walks_own_keys_asks_the_handler() {
    // Every entry point that enumerates or reads an object's properties has to go through
    // the internal methods. Missing one is silent in the worst way: it answers as if the
    // proxy were an empty object, so the value looks plausible instead of wrong.
    assert_eq!(
        eval(
            r#"
function kind(f) { try { return String(f()) } catch (e) { return e.constructor.name } }
print(kind(() => JSON.stringify(new Proxy({j: 1}, {}))));
print(kind(() => Object.assign({}, new Proxy({z: 9}, {}))).length > 0);
print(kind(() => JSON.stringify({...new Proxy({s: 2}, {})})));
print(kind(() => Object.prototype.hasOwnProperty.call(new Proxy({h: 1}, {}), "h")));
print(kind(() => Object.keys(new Proxy({k: 1}, {ownKeys: () => ["k"]}))));
print(kind(() => JSON.stringify(new Proxy({}, {
  ownKeys: () => ["hidden"],
  getOwnPropertyDescriptor: () => ({enumerable: false}),
}))));
"#
        ),
        "{\"j\":1}\ntrue\n{\"s\":2}\ntrue\nk\n{}\n"
    );
}

#[test]
fn an_array_method_reading_a_proxy_goes_through_the_get_trap() {
    // A proxy of an array has no element vector of its own, so a method that reads one
    // directly saw an empty array. All of them share one `elements()` helper, which is why
    // one fix covers `map`, `filter`, `reduce`, `join`, `slice`, `concat` and `flat`.
    assert_eq!(
        eval(
            r#"
var p = new Proxy([10, 20, 30], {get: (t, k) => k === "0" ? 99 : t[k]});
print(p.map(x => x * 2).join());
print(p.join("-"));
print(p.slice(1).join());
print([].concat(p).join());
print(p.length, p[0]);
var seen = [];
for (var v of p) seen.push(v);
print(seen.join());
print([...p].join());
print(Array.isArray(p));
"#
        ),
        "198,40,60\n99-20-30\n20,30\n99,20,30\n3 99\n99,20,30\n99,20,30\ntrue\n"
    );
}

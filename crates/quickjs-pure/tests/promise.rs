//! `Promise` and the async machinery.
//!
//! The tests that matter most here are the ones about *ordering*: a promise's handlers
//! run as microtasks, so `p.then(f); g()` calls `g` first, and a chain resolves
//! left-to-right. An implementation that runs handlers inline gets every value right and
//! every order wrong, which is invisible until two of them interleave.

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
        Ok(_) => panic!("{source:?} should have failed"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn handlers_run_as_microtasks_not_inline() {
    // Every `then` is deferred, even on an already-settled promise, so the synchronous
    // tail of the script runs first. A chain keeps its order because each link queues the
    // next only when it runs.
    assert_eq!(
        eval(
            r#"
var log = [];
Promise.resolve(1)
  .then(v => { log.push("a" + v); return v + 1 })
  .then(v => { log.push("b" + v) });
Promise.resolve(2).then(v => log.push("c" + v));
log.push("sync");
Promise.resolve().then(() => print(log.join(",")));
"#
        ),
        "sync,a1,c2\n"
    );
}

#[test]
fn async_functions_run_to_the_first_await_synchronously() {
    // `async function f(){ log.push("a"); await 0 }` records `a` before its caller's next
    // statement, so the body is *not* deferred as a whole — only what follows an `await`.
    assert_eq!(
        eval(
            r#"
var log = [];
async function f() {
  log.push("body-start");
  await Promise.resolve();
  log.push("after-await");
  return 1;
}
f().then(v => log.push("resolved" + v));
log.push("after-call");
Promise.resolve().then(() => {}).then(() => print(log.join(",")));
"#
        ),
        "body-start,after-call,after-await,resolved1\n"
    );
}

#[test]
fn an_awaited_rejection_that_is_caught_fulfils_the_caller() {
    // The body is resumed by a `throw`, catches it, and *returns* — which is a
    // fulfilment. Deciding the outcome from how the body was resumed rather than how it
    // finished turned this into a rejection carrying the return value.
    assert_eq!(
        eval(
            r#"
async function boom() { throw new Error("x") }
async function caught() {
  try { await boom() } catch (e) { return "caught:" + e.message }
}
async function rethrown() { await boom() }
caught().then(v => print(v));
rethrown().catch(e => print("rejected:" + e.message));
"#
        ),
        "caught:x\nrejected:x\n"
    );
}

#[test]
fn the_combinators_keep_input_order_and_their_own_failure_rules() {
    assert_eq!(
        eval(
            r#"
Promise.all([3, Promise.resolve(1), 2]).then(v => print("all:" + v.join()));
Promise.race([Promise.resolve("first"), Promise.resolve("second")])
  .then(v => print("race:" + v));
Promise.allSettled([1, Promise.reject(2)])
  .then(v => print("settled:" + v.map(x => x.status).join()));
Promise.all([Promise.reject("bad"), 1]).catch(e => print("all-rejects:" + e));
Promise.any([Promise.reject("a"), Promise.resolve("ok")])
  .then(v => print("any:" + v));
"#
        ),
        "all:3,1,2\nrace:first\nsettled:fulfilled,rejected\nall-rejects:bad\nany:ok\n"
    );
}

#[test]
fn finally_passes_the_original_outcome_through() {
    // `finally` is not `then(f, f)`: the callback's return value is discarded and the
    // original value or reason continues. It does wait when the callback returns a
    // thenable, which is the one thing the callback can influence.
    assert_eq!(
        eval(
            r#"
Promise.resolve(1).finally(() => 99).then(v => print("value:" + v));
Promise.reject(new Error("r")).finally(() => 1).catch(e => print("reason:" + e.message));
var order = [];
Promise.resolve().finally(() => Promise.resolve().then(() => order.push("slow")))
  .then(() => order.push("after"));
// Five throwaway ticks, so `order` is read after the finally chain has settled.
Promise.resolve().then(()=>{}).then(()=>{}).then(()=>{}).then(()=>{}).then(()=>{})
  .then(() => print(order.join(",")));
"#
        ),
        "value:1\nreason:r\nslow,after\n"
    );
}

#[test]
fn an_unhandled_rejection_is_fatal_after_the_script() {
    // The reference reports it and exits non-zero; the message is part of the contract
    // because it is what a user sees when a rejection escapes.
    let text = throws("Promise.reject(new Error('nobody'));");
    assert!(
        text.starts_with("Possibly unhandled promise rejection: Error: nobody"),
        "unexpected text: {text}"
    );
    // A rejection that *is* handled must stay quiet, including when the handler is
    // attached after the fact.
    assert_eq!(
        eval(
            r#"
var p = Promise.reject(new Error("late"));
Promise.resolve().then(() => p.catch(e => print("handled:" + e.message)));
"#
        ),
        "handled:late\n"
    );
}

#[test]
fn for_await_accepts_both_kinds_of_iterable() {
    assert_eq!(
        eval(
            r#"
var log = [];
async function overSync() {
  for await (var x of [1, 2]) log.push("s" + x);
}
async function overAsync() {
  var i = 0;
  var source = { [Symbol.asyncIterator]() { return this }, next() {
    return Promise.resolve(i < 2 ? { value: "a" + i++, done: false } : { done: true });
  } };
  for await (var x of source) log.push(x);
}
overSync().then(overAsync).then(() => print(log.join(",")));
"#
        ),
        "s1,s2,a0,a1\n"
    );
}

#[test]
fn an_async_generator_answers_promises_and_awaits_its_yields() {
    assert_eq!(
        eval(
            r#"
function q(v) { return v === undefined ? "undefined" : JSON.stringify(v) }
async function* gen() {
  yield 1;
  var x = await Promise.resolve(2);
  yield x;
  yield Promise.resolve(3);
  return 4;
}
async function main() {
  var it = gen();
  print(q(await it.next()));
  print(q(await it.next()));
  print(q(await it.next()));
  print(q(await it.next()));
  var seen = [];
  for await (var v of gen()) seen.push(v);
  print("loop:" + seen.join());
}
main();
"#
        ),
        "{\"value\":1,\"done\":false}\n{\"value\":2,\"done\":false}\n{\"value\":3,\"done\":false}\n{\"value\":4,\"done\":true}\nloop:1,2,3\n"
    );
}

#[test]
fn a_promise_reports_its_own_tag_and_constructor() {
    assert_eq!(
        eval(
            r#"
print(typeof Promise, typeof Promise.resolve);
print(Object.prototype.toString.call(Promise.resolve()));
print(Promise.resolve(1) instanceof Promise);
print(typeof Promise.try, typeof Promise.withResolvers);
Promise.try(() => 1).then(v => print("try:" + v));
Promise.try(() => { throw new Error("t") }).catch(e => print("try-throws:" + e.message));
var w = Promise.withResolvers();
w.promise.then(v => print("withResolvers:" + v));
w.resolve(7);
"#
        ),
        "function function\n[object Promise]\ntrue\nfunction function\ntry:1\ntry-throws:t\nwithResolvers:7\n"
    );
}

#[test]
fn an_async_body_adopts_a_returned_thenable() {
    // `return promise` in an async body *resolves* the caller's promise with it, so the
    // answer is a promise of the inner value — not a promise of a promise. Fulfilling
    // directly instead of resolving is the difference, and it is only observable here:
    // `then` would hand the caller the inner promise object.
    assert_eq!(
        eval(
            r#"
async function resolved() { return Promise.resolve(1) }
async function rejected() { return Promise.reject(new Error("inner")) }
async function thenable() { return { then(res) { res(9) } } }
resolved().then(v => print("value:" + v));
rejected().catch(e => print("reason:" + e.message));
thenable().then(v => print("thenable:" + v));
"#
        ),
        "thenable:9\nvalue:1\nreason:inner\n"
    );
}

#[test]
fn an_async_generator_awaits_its_return_value_too() {
    // A `yield` and the completing `return` both settle before the method answers, so the
    // value a `next()` reports is the resolved one.
    assert_eq!(
        eval(
            r#"
async function* gen() { return Promise.resolve(7) }
gen().next().then(r => print(JSON.stringify(r)));
"#
        ),
        "{\"value\":7,\"done\":true}\n"
    );
}

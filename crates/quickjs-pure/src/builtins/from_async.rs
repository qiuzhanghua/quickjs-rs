//! `Array.fromAsync` (ES2024 §23.1.2.2), as a built-in *async function*.
//!
//! The algorithm itself is written in JavaScript, in [`SOURCE`], and compiled at install
//! time into an ordinary function object. That is unusual for this crate — every other
//! builtin is a Rust `NativeFn` — but `fromAsync` is specified as an async function, and
//! the difference is observable:
//!
//! * It must answer a *promise* even when it fails, including when `mapfn` is not
//!   callable: `Array.fromAsync([], null)` rejects rather than throwing to its caller.
//! * It has two `await` points, and the spec's `IfAbruptCloseAsyncIterator` steps mean
//!   the iterator is closed *after* the mapped value rejects, not before. Six tests pin
//!   down exactly how many times a thenable's `then` runs (`…-awaits-once.js`).
//!
//! A native function would have to hand-roll that state machine: thread a promise
//! capability through every step of a loop that parks between iterations, with the
//! `return()` call itself being awaited. The engine already has exactly this machinery —
//! an async function's body runs as a coroutine that parks on `await` — so writing the
//! loop as an async function reuses it instead of duplicating it. The Rust side is then
//! only responsible for compiling the source, checking that it produced the function it
//! expected, and installing it with the right attributes.
//!
//! What is *not* reimplemented in JavaScript: `Symbol.asyncIterator`, `Symbol.iterator`,
//! promises, and `Reflect.construct`. The script is a client of the engine's own
//! intrinsics, so a change to them cannot silently diverge from a copy kept here. It
//! also never reads a global by name except through the intrinsics it captures into
//! module-private consts at compile time, so a script that replaces `Array` or
//! `Promise` afterwards cannot change `fromAsync`'s behaviour.

use crate::interp::{Interp, JsResult, Signal};
use crate::object::Property;
use crate::value::Value;

/// The implementation, as the body of an async function expression.
///
/// Written as `async function fromAsync(...)` so the compiled function has the right
/// name and so the source reads like the spec. Every step below cites the spec step it
/// implements; the numbered list in §23.1.2.2 is one long `Repeat`.
const SOURCE: &str = r#"
(function () {
  "use strict";

  var $asyncIterator = Symbol.asyncIterator;
  var $iterator = Symbol.iterator;
  var $Array = Array;
  var $Promise = Promise;
  var $Reflect = Reflect;
  var $Object = Object;
  var $TypeError = TypeError;

  // IsConstructor. `Reflect.construct` with a dummy target and this as `newTarget`
  // throws unless it can be constructed, which is the only way to ask from script.
  var isConstructor = function (value) {
    if (typeof value !== "function") return false;
    try {
      $Reflect.construct(function () {}, [], value);
      return true;
    } catch (error) {
      return false;
    }
  };

  // CreateDataPropertyOrThrow. `[[DefineOwnProperty]]` rather than assignment, so a
  // `this`-value whose elements have a setter still gets a plain writable property and
  // a non-writable one makes the call reject.
  var createDataProperty = function (target, key, value) {
    $Object.defineProperty(target, key, {
      __proto__: null,
      value: value,
      writable: true,
      enumerable: true,
      configurable: true,
    });
  };

  // CreateAsyncFromSyncIterator. Each `next`/`return` result's `value` is awaited, so a
  // synchronous iterator that yields promises produces them settled.
  //
  // `closeOnRejection` is true for `next` and false for `return`
  // (AsyncFromSyncIteratorContinuation's fourth parameter): when the yielded value
  // rejects, `next` has to *close the underlying synchronous iterator first*, so a
  // generator input runs its `finally`. Without that, `Array.fromAsync(gen())` where a
  // yielded thenable rejects left `finally` uncounted — which
  // `sync-iterable-with-rejecting-thenable-closes` checks.
  var createAsyncFromSyncIterator = function (syncIterator) {
    var closeSync = function (completion) {
      var returnMethod;
      try {
        returnMethod = syncIterator.return;
      } catch (error) {
        if (completion.threw) throw completion.value;
        throw error;
      }
      if (returnMethod === undefined || returnMethod === null) {
        if (completion.threw) throw completion.value;
        throw new $TypeError("iterator has no return method");
      }
      var inner;
      try {
        inner = returnMethod.call(syncIterator);
      } catch (error) {
        if (completion.threw) throw completion.value;
        throw error;
      }
      if ($Object(inner) !== inner) {
        if (completion.threw) throw completion.value;
        throw new $TypeError("iterator result is not an object");
      }
      if (completion.threw) throw completion.value;
    };
    var wrap = function (syncMethod, closeOnRejection) {
      return function () {
        var args = arguments;
        return new $Promise(function (resolve, reject) {
          var result;
          try {
            result = syncMethod.apply(syncIterator, args);
          } catch (error) {
            reject(error);
            return;
          }
          if ($Object(result) !== result) {
            reject(new $TypeError("iterator result is not an object"));
            return;
          }
          var done, value;
          try {
            done = !!result.done;
            value = result.value;
          } catch (error) {
            reject(error);
            return;
          }
          // `PromiseResolve` then a step: the value is awaited exactly once.
          $Promise.resolve(value).then(function (settled) {
            resolve({ __proto__: null, value: settled, done: done });
          }, function (reason) {
            if (!closeOnRejection) {
              reject(reason);
              return;
            }
            // Closing the sync iterator is synchronous and its own failure is
            // discarded: the original rejection is what the caller must see.
            try {
              closeSync({ __proto__: null, threw: true, value: reason });
            } catch (ignored) {
              // Swallowed on purpose — see above.
            }
            reject(reason);
          });
        });
      };
    };
    return {
      __proto__: null,
      next: wrap(syncIterator.next, true),
      return: wrap(
        typeof syncIterator.return === "function"
          ? syncIterator.return
          : function () {
              return { __proto__: null, value: undefined, done: true };
            },
        false,
      ),
    };
  };

  // AsyncIteratorClose(iteratorRecord, completion) — §7.4.15.
  //
  // `completion` is `{ threw, value }`. When the caller is already unwinding, a failure
  // from `return` must *not* replace the original error, and the original error is
  // re-thrown rather than returned: returning it would settle the promise as fulfilled.
  var closeAsync = async function (iterator, completion) {
    var returnMethod;
    try {
      returnMethod = iterator.return;
    } catch (error) {
      if (completion.threw) throw completion.value;
      throw error;
    }
    if (returnMethod === undefined || returnMethod === null) {
      if (completion.threw) throw completion.value;
      throw new $TypeError("iterator has no return method");
    }
    var inner;
    try {
      inner = await returnMethod.call(iterator);
    } catch (error) {
      if (completion.threw) throw completion.value;
      throw error;
    }
    if ($Object(inner) !== inner) {
      if (completion.threw) throw completion.value;
      throw new $TypeError("iterator result is not an object");
    }
    if (completion.threw) throw completion.value;
    return undefined;
  };

  // LengthOfArrayLike — §7.3.18, i.e. `ToLength(Get(obj, "length"))`.
  //
  // A length of `2^53` is *not* an allocation, so it must not be looped over: the
  // `asyncitems-arraylike-too-long` test uses exactly that to check the bound. `ToLength`
  // clamps and floors, and `NaN` becomes 0.
  var $floor = Math.floor;
  var lengthOfArrayLike = function (object) {
    var number = +object.length;
    if (number !== number || number <= 0) return 0;
    if (number === Infinity) return 9007199254740991;
    var floored = $floor(number);
    return floored > 9007199254740991 ? 9007199254740991 : floored;
  };

  var fromAsync = async function fromAsync(items, mapper, thisArg) {
    var ctor = this;

    // 1-2. `mapping`, and the callable check. A non-callable `mapfn` rejects here, before
    // anything is read from `items`.
    var mapping = false;
    if (mapper !== undefined) {
      if (typeof mapper !== "function") {
        throw new $TypeError("mapfn is not a function");
      }
      mapping = true;
    }

    // 3. Both protocols are looked up on `ToObject(items)`; `null` and `undefined` are
    // rejected rather than boxed, which is what `GetMethod` on them amounts to.
    if (items === undefined || items === null) {
      throw new $TypeError("items is null or undefined");
    }
    var object = $Object(items);
    var iterator;
    var usingAsyncIterator = object[$asyncIterator];
    if (usingAsyncIterator === undefined || usingAsyncIterator === null) {
      // 4-5. The sync fallback, wrapped so its values can be awaited.
      var usingSyncIterator = object[$iterator];
      if (usingSyncIterator !== undefined && usingSyncIterator !== null) {
        if (typeof usingSyncIterator !== "function") {
          throw new $TypeError("@@iterator is not callable");
        }
        var syncIterator = usingSyncIterator.call(object);
        if ($Object(syncIterator) !== syncIterator) {
          throw new $TypeError("iterator is not an object");
        }
        iterator = createAsyncFromSyncIterator(syncIterator);
      }
    } else {
      if (typeof usingAsyncIterator !== "function") {
        throw new $TypeError("@@asyncIterator is not callable");
      }
      iterator = usingAsyncIterator.call(object);
      if ($Object(iterator) !== iterator) {
        throw new $TypeError("iterator is not an object");
      }
    }

    if (iterator !== undefined) {
      // 9. `Construct(ctor)` happens *after* the iterator is obtained and before the
      // first step, so a constructor that throws rejects with that error and no value is
      // pulled from the iterator.
      var array = isConstructor(ctor) ? new ctor() : new $Array();
      var k = 0;
      for (;;) {
        // 9.a. The `2^53-1` guard closes the iterator before rejecting.
        if (k >= 9007199254740991) {
          return await closeAsync(iterator, {
            __proto__: null,
            threw: true,
            value: new $TypeError("array is too long"),
          });
        }
        var propertyKey = String(k);
        // 9.c-d. `Call` then `Await`: the result is awaited whether or not it is a
        // promise, which is what makes a thenable's `then` run once per iteration.
        var nextResult = await iterator.next();
        if ($Object(nextResult) !== nextResult) {
          throw new $TypeError("iterator result is not an object");
        }
        if (nextResult.done) {
          // 9.g. The length is set rather than grown element by element, so a `this`
          // constructor with a length setter sees exactly one assignment.
          array.length = k;
          return array;
        }
        var nextValue = nextResult.value;
        var mappedValue;
        if (mapping) {
          // 9.i. The call and the await are separate steps, each able to close the
          // iterator: a mapper that throws synchronously and one that rejects must both
          // end up in `IfAbruptCloseAsyncIterator`.
          try {
            mappedValue = mapper.call(thisArg, nextValue, k);
          } catch (error) {
            return await closeAsync(iterator, {
              __proto__: null,
              threw: true,
              value: error,
            });
          }
          try {
            mappedValue = await mappedValue;
          } catch (error) {
            return await closeAsync(iterator, {
              __proto__: null,
              threw: true,
              value: error,
            });
          }
        } else {
          mappedValue = nextValue;
        }
        // 9.k. A `this`-value that refuses the property rejects and closes the iterator.
        try {
          createDataProperty(array, propertyKey, mappedValue);
        } catch (error) {
          return await closeAsync(iterator, {
            __proto__: null,
            threw: true,
            value: error,
          });
        }
        k++;
      }
    }

    // 10. Not iterable, so an array-like: `length` is read once and each element awaited.
    var arrayLike = $Object(items);
    var length = lengthOfArrayLike(arrayLike);
    // `ArrayCreate(length)` is `new Array(length)`, not `[]` plus a length: the elements
    // it is about to define must not be shadowed by an inherited one, and the length has
    // to be right before the first `defineProperty`.
    var constructible = isConstructor(ctor);
    var result = constructible ? new ctor(length) : new $Array(length);
    for (var index = 0; index < length; index++) {
      var key = String(index);
      var element = await arrayLike[key];
      var value;
      if (mapping) {
        value = await mapper.call(thisArg, element, index);
      } else {
        value = element;
      }
      createDataProperty(result, key, value);
    }
    result.length = length;
    return result;
  };

  return fromAsync;
})()
"#;

/// Compile [`SOURCE`] and return the `fromAsync` function object it evaluates to.
///
/// The script is an IIFE that hands the function back, so nothing is left on the global
/// object and no name can collide with a script's own. Compiling it here means a syntax
/// error would be this crate's bug, not a user's, so it is reported as an internal
/// failure rather than as a JavaScript exception.
fn build(interp: &mut Interp<'_>) -> JsResult<Value> {
    let value = interp.eval_builtin_source(SOURCE)?;
    let Value::Object(function) = &value else {
        return Err(Signal::Throw(interp.make_error(
            "InternalError",
            "fromAsync source did not produce a function",
        )));
    };
    if !function.is_callable() {
        return Err(Signal::Throw(
            interp.make_error("InternalError", "fromAsync source is not callable"),
        ));
    }
    Ok(value)
}

/// Install `Array.fromAsync`.
///
/// The `name`/`length` a script observes come from the function itself: the source
/// declares `async function fromAsync(items, mapper, thisArg)`, which gives the name and
/// a length of 1. `length` is not writable and `name` is not either, matching every
/// other builtin, so both are pinned here rather than left to the declaration.
pub fn install(interp: &mut Interp<'_>) {
    let global = interp.realm.global.clone();
    let Some(Value::Object(constructor)) = global.own_property("Array").and_then(|p| p.value)
    else {
        return;
    };
    let function = match build(interp) {
        Ok(value) => value,
        // A failure here is an engine bug; leaving `Array.fromAsync` undefined makes it
        // visible (`typeof Array.fromAsync === "undefined"`) instead of fabricating a
        // half-working method.
        Err(Signal::Throw(reason)) => {
            let text = interp.describe_exception(&reason);
            panic!("fromAsync source failed: {text}");
        }
        Err(_) => return,
    };
    let Value::Object(function) = &function else {
        return;
    };
    let constructor = constructor.clone();
    // The spec makes every builtin function inherit from `Function.prototype`, and says
    // so explicitly for one that is not a constructor. A function created by `async
    // function` inherits from `%AsyncFunction.prototype%` instead, which is observable:
    // `Object.getPrototypeOf(Array.fromAsync) === Function.prototype` has to hold. The
    // prototype is replaced here rather than by building the function another way, and
    // the missing `prototype` property is already right — an async function has none.
    function.set_proto(Some(interp.realm.function_proto.clone()));
    // `length` is not writable and not enumerable, but it *is* configurable, like every
    // other builtin's. The declaration already gives length 1; this pins the attributes.
    function.define(
        "length",
        Property {
            value: Some(Value::Int(1)),
            writable: false,
            enumerable: false,
            configurable: true,
            getter: None,
            setter: None,
        },
    );
    constructor.define(
        "fromAsync",
        Property::with_attributes(Value::Object(function.clone()), true, false),
    );
}

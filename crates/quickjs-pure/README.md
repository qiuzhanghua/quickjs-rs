# quickjs-pure

An embeddable JavaScript engine written in **pure Rust**, aiming at behavioural
equivalence with [QuickJS](https://github.com/bellard/quickjs).

No C compiler, no `bindgen`, no FFI: the whole pipeline — lexer, parser, bytecode
compiler, VM, garbage collector — is Rust. That makes it buildable for
`wasm32-unknown-unknown` and for a target without a C toolchain, which the original
cannot do.

```toml
[dependencies]
quickjs-pure = "0.4"
```

The package is `quickjs-pure` but the library is `quickjs` (the plain name is taken on
crates.io by a 2019 FFI wrapper), so the import is:

```rust
use quickjs::{Context, Runtime};

let runtime = Runtime::new();
let mut context = Context::new(&runtime);
context.eval("print([1, 2, 3].map(x => x * 2).join('-'));")?;
assert_eq!(context.output(), "2-4-6\n");
# Ok::<(), quickjs::Error>(())
```

This example lives in `examples/embed.rs` and is run by the test suite.

## What works today

| Area | State |
|---|---|
| Lexer, parser, early errors | ES2025; same accept/reject verdicts as the reference |
| Bytecode compiler | instruction-identical to the reference for what it accepts |
| VM | calls, closures, prototypes, exceptions, `try`/`finally`, iterators, `for-of` |
| Objects | prototype chains, accessors, property attributes, `Object.freeze`/`seal` |
| Standard library | `Object`, `Function`, `Array`, `String`, `Number`, `Boolean`, `Math`, `JSON`, `Reflect`, the error classes |
| Scoping | `var`/`let`/`const`, block scope, TDZ, `const` read-only, per-iteration `for (let …)` |

Against the reference implementation's own `tests/test_builtin.js`, the statements that
depend only on the above pass: 229 of 237.

## What does not work yet

Stated plainly, because a version number only means something if the boundary is
honest. Calling any of these raises a distinct error rather than returning a wrong
answer:

- `class`, `new.target`, `super`
- generators, `async`/`await`, `for await`
- destructuring (declarations, parameters, `for-of` targets)
- function call spread, `...rest` parameters
- `Date`, `RegExp`, `Symbol`, `Map`/`Set`/`WeakMap`, `Promise`, `Proxy`
- `BigInt`, `TypedArray`, `ArrayBuffer`, `Atomics`
- modules (`import`/`export`), `eval` seeing its caller's scope
- `Error.stack` (needs the bytecode line table)

## Deterministic memory

The garbage collector is reference counting plus a cycle collector, so an object is
freed as soon as it becomes unreachable — there is no tracing pause and no finalizer
whose timing depends on the allocator. `Runtime::run_gc` reports what it freed, which
makes lifetime part of the test suite instead of a source of flaky failures.

## API shape

Idiomatic Rust rather than a transcription of the C API: `Runtime` (heap and limits),
`Context` (a global object plus its builtins), `Value`, and `Context::eval` with an
`Error` enum that separates *syntax*, *exception*, and *not implemented*.

## Status

Milestones M0–M4 are complete. The engine runs real JavaScript and embeds today; it is
not yet a drop-in replacement for every program, and the list above is the reason. A C
ABI compatibility layer (`JS_*`, 233 symbols) is planned as a separate crate so the
core representation stays free of C layout constraints.

[quickjs-rs]: https://github.com/qiuzhanghua/quickjs-rs

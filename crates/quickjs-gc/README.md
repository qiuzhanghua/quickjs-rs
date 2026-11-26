# quickjs-gc

Reference counting with cycle collection, as used by [`quickjs-pure`].

A JavaScript engine cannot use `Rc` alone: a closure over its own environment, and an
object that points back at itself, form cycles that plain reference counting never
frees. This crate adds a trial-deletion cycle collector on top of `Rc`, so a cycle is
still collected **deterministically** — no tracing pause, and `Drop` runs as soon as the
last strong handle goes.

```toml
[dependencies]
quickjs-gc = "0.4"
```

The package is `quickjs-gc` but the library is `js_gc`, so the import is:

```rust
use std::cell::RefCell;

use js_gc::{Gc, Heap, Trace, Tracer};

#[derive(Default)]
struct Node {
    next: RefCell<Option<Gc<Node>>>,
}

impl Trace for Node {
    fn trace(&self, tracer: &mut Tracer<'_>) {
        if let Some(next) = &*self.next.borrow() {
            tracer.visit(next);
        }
    }
}

let heap = Heap::new();
let a = Gc::new(&heap, Node::default());
let b = Gc::new(&heap, Node::default());
*a.next.borrow_mut() = Some(b.clone());
*b.next.borrow_mut() = Some(a.clone()); // a cycle
drop((a, b));

let stats = heap.collect();
assert_eq!(stats.collected, 2);
```

This example lives in `examples/cycle.rs` and is run by the test suite, so it cannot
drift from the real API.

## What is here

| Item | Role |
|---|---|
| `Heap` | owns every object; `collect()` runs the cycle collector |
| `Gc<T>` | strong handle — `clone` for a new one, `ptr_eq` for identity |
| `WeakGc<T>` | handle that does not keep the value alive; `upgrade` may fail |
| `Trace` | what an owner implements to declare the handles it holds |
| `Tracer` | passed to `Trace::trace`, and what records a visited handle |
| `CollectStats` | `before` / `collected` / `after` object counts and `passes` |

Every type stored in a `Gc` implements `Trace`. That is how the collector finds the
handles inside a value without knowing its concrete type, and why it can follow a cycle
it has never seen before.

## Determinism is the point

Collection is explicit (`Heap::collect`) and the outcome is observable
(`CollectStats`), so tests assert on object counts instead of waiting for a GC. Object
lifetime then becomes part of the engine's test suite rather than a source of flaky
failures. `set_threshold` additionally lets the heap collect itself once enough objects
have been allocated, and `live_count` / `allocated_bytes` make the heap observable.

## Status

Part of the [quickjs-rs] project — a from-scratch Rust implementation of
[QuickJS](https://github.com/bellard/quickjs). The crate is usable on its own; its API
may still change before 1.0.

[`quickjs-pure`]: https://crates.io/crates/quickjs-pure
[quickjs-rs]: https://github.com/qiuzhanghua/quickjs-rs

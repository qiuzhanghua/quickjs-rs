//! The example in README.md, kept compilable so the two cannot drift.
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

fn main() {
    let heap = Heap::new();
    let a = Gc::new(&heap, Node::default());
    let b = Gc::new(&heap, Node::default());
    *a.next.borrow_mut() = Some(b.clone());
    *b.next.borrow_mut() = Some(a.clone()); // a cycle
    drop((a, b));

    let stats = heap.collect();
    println!("collected={} live={}", stats.collected, stats.after);
    assert_eq!(stats.collected, 2, "the cycle should be collected");
}

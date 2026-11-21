//! Deterministic release: like QuickJS's reference counting, a value is freed as
//! soon as the last handle goes away, without waiting for the collector.

use js_gc::{Gc, Heap, Trace, Tracer};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

type Drops = Rc<Cell<usize>>;

fn drops() -> Drops {
    Rc::new(Cell::new(0))
}

struct Leaf {
    drops: Drops,
    child: RefCell<Option<Gc<Leaf>>>,
}

impl Leaf {
    fn new(drops: &Drops) -> Leaf {
        Leaf {
            drops: Rc::clone(drops),
            child: RefCell::new(None),
        }
    }
}

impl Trace for Leaf {
    fn trace(&self, tracer: &mut Tracer) {
        self.child.trace(tracer);
    }
}

impl Drop for Leaf {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn value_is_released_at_end_of_scope() {
    let heap = Heap::new();
    let drops = drops();
    {
        let _leaf = Gc::new(&heap, Leaf::new(&drops));
        assert_eq!(heap.live_count(), 1);
        assert_eq!(drops.get(), 0);
    }
    assert_eq!(
        drops.get(),
        1,
        "refcounting must not wait for the collector"
    );
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn chain_is_released_by_refcounting_alone() {
    let heap = Heap::new();
    let drops = drops();
    {
        let root = Gc::new(&heap, Leaf::new(&drops));
        let child = Gc::new(&heap, Leaf::new(&drops));
        let grandchild = Gc::new(&heap, Leaf::new(&drops));
        *root.child.borrow_mut() = Some(child.clone());
        *child.child.borrow_mut() = Some(grandchild.clone());
        assert_eq!(heap.live_count(), 3);
    }
    assert_eq!(drops.get(), 3);
    assert_eq!(heap.live_count(), 0);
    assert_eq!(
        heap.collect().collected,
        0,
        "nothing left for the collector"
    );
}

#[test]
fn cloning_keeps_the_value_alive_until_the_last_handle_dies() {
    let heap = Heap::new();
    let drops = drops();
    let leaf = Gc::new(&heap, Leaf::new(&drops));
    let clone = leaf.clone();
    assert_eq!(leaf.strong_count(), 2);
    drop(leaf);
    assert_eq!(drops.get(), 0);
    drop(clone);
    assert_eq!(drops.get(), 1);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn allocation_is_accounted_and_returned() {
    let heap = Heap::new();
    let drops = drops();
    {
        let _leaf = Gc::new(&heap, Leaf::new(&drops));
        assert!(heap.allocated_bytes() > 0);
    }
    assert_eq!(heap.allocated_bytes(), 0);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn slots_are_reused_after_release() {
    let heap = Heap::new();
    let drops = drops();
    for _ in 0..1000 {
        let leaf = Gc::new(&heap, Leaf::new(&drops));
        drop(leaf);
    }
    assert_eq!(drops.get(), 1000);
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.allocated_bytes(), 0);
}

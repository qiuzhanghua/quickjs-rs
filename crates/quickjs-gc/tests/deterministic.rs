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

#[test]
fn a_weak_handle_observes_the_release_without_causing_it() {
    let heap = Heap::new();
    let drops = drops();
    let leaf = Gc::new(&heap, Leaf::new(&drops));
    let weak = leaf.downgrade();

    // Holding only a weak handle does not keep the value alive...
    assert!(!weak.is_expired());
    assert!(weak.upgrade().is_some());
    assert_eq!(drops.get(), 0);

    drop(leaf);
    // ...and the release is immediate, because this is reference counting.
    assert_eq!(drops.get(), 1);
    assert!(weak.is_expired());
    assert!(weak.upgrade().is_none());
}

#[test]
fn a_weak_handle_keeps_the_header_alive_so_expiry_is_answerable() {
    let heap = Heap::new();
    let drops = drops();
    let weak = {
        let leaf = Gc::new(&heap, Leaf::new(&drops));
        leaf.downgrade()
    };
    // The value is gone but the box's header is not, which is what lets `is_expired` and
    // `ptr_eq` stay sound after the target died. Releasing the weak handle then frees it.
    assert_eq!(drops.get(), 1);
    assert!(weak.is_expired());
    let peers = vec![weak.clone()];
    drop(weak);
    assert!(peers[0].is_expired());
    drop(peers);
    assert_eq!(heap.allocated_bytes(), 0);
}

#[test]
fn a_weak_edge_does_not_keep_a_cycle_alive() {
    // The `WeakMap` shape: a strong cycle somewhere in the graph, plus a weak handle that
    // watches one end of it. The cycle must still be reclaimed — a weak edge is invisible to
    // `Trace` — and the weak handle must then report expiry. If `Trace for WeakGc` visited
    // its target, the cycle would be reachable and this would leak.
    struct Node {
        peer: RefCell<Option<Gc<Node>>>,
    }
    impl Trace for Node {
        fn trace(&self, tracer: &mut Tracer) {
            self.peer.trace(tracer);
        }
    }

    let heap = Heap::new();
    let left = Gc::new(
        &heap,
        Node {
            peer: RefCell::new(None),
        },
    );
    let right = Gc::new(
        &heap,
        Node {
            peer: RefCell::new(None),
        },
    );
    // `left -> right -> left`: each holds the other, so dropping the outer handles leaves
    // strong counts of 1 on both and reference counting alone cannot free either.
    *left.peer.borrow_mut() = Some(right.clone());
    *right.peer.borrow_mut() = Some(left.clone());
    let watcher = right.downgrade();
    drop(left);
    drop(right);

    assert_eq!(
        heap.live_count(),
        2,
        "both nodes of the cycle are still alive"
    );
    assert!(!watcher.is_expired());
    let stats = heap.collect();
    assert_eq!(stats.collected, 2, "the cycle should have been reclaimed");
    assert_eq!(heap.live_count(), 0);
    assert!(watcher.is_expired(), "the weak handle outlives its target");
}

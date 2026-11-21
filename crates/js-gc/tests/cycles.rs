//! Cycle collection behaviour: reachability, reclamation and weak handles.

use js_gc::{Gc, Heap, Trace, Tracer, WeakGc};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

/// `Rc<Cell<usize>>` counts value drops, so tests can observe reclamation.
type Drops = Rc<Cell<usize>>;

fn drops() -> Drops {
    Rc::new(Cell::new(0))
}

struct Node {
    #[allow(dead_code, reason = "kept for readable panic messages while debugging")]
    name: &'static str,
    next: RefCell<Option<Gc<Node>>>,
    alt: RefCell<Option<Gc<Node>>>,
    drops: Drops,
}

impl Node {
    fn new(name: &'static str, drops: &Drops) -> Node {
        Node {
            name,
            next: RefCell::new(None),
            alt: RefCell::new(None),
            drops: Rc::clone(drops),
        }
    }

    fn link(&self, other: &Gc<Node>) {
        *self.next.borrow_mut() = Some(other.clone());
    }

    fn link_alt(&self, other: &Gc<Node>) {
        *self.alt.borrow_mut() = Some(other.clone());
    }
}

impl Trace for Node {
    fn trace(&self, tracer: &mut Tracer) {
        self.next.trace(tracer);
        self.alt.trace(tracer);
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

#[test]
fn self_cycle_is_reclaimed_by_collect() {
    let heap = Heap::new();
    let drops = drops();

    let a = Gc::new(&heap, Node::new("a", &drops));
    a.link(&a.clone());
    assert_eq!(a.strong_count(), 2);

    drop(a);
    // Unreachable, but the cycle keeps the refcount above zero.
    assert_eq!(
        drops.get(),
        0,
        "cycle must not be freed by refcounting alone"
    );
    assert_eq!(heap.live_count(), 1);

    let stats = heap.collect();
    assert_eq!(stats.before, 1);
    assert_eq!(stats.collected, 1);
    assert_eq!(stats.after, 0);
    assert_eq!(
        drops.get(),
        1,
        "collector must run the destructor exactly once"
    );
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn ring_of_three_is_reclaimed() {
    let heap = Heap::new();
    let drops = drops();

    let a = Gc::new(&heap, Node::new("a", &drops));
    let b = Gc::new(&heap, Node::new("b", &drops));
    let c = Gc::new(&heap, Node::new("c", &drops));
    a.link(&b);
    b.link(&c);
    c.link(&a);
    drop((a, b, c));

    assert_eq!(heap.live_count(), 3);
    assert_eq!(heap.collect().collected, 3);
    assert_eq!(drops.get(), 3);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn cycle_reachable_from_stack_handle_survives_and_is_collected_later() {
    let heap = Heap::new();
    let drops = drops();

    let a = Gc::new(&heap, Node::new("a", &drops));
    let b = Gc::new(&heap, Node::new("b", &drops));
    a.link(&b);
    b.link(&a);

    // `a` is still referenced from the stack, so the whole cycle stays alive.
    assert_eq!(heap.collect().collected, 0);
    assert_eq!(heap.live_count(), 2);
    assert_eq!(drops.get(), 0);

    // `b` is still a stack handle too, so the cycle is still reachable.
    drop(a);
    assert_eq!(heap.collect().collected, 0);
    assert_eq!(heap.live_count(), 2);

    drop(b);
    assert_eq!(heap.collect().collected, 2);
    assert_eq!(drops.get(), 2);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn child_referenced_only_from_a_live_object_survives() {
    let heap = Heap::new();
    let drops = drops();

    let root = Gc::new(&heap, Node::new("root", &drops));
    let child = Gc::new(&heap, Node::new("child", &drops));
    root.link(&child);
    drop(child);

    // `child` has no external handle: all of its references come from `root`.
    // Its `gc_refs` is therefore 0, but `root` is a root and marks it.
    assert_eq!(heap.collect().collected, 0);
    assert_eq!(heap.live_count(), 2);

    // No cycle here, so plain refcounting releases the pair immediately; the
    // collector has nothing left to do (deterministic release, like QuickJS).
    drop(root);
    assert_eq!(drops.get(), 2);
    assert_eq!(heap.live_count(), 0);
    assert_eq!(heap.collect().collected, 0);
}

#[test]
fn handle_stored_in_a_plain_struct_counts_as_a_root() {
    // A `Gc` field inside a non-heap struct lives outside the heap, so the
    // collector must treat it as external.
    struct Holder {
        child: Gc<Node>,
    }

    let heap = Heap::new();
    let drops = drops();
    let holder = Holder {
        child: Gc::new(&heap, Node::new("held", &drops)),
    };
    assert_eq!(holder.child.strong_count(), 1);

    assert_eq!(heap.collect().collected, 0);
    assert_eq!(heap.live_count(), 1);
    assert_eq!(drops.get(), 0);

    drop(holder);
    assert_eq!(drops.get(), 1);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn diamond_sharing_is_not_collected() {
    let heap = Heap::new();
    let drops = drops();

    let top = Gc::new(&heap, Node::new("top", &drops));
    let left = Gc::new(&heap, Node::new("left", &drops));
    let right = Gc::new(&heap, Node::new("right", &drops));
    let shared = Gc::new(&heap, Node::new("shared", &drops));
    top.link(&left);
    top.link_alt(&right);
    left.link(&shared);
    right.link(&shared);
    drop((left, right, shared));

    assert_eq!(heap.collect().collected, 0);
    assert_eq!(heap.live_count(), 4);

    drop(top);
    assert_eq!(
        heap.collect().collected,
        0,
        "acyclic sharing is released by refcounting"
    );
    assert_eq!(drops.get(), 4);
    assert_eq!(heap.live_count(), 0);
}

#[test]
fn weak_handle_expires_after_collection() {
    let heap = Heap::new();
    let drops = drops();

    let a = Gc::new(&heap, Node::new("a", &drops));
    let weak: WeakGc<Node> = a.downgrade();
    a.link(&a.clone());

    assert!(!weak.is_expired());
    assert!(weak.upgrade().is_some());

    drop(a);
    assert!(
        !weak.is_expired(),
        "value is still kept alive by its own cycle"
    );

    heap.collect();
    assert!(weak.is_expired(), "collector must release the value");
    assert!(weak.upgrade().is_none());
    assert_eq!(
        heap.live_count(),
        0,
        "no strong handles left, header is freed with the last weak handle"
    );
    drop(weak);
}

#[test]
fn weak_handle_does_not_keep_objects_alive() {
    let heap = Heap::new();
    let drops = drops();

    let a = Gc::new(&heap, Node::new("a", &drops));
    let b = Gc::new(&heap, Node::new("b", &drops));
    a.link(&b);
    b.link(&a);
    let weak = a.downgrade();

    assert_eq!(heap.collect().collected, 0, "reachable cycle must survive");
    drop(a);
    assert_eq!(heap.collect().collected, 0, "`b` is still a stack handle");
    {
        let upgraded = weak.upgrade().expect("still alive");
        assert_eq!(upgraded.strong_count(), 2, "upgrade() adds a strong handle");
        assert_eq!(heap.collect().collected, 0);
    }
    assert!(weak.upgrade().is_some(), "the upgrade above did not leak");

    drop(b);
    // Only the collector can reclaim it: the weak handle does not count as a
    // reference, so the pair is garbage.
    assert_eq!(heap.collect().collected, 2);
    assert!(weak.is_expired());
    assert_eq!(drops.get(), 2);
}

#[test]
fn collect_on_empty_heap_is_a_noop() {
    let heap = Heap::new();
    let stats = heap.collect();
    assert_eq!((stats.before, stats.collected, stats.after), (0, 0, 0));
}

#[test]
fn repeated_collect_is_stable() {
    let heap = Heap::new();
    let drops = drops();

    for _ in 0..3 {
        let a = Gc::new(&heap, Node::new("a", &drops));
        a.link(&a.clone());
    }
    assert_eq!(heap.live_count(), 3);
    assert_eq!(heap.collect().collected, 3);
    assert_eq!(heap.collect().collected, 0);
    assert_eq!(drops.get(), 3);
}

#[test]
fn threshold_triggers_collection_during_allocation() {
    let heap = Heap::new();
    let drops = drops();
    heap.set_threshold(2);

    {
        let a = Gc::new(&heap, Node::new("a", &drops));
        a.link(&a.clone());
    }
    // Allocating past the threshold collects the garbage cycle above.
    let mut kept = Vec::new();
    for i in 0..4 {
        kept.push(Gc::new(&heap, Node::new("kept", &drops)));
        let _ = i;
    }
    assert!(
        drops.get() >= 1,
        "automatic collection should have reclaimed the cycle"
    );
    assert_eq!(heap.live_count(), kept.len());
    drop(kept);
    assert_eq!(heap.live_count(), 0);
}

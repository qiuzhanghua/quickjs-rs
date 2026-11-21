//! `js-gc` — reference counting with cycle collection.
//!
//! QuickJS frees memory by reference counting and removes reference cycles with a
//! periodic collector (see `JS_RunGC` / `JS_SetGCThreshold` in the C API). Its
//! observable behaviour — an object is released as soon as the last reference
//! goes away, finalizers run at that moment, and cycles are reclaimed only when
//! the collector runs — is part of the semantics we have to reproduce, so we use
//! the same algorithm instead of a tracing GC.
//!
//! This crate provides:
//!
//! * [`Heap`] — one per [`Runtime`]-equivalent; owns the registry of live boxes,
//!   the allocation accounting and the collector.
//! * [`Gc<T>`] — a strong, reference-counted handle. Cheap to clone, derefs to
//!   `T`, releases the value deterministically when the last handle dies.
//! * [`WeakGc<T>`] — a non-owning handle that does not keep the value alive and
//!   does not participate in cycle detection (used by `WeakRef`, `WeakMap` and
//!   `FinalizationRegistry`).
//! * [`Trace`] — implemented by every type that can own `Gc` handles, so the
//!   collector can walk the object graph.
//!
//! # The collector
//!
//! `Heap::collect` implements trial deletion (Bacon–Rajan "synchronous cycle
//! collection"):
//!
//! 1. Every live box starts with `gc_refs = strong_count`.
//! 2. Every internal edge (a `Gc` field inside another box) decrements the
//!    child's `gc_refs`. After this pass `gc_refs > 0` means "at least one
//!    reference lives outside the heap", i.e. the box is a root.
//! 3. Mark everything reachable from the roots.
//! 4. Unmarked boxes are only reachable from garbage: their values are dropped
//!    (which releases the handles they hold) and the boxes themselves are freed
//!    once their strong count reaches zero.
//!
//! # Contract
//!
//! * A [`Heap`] must outlive every [`Gc`] created from it. Dropping a heap with
//!   live boxes leaks them; it is not undefined behaviour.
//! * [`Trace::trace`] must not mutate the object graph, allocate, or panic:
//!   it runs while the collector holds a snapshot of the heap.
//! * `Gc<T>`/`WeakGc<T>` are `!Send + !Sync`: a heap is single threaded, which
//!   matches a QuickJS `JSRuntime` (`JS_SetSharedArrayBufferFunctions` is the
//!   only cross-thread escape hatch and it is handled in `js-value`).

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use std::alloc::{Layout, dealloc};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::fmt;
use std::ops::Deref;
use std::ptr::NonNull;

// ---------------------------------------------------------------------------
// Trace / Tracer
// ---------------------------------------------------------------------------

/// Types that may own [`Gc`] handles.
///
/// Implementations must visit *every* `Gc` field, directly or through another
/// type that implements `Trace`. Missing an edge makes the collector treat the
/// object as unreachable from that side, which only costs memory (a cycle that
/// is never reclaimed), never safety — but it is still a bug.
pub trait Trace {
    /// Report every `Gc` owned by `self` to `tracer`.
    fn trace(&self, tracer: &mut Tracer);
}

/// Visitor used by [`Trace`] implementations.
///
/// The collector drives this; user code only calls [`Tracer::visit`] (usually
/// through `#[derive]`-style impls for containers).
pub struct Tracer<'a> {
    out: &'a mut Vec<NonNull<Header>>,
}

impl Tracer<'_> {
    /// Record that `gc` is owned by the object currently being traced.
    #[inline]
    pub fn visit<T: Trace + 'static>(&mut self, gc: &Gc<T>) {
        self.out.push(gc.header());
    }
}

macro_rules! trace_noop {
    ($($t:ty),* $(,)?) => {$(
        impl Trace for $t {
            #[inline]
            fn trace(&self, _tracer: &mut Tracer) {}
        }
    )*};
}

trace_noop!(
    (),
    bool,
    char,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    String,
    &'static str,
);

impl<T: Trace + ?Sized> Trace for &T {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        (**self).trace(tracer);
    }
}

impl<T: Trace + ?Sized> Trace for Box<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        (**self).trace(tracer);
    }
}

impl<T: Trace> Trace for Option<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(v) = self {
            v.trace(tracer);
        }
    }
}

impl<T: Trace> Trace for Vec<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        for v in self {
            v.trace(tracer);
        }
    }
}

impl<T: Trace, const N: usize> Trace for [T; N] {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        for v in self {
            v.trace(tracer);
        }
    }
}

impl<T: Trace, U: Trace> Trace for (T, U) {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        self.0.trace(tracer);
        self.1.trace(tracer);
    }
}

impl<T: Trace> Trace for std::rc::Rc<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        (**self).trace(tracer);
    }
}

impl<T: Trace> Trace for RefCell<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        // A cell that is mutably borrowed while the collector runs would make
        // tracing panic; skipping the edge is the safe degradation (worst case:
        // we fail to reclaim a cycle, never a use-after-free).
        if let Ok(v) = self.try_borrow() {
            v.trace(tracer);
        }
    }
}

impl<T: Trace> Trace for Cell<T>
where
    T: Copy,
{
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        self.get().trace(tracer);
    }
}

// ---------------------------------------------------------------------------
// Header / box
// ---------------------------------------------------------------------------

/// Type-erased box header. Every `GcBox<T>` starts with one, so a
/// `NonNull<Header>` can identify, trace and free any box without knowing `T`.
struct Header {
    strong: Cell<usize>,
    weak: Cell<usize>,
    /// Scratch space for the collector: `strong` minus internal edges.
    gc_refs: Cell<isize>,
    marked: Cell<bool>,
    /// Index into [`Heap::boxes`], `usize::MAX` when not registered.
    slot: Cell<usize>,
    heap: NonNull<Heap>,
    trace_fn: unsafe fn(NonNull<Header>, &mut Tracer),
    drop_fn: unsafe fn(NonNull<Header>),
    size: usize,
    align: usize,
}

#[repr(C)]
struct GcBox<T: Trace + 'static> {
    header: Header,
    value: UnsafeCell<Option<T>>,
}

unsafe fn trace_impl<T: Trace + 'static>(h: NonNull<Header>, tracer: &mut Tracer) {
    let b = h.cast::<GcBox<T>>();
    // SAFETY: `h` came from a `GcBox<T>` allocation and the value is only
    // mutated by `drop_impl` / construction, never while tracing.
    let value = unsafe { &*(*b.as_ptr()).value.get() };
    if let Some(v) = value {
        v.trace(tracer);
    }
}

unsafe fn drop_impl<T: Trace + 'static>(h: NonNull<Header>) {
    let b = h.cast::<GcBox<T>>();
    // Take the value out before dropping it so that re-entrant `Gc` releases
    // (this drop can free other boxes) never observe a borrowed cell.
    // SAFETY: `h` came from a `GcBox<T>` allocation; the value is dropped once.
    let taken = unsafe { (*b.as_ptr()).value.get().as_mut() }.and_then(Option::take);
    drop(taken);
}

// ---------------------------------------------------------------------------
// Heap
// ---------------------------------------------------------------------------

/// Statistics returned by [`Heap::collect`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollectStats {
    /// Boxes registered before the collection.
    pub before: usize,
    /// Boxes reclaimed by this collection.
    pub collected: usize,
    /// Boxes still alive afterwards.
    pub after: usize,
    /// Collection passes performed (always 1 today; kept for API stability).
    pub passes: usize,
}

/// Owns the live-object registry for one JavaScript runtime.
pub struct Heap {
    boxes: RefCell<Vec<Option<NonNull<Header>>>>,
    free_slots: RefCell<Vec<usize>>,
    live: Cell<usize>,
    allocated: Cell<usize>,
    threshold: Cell<usize>,
}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    /// Create an empty heap. Runs the collector only when asked
    /// (see [`Heap::set_threshold`]).
    pub fn new() -> Heap {
        Heap {
            boxes: RefCell::new(Vec::new()),
            free_slots: RefCell::new(Vec::new()),
            live: Cell::new(0),
            allocated: Cell::new(0),
            threshold: Cell::new(usize::MAX),
        }
    }

    /// Trigger a collection automatically when more than `n` boxes are live.
    /// `usize::MAX` disables automatic collection (the default).
    pub fn set_threshold(&self, n: usize) {
        self.threshold.set(n);
    }

    /// Current automatic-collection threshold.
    pub fn threshold(&self) -> usize {
        self.threshold.get()
    }

    /// Number of live boxes.
    pub fn live_count(&self) -> usize {
        self.live.get()
    }

    /// Approximate number of bytes held by live boxes (excluding external
    /// allocations made by the values themselves).
    pub fn allocated_bytes(&self) -> usize {
        self.allocated.get()
    }

    fn register(&self, h: NonNull<Header>) {
        let mut boxes = self.boxes.borrow_mut();
        let slot = match self.free_slots.borrow_mut().pop() {
            Some(slot) => {
                boxes[slot] = Some(h);
                slot
            }
            None => {
                boxes.push(Some(h));
                boxes.len() - 1
            }
        };
        // SAFETY: `h` points at a live box we just allocated.
        let header = unsafe { h.as_ref() };
        header.slot.set(slot);
        self.live.set(self.live.get() + 1);
        self.allocated.set(self.allocated.get() + header.size);
    }

    fn deregister(&self, h: NonNull<Header>) {
        // SAFETY: `h` points at a live box (we are on its release path).
        let header = unsafe { h.as_ref() };
        let slot = header.slot.get();
        if slot == usize::MAX {
            return;
        }
        header.slot.set(usize::MAX);
        {
            let mut boxes = self.boxes.borrow_mut();
            boxes[slot] = None;
        }
        self.free_slots.borrow_mut().push(slot);
        self.live.set(self.live.get() - 1);
        self.allocated.set(self.allocated.get() - header.size);
    }

    /// Run the cycle collector.
    ///
    /// Objects that are only reachable from garbage are dropped and freed;
    /// everything reachable from a handle living outside the heap survives.
    pub fn collect(&self) -> CollectStats {
        let snapshot: Vec<NonNull<Header>> =
            self.boxes.borrow().iter().flatten().copied().collect();
        let before = snapshot.len();
        if before == 0 {
            return CollectStats {
                before: 0,
                collected: 0,
                after: 0,
                passes: 1,
            };
        }

        // 1. gc_refs = strong.
        for &h in &snapshot {
            // SAFETY: every pointer in the snapshot came from a registered box
            // that cannot be freed while we hold no handles to it... see note
            // below: no user code runs between snapshot and marking, and boxes
            // are only freed once their strong count reaches zero, which cannot
            // happen without user code.
            let header = unsafe { h.as_ref() };
            header.gc_refs.set(header.strong.get() as isize);
            header.marked.set(false);
        }

        // 2. Subtract internal edges.
        let mut buf: Vec<NonNull<Header>> = Vec::new();
        for &h in &snapshot {
            buf.clear();
            // SAFETY: see above; `trace` is a pure read of the graph.
            unsafe { trace_children(h, &mut buf) };
            for &child in &buf {
                // SAFETY: children are live boxes (they are referenced by a
                // live box, so their strong count is >= 1).
                let child = unsafe { child.as_ref() };
                child.gc_refs.set(child.gc_refs.get() - 1);
            }
        }

        // 3. Mark from roots (`gc_refs > 0` == referenced from outside the heap).
        let mut stack: Vec<NonNull<Header>> = Vec::new();
        for &h in &snapshot {
            // SAFETY: see above.
            let header = unsafe { h.as_ref() };
            if header.gc_refs.get() > 0 && !header.marked.get() {
                header.marked.set(true);
                stack.push(h);
            }
        }
        while let Some(h) = stack.pop() {
            buf.clear();
            // SAFETY: see above.
            unsafe { trace_children(h, &mut buf) };
            for &child in &buf {
                // SAFETY: see above.
                let header = unsafe { child.as_ref() };
                if !header.marked.get() {
                    header.marked.set(true);
                    stack.push(child);
                }
            }
        }

        // 4. Drop the unmarked boxes. Bump every one of them first so that
        //    releasing handles inside them cannot free a box we are still
        //    iterating over.
        let dead: Vec<NonNull<Header>> = snapshot
            .iter()
            .copied()
            .filter(|h| {
                // SAFETY: see above.
                !unsafe { h.as_ref() }.marked.get()
            })
            .collect();

        for &h in &dead {
            // SAFETY: see above.
            let header = unsafe { h.as_ref() };
            header.strong.set(header.strong.get() + 1);
        }
        for &h in &dead {
            // SAFETY: `drop_fn` only touches this box's own value.
            unsafe { (h.as_ref().drop_fn)(h) };
            // The value is gone, so the object left the object graph; do it now
            // so that a (later) collection never sees a value-less box.
            self.deregister(h);
        }
        for &h in &dead {
            // SAFETY: see above.
            let header = unsafe { h.as_ref() };
            let strong = header.strong.get();
            header.strong.set(strong - 1);
            if strong == 1 && header.weak.get() == 0 {
                // SAFETY: no strong or weak references remain.
                unsafe { dealloc_box(h) };
            }
        }

        let after = self.live_count();
        CollectStats {
            before,
            collected: before - after,
            after,
            passes: 1,
        }
    }
}

/// # Safety
/// `h` must point at a live box.
unsafe fn trace_children(h: NonNull<Header>, out: &mut Vec<NonNull<Header>>) {
    // SAFETY: caller guarantees `h` is live; `trace_fn` only reads the value.
    let trace_fn = unsafe { h.as_ref().trace_fn };
    let mut tracer = Tracer { out };
    // SAFETY: as above.
    unsafe { trace_fn(h, &mut tracer) };
}

/// # Safety
/// `h` must point at a box that has already been deregistered and whose strong
/// and weak counts both reached zero.
unsafe fn dealloc_box(h: NonNull<Header>) {
    // SAFETY: caller guarantees the header is still allocated.
    let (size, align) = {
        let header = unsafe { h.as_ref() };
        (header.size, header.align)
    };
    // SAFETY: the allocation was made with this exact layout.
    unsafe {
        dealloc(
            h.as_ptr() as *mut u8,
            Layout::from_size_align_unchecked(size, align),
        )
    };
}

// ---------------------------------------------------------------------------
// Gc
// ---------------------------------------------------------------------------

/// A strong, reference-counted handle to a heap value.
///
/// `Gc<T>` is a pointer-sized handle: cloning is cheap, [`Deref`] gives `&T`,
/// and the value is released as soon as the last handle is gone (cycles need
/// [`Heap::collect`]).
pub struct Gc<T: Trace + 'static> {
    ptr: NonNull<GcBox<T>>,
}

impl<T: Trace + 'static> Gc<T> {
    /// Allocate `value` on `heap` and return a handle to it.
    pub fn new(heap: &Heap, value: T) -> Gc<T> {
        let boxed = Box::new(GcBox {
            header: Header {
                strong: Cell::new(1),
                weak: Cell::new(0),
                gc_refs: Cell::new(1),
                marked: Cell::new(false),
                slot: Cell::new(usize::MAX),
                heap: NonNull::from(heap),
                trace_fn: trace_impl::<T>,
                drop_fn: drop_impl::<T>,
                size: std::mem::size_of::<GcBox<T>>(),
                align: std::mem::align_of::<GcBox<T>>(),
            },
            value: UnsafeCell::new(Some(value)),
        });
        // SAFETY: `Box::into_raw` never returns null.
        let ptr = unsafe { NonNull::new_unchecked(Box::into_raw(boxed)) };
        let gc = Gc { ptr };
        heap.register(gc.header());
        if heap.live_count() > heap.threshold() {
            heap.collect();
        }
        gc
    }

    /// The heap this value lives on.
    pub fn heap(&self) -> &Heap {
        // SAFETY: the heap outlives the box (documented contract).
        unsafe { self.ptr.as_ref().header.heap.as_ref() }
    }

    /// Number of strong handles (including internal edges).
    pub fn strong_count(&self) -> usize {
        // SAFETY: the handle keeps the box alive.
        unsafe { self.ptr.as_ref().header.strong.get() }
    }

    /// Whether two handles point at the same box.
    pub fn ptr_eq(&self, other: &Gc<T>) -> bool {
        self.ptr == other.ptr
    }

    /// Create a non-owning handle.
    pub fn downgrade(&self) -> WeakGc<T> {
        // SAFETY: the handle keeps the box alive.
        let header = unsafe { &self.ptr.as_ref().header };
        header.weak.set(header.weak.get() + 1);
        WeakGc { ptr: self.ptr }
    }

    #[inline]
    fn header(&self) -> NonNull<Header> {
        // SAFETY: `Header` is the first field of `GcBox<T>` (`#[repr(C)]`).
        unsafe { NonNull::new_unchecked(self.ptr.as_ptr().cast::<Header>()) }
    }
}

impl<T: Trace + 'static> Clone for Gc<T> {
    fn clone(&self) -> Gc<T> {
        // SAFETY: the handle keeps the box alive.
        let header = unsafe { &self.ptr.as_ref().header };
        header.strong.set(header.strong.get() + 1);
        Gc { ptr: self.ptr }
    }
}

impl<T: Trace + 'static> Drop for Gc<T> {
    fn drop(&mut self) {
        dec_strong(self.header());
    }
}

impl<T: Trace + 'static> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the handle keeps the box and its value alive.
        unsafe {
            (*self.ptr.as_ptr())
                .value
                .get()
                .as_ref()
                .expect("UnsafeCell pointer is never null")
                .as_ref()
                .expect("Gc value accessed after it was dropped")
        }
    }
}

impl<T: Trace + 'static> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Gc({:p}, strong={})",
            self.ptr.as_ptr(),
            self.strong_count()
        )
    }
}

impl<T: Trace + 'static> Trace for Gc<T> {
    #[inline]
    fn trace(&self, tracer: &mut Tracer) {
        tracer.visit(self);
    }
}

/// # Safety
/// `h` must point at a live box.
fn dec_strong(h: NonNull<Header>) {
    // SAFETY: caller guarantees liveness; a handle owns one strong reference.
    let header = unsafe { h.as_ref() };
    let strong = header.strong.get();
    debug_assert!(strong > 0, "Gc strong count underflow");
    header.strong.set(strong - 1);
    if strong == 1 {
        // SAFETY: we just dropped the last reference.
        unsafe { release(h) };
    }
}

/// # Safety
/// `h` must point at a box whose strong count just reached zero.
unsafe fn release(h: NonNull<Header>) {
    // SAFETY: caller guarantees the strong count is zero; dropping the value may
    // free other boxes but never this one (we still own its allocation).
    unsafe { (h.as_ref().drop_fn)(h) };
    // SAFETY: header is still valid (the allocation is ours until we free it).
    let header = unsafe { h.as_ref() };
    // A box without a value is no longer part of the object graph, so the
    // collector must not see it; weak handles may still point at the header.
    // SAFETY: the heap outlives its boxes (documented contract).
    unsafe { header.heap.as_ref() }.deregister(h);
    if header.weak.get() == 0 {
        // SAFETY: no strong or weak references remain.
        unsafe { dealloc_box(h) };
    }
}

// ---------------------------------------------------------------------------
// WeakGc
// ---------------------------------------------------------------------------

/// A non-owning handle to a heap value.
///
/// Keeps the box header (but not the value) alive, so [`WeakGc::upgrade`] can
/// tell whether the value is still there. Weak handles are not traced: they do
/// not keep anything alive and they do not take part in cycle detection, which
/// is what `WeakRef`/`WeakMap` need.
pub struct WeakGc<T: Trace + 'static> {
    ptr: NonNull<GcBox<T>>,
}

impl<T: Trace + 'static> WeakGc<T> {
    /// Try to obtain a strong handle.
    pub fn upgrade(&self) -> Option<Gc<T>> {
        // SAFETY: the weak handle keeps the box allocation alive.
        let header = unsafe { &self.ptr.as_ref().header };
        if header.strong.get() == 0 {
            None
        } else {
            header.strong.set(header.strong.get() + 1);
            Some(Gc { ptr: self.ptr })
        }
    }

    /// Whether the value has been released.
    pub fn is_expired(&self) -> bool {
        // SAFETY: the weak handle keeps the box allocation alive.
        unsafe { self.ptr.as_ref() }.header.strong.get() == 0
    }
}

impl<T: Trace + 'static> Clone for WeakGc<T> {
    fn clone(&self) -> WeakGc<T> {
        // SAFETY: the weak handle keeps the box allocation alive.
        let header = unsafe { &self.ptr.as_ref().header };
        header.weak.set(header.weak.get() + 1);
        WeakGc { ptr: self.ptr }
    }
}

impl<T: Trace + 'static> Drop for WeakGc<T> {
    fn drop(&mut self) {
        // SAFETY: this weak handle owns one weak reference.
        let header = unsafe { &self.ptr.as_ref().header };
        let weak = header.weak.get();
        debug_assert!(weak > 0, "WeakGc weak count underflow");
        header.weak.set(weak - 1);
        if weak == 1 && header.strong.get() == 0 {
            // SAFETY: the header keeps its address until both counts are zero,
            // and the last reference of either kind just went away.
            let h = unsafe { NonNull::new_unchecked(self.ptr.as_ptr().cast::<Header>()) };
            unsafe { dealloc_box(h) };
        }
    }
}

impl<T: Trace + 'static> fmt::Debug for WeakGc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "WeakGc({:p}, expired={})",
            self.ptr.as_ptr(),
            self.is_expired()
        )
    }
}

impl<T: Trace + 'static> Trace for WeakGc<T> {
    #[inline]
    fn trace(&self, _tracer: &mut Tracer) {}
}

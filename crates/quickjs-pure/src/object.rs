//! The object model: property tables, arrays, functions, and the prototype chain.
//!
//! QuickJS splits an object into a *shape* (the property names and their
//! attributes, shared between objects with the same layout) and a *property
//! vector* (the values). That split is a performance feature, not a semantic one,
//! and it only pays off once objects are created in bulk; this model keeps the
//! property table per object, in insertion order, and therefore reproduces the
//! ordering rules directly.
//!
//! Insertion order is load-bearing: `for (k in o)` and `Object.keys` iterate in the
//! order properties were defined, with integer-like keys first in ascending order.

use std::cell::RefCell;

use js_compile::bytecode::Function as CompiledFunction;
use js_gc::{Gc, Trace, Tracer};

use std::fmt;
use std::rc::Rc;

use crate::value::{Cell, JsSymbol, Value};

/// The bytes an `ArrayBuffer` owns, plus whether they have been detached.
///
/// `transfer()` moves the bytes to a new buffer and leaves this one empty and detached; a
/// view of a detached buffer reads `undefined` from every index.
pub struct BufferStorage {
    /// The bytes, in memory order.
    pub bytes: Vec<u8>,
    /// Whether `transfer()` has taken the bytes away.
    pub detached: bool,
}

impl JsObject {
    /// The storage of this object, when it is an `ArrayBuffer`.
    pub fn array_buffer_storage(&self) -> Option<Rc<RefCell<BufferStorage>>> {
        match &self.data.borrow().kind {
            ObjectKind::ArrayBuffer(storage) => Some(storage.clone()),
            _ => None,
        }
    }

    /// Make this object an `ArrayBuffer` owning `storage`.
    pub fn set_array_buffer(&self, storage: Rc<RefCell<BufferStorage>>) {
        self.data.borrow_mut().kind = ObjectKind::ArrayBuffer(storage);
    }

    /// `(buffer, element, offset, length)`, when this object is a typed-array view.
    pub fn typed_array_parts(
        &self,
    ) -> Option<(
        Gc<JsObject>,
        crate::builtins::typedarray::ElementType,
        usize,
        usize,
    )> {
        match &self.data.borrow().kind {
            ObjectKind::TypedArray {
                buffer,
                element,
                offset,
                length,
            } => Some((buffer.clone(), *element, *offset, *length)),
            _ => None,
        }
    }

    /// Make this object a typed-array view.
    pub fn set_typed_array(
        &self,
        buffer: Gc<JsObject>,
        element: crate::builtins::typedarray::ElementType,
        offset: usize,
        length: usize,
    ) {
        self.data.borrow_mut().kind = ObjectKind::TypedArray {
            buffer,
            element,
            offset,
            length,
        };
    }

    /// The element type, when this object is a typed-array view.
    pub fn typed_array_element_type(&self) -> Option<crate::builtins::typedarray::ElementType> {
        self.typed_array_parts().map(|(_, element, _, _)| element)
    }

    /// How many elements the view exposes, or `None` when this is not a view.
    ///
    /// A detached buffer reports `0`: the storage is gone, so there is nothing to index.
    pub fn typed_array_length(&self) -> Option<usize> {
        let (buffer, _, _, length) = self.typed_array_parts()?;
        let storage = buffer.array_buffer_storage()?;
        if storage.borrow().detached {
            return Some(0);
        }
        Some(length)
    }

    /// Read one view element.
    ///
    /// Out-of-range and detached reads are `undefined` rather than an error, which is what
    /// makes `a[999]` and a detached `a[0]` behave the way the language says.
    pub fn typed_array_element(&self, index: usize) -> Option<Value> {
        let (buffer, element, offset, length) = self.typed_array_parts()?;
        let storage = buffer.array_buffer_storage()?;
        let storage = storage.borrow();
        if storage.detached || index >= length {
            return Some(Value::Undefined);
        }
        Some(element.read(&storage.bytes, offset + index * element.size()))
    }

    /// Store one view element, applying the type's wrap or clamp.
    pub fn typed_array_set(&self, index: usize, value: Value) -> Option<()> {
        let (buffer, element, offset, length) = self.typed_array_parts()?;
        if index >= length {
            return Some(());
        }
        let storage = buffer.array_buffer_storage()?;
        let mut storage = storage.borrow_mut();
        if storage.detached {
            return Some(());
        }
        element.write(&mut storage.bytes, offset + index * element.size(), value);
        Some(())
    }
}

/// A property of an object.
#[derive(Clone, Default)]
pub struct Property {
    /// The value of a data property.
    pub value: Option<Value>,
    /// The getter of an accessor property.
    pub getter: Option<Value>,
    /// The setter of an accessor property.
    pub setter: Option<Value>,
    /// Whether the value can be replaced.
    pub writable: bool,
    /// Whether `for`/`Object.keys` sees the property.
    pub enumerable: bool,
    /// Whether the property can be deleted or redefined.
    pub configurable: bool,
}

impl Property {
    /// A writable, enumerable, configurable data property.
    ///
    /// This is what a plain assignment or an object literal creates, and the
    /// attributes only differ for built-ins and `Object.defineProperty`.
    pub fn data(value: Value) -> Property {
        Property {
            value: Some(value),
            writable: true,
            enumerable: true,
            configurable: true,
            ..Property::default()
        }
    }

    /// A data property with explicit attributes.
    pub fn with_attributes(value: Value, writable: bool, enumerable: bool) -> Property {
        Property {
            value: Some(value),
            writable,
            enumerable,
            configurable: writable,
            ..Property::default()
        }
    }

    /// An accessor property.
    /// An accessor property with explicit `enumerable`/`configurable` flags, which is
    /// what `Symbol.prototype.description` needs: it is enumerable: false in the spec.
    pub fn accessor_with(
        getter: Option<Value>,
        setter: Option<Value>,
        enumerable: bool,
    ) -> Property {
        Property {
            getter,
            setter,
            enumerable,
            configurable: true,
            ..Property::default()
        }
    }

    /// An accessor property with the default `enumerable`/`configurable` flags.
    pub fn accessor(getter: Option<Value>, setter: Option<Value>) -> Property {
        Property {
            getter,
            setter,
            enumerable: true,
            configurable: true,
            ..Property::default()
        }
    }

    /// Whether this is an accessor rather than a data property.
    pub fn is_accessor(&self) -> bool {
        self.getter.is_some() || self.setter.is_some()
    }
}

impl Trace for Property {
    fn trace(&self, tracer: &mut Tracer) {
        self.value.trace(tracer);
        self.getter.trace(tracer);
        self.setter.trace(tracer);
    }
}

/// A native (Rust) function reachable from JavaScript.
///
/// It receives the interpreter so a builtin can allocate (which needs the heap),
/// call back into JavaScript (which needs the frame stack) and raise exceptions.
pub type NativeFn =
    fn(&mut crate::interp::Interp<'_>, Value, &[Value]) -> crate::interp::JsResult<Value>;

/// The body of a callable object.
pub enum Callable {
    /// A function compiled from JavaScript source.
    Bytecode {
        /// The bytecode and slot mapping.
        code: Gc<CompiledCode>,
        /// The captured environment.
        env: Gc<ClosureEnv>,
    },
    /// A function produced by `Function.prototype.bind`.
    ///
    /// This is a variant rather than a native function carrying the state in
    /// properties because the state is not observable: `bind` has to remember its
    /// target, `this` and leading arguments, and a native trampoline cannot capture
    /// them.
    Bound {
        /// The function being called.
        target: Value,
        /// The `this` every call uses.
        this: Value,
        /// Arguments prepended to every call's own.
        args: Vec<Value>,
    },
    /// A built-in implemented in Rust.
    ///
    /// The `name` and `length` a script observes are ordinary properties on the
    /// function object, not fields here: they are writable (`f.name = "x"` works)
    /// and would otherwise be two sources of truth.
    Native {
        /// The implementation.
        func: NativeFn,
    },
}

impl Trace for Callable {
    fn trace(&self, tracer: &mut Tracer) {
        match self {
            Callable::Bytecode { code, env } => {
                tracer.visit(code);
                tracer.visit(env);
            }
            Callable::Bound { target, this, args } => {
                target.trace(tracer);
                this.trace(tracer);
                for argument in args {
                    argument.trace(tracer);
                }
            }
            Callable::Native { .. } => {}
        }
    }
}

/// The bytecode of one function, plus the slot-to-cell mapping.
///
/// This is a separate GC object from the closure so that a frame can hold a handle
/// to it and read it while the function object itself is free to be borrowed.
/// Holding a `RefCell` borrow across a call would deadlock the moment a function
/// referred to itself.
pub struct CompiledCode {
    /// The resolved bytecode.
    pub code: CompiledFunction,
    /// For each argument slot, the index of its reference cell, or `-1` when the
    /// slot holds its value directly.
    ///
    /// A captured binding lives in a cell so that closures outlive the frame; every
    /// other slot is a plain value. Precomputing the mapping keeps the hot
    /// `get_arg`/`put_arg` paths to one array read instead of a lookup.
    pub arg_cell: Vec<i32>,
    /// The same, for local slots.
    pub loc_cell: Vec<i32>,
}

impl CompiledCode {
    /// Build the slot-to-cell mapping for `code`.
    ///
    /// Cells are numbered the way [`CompiledFunction::var_ref_count`] counts them:
    /// the captured arguments first, then the captured locals, each in slot order.
    pub fn new(code: CompiledFunction) -> CompiledCode {
        let mut next = 0i32;
        let mut arg_cell = vec![-1; code.args.len()];
        for (slot, arg) in code.args.iter().enumerate() {
            if arg.is_captured {
                arg_cell[slot] = next;
                next += 1;
            }
        }
        let mut loc_cell = vec![-1; code.vars.len()];
        for (slot, local) in code.vars.iter().enumerate() {
            if local.is_captured {
                loc_cell[slot] = next;
                next += 1;
            }
        }
        CompiledCode {
            code,
            arg_cell,
            loc_cell,
        }
    }
}

impl Trace for CompiledCode {
    fn trace(&self, _tracer: &mut Tracer) {}
}

/// The reference cells a closure captured from enclosing scopes.
pub struct ClosureEnv {
    /// Indexed the way the bytecode's closure-variable table indexes them.
    pub cells: Vec<Gc<Cell>>,
    /// The `eval` scope this closure was created in, if any.
    ///
    /// A function built by `eval` has to keep seeing the caller's bindings after the
    /// eval has returned — `for (let i …) eval("push(() => i)")` collects closures that
    /// read `i` later. Those bindings live in the *caller's* frame, which is long gone
    /// by then, so the environment carries them by name instead. Ordinary closures leave
    /// this `None` and resolve globals exactly as before.
    pub eval_scope: Option<Gc<EvalScope>>,
    /// Variables a direct `eval` declared in the frame this closure was created in, and
    /// how much of that frame it may see.
    ///
    /// A closure is created at a *place* in a function, and a direct `eval` can add
    /// variables after that point. Those variables belong to the variable environment
    /// that was current at the closure's creation — the parameter list or the body — which
    /// is what `local_phase` records.
    pub local_vars: Option<Gc<LocalVars>>,
    /// `0` for a closure created in a parameter list, `1` for one in a body. A parameter
    /// list's closure must not see variables a later `eval` in the body declared:
    /// `function f(a, p = () => c) { eval("var c = 1") }` has `p()` resolve `c` outside
    /// the function entirely.
    pub local_phase: u8,
}

/// Variables a direct `eval` declared in one function call.
///
/// A sloppy `eval` declares its `var`s in the caller's variable environment, not on the
/// global object: `function f() { eval("var x = 1") } f(); x` is a `ReferenceError`, and
/// two calls to `f` get two `x`s. Each entry therefore records the phase it was declared
/// in, so a lookup from the parameter list cannot see a body declaration.
pub struct LocalVars {
    names: std::cell::RefCell<Vec<(String, Gc<Cell>, u8)>>,
}

impl Default for LocalVars {
    fn default() -> LocalVars {
        LocalVars::new()
    }
}

impl LocalVars {
    /// An empty set.
    pub fn new() -> LocalVars {
        LocalVars {
            names: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The cell `name` resolves to, when it was declared in `phase` or earlier.
    pub fn get(&self, name: &str, phase: u8) -> Option<Gc<Cell>> {
        self.names
            .borrow()
            .iter()
            .find(|(bound, _, at)| bound == name && *at <= phase)
            .map(|(_, cell, _)| cell.clone())
    }

    /// The cell for `name`, declaring it if this is the first time.
    pub fn declare(&self, name: &str, phase: u8, cell: Gc<Cell>) -> Gc<Cell> {
        let mut names = self.names.borrow_mut();
        if let Some((_, cell, _)) = names.iter().find(|(bound, _, _)| bound == name) {
            return cell.clone();
        }
        names.push((name.to_string(), cell.clone(), phase));
        cell
    }

    /// Whether `name` has been declared at all.
    pub fn contains(&self, name: &str) -> bool {
        self.names
            .borrow()
            .iter()
            .any(|(bound, _, _)| bound == name)
    }
}

impl Trace for LocalVars {
    fn trace(&self, tracer: &mut Tracer) {
        for (_, cell, _) in self.names.borrow().iter() {
            tracer.visit(cell);
        }
    }
}

impl Trace for ClosureEnv {
    fn trace(&self, tracer: &mut Tracer) {
        for cell in &self.cells {
            tracer.visit(cell);
        }
        if let Some(scope) = &self.eval_scope {
            tracer.visit(scope);
        }
        if let Some(local_vars) = &self.local_vars {
            tracer.visit(local_vars);
        }
    }
}

/// The bindings a direct `eval` made visible, as name → cell.
///
/// Only bindings that already live in a cell appear here, because sharing their
/// identity is the point: a closure created by the eval must observe later writes to
/// the same variable, and a copy would not. The compiler puts every binding of a
/// function that contains a direct `eval` into a cell, so in practice that is all of
/// them.
pub struct EvalScope {
    /// In lookup order: the innermost binding for a name first.
    pub names: Vec<(String, Gc<Cell>)>,
}

impl EvalScope {
    /// The cell `name` resolves to, if this scope binds it.
    pub fn get(&self, name: &str) -> Option<&Gc<Cell>> {
        self.names
            .iter()
            .find(|(bound, _)| bound == name)
            .map(|(_, cell)| cell)
    }
}

impl Trace for EvalScope {
    fn trace(&self, tracer: &mut Tracer) {
        for (_, cell) in &self.names {
            tracer.visit(cell);
        }
    }
}

/// What kind of object this is.
pub enum ObjectKind {
    /// An ordinary object: only its property table.
    Ordinary,
    /// An array: dense elements plus a property table for everything else.
    Array {
        /// The dense prefix of the array.
        elements: Vec<Value>,
    },
    /// A function or a class constructor.
    Function(Callable),
    /// A regular expression: the compiled pattern, plus `lastIndex` as a property.
    RegExp(crate::builtins::regexp::RegExpData),
    /// A `DisposableStack` or `AsyncDisposableStack`: the resources it holds.
    DisposableStack(crate::builtins::disposable::StackState),
    /// An `ArrayBuffer`: a byte range that typed-array views share.
    ///
    /// The bytes sit behind an `Rc<RefCell<…>>` rather than in the object so that every view
    /// of the buffer sees the same storage; the *views* hold the `Gc<JsObject>` of this
    /// buffer, which is what keeps it alive and what the collector traces.
    ArrayBuffer(std::rc::Rc<std::cell::RefCell<BufferStorage>>),
    /// A typed-array view: a window onto an `ArrayBuffer`.
    TypedArray {
        /// The buffer whose bytes this view reads.
        buffer: Gc<JsObject>,
        /// What one element is.
        element: crate::builtins::typedarray::ElementType,
        /// Byte offset of element zero.
        offset: usize,
        /// How many elements the view exposes.
        length: usize,
    },
    /// A generator object: a detached frame plus how far it got.
    ///
    /// The frame lives here rather than in the interpreter because a suspended generator
    /// has to keep its locals and operand stack alive for as long as the script can reach
    /// the generator object — and the collector has to see them, which it only does for
    /// things hanging off a `Gc`.
    Generator(Box<crate::interp::GeneratorState>),
}

impl Trace for ObjectKind {
    fn trace(&self, tracer: &mut Tracer) {
        match self {
            // Nothing in a compiled pattern is a GC value: it holds characters and
            // integers only.
            ObjectKind::RegExp(_) => {}
            ObjectKind::DisposableStack(state) => state.trace(tracer),
            // A buffer's bytes are plain bytes and hold no GC values; a view keeps its
            // buffer alive through the `Gc` it carries.
            ObjectKind::ArrayBuffer(_) => {}
            ObjectKind::TypedArray { buffer, .. } => tracer.visit(buffer),
            ObjectKind::Ordinary => {}
            ObjectKind::Array { elements } => {
                for element in elements {
                    element.trace(tracer);
                }
            }
            ObjectKind::Function(callable) => callable.trace(tracer),
            ObjectKind::Generator(state) => state.trace(tracer),
        }
    }
}

/// The mutable half of an object.
/// What a property is keyed by.
///
/// A string and a symbol are separate key spaces even when they look alike: `o["a"]`
/// and `o[Symbol("a")]` are two properties, and `o[Symbol("a")]` is not reachable by
/// any string. Symbols are also invisible to `for`/`Object.keys`/`JSON.stringify`,
/// which is the whole point of them, so the distinction has to survive into the
/// property table rather than being flattened into a name.
#[derive(Clone)]
pub enum PropertyKey {
    /// An ordinary string key.
    String(String),
    /// A symbol key.
    Symbol(Gc<JsSymbol>),
}

impl PropertyKey {
    /// The key as a string, for the operations that only accept one.
    ///
    /// A symbol has no string form that would address it, so this answers `None` —
    /// which is what makes `get_field` on a symbol key fall through to the general
    /// path instead of silently reading a same-named string property.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            PropertyKey::String(text) => Some(text),
            PropertyKey::Symbol(_) => None,
        }
    }

    /// Whether this is an ordinary string key.
    pub fn is_symbol(&self) -> bool {
        matches!(self, PropertyKey::Symbol(_))
    }
}

impl fmt::Display for PropertyKey {
    /// How a key reads inside an error message: strings bare, symbols in their
    /// `Symbol(description)` form.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropertyKey::String(text) => write!(f, "{text}"),
            PropertyKey::Symbol(symbol) => write!(f, "{}", symbol.to_display_string()),
        }
    }
}

impl PartialEq for PropertyKey {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (PropertyKey::String(a), PropertyKey::String(b)) => a == b,
            // Symbols compare by identity, never by description.
            (PropertyKey::Symbol(a), PropertyKey::Symbol(b)) => a.ptr_eq(b),
            _ => false,
        }
    }
}

impl From<&str> for PropertyKey {
    fn from(text: &str) -> PropertyKey {
        PropertyKey::String(text.to_string())
    }
}

impl Trace for PropertyKey {
    fn trace(&self, tracer: &mut Tracer) {
        if let PropertyKey::Symbol(symbol) = self {
            tracer.visit(symbol);
        }
    }
}

/// An object's mutable state: its prototype, property table and internal slots.
pub struct ObjectData {
    /// The prototype, or `None` for `Object.prototype` and for a null-prototype
    /// object created with `Object.create(null)`.
    pub proto: Option<Gc<JsObject>>,
    /// Properties in definition order, string and symbol keys interleaved.
    pub props: Vec<(PropertyKey, Property)>,
    /// What kind of object this is.
    pub kind: ObjectKind,
    /// Whether new properties may be added. `Object.preventExtensions`, `seal` and
    /// `freeze` clear it, and nothing clears it back.
    pub extensible: bool,
    /// Whether array elements may be replaced.
    ///
    /// Elements are not in the property table, so their attributes need somewhere to
    /// live; `Object.freeze` clears this and `Object.isFrozen` reads it. A *single*
    /// non-writable element is recorded in the property table instead, which is what
    /// `Object.defineProperty(a, "3", {configurable: false})` does.
    pub elements_writable: bool,
    /// The object a method was defined on, which is what `super` resolves against.
    ///
    /// Kept here rather than as a property because it is not observable: the spec calls
    /// it `[[HomeObject]]` and no script can read it, so putting it in the property
    /// table would leak it to `Object.getOwnPropertyNames` and `JSON.stringify`.
    pub home_object: Option<Gc<JsObject>>,
    /// Whether this function is a class constructor, which may not be called without
    /// `new`.
    pub class_constructor: bool,
}

impl Trace for ObjectData {
    fn trace(&self, tracer: &mut Tracer) {
        if let Some(proto) = &self.proto {
            tracer.visit(proto);
        }
        if let Some(home) = &self.home_object {
            tracer.visit(home);
        }
        for (key, property) in &self.props {
            key.trace(tracer);
            property.trace(tracer);
        }
        self.kind.trace(tracer);
    }
}

/// A JavaScript object.
///
/// The interior mutability is what makes `const o = {}; o.x = 1` work: the handle
/// is immutable, the object it points at is not.
pub struct JsObject {
    data: RefCell<ObjectData>,
}

impl JsObject {
    fn new(kind: ObjectKind, proto: Option<Gc<JsObject>>) -> JsObject {
        JsObject {
            data: RefCell::new(ObjectData {
                proto,
                props: Vec::new(),
                kind,
                extensible: true,
                elements_writable: true,
                home_object: None,
                class_constructor: false,
            }),
        }
    }

    /// An ordinary object with the given prototype.
    pub fn ordinary(heap: &js_gc::Heap, proto: Option<Gc<JsObject>>) -> Gc<JsObject> {
        Gc::new(heap, JsObject::new(ObjectKind::Ordinary, proto))
    }

    /// An array with the given prototype and initial capacity.
    pub fn array(heap: &js_gc::Heap, proto: Option<Gc<JsObject>>, capacity: usize) -> Gc<JsObject> {
        Gc::new(
            heap,
            JsObject::new(
                ObjectKind::Array {
                    elements: Vec::with_capacity(capacity),
                },
                proto,
            ),
        )
    }

    /// A callable object.
    pub fn function(
        heap: &js_gc::Heap,
        proto: Option<Gc<JsObject>>,
        callable: Callable,
    ) -> Gc<JsObject> {
        Gc::new(heap, JsObject::new(ObjectKind::Function(callable), proto))
    }

    /// Run `f` with the object's data borrowed.
    pub fn with<R>(&self, f: impl FnOnce(&ObjectData) -> R) -> R {
        f(&self.data.borrow())
    }

    /// Run `f` with the object's data mutably borrowed.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut ObjectData) -> R) -> R {
        f(&mut self.data.borrow_mut())
    }

    /// Whether this object can be called.
    ///
    /// Covers every [`Callable`] variant, `Bound` included: `typeof bound ===
    /// "function"` and `bound()` works, so from a script's point of view a bound
    /// function is a function.
    pub fn is_callable(&self) -> bool {
        matches!(self.data.borrow().kind, ObjectKind::Function(_))
    }

    /// The prototype.
    pub fn proto(&self) -> Option<Gc<JsObject>> {
        self.data.borrow().proto.clone()
    }

    /// Replace the prototype.
    pub fn set_proto(&self, proto: Option<Gc<JsObject>>) {
        self.data.borrow_mut().proto = proto;
    }

    /// The object a method was defined on, for `super` to resolve against.
    pub fn home_object(&self) -> Option<Gc<JsObject>> {
        self.data.borrow().home_object.clone()
    }

    /// Record the object a method was defined on.
    pub fn set_home_object(&self, home: Option<Gc<JsObject>>) {
        self.data.borrow_mut().home_object = home;
    }

    /// Whether this is a class constructor.
    pub fn is_class_constructor(&self) -> bool {
        self.data.borrow().class_constructor
    }

    /// Mark this function as a class constructor.
    pub fn set_class_constructor(&self, yes: bool) {
        self.data.borrow_mut().class_constructor = yes;
    }

    /// The elements of an array object, if this is one.
    pub fn array_elements(&self) -> Option<Vec<Value>> {
        match &self.data.borrow().kind {
            ObjectKind::Array { elements } => Some(elements.clone()),
            _ => None,
        }
    }

    /// Whether this object has array elements at all.
    pub fn is_array(&self) -> bool {
        matches!(self.data.borrow().kind, ObjectKind::Array { .. })
    }

    /// `length` for an array, reading it out of the element vector.
    /// The compiled pattern, when this object is a `RegExp`.
    pub fn regexp(&self) -> Option<crate::builtins::regexp::RegExpData> {
        match &self.data.borrow().kind {
            ObjectKind::RegExp(data) => Some(data.clone()),
            _ => None,
        }
    }

    /// The held resources, when this object is a `DisposableStack`.
    pub fn disposable_stack(&self) -> Option<crate::builtins::disposable::StackState> {
        match &self.data.borrow().kind {
            ObjectKind::DisposableStack(state) => Some(state.clone()),
            _ => None,
        }
    }

    /// Make this object a `DisposableStack` holding `state`.
    pub fn set_disposable_stack(&self, state: crate::builtins::disposable::StackState) {
        self.data.borrow_mut().kind = ObjectKind::DisposableStack(state);
    }

    /// Make this object a `RegExp` carrying `data`.
    pub fn set_regexp(&self, data: crate::builtins::regexp::RegExpData) {
        self.data.borrow_mut().kind = ObjectKind::RegExp(data);
    }

    /// Whether this object is a `RegExp`.
    pub fn is_regexp(&self) -> bool {
        matches!(self.data.borrow().kind, ObjectKind::RegExp(_))
    }

    /// An array's element count, when this object is an array.
    pub fn array_length(&self) -> Option<usize> {
        match &self.data.borrow().kind {
            ObjectKind::Array { elements } => Some(elements.len()),
            _ => None,
        }
    }

    /// Read one array element.
    pub fn array_get(&self, index: usize) -> Option<Value> {
        match &self.data.borrow().kind {
            ObjectKind::Array { elements } => elements.get(index).cloned(),
            _ => None,
        }
    }

    /// Append to an array and return the new length.
    pub fn array_push(&self, value: Value) -> usize {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements } => {
                elements.push(value);
                elements.len()
            }
            _ => 0,
        }
    }

    /// Store into an array, and return whether the write happened.
    ///
    /// The gap an out-of-range store leaves behind is filled with [`Value::Hole`], not
    /// `undefined`: `a = [1]; a[3] = 4` has holes at 1 and 2, which `forEach` skips
    /// and the debug printer reports. A store *of* `undefined` is a real element.
    pub fn array_set(&self, index: usize, value: Value) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements } => {
                if index >= elements.len() {
                    elements.resize(index + 1, Value::Hole);
                }
                elements[index] = value;
                true
            }
            _ => false,
        }
    }

    /// Store an element, honouring its attributes, reporting whether it happened.
    ///
    /// Array methods that write back (`sort`, `reverse`, `splice`, …) use this rather
    /// than [`Self::array_set`]: the spec has them go through `[[Set]]`, so a frozen
    /// element makes them throw instead of being quietly overwritten.
    pub fn array_store(&self, index: usize, value: Value) -> bool {
        if self.elements_writable() {
            return self.array_set(index, value);
        }
        if self.element_attributes(index as u32).is_some() {
            // A recorded descriptor decides on its own.
            return !self.element_is_locked(index as u32) && self.array_set(index, value);
        }
        false
    }

    /// Store `value` as a plain element without creating holes, used by `push`.
    pub fn array_push_hole(&self) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements } => {
                elements.push(Value::Hole);
                true
            }
            _ => false,
        }
    }

    /// Set the length of an array, truncating or extending with holes.
    ///
    /// `a.length = 5` on a two-element array leaves three holes, so the fill value is
    /// [`Value::Hole`] rather than `undefined`.
    pub fn array_set_length(&self, length: usize) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements } => {
                elements.resize(length, Value::Hole);
                true
            }
            _ => false,
        }
    }

    /// Truncate an array to `length` elements without extending it.
    ///
    /// This is what `arr.length = n` does for `n` smaller than the current length;
    /// it is separate from [`Self::array_set_length`] because growing an array by
    /// assignment produces holes, which are indistinguishable from `undefined`
    /// here.
    pub fn array_truncate(&self, length: usize) -> bool {
        match &mut self.data.borrow_mut().kind {
            ObjectKind::Array { elements } => {
                if length < elements.len() {
                    elements.truncate(length);
                }
                true
            }
            _ => false,
        }
    }

    /// The own data property `key`, if there is one.
    pub fn own_property_key(&self, key: &PropertyKey) -> Option<Property> {
        let data = self.data.borrow();
        data.props
            .iter()
            .find(|(existing, _)| existing == key)
            .map(|(_, property)| property.clone())
    }

    /// The own data property `name`, if there is one.
    pub fn own_property(&self, name: &str) -> Option<Property> {
        self.own_property_key(&PropertyKey::from(name))
    }

    /// Whether the object has an own property `key`.
    pub fn has_own_key(&self, key: &PropertyKey) -> bool {
        let data = self.data.borrow();
        data.props.iter().any(|(existing, _)| existing == key)
    }

    /// Whether the object has an own property `name`.
    pub fn has_own(&self, name: &str) -> bool {
        self.has_own_key(&PropertyKey::from(name))
    }

    /// Define or replace an own property, keeping the original position when the
    /// key already exists.
    pub fn define_key(&self, key: &PropertyKey, property: Property) {
        let mut data = self.data.borrow_mut();
        match data.props.iter_mut().find(|(existing, _)| existing == key) {
            Some((_, slot)) => *slot = property,
            None => data.props.push((key.clone(), property)),
        }
    }

    /// Define or replace an own property under a string key.
    pub fn define(&self, name: &str, property: Property) {
        self.define_key(&PropertyKey::from(name), property);
    }

    /// Define an own data property, with the default attributes.
    ///
    /// This *creates*: on an existing property it overwrites the attributes too, so
    /// it is the wrong call for a plain assignment. Use [`Self::write_value`] for that.
    pub fn define_data(&self, name: &str, value: Value) {
        self.define(name, Property::data(value));
    }

    /// Define an own data property under `key`.
    pub fn define_data_key(&self, key: &PropertyKey, value: Value) {
        self.define_key(key, Property::data(value));
    }

    /// Store a value into an existing own data property, keeping its attributes.
    ///
    /// Returns `false` when the property is absent, is an accessor, or is not
    /// writable. An assignment has to go through this rather than
    /// [`Self::define_data`], because redefining is how a non-configurable global
    /// silently became configurable again and started answering `delete` with `true`.
    pub fn write_value(&self, name: &str, value: Value) -> bool {
        self.write_value_key(&PropertyKey::from(name), value)
    }

    /// Store a value into an existing own data property under `key`.
    pub fn write_value_key(&self, key: &PropertyKey, value: Value) -> bool {
        let mut data = self.data.borrow_mut();
        let Some((_, property)) = data.props.iter_mut().find(|(existing, _)| existing == key)
        else {
            return false;
        };
        if property.is_accessor() || !property.writable {
            return false;
        }
        property.value = Some(value);
        true
    }

    /// Delete an own property, reporting whether it was there and removable.
    pub fn delete(&self, name: &str) -> bool {
        self.delete_key(&PropertyKey::from(name))
    }

    /// Delete an own property under `key`.
    pub fn delete_key(&self, key: &PropertyKey) -> bool {
        let mut data = self.data.borrow_mut();
        match data.props.iter().position(|(existing, _)| existing == key) {
            Some(index) => {
                if data.props[index].1.configurable {
                    data.props.remove(index);
                    true
                } else {
                    false
                }
            }
            None => true,
        }
    }

    /// Whether new properties may be added.
    pub fn is_extensible(&self) -> bool {
        self.data.borrow().extensible
    }

    /// Make the object non-extensible.
    pub fn set_extensible(&self, extensible: bool) {
        self.data.borrow_mut().extensible = extensible;
    }

    /// Whether array elements may be replaced.
    pub fn elements_writable(&self) -> bool {
        self.data.borrow().elements_writable
    }

    /// Make array elements read-only, as `Object.freeze` does.
    pub fn set_elements_writable(&self, writable: bool) {
        self.data.borrow_mut().elements_writable = writable;
    }

    /// The attributes recorded for an array index, if `defineProperty` set any.
    pub fn element_attributes(&self, index: u32) -> Option<Property> {
        self.own_property(&index.to_string())
    }

    /// Whether a store at `index` would be rejected.
    ///
    /// An element is locked either by `Object.freeze` on the whole object or by a
    /// non-configurable, non-writable descriptor recorded for that one index.
    pub fn element_is_locked(&self, index: u32) -> bool {
        if !self.elements_writable() {
            return true;
        }
        self.element_attributes(index)
            .is_some_and(|property| !property.writable && !property.configurable)
    }

    /// The own property *names*, in definition order. Symbol keys are excluded:
    /// `Object.keys` and `for…in` cannot see them.
    pub fn own_keys(&self) -> Vec<String> {
        self.data
            .borrow()
            .props
            .iter()
            .filter_map(|(key, _)| key.as_str().map(str::to_string))
            .collect()
    }

    /// The own *enumerable* property names, in definition order.
    pub fn own_enumerable_keys(&self) -> Vec<String> {
        self.data
            .borrow()
            .props
            .iter()
            .filter(|(_, property)| property.enumerable)
            .filter_map(|(key, _)| key.as_str().map(str::to_string))
            .collect()
    }

    /// The own symbol keys, in definition order, for `Object.getOwnPropertySymbols`.
    pub fn own_symbol_keys(&self) -> Vec<Gc<JsSymbol>> {
        self.data
            .borrow()
            .props
            .iter()
            .filter_map(|(key, _)| match key {
                PropertyKey::Symbol(symbol) => Some(symbol.clone()),
                PropertyKey::String(_) => None,
            })
            .collect()
    }

    /// Every own key, strings first then symbols — the order `Reflect.ownKeys` and
    /// `Object.getOwnPropertyDescriptors` report, and the order `JSON.stringify`
    /// walks before it skips the symbols.
    pub fn own_property_keys(&self) -> Vec<PropertyKey> {
        let data = self.data.borrow();
        let mut strings: Vec<PropertyKey> = Vec::new();
        let mut symbols: Vec<PropertyKey> = Vec::new();
        for (key, _) in &data.props {
            if key.is_symbol() {
                symbols.push(key.clone());
            } else {
                strings.push(key.clone());
            }
        }
        strings.extend(symbols);
        strings
    }

    /// The prototype chain, starting with the prototypes of this object.
    pub fn chain(&self) -> Vec<Gc<JsObject>> {
        let mut out = Vec::new();
        let mut proto = self.proto();
        while let Some(object) = proto {
            proto = object.proto();
            out.push(object);
        }
        out
    }
}

/// Walk the prototype chain from `object` looking for `name`.
///
/// Returns the object that owns the property along with the property itself. The
/// owner matters for a store: an inherited accessor is invoked on the receiver,
/// while an inherited *data* property is shadowed by a new own property.
pub fn lookup_key(object: &Gc<JsObject>, key: &PropertyKey) -> Option<(Gc<JsObject>, Property)> {
    if let Some(property) = object.own_property_key(key) {
        return Some((object.clone(), property));
    }
    for ancestor in object.chain() {
        if let Some(property) = ancestor.own_property_key(key) {
            return Some((ancestor, property));
        }
    }
    None
}

/// [`lookup_key`] for a string key.
pub fn lookup(object: &Gc<JsObject>, name: &str) -> Option<(Gc<JsObject>, Property)> {
    lookup_key(object, &PropertyKey::from(name))
}

/// The array index a property key names, if it names one.
///
/// A key is an index when it is the canonical decimal spelling of an integer below
/// `2^32 - 1` — the same rule the atom table uses to decide whether a key is a
/// tagged index. `"01"` and `"-1"` are ordinary string keys.
pub fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 {
        return None;
    }
    let bytes = key.as_bytes();
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() != 1 {
        return None;
    }
    let value: u32 = key.parse().ok()?;
    // `2^32 - 1` is reserved for the array length, so it is not an index.
    (value < u32::MAX).then_some(value)
}

impl Trace for JsObject {
    fn trace(&self, tracer: &mut Tracer) {
        self.data.borrow().trace(tracer);
    }
}

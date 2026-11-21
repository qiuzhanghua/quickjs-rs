//! The bytecode interpreter.
//!
//! The machine is the same shape as the reference's: a value stack per frame, a
//! program counter into `js_compile`'s resolved bytecode, and a frame chain. Two
//! properties of that design are worth stating up front because they are visible
//! everywhere below.
//!
//! **Frames are explicit, not recursive.** A JavaScript call pushes a frame onto
//! [`Interp::frames`] and the loop continues; it never recurses in Rust. Deep
//! JavaScript recursion therefore costs heap, not C stack, and the depth limit is
//! ours to enforce rather than the host's to crash.
//!
//! **Exceptions live on the value stack.** `catch` pushes a [`Value::CatchOffset`]
//! marker and an exception is found by walking the frame's stack downwards looking
//! for the innermost one. There is no side table, which is why a `try` inside a
//! loop costs nothing until it throws.

use std::collections::BTreeMap;

use js_compile::bytecode::{Atom, ClosureType, Constant, Function as CompiledFunction};
use js_compile::opcode::{OpFormat, Opcode};
use js_gc::{Gc, Heap};

use crate::object::{
    Callable, ClosureEnv, CompiledCode, JsObject, NativeFn, ObjectKind, Property, array_index,
    lookup,
};
use crate::value::{Cell, Value, number_to_string};

/// Why execution stopped.
#[derive(Debug, Clone)]
pub enum Signal {
    /// A JavaScript exception carrying the thrown value.
    Throw(Value),
    /// The runtime ran out of memory.
    OutOfMemory,
}

/// What an execution step produced.
pub type JsResult<T> = std::result::Result<T, Signal>;

/// The result of one instruction.
enum Step {
    /// Keep executing the current frame.
    Continue,
    /// The current frame returned this value.
    Return(Value),
}

/// Everything one JavaScript realm owns.
///
/// A realm is the set of objects a script can reach without naming them: the global
/// object, the prototype chain roots, and the global binding cells.
pub struct Realm {
    /// The global object.
    pub global: Gc<JsObject>,
    /// `Object.prototype`.
    pub object_proto: Gc<JsObject>,
    /// `Function.prototype`, which is itself callable.
    pub function_proto: Gc<JsObject>,
    /// `Array.prototype`.
    pub array_proto: Gc<JsObject>,
    /// Reference cells for globals declared by the script.
    ///
    /// A declared global is read with `get_var` (by name) but written with
    /// `put_var_ref` (through the cell), so the cell is what makes a function
    /// declaration and its uses agree on one binding.
    pub globals: BTreeMap<String, Gc<Cell>>,
    /// Names of the globals that came from a *declaration*, which are
    /// non-configurable properties of the global object.
    pub declared: Vec<String>,
}

/// One activation record.
struct Frame {
    /// The bytecode being run.
    code: Gc<CompiledCode>,
    /// The cells this closure captured.
    env: Gc<ClosureEnv>,
    /// The `this` binding.
    this: Value,
    /// Argument slots.
    args: Vec<Value>,
    /// Local slots.
    locals: Vec<Value>,
    /// Reference cells for this frame's own captured bindings.
    cells: Vec<Gc<Cell>>,
    /// The operand stack.
    stack: Vec<Value>,
    /// Byte offset of the next instruction.
    pc: usize,
    /// The callee, kept for the stack traces that arrive with the error objects.
    #[allow(dead_code)]
    callee: Value,
}

/// The interpreter: a realm plus a frame stack.
pub struct Interp<'a> {
    /// The heap everything is allocated on.
    pub heap: &'a Heap,
    /// The realm.
    pub realm: Realm,
    /// Active frames, outermost first.
    frames: Vec<Frame>,
    /// Text written by `print` and friends, in order.
    ///
    /// Buffered rather than streamed so a test can inspect it; the CLI flushes it
    /// once at the end, which is also when upstream's line buffering becomes
    /// visible for the cases compared here.
    pub output: String,
    /// The most frames that may be live at once.
    pub max_frames: usize,
}

impl<'a> Interp<'a> {
    /// Build an interpreter over `heap`, with a fresh realm.
    pub fn new(heap: &'a Heap) -> Interp<'a> {
        let object_proto = JsObject::ordinary(heap, None);
        let function_proto = JsObject::function(
            heap,
            Some(object_proto.clone()),
            Callable::Native {
                name: "",
                length: 0,
                func: |_, _, _| Ok(Value::Undefined),
            },
        );
        let array_proto = JsObject::array(heap, Some(object_proto.clone()), 0);
        let global = JsObject::ordinary(heap, Some(object_proto.clone()));
        let mut interp = Interp {
            heap,
            realm: Realm {
                global,
                object_proto,
                function_proto,
                array_proto,
                globals: BTreeMap::new(),
                declared: Vec::new(),
            },
            frames: Vec::new(),
            output: String::new(),
            max_frames: 4096,
        };
        crate::builtins::install(&mut interp);
        interp
    }

    // -- allocation helpers -------------------------------------------------

    /// A new string value.
    pub fn string(&self, text: impl Into<String>) -> Value {
        Value::string(self.heap, text)
    }

    /// A new ordinary object with the given prototype.
    pub fn new_object(&self, proto: Option<Gc<JsObject>>) -> Gc<JsObject> {
        JsObject::ordinary(self.heap, proto)
    }

    /// A new array.
    pub fn new_array(&self, elements: Vec<Value>) -> Gc<JsObject> {
        let array = JsObject::array(
            self.heap,
            Some(self.realm.array_proto.clone()),
            elements.len(),
        );
        for (index, element) in elements.into_iter().enumerate() {
            array.array_set(index, element);
        }
        array
    }

    /// A new native function object.
    pub fn new_native(&self, name: &'static str, length: u32, func: NativeFn) -> Gc<JsObject> {
        JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Native { name, length, func },
        )
    }

    /// A new reference cell.
    pub fn new_cell(&self, value: Value) -> Gc<Cell> {
        Gc::new(self.heap, Cell::new(value))
    }

    // -- running ------------------------------------------------------------

    /// Evaluate `code` as a script body and return its completion value.
    pub fn run_script(&mut self, code: CompiledFunction) -> JsResult<Value> {
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        let closure_vars = compiled.code.closure_vars.clone();
        let env = self.global_env(&closure_vars);
        let this = Value::Object(self.realm.global.clone());
        let callee = Value::Undefined;
        self.execute(compiled, env, this, Vec::new(), callee)
    }

    /// The environment of a script or of any function compiled at global scope:
    /// every entry is a global binding, so each one resolves to the realm's cell.
    fn global_env(&mut self, closure_vars: &[js_compile::bytecode::ClosureVar]) -> Gc<ClosureEnv> {
        let mut cells = Vec::with_capacity(closure_vars.len());
        for closure_var in closure_vars {
            let cell = match closure_var.closure_type {
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport => {
                    // A declaration: the cell is created here, which is what makes
                    // the binding exist before its initialiser runs.
                    self.declare_global(&closure_var.name);
                    self.realm.globals[&closure_var.name].clone()
                }
                _ => {
                    // An undeclared global is read by name; the cell is a
                    // placeholder that nothing indexes.
                    self.new_cell(Value::Undefined)
                }
            };
            cells.push(cell);
        }
        Gc::new(self.heap, ClosureEnv { cells })
    }

    /// Push a frame and run until it returns.
    fn execute(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
        this: Value,
        args: Vec<Value>,
        callee: Value,
    ) -> JsResult<Value> {
        let depth = self.frames.len();
        self.push_frame(code, env, this, args, callee)?;
        loop {
            match self.step() {
                Ok(Step::Continue) => {}
                Ok(Step::Return(value)) => {
                    self.frames.pop();
                    if self.frames.len() == depth {
                        return Ok(value);
                    }
                    // The caller resumes with the value on its stack.
                    self.frames
                        .last_mut()
                        .expect("a caller is live")
                        .stack
                        .push(value);
                }
                Err(Signal::Throw(value)) => {
                    // A handler lives on the stack of the frame that is throwing —
                    // `catch` pushed it there — so this frame gets the first chance
                    // to catch before it is popped.
                    let value = value;
                    loop {
                        if self.frames.len() <= depth {
                            return Err(Signal::Throw(value));
                        }
                        if self.catch_here(value.clone()).is_some() {
                            break;
                        }
                        self.frames.pop();
                    }
                }
                Err(Signal::OutOfMemory) => return Err(Signal::OutOfMemory),
            }
        }
    }

    /// Install a handler for `value` if this frame has one, resuming at it.
    ///
    /// The scan is the reference's: walk the frame's stack downwards looking for the
    /// innermost [`Value::CatchOffset`], discarding whatever sat above it. Offset
    /// `0` is an enumerator marker rather than a handler — it exists so that a
    /// `break` out of a `for-of` can find and close the iterator — so it is skipped
    /// here and the unwinding continues.
    fn catch_here(&mut self, value: Value) -> Option<()> {
        let frame = self.frames.last_mut()?;
        while let Some(top) = frame.stack.pop() {
            if let Value::CatchOffset(offset) = top {
                if offset == 0 {
                    continue;
                }
                frame.stack.push(value);
                frame.pc = offset as usize;
                return Some(());
            }
        }
        None
    }

    fn push_frame(
        &mut self,
        code: Gc<CompiledCode>,
        env: Gc<ClosureEnv>,
        this: Value,
        mut args: Vec<Value>,
        callee: Value,
    ) -> JsResult<()> {
        if self.frames.len() >= self.max_frames {
            return Err(Signal::Throw(
                self.string("Maximum call stack size exceeded"),
            ));
        }
        let arg_count = code.code.args.len();
        // Missing arguments are `undefined`, which is different from not passing
        // them only for `arguments`, which this milestone does not implement.
        args.resize(arg_count, Value::Undefined);
        args.truncate(arg_count);
        let cell_count = code.code.var_ref_count as usize;
        let mut cells = Vec::with_capacity(cell_count);
        for _ in 0..cell_count {
            cells.push(self.new_cell(Value::Undefined));
        }
        let locals = vec![Value::Undefined; code.code.vars.len()];
        // A captured argument lives in its cell, not in the argument slot, so the
        // incoming value has to be moved there before the body runs.
        for (slot, cell) in code.arg_cell.iter().enumerate() {
            if *cell >= 0
                && let Some(value) = args.get(slot).cloned()
            {
                cells[*cell as usize].set(value);
            }
        }
        // A sloppy-mode function called without a receiver sees the global object.
        let this = if this.is_nullish() && !code.code.strict {
            Value::Object(self.realm.global.clone())
        } else {
            this
        };
        self.frames.push(Frame {
            code,
            env,
            this,
            args,
            locals,
            cells,
            stack: Vec::with_capacity(16),
            pc: 0,
            callee,
        });
        Ok(())
    }

    fn frame(&self) -> &Frame {
        self.frames.last().expect("a frame is live")
    }

    fn frame_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("a frame is live")
    }

    // -- slot access --------------------------------------------------------

    fn read_slot(&self, cell: i32, value: &Value) -> Value {
        match cell {
            -1 => value.clone(),
            index => self
                .frame()
                .cells
                .get(index as usize)
                .expect("a cell index in range")
                .get(),
        }
    }

    fn get_arg(&self, slot: usize) -> Value {
        let frame = self.frame();
        let cell = frame.code.arg_cell.get(slot).copied().unwrap_or(-1);
        let value = frame.args.get(slot).cloned().unwrap_or(Value::Undefined);
        self.read_slot(cell, &value)
    }

    fn put_arg(&mut self, slot: usize, value: Value) {
        let cell = self.frame().code.arg_cell.get(slot).copied().unwrap_or(-1);
        if cell == -1 {
            if let Some(slot) = self.frame_mut().args.get_mut(slot) {
                *slot = value;
            }
        } else {
            self.frame().cells[cell as usize].set(value);
        }
    }

    fn get_loc(&self, slot: usize) -> Value {
        let frame = self.frame();
        let cell = frame.code.loc_cell.get(slot).copied().unwrap_or(-1);
        let value = frame.locals.get(slot).cloned().unwrap_or(Value::Undefined);
        self.read_slot(cell, &value)
    }

    fn put_loc(&mut self, slot: usize, value: Value) {
        let cell = self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1);
        if cell == -1 {
            if let Some(slot) = self.frame_mut().locals.get_mut(slot) {
                *slot = value;
            }
        } else {
            self.frame().cells[cell as usize].set(value);
        }
    }

    // -- globals ------------------------------------------------------------

    /// The name a global-slot operand refers to.
    fn global_name(&self, slot: usize) -> String {
        self.frame()
            .code
            .code
            .closure_vars
            .get(slot)
            .map(|c| c.name.clone())
            .unwrap_or_default()
    }

    fn get_global(&mut self, slot: usize) -> JsResult<Value> {
        let name = self.global_name(slot);
        if let Some(cell) = self.realm.globals.get(&name) {
            let value = cell.get();
            if matches!(value, Value::Uninitialized) {
                return Err(self.throw_reference(&name));
            }
            return Ok(value);
        }
        match self.get_property(&self.realm.global.clone(), &name)? {
            Some(value) => Ok(value),
            None => Err(self.throw_reference(&name)),
        }
    }

    fn put_global(&mut self, slot: usize, value: Value) {
        let name = self.global_name(slot);
        if let Some(cell) = self.realm.globals.get(&name).cloned() {
            cell.set(value);
            return;
        }
        let global = self.realm.global.clone();
        global.define_data(&name, value);
    }

    // -- properties ---------------------------------------------------------

    /// Read `object[key]`, following the prototype chain.
    ///
    /// An inherited accessor runs with the *receiver* as `this`, which is why the
    /// lookup returns the owner rather than just the value.
    pub fn get_property(&mut self, object: &Gc<JsObject>, key: &str) -> JsResult<Option<Value>> {
        if let Some(value) = self.own_element(object, key) {
            return Ok(Some(value));
        }
        // `length` is not stored as a property: an array's length is its element
        // count, and keeping one copy of it is what makes `push`/`pop` and
        // `a.length = n` agree.
        if key == "length"
            && let Some(length) = object.array_length()
        {
            return Ok(Some(Value::Int(length as i32)));
        }
        match lookup(object, key) {
            Some((_, property)) => {
                if property.is_accessor() {
                    match &property.getter {
                        Some(getter) if !matches!(getter, Value::Undefined) => {
                            let receiver = Value::Object(object.clone());
                            self.call_value(getter.clone(), receiver, Vec::new())
                                .map(Some)
                        }
                        // An accessor with no getter reads as `undefined`.
                        _ => Ok(Some(Value::Undefined)),
                    }
                } else {
                    Ok(property.value)
                }
            }
            None => Ok(None),
        }
    }

    /// The own array element `key` names, if this object has one.
    fn own_element(&self, object: &Gc<JsObject>, key: &str) -> Option<Value> {
        let index = array_index(key)?;
        object.array_get(index as usize)
    }

    /// Store `object[key] = value`, honouring setters and inherited properties.
    pub fn set_property(&mut self, object: &Gc<JsObject>, key: &str, value: Value) -> JsResult<()> {
        if let Some(index) = array_index(key) {
            if object.is_array() {
                object.array_set(index as usize, value);
                return Ok(());
            }
        }
        if key == "length" && object.is_array() {
            let length = value.to_number();
            if length.is_finite() && length >= 0.0 {
                let length = length as usize;
                object.array_set_length(length);
                return Ok(());
            }
        }
        if let Some((owner, property)) = lookup(object, key) {
            if property.is_accessor() {
                if let Some(setter) = &property.setter
                    && !matches!(setter, Value::Undefined)
                {
                    let receiver = Value::Object(object.clone());
                    self.call_value(setter.clone(), receiver, vec![value])?;
                }
                return Ok(());
            }
            // An inherited *data* property is shadowed by an own property on the
            // receiver; a non-writable one is not.
            if owner.ptr_eq(object) && !property.writable {
                return Ok(());
            }
        }
        object.define_data(key, value);
        Ok(())
    }

    /// Delete `object[key]`.
    pub fn delete_property(&mut self, object: &Gc<JsObject>, key: &str) -> bool {
        if let Some(index) = array_index(key) {
            if let Some(elements) = object.array_elements()
                && (index as usize) < elements.len()
            {
                // A hole rather than a shift: `delete a[0]` leaves `length` alone.
                object.array_set(index as usize, Value::Undefined);
                return true;
            }
        }
        object.delete(key)
    }

    // -- calling ------------------------------------------------------------

    /// Call `callee` with `this` and `args`.
    pub fn call_value(&mut self, callee: Value, this: Value, args: Vec<Value>) -> JsResult<Value> {
        let Value::Object(function) = &callee else {
            // `typeof x === "function"` is not enough to make `x()` legal: only a
            // callable object is. `null` and `undefined` need no special wording
            // here — their `ToString` already reads the way the reference's does.
            let what = format!("{} is not a function", callee.to_js_string());
            return Err(Signal::Throw(self.make_error("TypeError", &what)));
        };
        let callable = function.with(|data| match &data.kind {
            ObjectKind::Function(callable) => match callable {
                Callable::Native { func, .. } => Some(NativeCall::Native(*func)),
                Callable::Bytecode { code, env } => {
                    Some(NativeCall::Bytecode(code.clone(), env.clone()))
                }
            },
            _ => None,
        });
        match callable {
            Some(NativeCall::Native(func)) => func(self, this, &args),
            Some(NativeCall::Bytecode(code, env)) => self.execute(code, env, this, args, callee),
            None => Err(Signal::Throw(
                self.make_error("TypeError", "not a function"),
            )),
        }
    }

    /// Give a freshly built function its own `prototype` object.
    ///
    /// Every function that can be used with `new` carries one, and its
    /// `constructor` points back at the function — a reference cycle, which is
    /// exactly what the collector's cycle pass exists to reclaim.
    fn install_function_prototype(&mut self, function: &Gc<JsObject>) {
        let prototype = self.new_object(Some(self.realm.object_proto.clone()));
        let constructor = Value::Object(function.clone());
        prototype.define(
            "constructor",
            Property::with_attributes(constructor, true, false),
        );
        let value = Value::Object(prototype);
        function.define("prototype", Property::with_attributes(value, true, false));
    }

    /// Call `callee`, or push its frame and let the main loop run it.
    ///
    /// This is what keeps deep JavaScript recursion off the Rust stack: a bytecode
    /// callee becomes a frame on [`Interp::frames`] and the instruction loop simply
    /// continues, so `function f(){ return f() }` costs heap and stops at
    /// [`Interp::max_frames`] instead of aborting the process. Only a native callee
    /// is entered directly, and natives do not re-enter JavaScript in this
    /// milestone.
    ///
    /// `Ok(None)` means a frame was pushed: the caller must not push a result,
    /// because the returning frame will.
    fn call_or_push(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Option<Value>> {
        if let Value::Object(function) = &callee {
            let bytecode = function.with(|data| match &data.kind {
                ObjectKind::Function(Callable::Bytecode { code, env }) => {
                    Some((code.clone(), env.clone()))
                }
                _ => None,
            });
            if let Some((code, env)) = bytecode {
                self.push_frame(code, env, this, args, callee)?;
                return Ok(None);
            }
        }
        self.call_value(callee, this, args).map(Some)
    }

    /// `new callee(args)`.
    ///
    /// A constructor differs from a plain call in three ways: it gets a fresh object
    /// as `this` whose prototype is the constructor's `prototype` property, its
    /// return value is *replaced* by that object unless it returns an object itself,
    /// and a native constructor is expected to initialise the object it is given
    /// rather than build one.
    pub fn construct(
        &mut self,
        callee: Value,
        _new_target: Value,
        args: Vec<Value>,
    ) -> JsResult<Value> {
        let Value::Object(function) = &callee else {
            return Err(Signal::Throw(
                self.make_error("TypeError", "not a constructor"),
            ));
        };
        // `new` on a native builtin: hand it the fresh object, which is how the
        // error classes and the object wrappers will initialise their own fields.
        let prototype = self.get_property(function, "prototype")?;
        let prototype = match prototype {
            Some(Value::Object(proto)) => Some(proto),
            _ => Some(self.realm.object_proto.clone()),
        };
        let this = Value::Object(self.new_object(prototype));
        let result = self.call_value(callee, this.clone(), args)?;
        // A constructor that returns an object overrides `this`; any other return
        // value (including `undefined`) is ignored.
        Ok(match result {
            Value::Object(_) => result,
            _ => this,
        })
    }

    /// A `TypeError`/`ReferenceError`/`RangeError` object, as the value to throw.
    ///
    /// The error classes themselves arrive with the builtins milestone; what
    /// matters here is that the thrown value stringifies the way a script printing
    /// the caught value expects.
    pub fn make_error(&self, name: &str, message: &str) -> Value {
        let error = self.new_object(Some(self.realm.object_proto.clone()));
        error.define_data("name", self.string(name));
        error.define_data("message", self.string(message));
        Value::Object(error)
    }

    /// The `ReferenceError` a read of an uninitialised binding raises.
    fn dead_zone(&self, slot: usize, kind: VarSlot) -> Signal {
        let name = match kind {
            VarSlot::Loc => self
                .frame()
                .code
                .code
                .vars
                .get(slot)
                .map(|v| v.name.clone()),
            VarSlot::Closure => self
                .frame()
                .code
                .code
                .closure_vars
                .get(slot)
                .map(|c| c.name.clone()),
        }
        .unwrap_or_default();
        Signal::Throw(self.make_error(
            "ReferenceError",
            &format!("cannot access '{name}' before initialization"),
        ))
    }

    fn throw_reference(&self, name: &str) -> Signal {
        Signal::Throw(self.make_error("ReferenceError", &format!("{name} is not defined")))
    }

    // -- the instruction loop -----------------------------------------------

    fn step(&mut self) -> JsResult<Step> {
        let pc = self.frame().pc;
        let code = self.frame().code.clone();
        let bytes = &code.code.code;
        if pc >= bytes.len() {
            return Ok(Step::Return(Value::Undefined));
        }
        let Some(decoded) = decode(bytes, pc) else {
            return Err(Signal::Throw(
                self.make_error("InternalError", "undecodable bytecode"),
            ));
        };
        let (op, a, b, next) = decoded;
        self.frame_mut().pc = next;
        self.execute_instruction(op, a, b, next)
    }

    fn push(&mut self, value: Value) {
        self.frame_mut().stack.push(value);
    }

    fn pop(&mut self) -> Value {
        self.frame_mut().stack.pop().unwrap_or(Value::Undefined)
    }

    fn pop_n(&mut self, count: usize) -> Vec<Value> {
        let frame = self.frame_mut();
        let start = frame.stack.len().saturating_sub(count);
        frame.stack.split_off(start)
    }

    fn peek(&self, back: usize) -> Value {
        let frame = self.frame();
        frame
            .stack
            .get(frame.stack.len().wrapping_sub(1 + back))
            .cloned()
            .unwrap_or(Value::Undefined)
    }

    #[allow(clippy::too_many_lines)]
    fn execute_instruction(&mut self, op: Opcode, a: u32, b: u32, next: usize) -> JsResult<Step> {
        use Opcode::*;
        match op {
            // -- constants and stack shuffling -----------------------------
            Nop => {}
            PushMinus1 => self.push(Value::Int(-1)),
            Push0 | Push1 | Push2 | Push3 | Push4 | Push5 | Push6 | Push7 => {
                let value = match op {
                    Push0 => 0,
                    Push1 => 1,
                    Push2 => 2,
                    Push3 => 3,
                    Push4 => 4,
                    Push5 => 5,
                    Push6 => 6,
                    _ => 7,
                };
                self.push(Value::Int(value));
            }
            PushI8 | PushI16 | PushI32 => self.push(Value::Int(a as i32)),
            PushTrue => self.push(Value::Bool(true)),
            PushFalse => self.push(Value::Bool(false)),
            PushEmptyString => self.push(self.string("")),
            Undefined => self.push(Value::Undefined),
            Null => self.push(Value::Null),
            Object => {
                let proto = self.realm.object_proto.clone();
                self.push(Value::Object(self.new_object(Some(proto))));
            }
            PushThis => {
                let this = self.frame().this.clone();
                self.push(this);
            }
            PushConst | PushConst8 => {
                let value = self.constant(a as usize)?;
                self.push(value);
            }
            PushAtomValue => {
                let text = self.atom_text(a as usize);
                self.push(self.string(text));
            }
            Dup => {
                let value = self.peek(0);
                self.push(value);
            }
            Dup1 => {
                let value = self.peek(1);
                self.push(value);
            }
            Drop => {
                self.pop();
            }
            Nip => {
                // `a b -> b`
                let top = self.pop();
                self.pop();
                self.push(top);
            }
            Insert2 => {
                // `obj a -> a obj a`
                let value = self.pop();
                let object = self.pop();
                self.push(value.clone());
                self.push(object);
                self.push(value);
            }
            Insert3 => {
                // `obj prop a -> a obj prop a`
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                self.push(value.clone());
                self.push(object);
                self.push(key);
                self.push(value);
            }
            Rot3r => {
                // `a b c -> c a b`
                let c = self.pop();
                let b = self.pop();
                let a_value = self.pop();
                self.push(c);
                self.push(a_value);
                self.push(b);
            }

            // -- slots ------------------------------------------------------
            GetLoc | GetLoc8 | GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.get_loc(slot);
                self.push(value);
            }
            PutLoc | PutLoc8 | PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_loc(slot, value);
            }
            SetLoc | SetLoc8 | SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_loc(slot, value.clone());
                self.push(value);
            }
            // The `_check` forms are the temporal dead zone: they throw when the
            // slot still holds [`Value::Uninitialized`]. Only `get_loc_check`
            // *reads*, so only the two writes may pop — mishandling that quietly
            // consumes the value the caller pushed just before.
            GetLocCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.get_loc(slot);
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Loc));
                }
                self.push(value);
            }
            PutLocCheck | SetLocCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Loc));
                }
                if op == SetLocCheck {
                    self.put_loc(slot, value.clone());
                    self.push(value);
                } else {
                    self.put_loc(slot, value);
                }
            }
            SetLocUninitialized => {
                let slot = self.slot_operand(op, a);
                self.put_loc(slot, Value::Uninitialized);
            }
            CloseLoc => {
                let slot = self.slot_operand(op, a);
                self.put_loc(slot, Value::Undefined);
            }
            GetArg | GetArg0 | GetArg1 | GetArg2 | GetArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.get_arg(slot);
                self.push(value);
            }
            PutArg | PutArg0 | PutArg1 | PutArg2 | PutArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_arg(slot, value);
            }
            SetArg | SetArg0 | SetArg1 | SetArg2 | SetArg3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.put_arg(slot, value.clone());
                self.push(value);
            }

            // -- closure variables ------------------------------------------
            GetVarRef | GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 => {
                let slot = self.slot_operand(op, a);
                let value = self.cell_value(slot);
                self.push(value);
            }
            GetVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.cell_value(slot);
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Closure));
                }
                self.push(value);
            }
            PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 | PutVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.set_cell(slot, value);
            }
            SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.set_cell(slot, value.clone());
                self.push(value);
            }

            // -- globals ----------------------------------------------------
            GetVar => {
                let value = self.get_global(a as usize)?;
                self.push(value);
            }
            GetVarUndef => {
                let name = self.global_name(a as usize);
                let value = if let Some(cell) = self.realm.globals.get(&name) {
                    cell.get()
                } else {
                    let global = self.realm.global.clone();
                    self.get_property(&global, &name)?
                        .unwrap_or(Value::Undefined)
                };
                self.push(value);
            }
            PutVar => {
                let value = self.pop();
                self.put_global(a as usize, value);
            }
            PutVarInit => {
                let slot = a as usize;
                let value = self.pop();
                // A declared global gets its cell here, which is what makes later
                // `put_var_ref` stores and `get_var` reads agree.
                let name = self.global_name(slot);
                if !self.realm.globals.contains_key(&name) {
                    self.declare_global(&name);
                }
                self.put_global(slot, value);
            }

            // -- arithmetic -------------------------------------------------
            Add => self.binary_add()?,
            Sub | Mul | Div | Mod | Pow | And | Or | Xor | Shl | Sar | Shr => {
                let raw = self.pop();
                let right = self.numeric(raw)?;
                let raw = self.pop();
                let left = self.numeric(raw)?;
                self.push(arithmetic(op, left, right)?);
            }
            Neg => {
                let raw = self.pop();
                let value = self.numeric(raw)?;
                self.push(negate(value));
            }
            Plus => {
                let value = self.pop();
                let numeric = self.numeric_of(value)?;
                self.push(numeric);
            }
            Inc => {
                let raw = self.pop();
                let value = self.numeric(raw)?;
                self.push(increment(value));
            }
            Dec => {
                let raw = self.pop();
                let value = self.numeric(raw)?;
                self.push(decrement(value));
            }
            PostInc | PostDec => {
                // `x -> x x+1`: the *new* value ends up on top, so the store the
                // compiler emits right after (`put_var`) writes the incremented
                // one, while the old value stays underneath for the expression's
                // own result. Getting this backwards makes `i++` a no-op, which
                // turns `while (i < 3)` into an infinite loop.
                let raw = self.pop();
                let value = self.numeric(raw)?;
                let updated = if op == PostInc {
                    increment(value.clone())
                } else {
                    decrement(value.clone())
                };
                self.push(value);
                self.push(updated);
            }
            Not => {
                let raw = self.pop();
                let value = self.numeric(raw)?;
                self.push(Value::Int(!value.to_int32()));
            }
            Lnot => {
                let value = self.pop();
                self.push(Value::Bool(!value.to_boolean()));
            }

            // -- comparison -------------------------------------------------
            Lt | Lte | Gt | Gte => {
                let right = self.pop();
                let left = self.pop();
                self.push(compare(op, left, right, self));
            }
            Eq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(left.loose_equals(&right)));
            }
            Neq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(!left.loose_equals(&right)));
            }
            StrictEq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(left.strict_equals(&right)));
            }
            StrictNeq => {
                let right = self.pop();
                let left = self.pop();
                self.push(Value::Bool(!left.strict_equals(&right)));
            }
            IsUndefinedOrNull => {
                let value = self.pop();
                self.push(Value::Bool(value.is_nullish()));
            }
            Typeof => {
                let value = self.pop();
                self.push(self.string(value.type_of()));
            }
            Instanceof => {
                let constructor = self.pop();
                let object = self.pop();
                let answer = self.instance_of(&object, &constructor)?;
                self.push(Value::Bool(answer));
            }
            In => {
                let object = self.pop();
                let key = self.pop();
                let answer = self.has_property(&key, &object)?;
                self.push(Value::Bool(answer));
            }

            // -- control flow -----------------------------------------------
            Goto | Goto8 | Goto16 => {
                self.frame_mut().pc = a as usize;
            }
            IfTrue | IfTrue8 => {
                let value = self.pop();
                if value.to_boolean() {
                    self.frame_mut().pc = a as usize;
                }
            }
            IfFalse | IfFalse8 => {
                let value = self.pop();
                if !value.to_boolean() {
                    self.frame_mut().pc = a as usize;
                }
            }
            Catch => {
                // Pushes the handler target onto the value stack; `unwind` finds it.
                self.push(Value::CatchOffset(a));
            }
            Gosub => {
                self.push(Value::ReturnAddress(next as u32));
                self.frame_mut().pc = a as usize;
            }
            Ret => {
                let value = self.pop();
                match value {
                    Value::ReturnAddress(address) => self.frame_mut().pc = address as usize,
                    _ => {
                        return Err(Signal::Throw(
                            self.make_error("InternalError", "invalid return address"),
                        ));
                    }
                }
            }
            NipCatch => {
                // `catch_offset ... ret_val -> ret_val`: drop everything down to and
                // including the marker, keeping the top value.
                let top = self.pop();
                loop {
                    match self.pop() {
                        Value::CatchOffset(_) | Value::Undefined => break,
                        _ => {}
                    }
                    if self.frame().stack.is_empty() {
                        break;
                    }
                }
                self.push(top);
            }
            Throw => {
                let value = self.pop();
                return Err(Signal::Throw(value));
            }
            ThrowError => {
                let name = self.atom_text(a as usize);
                let (class, message) = match b {
                    0 => ("TypeError", format!("'{name}' is read-only")),
                    1 => ("SyntaxError", format!("redeclaration of '{name}'")),
                    2 => ("ReferenceError", format!("'{name}' is not initialized")),
                    _ => ("TypeError", format!("invalid operation on '{name}'")),
                };
                return Err(Signal::Throw(self.make_error(class, &message)));
            }
            Return => {
                let value = self.pop();
                return Ok(Step::Return(value));
            }
            ReturnUndef => return Ok(Step::Return(Value::Undefined)),

            // -- calls ------------------------------------------------------
            Call | Call0 | Call1 | Call2 | Call3 => {
                let argc = self.argc_operand(op, a);
                let args = self.pop_n(argc);
                let callee = self.pop();
                // A plain call has no receiver; the callee decides what to do with
                // `undefined` (a sloppy function substitutes the global object).
                if let Some(result) = self.call_or_push(callee, Value::Undefined, args)? {
                    self.push(result);
                }
            }
            CallConstructor => {
                // `constructor this_obj args… -> object`. The duplicated
                // constructor is `new.target`; this milestone does not expose it,
                // so the extra copy is simply consumed here.
                let argc = a as usize;
                let args = self.pop_n(argc);
                let new_target = self.pop();
                let callee = self.pop();
                let result = self.construct(callee, new_target, args)?;
                self.push(result);
            }
            CallMethod => {
                let argc = a as usize;
                let args = self.pop_n(argc);
                let callee = self.pop();
                let this = self.pop();
                if let Some(result) = self.call_or_push(callee, this, args)? {
                    self.push(result);
                }
            }

            // -- objects ----------------------------------------------------
            GetField | GetField2 => {
                let object = self.pop();
                let key = self.atom_text(a as usize);
                let value = self.read_field(&object, &key)?;
                if op == GetField2 {
                    self.push(object);
                }
                self.push(value);
            }
            GetArrayEl | GetArrayEl2 | GetArrayEl3 => {
                let key = self.pop();
                let object = self.pop();
                let text = self.propkey_of(&key);
                let value = self.read_field(&object, &text)?;
                if matches!(op, GetArrayEl2 | GetArrayEl3) {
                    self.push(object);
                }
                if op == GetArrayEl3 {
                    self.push(key);
                }
                self.push(value);
            }
            PutField => {
                let value = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                self.write_field(&object, &key, value)?;
            }
            PutArrayEl => {
                let value = self.pop();
                let key = self.pop();
                let object = self.pop();
                let text = self.propkey_of(&key);
                self.write_field(&object, &text, value)?;
            }
            DefineField => {
                let value = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                if let Some(target) = object.as_object() {
                    // Only an *array* stores into the element vector. A numeric key
                    // on an ordinary object is an ordinary property, which is what
                    // makes `{1: 'a'}` iterate as a key rather than disappear.
                    let stored = target.is_array()
                        && array_index(&key)
                            .is_some_and(|index| target.array_set(index as usize, value.clone()));
                    if !stored {
                        target.define_data(&key, value);
                    }
                }
                self.push(object);
            }
            DefineArrayEl => {
                // `obj key value -> obj key`
                let value = self.pop();
                let key = self.pop();
                let object = self.peek(0);
                let text = self.propkey_of(&key);
                self.write_field(&object, &text, value)?;
                self.push(key);
            }
            DefineMethod => {
                let function = self.pop();
                let object = self.pop();
                let key = self.atom_text(a as usize);
                if let Some(array) = object.as_object() {
                    define_function_property(array, &key, function, b);
                }
                self.push(object);
            }
            DefineMethodComputed => {
                let function = self.pop();
                let key = self.pop();
                let object = self.pop();
                let text = self.propkey_of(&key);
                if let Some(array) = object.as_object() {
                    define_function_property(array, &text, function, a);
                }
                self.push(object);
            }
            SetProto => {
                let proto = self.pop();
                let object = self.pop();
                if let Some(target) = object.as_object() {
                    target.set_proto(proto.as_object().cloned());
                }
                self.push(object);
            }
            CopyDataProperties => {
                let exclusion = self.pop();
                let source = self.pop();
                let target = self.peek(0);
                self.copy_properties(&target, &source, &exclusion)?;
                self.push(source);
                self.push(exclusion);
            }
            ToPropkey => {
                let value = self.pop();
                let key = self.propkey_of(&value);
                self.push(self.string(key));
            }
            SetName => {
                let function = self.peek(0);
                let name = self.atom_text(a as usize);
                self.set_function_name(&function, &name);
            }
            SetNameComputed => {
                let key = self.peek(1);
                let function = self.peek(0);
                let name = self.propkey_of(&key);
                self.set_function_name(&function, &name);
            }

            // -- arrays -----------------------------------------------------
            ArrayFrom => {
                let count = a as usize;
                let elements = self.pop_n(count);
                let array = self.new_array(elements);
                self.push(Value::Object(array));
            }
            Append => {
                // `array index iterable -> array new_index`
                let iterable = self.pop();
                self.pop();
                let array = self.peek(0);
                let added = self.append_enumerate(&array, &iterable)?;
                self.push(Value::Int(added));
            }

            // -- iteration --------------------------------------------------
            ForInStart => {
                let object = self.realm.global.clone();
                let _ = &object;
                let value = self.pop();
                let record = self.for_in_record(&value);
                self.push(Value::Object(record));
            }
            ForInNext => {
                // `record -> record key done`. The flag is *done*, not "found":
                // the compiler tests it with `if_false` to jump back into the body,
                // so reporting `true` while keys remain would end the loop at once.
                let record = self.peek(0);
                match self.for_in_advance(&record) {
                    Some(key) => {
                        let text = self.string(key);
                        self.push(text);
                        self.push(Value::Bool(false));
                    }
                    None => {
                        self.push(Value::Undefined);
                        self.push(Value::Bool(true));
                    }
                }
            }
            ForOfStart => {
                let value = self.pop();
                let iterator = self.get_iterator(&value)?;
                self.push(iterator.object);
                self.push(iterator.next);
                self.push(Value::CatchOffset(0));
            }
            ForOfNext => {
                let offset = b as usize;
                let _ = offset;
                // `iter next catch_offset -> iter next catch_offset value done`
                // `iter next catch_offset -> iter next catch_offset value done`
                let iterator = Iterator {
                    object: self.peek(2),
                    next: self.peek(1),
                };
                let (value, done) = self.iterator_step(&iterator)?;
                self.push(value);
                self.push(Value::Bool(done));
            }
            IteratorClose => {
                // `iter next catch_offset ->`
                self.pop();
                self.pop();
                self.pop();
            }
            ForAwaitOfStart => {
                return Err(Signal::Throw(
                    self.make_error("TypeError", "for await is not implemented yet"),
                ));
            }

            // -- exceptions and bookkeeping ---------------------------------
            Fclosure | Fclosure8 => {
                let value = self.closure(a as usize)?;
                self.push(value);
            }

            other => {
                let name = other.info().name;
                return Err(Signal::Throw(self.make_error(
                    "InternalError",
                    &format!("{name} is not implemented yet"),
                )));
            }
        }
        Ok(Step::Continue)
    }

    // -- operand helpers ----------------------------------------------------

    /// The slot index an operand names, taking short forms into account.
    fn slot_operand(&self, op: Opcode, a: u32) -> usize {
        use Opcode::*;
        match op {
            GetLoc0 | PutLoc0 | SetLoc0 | GetArg0 | PutArg0 | SetArg0 | GetVarRef0 | PutVarRef0
            | SetVarRef0 => 0,
            GetLoc1 | PutLoc1 | SetLoc1 | GetArg1 | PutArg1 | SetArg1 | GetVarRef1 | PutVarRef1
            | SetVarRef1 => 1,
            GetLoc2 | PutLoc2 | SetLoc2 | GetArg2 | PutArg2 | SetArg2 | GetVarRef2 | PutVarRef2
            | SetVarRef2 => 2,
            GetLoc3 | PutLoc3 | SetLoc3 | GetArg3 | PutArg3 | SetArg3 | GetVarRef3 | PutVarRef3
            | SetVarRef3 => 3,
            _ => a as usize,
        }
    }

    /// The argument count of a call, taking `call0`…`call3` into account.
    fn argc_operand(&self, op: Opcode, a: u32) -> usize {
        use Opcode::*;
        match op {
            Call0 => 0,
            Call1 => 1,
            Call2 => 2,
            Call3 => 3,
            _ => a as usize,
        }
    }

    fn atom_text(&self, index: usize) -> String {
        self.frame()
            .code
            .code
            .atoms
            .get(index)
            .map(|atom| match atom {
                Atom::Str(text) => text.clone(),
                Atom::Index(value) => value.to_string(),
            })
            .unwrap_or_default()
    }

    fn constant(&self, index: usize) -> JsResult<Value> {
        let constant = self.frame().code.code.cpool.get(index);
        Ok(match constant {
            Some(Constant::Number(number)) => Value::Float(*number),
            Some(Constant::String(text)) => self.string(text.clone()),
            Some(Constant::Function(_)) => {
                return Err(Signal::Throw(
                    self.make_error("InternalError", "a bare function constant"),
                ));
            }
            _ => Value::Undefined,
        })
    }

    /// The reference cell a closure-variable slot names.
    ///
    /// Two kinds of binding share this addressing: a genuine closure variable
    /// (captured from an enclosing function) lives in this frame's environment,
    /// while a *declared global* lives in the realm so that a script, a function
    /// declaration and their uses all see one binding. A plain `Global` — an
    /// undeclared name that may be satisfied by the global object — has no cell at
    /// all, which is how `typeof x` still works for a name that does not exist.
    /// The reference cell a closure-variable slot names.
    ///
    /// The environment is resolved once, when the closure is built, and this is
    /// just the lookup afterwards. A *declared global* is in there too, pointing at
    /// the realm's cell, so a script, a hoisted function declaration and their uses
    /// all share one binding. A plain `Global` — a name the script never declared —
    /// is read and written through `get_var`/`put_var` by name instead, which is
    /// what lets the global object satisfy it.
    fn var_ref_cell(&self, slot: usize) -> Option<Gc<Cell>> {
        self.frame().env.cells.get(slot).cloned()
    }

    fn cell_value(&self, slot: usize) -> Value {
        self.var_ref_cell(slot)
            .map(|cell| cell.get())
            .unwrap_or(Value::Undefined)
    }

    fn set_cell(&mut self, slot: usize, value: Value) {
        if let Some(cell) = self.var_ref_cell(slot) {
            cell.set(value);
            return;
        }
        // A store through a declared-global reference is also what *creates* the
        // binding: function declarations are hoisted by storing into it before the
        // body runs.
        if let Some(name) = self
            .frame()
            .code
            .code
            .closure_vars
            .get(slot)
            .map(|c| c.name.clone())
            && matches!(
                self.frame().code.code.closure_vars[slot].closure_type,
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport
            )
        {
            self.declare_global(&name);
            if let Some(cell) = self.realm.globals.get(&name) {
                cell.set(value);
            }
        }
    }

    // -- closures -----------------------------------------------------------

    /// Build the closure for the function constant `index`.
    ///
    /// Every entry in the child's closure table names where its binding lives in
    /// *this* frame: a local or argument slot that was marked captured, or a cell
    /// this frame is itself holding. That is how a variable three functions deep
    /// stays one binding.
    fn closure(&mut self, index: usize) -> JsResult<Value> {
        let Some(Constant::Function(function)) = self.frame().code.code.cpool.get(index).cloned()
        else {
            return Err(Signal::Throw(
                self.make_error("InternalError", "fclosure of a non-function"),
            ));
        };
        let mut cells = Vec::with_capacity(function.closure_vars.len());
        for closure_var in &function.closure_vars {
            let cell = match closure_var.closure_type {
                ClosureType::Arg => {
                    let slot = closure_var.var_idx as usize;
                    self.frame().code.arg_cell.get(slot).copied().unwrap_or(-1)
                }
                ClosureType::Local => {
                    let slot = closure_var.var_idx as usize;
                    self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1)
                }
                ClosureType::Ref | ClosureType::GlobalRef => {
                    // A `Ref` names a cell in this frame's own closure table; a
                    // `GlobalRef` also starts there, then resolves by name below.
                    let slot = closure_var.var_idx as usize;
                    if let Some(cell) = self.frame().env.cells.get(slot) {
                        cells.push(cell.clone());
                        continue;
                    }
                    -1
                }
                _ => -1,
            };
            if cell >= 0 {
                let cell = self.frame().cells[cell as usize].clone();
                cells.push(cell);
                continue;
            }
            // A global: the cell is created on demand and shared through the realm,
            // so every function sees one binding.
            let name = closure_var.name.clone();
            let cell = match self.realm.globals.get(&name) {
                Some(cell) => cell.clone(),
                None => {
                    let cell = self.new_cell(Value::Undefined);
                    self.realm.globals.insert(name.clone(), cell.clone());
                    cell
                }
            };
            cells.push(cell);
        }
        let code = Gc::new(self.heap, CompiledCode::new(*function));
        let env = Gc::new(self.heap, ClosureEnv { cells });
        let object = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Bytecode { code, env },
        );
        self.install_function_prototype(&object);
        Ok(Value::Object(object))
    }

    // -- conversions used by the operators ----------------------------------

    fn numeric_of(&mut self, value: Value) -> JsResult<Value> {
        if let Value::Object(_) = value {
            // `+obj` runs `ToPrimitive` first; without it an object is `NaN`.
            let number = value.to_number();
            return Ok(Value::Float(number));
        }
        Ok(value)
    }

    fn numeric(&mut self, value: Value) -> JsResult<Value> {
        Ok(match value {
            Value::Int(_) | Value::Float(_) => value,
            other => Value::Float(other.to_number()),
        })
    }

    fn binary_add(&mut self) -> JsResult<()> {
        let right = self.pop();
        let left = self.pop();
        // `+` concatenates when either operand is a string *after* `ToPrimitive`.
        let left = self.primitive_of(left);
        let right = self.primitive_of(right);
        if matches!(left, Value::String(_)) || matches!(right, Value::String(_)) {
            let text = format!("{}{}", left.to_js_string(), right.to_js_string());
            self.push(self.string(text));
            return Ok(());
        }
        let left = self.numeric(left)?;
        let right = self.numeric(right)?;
        self.push(add_numbers(left, right));
        Ok(())
    }

    fn primitive_of(&mut self, value: Value) -> Value {
        match &value {
            Value::Object(_) => Value::Float(value.to_number()),
            _ => value,
        }
    }

    fn propkey_of(&mut self, value: &Value) -> String {
        match value {
            Value::Int(_) | Value::Float(_) => number_to_string(value.to_number()),
            other => other.to_js_string(),
        }
    }

    // -- object operations used by instructions -----------------------------

    fn read_field(&mut self, object: &Value, key: &str) -> JsResult<Value> {
        let Value::Object(object) = object else {
            // Reading a property of `undefined`/`null` is the classic TypeError.
            if object.is_nullish() {
                return Err(Signal::Throw(self.make_error(
                    "TypeError",
                    &format!("cannot read property '{key}' of {}", object.to_js_string()),
                )));
            }
            // A string has `length` and index access without being boxed.
            if let Value::String(text) = object {
                if key == "length" {
                    return Ok(Value::Int(text.utf16_len() as i32));
                }
                if let Some(index) = array_index(key) {
                    let units: Vec<u16> = text.as_str().encode_utf16().collect();
                    return Ok(match units.get(index as usize) {
                        Some(unit) => {
                            let character = String::from_utf16_lossy(&[*unit]);
                            self.string(character)
                        }
                        None => Value::Undefined,
                    });
                }
            }
            return Ok(Value::Undefined);
        };
        Ok(self.get_property(object, key)?.unwrap_or(Value::Undefined))
    }

    fn write_field(&mut self, object: &Value, key: &str, value: Value) -> JsResult<()> {
        let Value::Object(object) = object else {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                &format!("cannot set property '{key}' of {}", object.to_js_string()),
            )));
        };
        self.set_property(object, key, value)
    }

    fn has_property(&mut self, key: &Value, object: &Value) -> JsResult<bool> {
        let Value::Object(object) = object else {
            return Err(Signal::Throw(
                self.make_error("TypeError", "cannot use 'in' on a non-object"),
            ));
        };
        let text = self.propkey_of(key);
        if object.own_property(&text).is_some() {
            return Ok(true);
        }
        if object
            .array_get(array_index(&text).unwrap_or(u32::MAX) as usize)
            .is_some()
        {
            return Ok(true);
        }
        Ok(lookup(object, &text).is_some())
    }

    fn instance_of(&mut self, object: &Value, constructor: &Value) -> JsResult<bool> {
        let Value::Object(target) = object else {
            return Ok(false);
        };
        let Value::Object(function) = constructor else {
            return Err(Signal::Throw(self.make_error(
                "TypeError",
                "right-hand side of 'instanceof' is not callable",
            )));
        };
        let prototype = self.get_property(function, "prototype")?;
        let Some(Value::Object(prototype)) = prototype else {
            return Ok(false);
        };
        Ok(target
            .chain()
            .iter()
            .any(|ancestor| ancestor.ptr_eq(&prototype)))
    }

    fn copy_properties(
        &mut self,
        target: &Value,
        source: &Value,
        _exclusion: &Value,
    ) -> JsResult<()> {
        let (Some(target), Value::Object(source)) = (target.as_object(), source) else {
            // Spreading `undefined`/`null` is a no-op, and spreading a primitive
            // copies nothing that a script can observe here.
            return Ok(());
        };
        let mut keys = source.own_enumerable_keys();
        if let Some(length) = source.array_length() {
            keys.retain(|key| key != "length");
            for index in 0..length {
                if let Some(value) = source.array_get(index) {
                    target.define_data(&index.to_string(), value);
                }
            }
        }
        for key in keys {
            if let Some(property) = source.own_property(&key)
                && let Some(value) = property.value
            {
                target.define_data(&key, value);
            }
        }
        Ok(())
    }

    // -- iteration ---------------------------------------------------------

    /// Build the key list a `for-in` loop walks.
    ///
    /// The order is the specification's: integer-like keys in ascending numeric
    /// order, then the string keys in insertion order, own properties before
    /// inherited ones, and a shadowed key only appears once.
    fn for_in_record(&mut self, value: &Value) -> Gc<JsObject> {
        let record = self.new_object(None);
        let mut keys: Vec<String> = Vec::new();
        if let Value::Object(object) = value {
            let mut chain = vec![object.clone()];
            chain.extend(object.chain());
            for object in chain {
                let mut own = object.own_enumerable_keys();
                if let Some(length) = object.array_length() {
                    own.retain(|key| key != "length");
                    for index in 0..length {
                        if object.array_get(index).is_some() {
                            own.push(index.to_string());
                        }
                    }
                }
                for key in own {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
            }
        }
        let indices: Vec<String> = {
            let mut numeric: Vec<u32> = keys.iter().filter_map(|k| array_index(k)).collect();
            numeric.sort_unstable();
            numeric.into_iter().map(|n| n.to_string()).collect()
        };
        let strings: Vec<String> = keys
            .into_iter()
            .filter(|key| array_index(key).is_none())
            .collect();
        let mut ordered = indices;
        ordered.extend(strings);
        for (index, key) in ordered.into_iter().enumerate() {
            record.define_data(&index.to_string(), self.string(key));
        }
        record.define_data("__index", Value::Int(0));
        record
    }

    fn for_in_advance(&mut self, record: &Value) -> Option<String> {
        let Value::Object(record) = record else {
            return None;
        };
        let index = record
            .own_property("__index")
            .and_then(|property| property.value)
            .map(|value| value.to_int32())
            .unwrap_or(0);
        match record.own_property(&index.to_string()) {
            Some(property) => {
                record.define_data("__index", Value::Int(index + 1));
                property.value.map(|value| value.to_js_string())
            }
            None => None,
        }
    }

    /// The iterator for `for-of`: the object and its `next` method.
    fn get_iterator(&mut self, value: &Value) -> JsResult<Iterator> {
        // Arrays and strings are the two iterables this milestone reaches; the
        // full `Symbol.iterator` protocol arrives with the builtins.
        if let Value::Object(object) = value
            && object.is_array()
        {
            let length = object.array_length().unwrap_or(0);
            let iterator = self.new_object(None);
            iterator.define_data("target", value.clone());
            iterator.define_data("__length", Value::Int(length as i32));
            iterator.define_data("__index", Value::Int(0));
            return Ok(Iterator {
                object: Value::Object(iterator.clone()),
                next: Value::Undefined,
            });
        }
        if let Value::String(text) = value {
            let iterator = self.new_object(None);
            let chars: Vec<Value> = (0..text.utf16_len())
                .map(|index| self.string(text.as_str().get(index..index + 1).unwrap_or("")))
                .collect();
            let array = self.new_array(chars);
            iterator.define_data("target", Value::Object(array));
            iterator.define_data("__length", Value::Int(0));
            iterator.define_data("__index", Value::Int(0));
            return Ok(Iterator {
                object: Value::Object(iterator.clone()),
                next: Value::Undefined,
            });
        }
        Err(Signal::Throw(
            self.make_error("TypeError", "value is not iterable"),
        ))
    }

    /// One step of an internal iterator: the value and whether it is exhausted.
    fn iterator_step(&mut self, iterator: &Iterator) -> JsResult<(Value, bool)> {
        let Value::Object(object) = &iterator.object else {
            return Ok((Value::Undefined, true));
        };
        let Some(target) = object.own_property("target").and_then(|p| p.value) else {
            return Ok((Value::Undefined, true));
        };
        let index = object
            .own_property("__index")
            .and_then(|p| p.value)
            .map(|v| v.to_int32())
            .unwrap_or(0);
        let Value::Object(sequence) = &target else {
            return Ok((Value::Undefined, true));
        };
        if index as usize >= sequence.array_length().unwrap_or(0) {
            return Ok((Value::Undefined, true));
        }
        object.define_data("__index", Value::Int(index + 1));
        Ok((
            sequence
                .array_get(index as usize)
                .unwrap_or(Value::Undefined),
            false,
        ))
    }

    /// Append every element of `iterable` to `array`, returning how many.
    fn append_enumerate(&mut self, array: &Value, iterable: &Value) -> JsResult<i32> {
        let Some(target) = array.as_object() else {
            return Ok(0);
        };
        let start = target.array_length().unwrap_or(0);
        let mut added = 0;
        if let Value::Object(source) = iterable
            && source.is_array()
        {
            for index in 0..source.array_length().unwrap_or(0) {
                if let Some(value) = source.array_get(index) {
                    target.array_set(start + added, value);
                    added += 1;
                }
            }
        }
        Ok((start + added) as i32)
    }

    /// How an uncaught exception is reported.
    ///
    /// An error object prints as `Name: message`, the way `qjs` reports it. Any
    /// other thrown value prints as its own `ToString`, wrapped in the "uncaught"
    /// phrasing the reference uses.
    pub fn describe_exception(&self, value: &Value) -> String {
        if let Value::Object(object) = value
            && let Some(Value::String(name)) = object.own_property("name").and_then(|p| p.value)
        {
            let message = object
                .own_property("message")
                .and_then(|p| p.value)
                .map(|value| value.to_js_string())
                .unwrap_or_default();
            if message.is_empty() {
                return format!("Uncaught {}", name.as_str());
            }
            return format!("Uncaught {}: {message}", name.as_str());
        }
        format!("Uncaught {}", display(value))
    }

    /// Give an anonymous function the name of the property it is stored in.
    ///
    /// `{m: function(){}}.m.name` is `"m"`. The name property is not writable, not
    /// enumerable and not configurable, and an already-named function keeps its
    /// name.
    fn set_function_name(&mut self, function: &Value, name: &str) {
        let Value::Object(object) = function else {
            return;
        };
        if object.has_own("name") {
            return;
        }
        let text = self.string(name);
        object.define("name", Property::with_attributes(text, false, false));
    }

    // -- global declarations -----------------------------------------------

    /// Declare a global binding, creating its reference cell.
    pub fn declare_global(&mut self, name: &str) {
        if !self.realm.globals.contains_key(name) {
            let cell = self.new_cell(Value::Undefined);
            self.realm.globals.insert(name.to_string(), cell);
            self.realm.declared.push(name.to_string());
        }
    }
}

/// A native call target, extracted before the call so no borrow is held.
enum NativeCall {
    Native(NativeFn),
    Bytecode(Gc<CompiledCode>, Gc<ClosureEnv>),
}

/// Which table a binding slot indexes, for the messages that name it.
#[derive(Clone, Copy)]
enum VarSlot {
    Loc,
    Closure,
}

/// An internal iterator: the sequence plus a cursor.
struct Iterator {
    object: Value,
    next: Value,
}

/// Define a method or accessor property, per the `define_method` flag byte.
///
/// The low two bits pick value/getter/setter and bit 2 marks the property
/// enumerable; a property defined by a method is not configurable in an object
/// literal, but nothing in this milestone can tell the difference.
fn define_function_property(object: &Gc<JsObject>, key: &str, function: Value, flags: u32) {
    let enumerable = flags & 4 != 0;
    let kind = flags & 3;
    let property = match kind {
        1 => Property {
            getter: Some(function),
            enumerable,
            configurable: true,
            ..Property::default()
        },
        2 => Property {
            setter: Some(function),
            enumerable,
            configurable: true,
            ..Property::default()
        },
        _ => Property {
            value: Some(function),
            writable: true,
            enumerable,
            configurable: true,
            ..Property::default()
        },
    };
    object.define(key, property);
}

// ---------------------------------------------------------------------------
// Operand decoding
// ---------------------------------------------------------------------------

/// Decode the instruction at `pc` into `(opcode, operand a, operand b, next pc)`.
///
/// Jump operands are resolved to absolute byte offsets here, because the encoding
/// stores them relative to the byte after the opcode and every use site would
/// otherwise repeat that arithmetic.
fn decode(code: &[u8], pc: usize) -> Option<(Opcode, u32, u32, usize)> {
    let op = Opcode::from_u8(*code.get(pc)?)?;
    let info = op.info();
    let base = pc + 1;
    let read_u8 = |offset: usize| -> u32 { code.get(base + offset).copied().unwrap_or(0) as u32 };
    let read_u16 = |offset: usize| -> u32 {
        let mut value = 0u32;
        for index in 0..2 {
            value = (value << 8) | code.get(base + offset + index).copied().unwrap_or(0) as u32;
        }
        value
    };
    let read_u32 = |offset: usize| -> u32 {
        let mut value = 0u32;
        for index in 0..4 {
            value = (value << 8) | code.get(base + offset + index).copied().unwrap_or(0) as u32;
        }
        value
    };
    let jump = |offset: usize, width: usize| -> u32 {
        let raw = match width {
            1 => read_u8(offset) as u8 as i8 as i64,
            2 => read_u16(offset) as u16 as i16 as i64,
            _ => read_u32(offset) as i32 as i64,
        };
        (base as i64 + raw) as u32
    };
    let (a, b) = match info.format {
        OpFormat::None
        | OpFormat::NoneInt
        | OpFormat::NoneLoc
        | OpFormat::NoneArg
        | OpFormat::NoneVarRef
        | OpFormat::Npopx => (0, 0),
        OpFormat::U8 | OpFormat::Loc8 | OpFormat::Const8 => (read_u8(0), 0),
        OpFormat::I8 => (read_u8(0) as u8 as i8 as i32 as u32, 0),
        OpFormat::Label8 => (jump(0, 1), 0),
        OpFormat::U16 | OpFormat::Npop | OpFormat::Loc | OpFormat::Arg | OpFormat::VarRef => {
            (read_u16(0), 0)
        }
        OpFormat::I16 => (read_u16(0) as u16 as i16 as i32 as u32, 0),
        OpFormat::Label16 => (jump(0, 2), 0),
        OpFormat::NpopU16 => (read_u16(0), read_u16(2)),
        OpFormat::U32 | OpFormat::Const | OpFormat::Atom => (read_u32(0), 0),
        OpFormat::I32 | OpFormat::Label => (
            if info.format == OpFormat::Label {
                jump(0, 4)
            } else {
                read_u32(0) as i32 as u32
            },
            0,
        ),
        OpFormat::AtomU8 | OpFormat::AtomLabelU8 => (read_u32(0), read_u8(4)),
        OpFormat::AtomU16 | OpFormat::AtomLabelU16 | OpFormat::LabelU16 => {
            (read_u32(0), read_u16(4))
        }
    };
    Some((op, a, b, pc + info.size as usize))
}

/// How `print` renders a value.
///
/// `ToString`, except that negative zero keeps its sign — the reference's host
/// printer does that while `String(-0)` is `"0"`.
pub fn display(value: &Value) -> String {
    match value {
        Value::Float(number) => crate::value::number_to_string_for_print(*number),
        Value::Uninitialized | Value::CatchOffset(_) | Value::ReturnAddress(_) => {
            "undefined".to_string()
        }
        other => other.to_js_string(),
    }
}

// ---------------------------------------------------------------------------
// Numeric operators
// ---------------------------------------------------------------------------

/// `a + b` keeping the `Int` representation when it fits.
fn add_numbers(left: Value, right: Value) -> Value {
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => match a.checked_add(*b) {
            Some(sum) => Value::Int(sum),
            None => Value::Float(*a as f64 + *b as f64),
        },
        _ => Value::Float(left.to_number() + right.to_number()),
    }
}

/// The arithmetic operators other than `+`, which needs string concatenation.
fn arithmetic(op: Opcode, left: Value, right: Value) -> JsResult<Value> {
    use Opcode::*;
    let (a, b) = (left.to_number(), right.to_number());
    Ok(match op {
        Sub => match (&left, &right) {
            (Value::Int(x), Value::Int(y)) => match x.checked_sub(*y) {
                Some(value) => Value::Int(value),
                None => Value::Float(a - b),
            },
            _ => Value::Float(a - b),
        },
        Mul => Value::Float(a * b),
        Div => Value::Float(a / b),
        Mod => Value::Float(js_mod(a, b)),
        Pow => Value::Float(a.powf(b)),
        And => Value::Int(left.to_int32() & right.to_int32()),
        Or => Value::Int(left.to_int32() | right.to_int32()),
        Xor => Value::Int(left.to_int32() ^ right.to_int32()),
        Shl => Value::Int(left.to_int32().wrapping_shl(right.to_uint32() & 31)),
        Sar => Value::Int(left.to_int32().wrapping_shr(right.to_uint32() & 31)),
        Shr => Value::Int((left.to_uint32() >> (right.to_uint32() & 31)) as i32),
        _ => Value::Float(a),
    })
}

/// `%`, which follows the sign of the dividend rather than Rust's.
fn js_mod(left: f64, right: f64) -> f64 {
    if right == 0.0 || left.is_nan() || right.is_nan() || left.is_infinite() {
        return f64::NAN;
    }
    if right.is_infinite() {
        return left;
    }
    let remainder = left % right;
    // `-1 % 3` is `-1` in both languages, so only the exact-zero case needs care:
    // the spec makes the sign match the dividend.
    if remainder == 0.0 {
        0.0f64.copysign(left)
    } else {
        remainder
    }
}

fn negate(value: Value) -> Value {
    match value {
        Value::Int(0) => Value::Float(-0.0),
        Value::Int(i) => Value::Int(-i),
        other => Value::Float(-other.to_number()),
    }
}

fn increment(value: Value) -> Value {
    match value {
        Value::Int(i) => match i.checked_add(1) {
            Some(next) => Value::Int(next),
            None => Value::Float(i as f64 + 1.0),
        },
        other => Value::Float(other.to_number() + 1.0),
    }
}

fn decrement(value: Value) -> Value {
    match value {
        Value::Int(i) => match i.checked_sub(1) {
            Some(next) => Value::Int(next),
            None => Value::Float(i as f64 - 1.0),
        },
        other => Value::Float(other.to_number() - 1.0),
    }
}

/// The relational operators.
///
/// Strings compare by UTF-16 code unit when both sides are strings, which is why
/// `"10" < "9"` is true.
fn compare(op: Opcode, left: Value, right: Value, interp: &Interp<'_>) -> Value {
    use Opcode::*;
    let ordering = if let (Value::String(a), Value::String(b)) = (&left, &right) {
        Some(a.as_str().cmp(b.as_str()))
    } else {
        let (a, b) = (left.to_number(), right.to_number());
        // Any comparison involving `NaN` is false, which `partial_cmp` expresses.
        a.partial_cmp(&b)
    };
    let _ = interp;
    let answer = match (op, ordering) {
        (_, None) => false,
        (Lt, Some(ordering)) => ordering.is_lt(),
        (Lte, Some(ordering)) => ordering.is_le(),
        (Gt, Some(ordering)) => ordering.is_gt(),
        (Gte, Some(ordering)) => ordering.is_ge(),
        _ => false,
    };
    Value::Bool(answer)
}

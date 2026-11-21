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

use std::collections::{BTreeMap, BTreeSet};

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
    /// Names among the lexical globals that are `const`.
    ///
    /// `const`-ness cannot be read back off the bytecode in every case: a closure that
    /// refers to a global `const` gets a `GlobalRef` entry whose flags the forwarding
    /// path does not carry, so a store through it would look like a plain `let`. The
    /// realm records it once, at declaration, and answers for every reader.
    pub lexical_consts: BTreeSet<String>,
    /// The error constructors by class name, so a runtime error can throw a real
    /// `TypeError` instance rather than a look-alike object.
    pub errors: BTreeMap<&'static str, Gc<JsObject>>,
    /// `String.prototype`, used by `ToString` on primitives and by the boxing path.
    pub string_proto: Option<Gc<JsObject>>,
    /// `Number.prototype`.
    pub number_proto: Option<Gc<JsObject>>,
    /// `Boolean.prototype`.
    pub boolean_proto: Option<Gc<JsObject>>,
    /// `Error.prototype`, the prototype of every error instance.
    pub error_proto: Gc<JsObject>,
    /// `Date.prototype`, so the constructor can build an instance in the
    /// function-call form (`Date()` returns a string) without going through `new`.
    pub date_proto: Gc<JsObject>,
}

/// One binding of the frame that called `eval`, so the eval'd code can read and
/// write it by name.
struct EvalBinding {
    /// The name it was declared with.
    name: String,
    /// Which entry of [`Interp::frames`] owns it. An `eval` sees the whole chain of
    /// calling frames, not only the innermost one: `function f(){ eval("a") }` inside
    /// a function that declares `a` must find that `a`.
    frame: usize,
    /// Which table of that frame it lives in.
    slot: EvalSlot,
    /// The slot index within that table.
    index: usize,
}

/// Which table of a frame an [`EvalBinding`] indexes.
#[derive(Clone, Copy)]
enum EvalSlot {
    Arg,
    Local,
}

/// One activation record.
struct Frame {
    /// The bytecode being run.
    code: Gc<CompiledCode>,
    /// The cells this closure captured.
    env: Gc<ClosureEnv>,
    /// The `this` binding.
    this: Value,
    /// Argument slots, sized to the declared parameter count.
    args: Vec<Value>,
    /// The values the caller actually passed, which is what `arguments` exposes.
    passed: Vec<Value>,
    /// Local slots.
    locals: Vec<Value>,
    /// Reference cells for this frame's own captured bindings.
    cells: Vec<Gc<Cell>>,
    /// The operand stack.
    stack: Vec<Value>,
    /// Byte offset of the next instruction.
    pc: usize,
    /// Byte offset of the instruction being executed, which is what a stack trace
    /// reports. `pc` has already moved past it by the time an instruction runs.
    at: usize,
    /// The callee, so a native builtin can tell which constructor it is.
    callee: Value,
    /// Whether this frame's body ended in a `tail_call`.
    ///
    /// The reference's `tail_call` is not a frame-reusing tail call: it calls the callee
    /// with its own frame just like `call` does and only skips storing the result
    /// (`goto done` in `quickjs.c`). So a runaway tail recursion still hits
    /// `InternalError: stack overflow` there, and the flag reproduces that: the frame is
    /// pushed as usual, and this marker makes its return value the *caller's* return
    /// value without running the instructions after the call.
    tail: bool,
}

/// The interpreter: a realm plus a frame stack.
pub struct Interp<'a> {
    /// The heap everything is allocated on.
    pub heap: &'a Heap,
    /// The realm.
    pub realm: Realm,
    /// Active frames, outermost first.
    frames: Vec<Frame>,
    /// A strict-mode `eval`'s own variable scope, while its script runs.
    ///
    /// Strict eval gets a *fresh* variable environment, so a `var` inside it does not
    /// become a global. Name lookups still fall through to the realm, which is why
    /// `eval("a")` sees a global `a` and `eval("a = 3")` writes it.
    eval_scope: Option<BTreeMap<String, Value>>,
    /// The calling frame's own bindings, which the running `eval` can read and write.
    ///
    /// Held separately from [`Interp::eval_scope`] because these already exist: reads
    /// come from the frame, and a write is applied back to the frame (or to the cell
    /// the frame captured) when the eval finishes. That is what makes
    /// `f("a = 3")` visible to the caller as `a === 3`.
    eval_bindings: Vec<EvalBinding>,
    /// The calling frame's `arguments`, while an `eval` runs.
    ///
    /// Kept apart from [`Interp::eval_scope`] because a sloppy eval has no scope of its
    /// own yet still has to see the caller's `arguments`.
    eval_arguments: Option<Value>,
    /// Names of the native functions currently executing, outermost first.
    ///
    /// A native has no [`Frame`] — it runs on the Rust stack — but the reference's
    /// trace still names it with `(native)` instead of a position, so the names are
    /// tracked separately and stitched into [`Interp::stack_trace`].
    native_frames: Vec<String>,
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
                lexical_consts: BTreeSet::new(),
                errors: BTreeMap::new(),
                string_proto: None,
                number_proto: None,
                boolean_proto: None,
                error_proto: JsObject::ordinary(heap, None),
                date_proto: JsObject::ordinary(heap, None),
            },
            frames: Vec::new(),
            eval_scope: None,
            eval_bindings: Vec::new(),
            eval_arguments: None,
            native_frames: Vec::new(),
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
    ///
    /// The observable `name` and `length` are installed as properties here, which is
    /// what `Function.prototype.name` and `.length` read; nothing else stores them,
    /// so `f.name = "x"` behaves like the writable property it is.
    pub fn new_native(&self, name: impl Into<String>, length: u32, func: NativeFn) -> Gc<JsObject> {
        let function = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Native { func },
        );
        function.define(
            "name",
            Property::with_attributes(self.string(name), false, false),
        );
        function.define(
            "length",
            Property::with_attributes(Value::Int(length as i32), false, false),
        );
        function
    }

    /// A new bound function.
    pub fn new_bound(&self, target: Value, this: Value, args: Vec<Value>) -> Gc<JsObject> {
        JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Bound { target, this, args },
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

    /// Compile and run `source` in global scope, returning its completion value.
    ///
    /// This is what `eval` and `new Function` use. Running it as a script means it
    /// sees the global object and nothing else, which is right for `eval("1+1")` and
    /// for a `Function` body; a *direct* eval that reads a local variable would need
    /// the compiler to hand its variable environment over, which is not implemented.
    pub fn eval_in_global(&mut self, source: &str) -> JsResult<Value> {
        // The reference names eval'd code `<input>` rather than inheriting the caller's
        // file name, and stack traces show it, so `eval("1 + x")` failing reports
        // `(<input>:1:5)`.
        const EVAL_FILE: &str = "<input>";
        // A *direct* eval inherits the strictness of the code that called it, which is
        // why `eval("x = 1")` in a strict function is a ReferenceError instead of an
        // implicit global. The caller's frame is the innermost one here, because a
        // native function does not push a frame of its own.
        let strict = self.current_strict();
        let parsed = if strict {
            js_parse::parse_script_strict(source)
        } else {
            js_parse::parse_script(source)
        };
        let program = match parsed {
            Ok(program) => program,
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_at(
                    EVAL_FILE,
                    error.span.start.line,
                    error.span.start.column,
                    &format!("invalid source: {error}"),
                )));
            }
        };
        let mut compiled = match js_compile::compile_script(&program) {
            Ok(compiled) => compiled,
            // A construct the compiler does not handle yet is reported the same way a
            // parse error is: it happens before anything runs, so the trace is the
            // position in the source followed by the caller.
            Err(error) => {
                return Err(Signal::Throw(self.syntax_error_at(
                    EVAL_FILE,
                    error.span.start.line,
                    error.span.start.column,
                    &error.message,
                )));
            }
        };
        compiled.set_filename(EVAL_FILE);
        let strict = compiled.strict;
        self.run_eval_script(compiled, strict)
    }

    /// Run an `eval`'d script in the caller's scope.
    ///
    /// Three things have to come from the calling frame, because the eval'd code is a
    /// script and would otherwise see only the global object:
    ///
    /// * `this`, so `f.call(1, "this")` evaluates to `1`;
    /// * `arguments`, exposed as a binding of the eval's own scope, so
    ///   `f("arguments.length", 1)` sees the caller's arguments;
    /// * a fresh variable environment for `var`, so a strict `eval` does not leak
    ///   `eval("var v = 1")` into the global object.
    ///
    /// Locals of the calling function are *not* reachable: that needs the compiler to
    /// hand its variable environment over, which this milestone does not do.
    fn run_eval_script(&mut self, code: CompiledFunction, code_strict: bool) -> JsResult<Value> {
        // `this` and `arguments` come from the innermost frame only — those are not
        // lexical — while the name bindings come from the whole chain of frames, part
        // of it being ordinary scope resolution.
        let (this, passed) = match self.frames.last() {
            Some(frame) => (frame.this.clone(), frame.passed.clone()),
            // No frame at all: the eval is running at the top level, so there is
            // nothing to inherit and the global receiver is right.
            None => (Value::Object(self.realm.global.clone()), Vec::new()),
        };
        // Innermost frame first, so the first binding found for a name is the one that
        // shadows the rest. A script frame (`<eval>`) contributes nothing.
        let mut bindings: Vec<EvalBinding> = Vec::new();
        for frame_index in (0..self.frames.len()).rev() {
            let frame = &self.frames[frame_index];
            for (index, arg) in frame.code.code.args.iter().enumerate() {
                bindings.push(EvalBinding {
                    name: arg.name.clone(),
                    frame: frame_index,
                    slot: EvalSlot::Arg,
                    index,
                });
            }
            for (index, local) in frame.code.code.vars.iter().enumerate() {
                bindings.push(EvalBinding {
                    name: local.name.clone(),
                    frame: frame_index,
                    slot: EvalSlot::Local,
                    index,
                });
            }
        }
        let mut seen = BTreeSet::new();
        bindings.retain(|binding| seen.insert(binding.name.clone()));

        // The caller's `arguments`, as an array-like. Reading `length` and the indexed
        // elements is all the spec's ordinary uses need.
        let arguments = Value::Object(self.new_array(passed));

        // Only a *strict* eval gets a variable environment of its own. A sloppy one
        // declares its `var`s in the caller's — which for a top-level eval is the global
        // object, so `eval("var gv=1")` leaves `gv` globally visible. Deciding this on
        // strictness alone is the spec's rule; getting it wrong made every sloppy `var`
        // vanish into a scope nobody reads back from.
        let previous_scope = if code_strict {
            self.eval_scope.replace(BTreeMap::new())
        } else {
            None
        };
        let previous_arguments = self.eval_arguments.replace(arguments);
        let previous_bindings = std::mem::replace(&mut self.eval_bindings, bindings);
        let compiled = Gc::new(self.heap, CompiledCode::new(code));
        let closure_vars = compiled.code.closure_vars.clone();
        let env = self.global_env(&closure_vars);
        let callee = Value::Undefined;
        let result = self.execute(compiled, env, this, Vec::new(), callee);
        self.eval_scope = previous_scope;
        self.eval_arguments = previous_arguments;
        self.eval_bindings = previous_bindings;
        result
    }

    /// A `SyntaxError` that reports where in the source it happened.
    ///
    /// Compile-time failures never run, so they have no frame of their own; the
    /// reference still puts the position first in the trace, with no function name and
    /// no parentheses, and the caller's frames after it. It also defines `fileName`,
    /// `lineNumber` and `columnNumber` on the object (a SpiderMonkey convention the
    /// reference borrows), which is how a script reads the position without parsing
    /// the trace.
    fn syntax_error_at(&mut self, file: &str, line: u32, column: u32, message: &str) -> Value {
        let blamed = format!("    at {file}:{line}:{column}\n{}", self.stack_trace());
        let value = self.make_error("SyntaxError", message);
        if let Value::Object(object) = &value {
            let text = self.string(blamed);
            object.define("stack", Property::with_attributes(text, true, true));
            let name = self.string(file);
            object.define("fileName", Property::with_attributes(name, true, true));
            object.define(
                "lineNumber",
                Property::with_attributes(Value::Int(line as i32), true, true),
            );
            object.define(
                "columnNumber",
                Property::with_attributes(Value::Int(column as i32), true, true),
            );
        }
        value
    }

    /// Run a compiled script whose completion value is a function, returning it.
    ///
    /// `new Function(...)` compiles `(function(params) { body })` and this runs it to
    /// get the closure out, which keeps the constant-pool plumbing in one place.
    pub fn run_function_source(&mut self, code: CompiledFunction) -> JsResult<Value> {
        self.run_script(code)
    }

    /// The environment of a script or of any function compiled at global scope:
    /// every entry is a global binding, so each one resolves to the realm's cell.
    fn global_env(&mut self, closure_vars: &[js_compile::bytecode::ClosureVar]) -> Gc<ClosureEnv> {
        let mut cells = Vec::with_capacity(closure_vars.len());
        for closure_var in closure_vars {
            let cell = match closure_var.closure_type {
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport
                    if closure_var.is_lexical =>
                {
                    // A lexical declaration: the cell is created here holding the
                    // dead-zone marker, which is what makes the binding exist and
                    // unreadable before its initialiser runs.
                    self.declare_global(&closure_var.name, closure_var.is_const);
                    self.realm.globals[&closure_var.name].clone()
                }
                ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport => {
                    // A `var` or function declaration becomes a property of the global
                    // object, and it exists from here on — which is why
                    // `print(v); var v = 1` prints `undefined` instead of raising.
                    // Declaring it at instantiation time (rather than at the
                    // initialiser) is also what makes it non-configurable: the store
                    // then lands on an existing property instead of creating one.
                    self.declare_var_global(&closure_var.name);
                    // The cell is a placeholder that nothing indexes; it cannot be
                    // omitted because the slot has to hold something.
                    self.new_cell(Value::Undefined)
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
                    // A frame that ended in a `tail_call` hands its value straight to
                    // its caller: nothing after the call can run, so every frame above
                    // the callee unwinds with the same value. See [`Frame::tail`] —
                    // the reference does not reuse frames either, so recursion depth
                    // and the stack-overflow limit behave the same way.
                    while self.frames.len() > depth && self.frame().tail {
                        self.frames.pop();
                    }
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
        // How many were actually passed, which `arguments.length` reports and
        // `assert(x)` depends on: a call with one argument must not look like a call
        // with one argument and an explicit `undefined`.
        let passed = args.len();
        // Missing arguments are `undefined` in their slots; extra ones are dropped
        // from the slots but still reachable through `arguments`.
        let mut passed_values = args.clone();
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
        // Sloppy mode substitutes the global object for a missing receiver and boxes a
        // primitive one, so `function f(){ return typeof this }` called as `f.call(1)`
        // reports `object`. Strict mode passes the value through untouched.
        let this = if code.code.strict {
            this
        } else if this.is_nullish() {
            Value::Object(self.realm.global.clone())
        } else if matches!(this, Value::Object(_)) {
            this
        } else {
            match crate::builtins::this_object(self, &this) {
                Ok(boxed) => Value::Object(boxed),
                Err(_) => this,
            }
        };
        self.frames.push(Frame {
            code,
            env,
            this,
            args,
            passed: passed_values.drain(..passed).collect(),
            locals,
            cells,
            stack: Vec::with_capacity(16),
            pc: 0,
            at: 0,
            callee,
            tail: false,
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

    /// The facts about a global binding that decide where it lives.
    ///
    /// A script's globals split in two, and the split is observable: `var` and
    /// function declarations become properties of the global object (`globalThis.v`
    /// works, `delete` refuses, `for-in` lists them), while `let`/`const`/`class`
    /// declarations live in the global *lexical* environment and are invisible there
    /// (`"c" in globalThis` is false). Keeping the difference in the closure var's
    /// `is_lexical` flag means the answer is already in the bytecode.
    fn global_binding(&self, slot: usize) -> (String, bool, bool) {
        let name = self.global_name(slot);
        // "Lexical" is *not* read from the closure var's flag: a `GlobalRef` created
        // while forwarding a reference through a nested function is built with the
        // flags cleared, so `const c` would look like an ordinary `var` from inside a
        // closure. Presence in the realm's lexical table is the same fact, recorded
        // once, and every slot sees it.
        let lexical = self.realm.globals.contains_key(&name);
        let is_const = self.realm.lexical_consts.contains(&name);
        (name, lexical, is_const)
    }

    fn get_global(&mut self, slot: usize) -> JsResult<Value> {
        let (name, lexical, _) = self.global_binding(slot);
        if name == "arguments"
            && let Some(arguments) = &self.eval_arguments
        {
            return Ok(arguments.clone());
        }
        // The frame that called `eval` is the innermost scope the eval'd code sees, so
        // its local `a` wins over a global `a`.
        if let Some(value) = self.eval_binding_value(&name) {
            return Ok(value);
        }
        // A binding declared by the strict `eval` currently running shadows anything
        // further out, so `eval("var v = 1")`'s `v` is found before `globalThis.v`.
        if let Some(scope) = &self.eval_scope
            && let Some(value) = scope.get(&name)
        {
            return Ok(value.clone());
        }
        if lexical {
            // A lexical global is a binding in a side table, so the dead zone is
            // visible and it is nowhere on the global object.
            if let Some(cell) = self.realm.globals.get(&name) {
                let value = cell.get();
                if matches!(value, Value::Uninitialized) {
                    return Err(self.throw_reference(&name));
                }
                return Ok(value);
            }
            return Err(self.throw_reference(&name));
        }
        // A `var` global is an ordinary property, so the prototype chain and the
        // global object's own builtins are all reachable through it.
        match self.get_property(&self.realm.global.clone(), &name)? {
            Some(value) => Ok(value),
            None => Err(self.throw_reference(&name)),
        }
    }

    /// Store to a global binding.
    ///
    /// Returns a signal when the store is refused: a `const` global may only be
    /// written by its initialiser, and a non-configurable `var` global may not be
    /// deleted but is always writable.
    fn put_global(&mut self, slot: usize, value: Value) -> JsResult<()> {
        let (name, lexical, is_const) = self.global_binding(slot);
        // A binding of the frame that called `eval` takes the write, which is what
        // makes `f("a = 3")` change the caller's `a`.
        if self.set_eval_binding(&name, &value) {
            return Ok(());
        }
        // Inside a strict `eval`, a name it declared itself is stored there. An
        // *assignment* to a name nobody declared is not a declaration, so it falls
        // through: strict mode raises `ReferenceError` for it, which is what
        // `eval("1 + (@not_def = 1)")` has to do.
        if let Some(scope) = self.eval_scope.as_mut()
            && scope.contains_key(&name)
        {
            scope.insert(name, value);
            return Ok(());
        }
        if lexical {
            if let Some(cell) = self.realm.globals.get(&name).cloned() {
                if is_const && !matches!(cell.get(), Value::Uninitialized) {
                    return Err(self.read_only_global(&name));
                }
                cell.set(value);
                return Ok(());
            }
            // Writing an undeclared lexical global is a `ReferenceError` in strict
            // mode and a silent global creation otherwise; creating the cell here
            // keeps both paths from losing the value.
            let cell = self.new_cell(value);
            self.realm.globals.insert(name, cell);
            return Ok(());
        }
        let global = self.realm.global.clone();
        match global.own_property(&name) {
            // A `var` global is writable, so the store goes straight through — but
            // through the property, so `globalThis.v = 3` and `v = 3` agree. A failed
            // store (a non-writable or accessor-without-setter global) is a silent
            // no-op in sloppy mode and a `TypeError` in strict mode, which is the same
            // split every other assignment has.
            Some(_) => {
                let on_failure = if self.current_strict() {
                    DeleteFailure::Throw
                } else {
                    DeleteFailure::Ignore
                };
                self.set_property_with(&global, &name, value, on_failure)
            }
            // Only an implicit global reaches here. In sloppy mode `x = 1` with no
            // declaration creates a *configurable* property, which is what makes it
            // deletable and is the one way a global differs from a declared `var`.
            // In strict mode the same store is a `ReferenceError` instead: that is the
            // whole point of strict mode, and creating the global silently meant
            // `"use strict"; x = 1` looked like it worked.
            None => {
                if self.current_strict() {
                    return Err(self.throw_reference(&name));
                }
                global.define_data(&name, value);
                Ok(())
            }
        }
    }

    /// The value of a binding of the frame that called `eval`, if there is one.
    fn eval_binding_value(&self, name: &str) -> Option<Value> {
        let binding = self
            .eval_bindings
            .iter()
            .find(|binding| binding.name == name)?;
        let owner = self.frames.get(binding.frame)?;
        let (slot, index) = (binding.slot, binding.index);
        Some(match slot {
            EvalSlot::Arg => match owner.code.arg_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => owner
                    .cells
                    .get(cell as usize)
                    .map(|cell| cell.get())
                    .unwrap_or(Value::Undefined),
                _ => owner.args.get(index).cloned().unwrap_or(Value::Undefined),
            },
            EvalSlot::Local => match owner.code.loc_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => owner
                    .cells
                    .get(cell as usize)
                    .map(|cell| cell.get())
                    .unwrap_or(Value::Undefined),
                _ => owner.locals.get(index).cloned().unwrap_or(Value::Undefined),
            },
        })
    }

    /// Write to a binding of the frame that called `eval`. Returns whether one matched.
    fn set_eval_binding(&mut self, name: &str, value: &Value) -> bool {
        let Some(binding) = self
            .eval_bindings
            .iter()
            .find(|binding| binding.name == name)
        else {
            return false;
        };
        let (slot, index, frame) = (binding.slot, binding.index, binding.frame);
        let Some(owner) = self.frames.get_mut(frame) else {
            return false;
        };
        match slot {
            EvalSlot::Arg => match owner.code.arg_cell.get(index).copied().unwrap_or(-1) {
                cell if cell >= 0 => {
                    if let Some(cell) = owner.cells.get(cell as usize) {
                        cell.set(value.clone());
                    }
                }
                _ => {
                    if let Some(slot) = owner.args.get_mut(index) {
                        *slot = value.clone();
                    }
                }
            },
            EvalSlot::Local => {
                // A sloppy `eval("m = 1")` where `m` is the name of a named function
                // expression is ignored, the same as a direct store would be.
                if owner.code.code.vars.get(index).is_some_and(|var| {
                    var.kind == js_compile::VarKind::FunctionName && !var.is_const
                }) {
                    return true;
                }
                match owner.code.loc_cell.get(index).copied().unwrap_or(-1) {
                    cell if cell >= 0 => {
                        if let Some(cell) = owner.cells.get(cell as usize) {
                            cell.set(value.clone());
                        }
                    }
                    _ => {
                        if let Some(slot) = owner.locals.get_mut(index) {
                            *slot = value.clone();
                        }
                    }
                }
            }
        }
        true
    }

    /// Whether the realm already has a binding for `name`, lexically or as an *own*
    /// property of the global object. Used to decide whether a store inside an
    /// `eval` writes through to the realm or declares a binding of its own.
    ///
    /// Own rather than inherited: `eval("var toString = 1")` must not be taken for a
    /// write to `Object.prototype.toString`, and must not shadow it either.
    fn global_knows(&mut self, name: &str) -> bool {
        if self.realm.globals.contains_key(name) {
            return true;
        }
        self.realm.global.own_property(name).is_some()
    }

    /// Whether the code currently running is strict.
    ///
    /// Read off the frame's function, which is where the parser's answer ended up; a
    /// native function never runs strict-only checks, and there is no frame at all
    /// before the first script.
    pub fn current_strict(&self) -> bool {
        self.frames
            .last()
            .is_some_and(|frame| frame.code.code.strict)
    }

    /// Initialise a declared global, which is what ends its dead zone.
    ///
    /// A `var` global becomes a property of the global object: writable, so a later
    /// assignment goes through, and non-configurable, so `delete` refuses.
    fn init_global(&mut self, slot: usize, value: Value) -> JsResult<()> {
        let (name, lexical, _) = self.global_binding(slot);
        // A `var` declared by the strict `eval` currently running belongs to the
        // eval's scope, not to the global object: `eval("var v = 1")` must not make
        // `typeof v` answer `"number"` afterwards. A name the realm already has is
        // initialised in place, which is what a re-declaration inside eval does.
        if self.eval_scope.is_some() && !lexical && !self.global_knows(&name) {
            self.eval_scope
                .as_mut()
                .expect("checked above")
                .insert(name, value);
            return Ok(());
        }
        if let Some(scope) = self.eval_scope.as_mut()
            && scope.contains_key(&name)
        {
            scope.insert(name, value);
            return Ok(());
        }
        if lexical {
            // The cell exists (created at declaration time) holding the dead-zone
            // marker; storing the value is what makes the binding readable.
            if let Some(cell) = self.realm.globals.get(&name).cloned() {
                cell.set(value);
                return Ok(());
            }
            let cell = self.new_cell(value);
            self.realm.globals.insert(name, cell);
            return Ok(());
        }
        let global = self.realm.global.clone();
        // Writable and enumerable, but *not* configurable: `delete globalThis.v` has
        // to answer `false` and leave the binding in place. `with_attributes` ties
        // `configurable` to `writable`, so the property is built field by field.
        global.define(
            &name,
            Property {
                value: Some(value),
                writable: true,
                enumerable: true,
                configurable: false,
                ..Property::default()
            },
        );
        Ok(())
    }

    /// The `TypeError` a store to a `const` global raises.
    fn read_only_global(&self, name: &str) -> Signal {
        Signal::Throw(self.make_error("TypeError", &format!("'{name}' is read-only")))
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
        // A hole reads as `undefined` but is not a stored `undefined`; mapping it
        // here keeps that distinction out of every read site.
        object.array_get(index as usize).map(|value| match value {
            Value::Hole => Value::Undefined,
            other => other,
        })
    }

    /// Store `object[key] = value`, honouring setters and inherited properties.
    pub fn set_property(&mut self, object: &Gc<JsObject>, key: &str, value: Value) -> JsResult<()> {
        self.set_property_with(object, key, value, DeleteFailure::Throw)
    }

    /// Store a property, choosing what to do when the object refuses.
    ///
    /// A rejected store is a `TypeError` in strict mode and a silent no-op in sloppy
    /// mode, which is the one place assignment differs by mode. Native code (the array
    /// methods, `JSON`) always asks for the throw.
    pub fn set_property_with(
        &mut self,
        object: &Gc<JsObject>,
        key: &str,
        value: Value,
        on_failure: DeleteFailure,
    ) -> JsResult<()> {
        if let Some(index) = array_index(key) {
            if object.is_array() {
                // Adding an index past the end makes the array longer, which a
                // non-extensible array refuses.
                if index as usize >= object.array_length().unwrap_or(0) && !object.is_extensible() {
                    return match on_failure {
                        DeleteFailure::Throw => Err(self.type_error(format!(
                            "cannot add property '{key}', object is not extensible"
                        ))),
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                if object.element_is_locked(index) {
                    return match on_failure {
                        DeleteFailure::Throw => {
                            Err(self.type_error(format!("'{key}' is read-only")))
                        }
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                object.array_set(index as usize, value);
                return Ok(());
            }
        }
        if key == "length" && object.is_array() {
            let length = value.to_number();
            if length.is_finite() && length >= 0.0 {
                let length = length as usize;
                // `a.length = n` with `n` below a non-configurable element cannot
                // drop that element, so the whole assignment fails. The spec makes
                // this a `TypeError` rather than a silent no-op, in any mode.
                if let Some(blocked) = (length..object.array_length().unwrap_or(0))
                    .find(|index| object.element_is_locked(*index as u32))
                {
                    // Sloppy mode stops the truncation at the blocking element
                    // instead of failing; strict mode raises.
                    // Both modes stop at the blocking element; they differ in
                    // whether that is reported. The reference truncates *then*
                    // throws, so the array is left in the shortened state.
                    object.array_truncate(blocked + 1);
                    return match on_failure {
                        DeleteFailure::Throw => {
                            Err(self.type_error(format!("cannot delete property '{blocked}'")))
                        }
                        DeleteFailure::Ignore => Ok(()),
                    };
                }
                object.array_set_length(length);
                return Ok(());
            }
        }
        if !object.is_extensible() && !object.has_own(key) && lookup(object, key).is_none() {
            return match on_failure {
                DeleteFailure::Throw => Err(self.type_error(format!(
                    "cannot add property '{key}', object is not extensible"
                ))),
                DeleteFailure::Ignore => Ok(()),
            };
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
                return match on_failure {
                    DeleteFailure::Throw => Err(self.type_error(format!("'{key}' is read-only"))),
                    DeleteFailure::Ignore => Ok(()),
                };
            }
            // An existing writable own property keeps its attributes: only the value
            // moves. Redefining instead would reset them, which is how a
            // non-configurable global `var` used to become deletable.
            if owner.ptr_eq(object) && object.write_value(key, value.clone()) {
                return Ok(());
            }
        }
        // No own data property: create one, shadowing whatever the prototype holds.
        object.define_data(key, value);
        Ok(())
    }

    /// Delete `object[key]`.
    pub fn delete_property(&mut self, object: &Gc<JsObject>, key: &str) -> bool {
        if let Some(index) = array_index(key) {
            if let Some(elements) = object.array_elements()
                && (index as usize) < elements.len()
            {
                // A hole rather than a shift: `delete a[0]` leaves `length` alone and
                // the index stops answering `in`.
                if object.element_is_locked(index) {
                    return false;
                }
                object.array_set(index as usize, Value::Hole);
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
                Callable::Native { func } => Some(NativeCall::Native(*func)),
                Callable::Bound {
                    target,
                    this,
                    args: leading,
                } => Some(NativeCall::Bound {
                    target: target.clone(),
                    this: this.clone(),
                    leading: leading.clone(),
                }),
                Callable::Bytecode { code, env } => {
                    Some(NativeCall::Bytecode(code.clone(), env.clone()))
                }
            },
            _ => None,
        });
        match callable {
            Some(NativeCall::Native(func)) => {
                // A function marked `_frame_less` is one the reference implements
                // without a call frame — `eval` is the only one so far.
                let frame_less = function
                    .own_property("_frame_less")
                    .and_then(|property| property.value)
                    .is_some_and(|value| value.to_boolean());
                let name = function
                    .own_property("name")
                    .and_then(|property| property.value)
                    .map(|value| value.to_js_string())
                    .unwrap_or_default();
                if !frame_less {
                    self.native_frames.push(name);
                }
                let result = func(self, this, &args);
                if !frame_less {
                    self.native_frames.pop();
                }
                result
            }
            Some(NativeCall::Bound {
                target,
                this: bound_this,
                leading,
            }) => {
                let mut all = leading;
                all.extend(args);
                self.call_value(target, bound_this, all)
            }
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

    /// The object `special_object` builds.
    ///
    /// Only `arguments` is implemented. It is an ordinary object with a `length` and
    /// one property per passed argument, which is what `typeof`, `arguments[i]` and
    /// `arguments.length` observe. Mapped arguments — sloppy mode's aliasing between
    /// `arguments[i]` and the parameter it came from — is *not* implemented, so a
    /// write through `arguments` does not reach the parameter.
    fn special_object(&mut self, magic: u32) -> JsResult<Value> {
        match magic {
            0 | 1 => {
                let args = self.frame().passed.clone();
                let proto = self.realm.object_proto.clone();
                let object = self.new_object(Some(proto));
                object.define_data("length", Value::Int(args.len() as i32));
                for (index, value) in args.into_iter().enumerate() {
                    object.define_data(&index.to_string(), value);
                }
                Ok(Value::Object(object))
            }
            // `this function`: what a named function expression binds its own name to.
            2 => Ok(self
                .current_callee()
                .map_or(Value::Undefined, Value::Object)),
            other => Err(Signal::Throw(self.make_error(
                "InternalError",
                &format!("special_object {other} is not implemented yet"),
            ))),
        }
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
    /// The elements of an array-like value, as a call argument list.
    ///
    /// `apply` accepts any array-like, not only a real array: the reference reads
    /// `length` and then each index, so `{length: 2, 0: "a", 1: "b"}` works too.
    fn array_like_to_args(&mut self, value: &Value) -> JsResult<Vec<Value>> {
        let Some(object) = value.as_object().cloned() else {
            if value.is_nullish() {
                // `f.apply(this, null)` passes no arguments at all.
                return Ok(Vec::new());
            }
            return Err(self.type_error("createListFromArrayLike called on non-object"));
        };
        let length = match self.get_property(&object, "length")? {
            Some(length) => length.to_number(),
            None => 0.0,
        };
        if !length.is_finite() || length <= 0.0 {
            return Ok(Vec::new());
        }
        let length = (length as usize).min(1_000_000);
        let mut args = Vec::with_capacity(length);
        for index in 0..length {
            let element = self.get_property(&object, &index.to_string())?;
            args.push(element.unwrap_or(Value::Undefined));
        }
        Ok(args)
    }

    fn call_or_push(
        &mut self,
        callee: Value,
        this: Value,
        args: Vec<Value>,
    ) -> JsResult<Option<Value>> {
        if let Value::Object(function) = &callee
            && let Some((code, env)) = bytecode_of(function)
        {
            self.push_frame(code, env, this, args, callee)?;
            return Ok(None);
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
        // `new` on a bound function constructs its *target*, prepending the arguments
        // the bind fixed and ignoring the bound `this` — a bound function is a
        // constructor only when its target is one, and `new` always makes a fresh
        // object of the target's prototype rather than reusing the bound receiver.
        // Without this unwrapping, `constructor1.bind(null, 1)` called under `new`
        // ran the body with `this === null` and threw on `this.x = a`.
        let mut callee = callee;
        let mut args = args;
        loop {
            let Some(object) = callee.as_object().cloned() else {
                return Err(Signal::Throw(
                    self.make_error("TypeError", "not a constructor"),
                ));
            };
            let bound = object.with(|data| match &data.kind {
                ObjectKind::Function(Callable::Bound {
                    target,
                    args: leading,
                    ..
                }) => Some((target.clone(), leading.clone())),
                _ => None,
            });
            let Some((target, leading)) = bound else {
                break;
            };
            let mut all = leading;
            all.extend(args);
            args = all;
            callee = target;
        }
        let Value::Object(function) = &callee else {
            unreachable!("the loop above only breaks on an object");
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
        let proto = self
            .realm
            .errors
            .get(name)
            .map(|constructor| self.prototype_of(constructor))
            .unwrap_or_else(|| self.realm.error_proto.clone());
        let error = self.new_object(Some(proto));
        // `name` and `message` are own properties of the instance, which is what
        // makes `e.name` cheap and `Error.prototype.name` the fallback for a plain
        // `Error`.
        error.define(
            "name",
            Property::with_attributes(self.string(name), true, false),
        );
        error.define(
            "message",
            Property::with_attributes(self.string(message), true, false),
        );
        // The trace is captured where the error is *made*, which is where the reference
        // captures it too: `JS_ThrowTypeError` and friends call `build_backtrace` on the
        // way out, so a caught engine error already has its stack even if the handler
        // never rethrows it.
        self.add_stack(&error);
        Value::Object(error)
    }

    /// Give an error object its `stack` property, from the current frame stack.
    ///
    /// Only objects get one — `throw 42` has no stack anywhere — and an object that
    /// already has a `stack` keeps it, so an error caught and rethrown, or one whose
    /// `stack` a script set, is not rewritten.
    pub fn add_stack(&self, object: &Gc<JsObject>) {
        self.add_stack_skipping(object, 0);
    }

    /// [`Interp::add_stack`], omitting the `skip` innermost frames.
    ///
    /// An `Error` constructor passes 1: the reference builds those traces with
    /// `JS_BACKTRACE_FLAG_SKIP_FIRST_LEVEL`, so `new Error()` does not report the
    /// constructor itself, while a `RangeError` raised inside `toISOString` does
    /// report `at toISOString (native)`.
    pub fn add_stack_skipping(&self, object: &Gc<JsObject>, skip: usize) {
        if object.has_own("stack") {
            return;
        }
        let trace = self.stack_trace_skipping(skip);
        let text = self.string(trace);
        object.define("stack", Property::with_attributes(text, true, true));
    }

    /// The function object currently being called, if any.
    ///
    /// A native function is a bare `fn` pointer, so it cannot capture which class
    /// it belongs to; it reads that from the callee instead. This is also what a
    /// stack trace would use.
    pub fn current_callee(&self) -> Option<Gc<JsObject>> {
        match &self.frames.last()?.callee {
            Value::Object(object) => Some(object.clone()),
            _ => None,
        }
    }

    /// The `prototype` property of a constructor, falling back to
    /// `Object.prototype` for anything that is not shaped like one.
    pub fn prototype_of(&self, constructor: &Gc<JsObject>) -> Gc<JsObject> {
        constructor
            .own_property("prototype")
            .and_then(|property| property.value)
            .and_then(|value| value.as_object().cloned())
            .unwrap_or_else(|| self.realm.object_proto.clone())
    }

    /// Throw a `TypeError` carrying `message`.
    pub fn type_error(&self, message: impl AsRef<str>) -> Signal {
        Signal::Throw(self.make_error("TypeError", message.as_ref()))
    }

    /// Throw a `RangeError` carrying `message`.
    pub fn range_error(&self, message: impl AsRef<str>) -> Signal {
        Signal::Throw(self.make_error("RangeError", message.as_ref()))
    }

    /// The `ReferenceError` a read of an uninitialised binding raises.
    /// The name of the binding `slot` refers to, for an error message.
    fn binding_name(&self, slot: usize, kind: VarSlot) -> String {
        match kind {
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
        .unwrap_or_default()
    }

    /// Whether `slot` names a `const` binding, which rejects every store after its
    /// initialiser.
    fn is_read_only(&self, slot: usize, kind: VarSlot) -> bool {
        match kind {
            VarSlot::Loc => self
                .frame()
                .code
                .code
                .vars
                .get(slot)
                .is_some_and(|v| v.is_const),
            VarSlot::Closure => self
                .frame()
                .code
                .code
                .closure_vars
                .get(slot)
                .is_some_and(|c| c.is_const),
        }
    }

    /// The `TypeError` a store to a `const` binding raises.
    ///
    /// `const` is enforced here rather than at compile time because the binding can be
    /// assigned from a place the compiler cannot classify — `c = 2` inside a closure
    /// is just a `put_var_ref_check`. `PutLocCheck`/`PutVarRefCheck` therefore check
    /// both halves of what "checked store" means: the dead zone is over, and the
    /// binding is writable.
    fn read_only(&self, slot: usize, kind: VarSlot) -> Signal {
        let name = self.binding_name(slot, kind);
        Signal::Throw(self.make_error("TypeError", &format!("'{name}' is read-only")))
    }

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
        self.frame_mut().at = pc;
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
            Swap => {
                // `a b -> b a`
                let bottom = self.pop();
                let top = self.pop();
                self.push(bottom);
                self.push(top);
            }
            Perm3 => {
                // `a b c -> b a c`, the reference's reordering for a spread method
                // call: `obj func array` becomes the `func this array` that `apply`
                // takes.
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push(b);
                self.push(a);
                self.push(c);
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
            Perm4 => {
                // `obj prop a b -> a obj prop b`, the reference's reordering for a
                // postfix update of a computed element: it puts the *old* value
                // underneath the receiver and key so `put_array_el` can consume them
                // and leave the old value as the expression's result.
                let d = self.pop();
                let c = self.pop();
                let b = self.pop();
                let a = self.pop();
                self.push(c);
                self.push(a);
                self.push(b);
                self.push(d);
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
                // The initialiser goes through `put_loc`, not this, so any store that
                // reaches here is a later assignment — which a `const` refuses.
                if self.is_read_only(slot, VarSlot::Loc) {
                    return Err(self.read_only(slot, VarSlot::Loc));
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
                // "Close" a binding: give it a *fresh* cell holding the current
                // value. Closures created before this point keep the old cell and the
                // ones created after see the new one, which is what makes every
                // iteration of `for (let i ...)` capture its own `i`, and what freezes
                // a block-scoped binding when the block ends.
                //
                // Clearing the slot instead (which this used to do) broke both: a
                // closure over a captured block variable read `undefined`, and a loop
                // terminated after one iteration.
                let slot = self.slot_operand(op, a);
                let index = self.frame().code.loc_cell.get(slot).copied().unwrap_or(-1);
                if index >= 0 {
                    let current = self.frame().cells[index as usize].get();
                    let fresh = self.new_cell(current);
                    if let Some(cell) = self.frame_mut().cells.get_mut(index as usize) {
                        *cell = fresh;
                    }
                }
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
                let value = self.cell_value(slot)?;
                self.push(value);
            }
            GetVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.cell_value(slot)?;
                if matches!(value, Value::Uninitialized) {
                    return Err(self.dead_zone(slot, VarSlot::Closure));
                }
                self.push(value);
            }
            PutVarRef | PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 | PutVarRefCheck => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                // Same rule as `PutLocCheck`: a checked store to a captured binding
                // refuses a `const` once its initialiser has run.
                if op == PutVarRefCheck && self.is_read_only(slot, VarSlot::Closure) {
                    return Err(self.read_only(slot, VarSlot::Closure));
                }
                self.set_cell(slot, value)?;
            }
            SetVarRef | SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => {
                let slot = self.slot_operand(op, a);
                let value = self.pop();
                self.set_cell(slot, value.clone())?;
                self.push(value);
            }

            // -- globals ----------------------------------------------------
            GetVar => {
                let value = self.get_global(a as usize)?;
                self.push(value);
            }
            GetVarUndef => {
                let value = self
                    .get_global(a as usize)
                    .or_else(|_| Ok(Value::Undefined))?;
                self.push(value);
            }
            DeleteVar => {
                // `delete name` for a name that is not a declared binding. A lexical
                // global lives in the reference table and cannot be deleted; anything
                // reachable only as a property of the global object is *implicit* —
                // `x = 1` with no declaration — and goes away if it is configurable,
                // which is what `delete` on an implicit global answers.
                let name = self.atom_text(a as usize);
                let global = self.realm.global.clone();
                let deleted = if self.realm.globals.contains_key(&name) {
                    false
                } else if global.own_property(&name).is_some() {
                    self.delete_property(&global, &name)
                } else {
                    true
                };
                self.push(Value::Bool(deleted));
            }
            PutVar => {
                let value = self.pop();
                self.put_global(a as usize, value)?;
            }
            PutVarInit => {
                let slot = a as usize;
                let value = self.pop();
                self.init_global(slot, value)?;
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
            Eq | Neq => {
                let right = self.pop();
                let left = self.pop();
                let equal = self.abstract_equals(left, right)?;
                self.push(Value::Bool(if op == Eq { equal } else { !equal }));
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
            Apply => {
                // `func this array -> result`, the spread-call form: the compiler
                // builds one array of arguments, and all that is left here is to
                // unpack it. Magic 1 is `new` (`apply 1`), which constructs.
                let array = self.pop();
                let this = self.pop();
                let callee = self.pop();
                let args = self.array_like_to_args(&array)?;
                if a & 1 != 0 {
                    let result = self.construct(callee, this, args)?;
                    self.push(result);
                } else if let Some(result) = self.call_or_push(callee, this, args)? {
                    self.push(result);
                }
            }
            TailCall | TailCallMethod => {
                // `tail_call` is `call` whose result is returned immediately — the
                // reference jumps straight to `done` instead of storing the value and
                // running on. The callee still gets its own frame, so deep tail
                // recursion hits the stack limit exactly like plain recursion does;
                // marking this frame lets the return path unwind without needing
                // instructions after the call.
                let argc = if op == TailCallMethod {
                    a as usize
                } else {
                    self.argc_operand(op, a)
                };
                let args = self.pop_n(argc);
                let callee = self.pop();
                let this = if op == TailCallMethod {
                    self.pop()
                } else {
                    Value::Undefined
                };
                if let Value::Object(function) = &callee
                    && let Some((code, env)) = bytecode_of(function)
                {
                    self.frame_mut().tail = true;
                    self.push_frame(code, env, this, args, callee)?;
                    return Ok(Step::Continue);
                }
                // A native callee has no frame to reuse, so its value is simply
                // returned.
                let value = self.call_value(callee, this, args)?;
                return Ok(Step::Return(value));
            }

            // -- objects ----------------------------------------------------
            SpecialObject => {
                let value = self.special_object(a)?;
                self.push(value);
            }
            GetLength => {
                // `get_length` is the `.length` read; an array and a string answer
                // from their own length, anything else from the property.
                let target = self.pop();
                let value = match &target {
                    Value::Object(object) => match object.array_length() {
                        Some(length) => Value::Int(length as i32),
                        None => self.read_field(&target, "length")?,
                    },
                    Value::String(text) => Value::Int(text.utf16_len() as i32),
                    _ => self.read_field(&target, "length")?,
                };
                self.push(value);
            }
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
                // `{m: function(){}}` names the function `m`, the same way assigning
                // it to a property does. Without this the frame is anonymous and a
                // stack trace reads `at  (file:1:2)`.
                self.set_function_name(&value, &key);
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
                self.set_function_name(&function, &key_with_accessor_prefix(&key, b));
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
                self.set_function_name(&function, &key_with_accessor_prefix(&text, a));
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

            Delete => {
                // `delete obj[key]`, where the key was just pushed. Deleting an array
                // index leaves a *hole* rather than closing the gap, and a
                // non-configurable property answers `false` and stays.
                let key = self.pop();
                let target = self.pop();
                let key = self.propkey_of(&key);
                let deleted = match &target {
                    Value::Object(object) => {
                        let object = object.clone();
                        self.delete_property(&object, &key)
                    }
                    // A string's `length` and its index properties are
                    // non-configurable, so deleting one answers `false`.
                    Value::String(text) => {
                        !(key == "length"
                            || array_index(&key)
                                .is_some_and(|index| (index as usize) < text.utf16_len()))
                    }
                    // Any other primitive has no own properties; deleting nothing
                    // succeeds.
                    _ => true,
                };
                self.push(Value::Bool(deleted));
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

    /// Whether `slot` refers to a global binding through the reference table.
    ///
    /// A global's cell in the closure environment only exists so the slot has
    /// something to point at; the binding itself lives on the global object (a `var`)
    /// or in the realm's lexical table (a `let`). Reads and writes therefore have to
    /// be routed, not performed on the placeholder.
    fn is_global_slot(&self, slot: usize) -> bool {
        self.frame()
            .code
            .code
            .closure_vars
            .get(slot)
            .is_some_and(|c| {
                matches!(
                    c.closure_type,
                    ClosureType::GlobalDecl
                        | ClosureType::Global
                        | ClosureType::GlobalRef
                        | ClosureType::ModuleDecl
                        | ClosureType::ModuleImport
                )
            })
    }

    fn cell_value(&mut self, slot: usize) -> JsResult<Value> {
        if self.is_global_slot(slot) {
            return self.get_global(slot);
        }
        Ok(self
            .var_ref_cell(slot)
            .map(|cell| cell.get())
            .unwrap_or(Value::Undefined))
    }

    fn set_cell(&mut self, slot: usize, value: Value) -> JsResult<()> {
        // A function declaration is hoisted by storing into a global reference before
        // the body runs, so this path is also what puts it on the global object.
        if self.is_global_slot(slot) {
            return self.put_global(slot, value);
        }
        if let Some(cell) = self.var_ref_cell(slot) {
            cell.set(value);
        }
        Ok(())
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
        let name = function.name.clone();
        let length = function.args.len() as u32;
        let code = Gc::new(self.heap, CompiledCode::new(*function));
        let env = Gc::new(self.heap, ClosureEnv { cells });
        let object = JsObject::function(
            self.heap,
            Some(self.realm.function_proto.clone()),
            Callable::Bytecode { code, env },
        );
        object.define(
            "length",
            Property::with_attributes(Value::Int(length as i32), false, false),
        );
        if !name.is_empty() {
            object.define(
                "name",
                Property::with_attributes(self.string(name), false, false),
            );
        }
        self.install_function_prototype(&object);
        Ok(Value::Object(object))
    }

    // -- conversions used by the operators ----------------------------------

    fn numeric_of(&mut self, value: Value) -> JsResult<Value> {
        // `+x` is `ToNumber`, which for a string parses it and for an object runs
        // `ToPrimitive` first. Returning a string unchanged would let `+"1"` stay a
        // string, which then behaves differently in every later comparison.
        match value {
            Value::Int(_) | Value::Float(_) => Ok(value),
            other => Ok(Value::Float(self.to_js_number(&other)?)),
        }
    }

    fn numeric(&mut self, value: Value) -> JsResult<Value> {
        // `ToNumber` on an object runs `ToPrimitive` first, which is how
        // `2 * {valueOf(){return 3}}` sees the 3 — and how a `valueOf` that throws is
        // heard at all. Reading the object's own tag instead (`other.to_number()`)
        // turned the whole operand into `NaN` and swallowed the exception.
        match value {
            Value::Int(_) | Value::Float(_) => Ok(value),
            other => Ok(Value::Float(self.to_js_number(&other)?)),
        }
    }

    fn binary_add(&mut self) -> JsResult<()> {
        let right = self.pop();
        let left = self.pop();
        // `+` concatenates when either operand is a string *after* `ToPrimitive`.
        let left = self.primitive_of(left)?;
        let right = self.primitive_of(right)?;
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

    fn primitive_of(&mut self, value: Value) -> JsResult<Value> {
        self.to_primitive(value, Hint::Default)
    }

    /// `ToPrimitive`, which is what makes `"" + obj` produce `obj.toString()`.
    ///
    /// The order depends on the hint: the "number" order tries `valueOf` first and
    /// `toString` second, the "string" order the other way round, and the default
    /// hint uses the number order. A result that is still an object is a `TypeError`,
    /// because no built-in `valueOf`/`toString` returns one.
    pub fn to_primitive(&mut self, value: Value, hint: Hint) -> JsResult<Value> {
        let Value::Object(object) = &value else {
            return Ok(value);
        };
        // A Date overrides the default hint to mean "string", which is what
        // `Date.prototype[Symbol.toPrimitive]` does with hint `"default"`. Without
        // this, `"" + new Date()` would print the milliseconds, not the date.
        let hint = if hint == Hint::Default && is_date_object(object) {
            Hint::String
        } else {
            hint
        };
        let order: [&str; 2] = match hint {
            Hint::String => ["toString", "valueOf"],
            Hint::Number | Hint::Default => ["valueOf", "toString"],
        };
        let receiver = object.clone();
        for method in order {
            if let Some(function) = self.get_property(&receiver, method)?
                && matches!(&function, Value::Object(object) if object.is_callable())
            {
                let result =
                    self.call_value(function, Value::Object(receiver.clone()), Vec::new())?;
                if !matches!(result, Value::Object(_)) {
                    return Ok(result);
                }
            }
        }
        Err(self.type_error("cannot convert object to primitive value"))
    }

    /// `ToString` with the object case going through `ToPrimitive`.
    pub fn to_js_string(&mut self, value: &Value) -> JsResult<String> {
        if matches!(value, Value::Object(_)) {
            let primitive = self.to_primitive(value.clone(), Hint::String)?;
            return Ok(primitive.to_js_string());
        }
        Ok(value.to_js_string())
    }

    /// `ToNumber` with the object case going through `ToPrimitive`.
    pub fn to_js_number(&mut self, value: &Value) -> JsResult<f64> {
        if matches!(value, Value::Object(_)) {
            let primitive = self.to_primitive(value.clone(), Hint::Number)?;
            return Ok(primitive.to_number());
        }
        Ok(value.to_number())
    }

    fn propkey_of(&mut self, value: &Value) -> String {
        match value {
            Value::Int(_) | Value::Float(_) => number_to_string(value.to_number()),
            // An object key runs `ToPropertyKey`, which needs `ToPrimitive` first;
            // falling back to its `ToString` would print `[object Object]`.
            Value::Object(_) => self
                .to_js_string(value)
                .unwrap_or_else(|_| "[object Object]".to_string()),
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
            // A property of a primitive is looked up on its prototype: this is what
            // makes `"a".toUpperCase()` reach `String.prototype` without boxing.
            let prototype = match object {
                Value::String(_) => self.realm.string_proto.clone(),
                Value::Int(_) | Value::Float(_) => self.realm.number_proto.clone(),
                Value::Bool(_) => self.realm.boolean_proto.clone(),
                _ => None,
            };
            if let Some(prototype) = prototype
                && let Some(value) = self.get_property(&prototype, key)?
            {
                return Ok(value);
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
        // `put_field`/`put_array_el` are plain assignments, so the mode decides.
        let on_failure = if self.frame().code.code.strict {
            DeleteFailure::Throw
        } else {
            DeleteFailure::Ignore
        };
        self.set_property_with(object, key, value, on_failure)
    }

    /// `a == b`, the coercing comparison.
    ///
    /// An object operand is converted with `ToPrimitive` *through the interpreter*,
    /// which is what makes `[] == ""` true: `[]` becomes `""` via
    /// `Array.prototype.toString`. [`Value::loose_equals`] cannot do that on its own
    /// because the conversion is allowed to call script code, so the two primitive
    /// operands are handed back to it.
    fn abstract_equals(&mut self, left: Value, right: Value) -> JsResult<bool> {
        if !matches!(left, Value::Object(_)) && !matches!(right, Value::Object(_)) {
            return Ok(left.loose_equals(&right));
        }
        // `null == undefined` holds even though neither is an object.
        if matches!(left, Value::Undefined | Value::Null)
            && matches!(right, Value::Undefined | Value::Null)
        {
            return Ok(true);
        }
        if matches!(left, Value::Undefined | Value::Null)
            || matches!(right, Value::Undefined | Value::Null)
        {
            return Ok(false);
        }
        // Two objects compare by identity. Only an object *against a primitive* is
        // converted, which is why `{} == {}` is false while `[] == ""` is true.
        if let (Value::Object(a), Value::Object(b)) = (&left, &right) {
            return Ok(a.ptr_eq(b));
        }
        let left = self.to_primitive(left, Hint::Default)?;
        let right = self.to_primitive(right, Hint::Default)?;
        Ok(left.loose_equals(&right))
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
        // An array hole is an index that exists as a slot but not as a property, so
        // `1 in [1,,3]` is `false` even though reading `a[1]` yields `undefined`.
        if let Some(index) = array_index(&text)
            && object
                .array_get(index as usize)
                .is_some_and(|value| !matches!(value, Value::Hole))
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
        // `CopyDataProperties` starts with `ToObject(source)`, so a *string* primitive
        // does contribute: its index properties are own, enumerable and writable, which
        // is why `{..."ab"}` is `{"0":"a","1":"b"}`. Numbers and booleans box to objects
        // with no own properties, so they copy nothing, and spreading `undefined`/`null`
        // is a no-op.
        let Some(target) = target.as_object() else {
            return Ok(());
        };
        let source_object = match source {
            Value::Object(object) => object.clone(),
            Value::String(text) => {
                for (index, character) in text.as_str().chars().enumerate() {
                    target.define_data(&index.to_string(), self.string(character.to_string()));
                }
                return Ok(());
            }
            _ => return Ok(()),
        };
        let source = &source_object;
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
    ///
    /// An object that already carries a `next` method is used as it is — that is what
    /// `arr.values()` returns — and only arrays and strings get the built-in
    /// sequence iterator.
    fn get_iterator(&mut self, value: &Value) -> JsResult<Iterator> {
        // An existing iterator: `for (x of arr.entries())`.
        if let Value::Object(object) = value {
            if let Some(next) = self.get_property(object, "next")?
                && matches!(&next, Value::Object(function) if object.is_callable() || function.is_callable())
            {
                return Ok(Iterator {
                    object: value.clone(),
                    next,
                });
            }
        }
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
        // A real iterator (`next` is a function) answers with a result object.
        if matches!(&iterator.next, Value::Object(object) if object.is_callable()) {
            let result =
                self.call_value(iterator.next.clone(), iterator.object.clone(), Vec::new())?;
            let Some(result) = result.as_object().cloned() else {
                return Err(self.type_error("iterator result is not an object"));
            };
            let done = self
                .get_property(&result, "done")?
                .is_some_and(|value| value.to_boolean());
            let value = self
                .get_property(&result, "value")?
                .unwrap_or(Value::Undefined);
            return Ok((value, done));
        }
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
        // Iteration yields `undefined` for a hole — `[...[1,,3]]` is
        // `[1, undefined, 3]` with a real element in the middle, because the spread
        // stores whatever the iterator produced.
        Ok((
            match sequence.array_get(index as usize) {
                Some(Value::Hole) | None => Value::Undefined,
                Some(value) => value,
            },
            false,
        ))
    }

    /// Append every element of `iterable` to `array`, returning how many.
    /// `[...iterable]`: append everything the iterator yields, and answer the next
    /// index.
    ///
    /// This goes through the same iterator machinery `for-of` uses rather than copying
    /// an array's elements, because a spread source is any iterable: a string spreads
    /// into its characters and an array iterator spreads into its results. Handling
    /// only arrays made `[..."ab"]` come out empty with no error at all.
    fn append_enumerate(&mut self, array: &Value, iterable: &Value) -> JsResult<i32> {
        let Some(target) = array.as_object().cloned() else {
            return Ok(0);
        };
        let start = target.array_length().unwrap_or(0);
        let mut added = 0;
        let iterator = self.get_iterator(iterable)?;
        loop {
            let (value, done) = self.iterator_step(&iterator)?;
            if done {
                break;
            }
            target.array_set(start + added, value);
            added += 1;
        }
        Ok((start + added) as i32)
    }

    /// How an uncaught exception is reported.
    ///
    /// The reference reports a pending exception through `JS_PrintValue`, which ends
    /// up calling the thrown value's `toString`, so `throw new TypeError("x")` prints
    /// `TypeError: x`. Reading `name`/`message` off the object directly got this wrong
    /// two ways: `name` usually lives on the prototype rather than on the instance, and
    /// a thrown non-error object was not consulted at all, coming out as
    /// `[object Object]`. Routing through `ToString` keeps every case on one path and
    /// honours a user-defined `toString`, as the reference does.
    ///
    /// A `toString` that throws must not take the reporter down with it, so that case
    /// falls back to the type-level rendering. The reference's printer also walks into
    /// plain objects (`throw 1, {a:1}` prints `{ a: 1 }`); that formatting is a gap
    /// shared with `console.log`.
    pub fn describe_exception(&mut self, value: &Value) -> String {
        self.to_js_string(value).unwrap_or_else(|_| display(value))
    }

    /// The current call stack, formatted the way `qjs` prints `error.stack`.
    ///
    /// One `    at name (file:line:col)` line per active frame, innermost first. The
    /// position is the one recorded for the instruction each frame is stopped on, which
    /// is why the compiler builds a pc-to-position table per function: without it a
    /// trace could only name the function, not the line inside it.
    ///
    /// A frame whose code has no positions prints just `(file)`, matching the
    /// reference's output for stripped bytecode.
    pub fn stack_trace(&self) -> String {
        self.stack_trace_skipping(0)
    }

    /// [`Interp::stack_trace`] with the `skip` innermost frames left out.
    fn stack_trace_skipping(&self, skip: usize) -> String {
        let mut out = String::new();
        let mut skip = skip;
        // Natives run on the Rust stack, so they sit innermost. They never re-enter
        // JavaScript, which is what makes it correct to emit them all before the
        // bytecode frames rather than interleaved.
        for name in self.native_frames.iter().rev() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            out.push_str("    at ");
            out.push_str(name);
            out.push_str(" (native)\n");
        }
        for frame in self.frames.iter().rev() {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let code = &frame.code.code;
            let file = code.filename.as_deref().unwrap_or("<input>");
            // The name comes from the function *object*, not from the compiled code:
            // an anonymous function expression is named by the property it is stored
            // in (`{valueOf: function(){}}`), which only the object knows.
            let bound_name = match &frame.callee {
                Value::Object(object) => {
                    object
                        .own_property("name")
                        .and_then(|property| match property.value {
                            Some(Value::String(text)) if !text.as_str().is_empty() => {
                                Some(text.as_str().to_string())
                            }
                            _ => None,
                        })
                }
                _ => None,
            };
            out.push_str("    at ");
            out.push_str(bound_name.as_deref().unwrap_or(&code.name));
            match code.position_at(frame.at as u32) {
                Some((line, column)) => {
                    out.push_str(&format!(" ({file}:{line}:{column})\n"));
                }
                None => out.push_str(&format!(" ({file})\n")),
            }
        }
        out
    }

    /// Give an anonymous function the name of the property it is stored in.
    ///
    /// `{m: function(){}}.m.name` is `"m"`. The name property is not writable, not
    /// enumerable and not configurable, and a function that already has a real name
    /// keeps it.
    ///
    /// "Already named" means a *non-empty* name: every function object is created with
    /// a `name` of `""` so the property exists on all of them, and treating that empty
    /// string as a name left every anonymous function nameless — which is how a stack
    /// trace came out as `at  (file:1:2)` instead of `at valueOf (file:1:2)`.
    fn set_function_name(&mut self, function: &Value, name: &str) {
        let Value::Object(object) = function else {
            return;
        };
        if let Some(Value::String(existing)) = object.own_property("name").and_then(|p| p.value)
            && !existing.as_str().is_empty()
        {
            return;
        }
        let text = self.string(name);
        object.define("name", Property::with_attributes(text, false, false));
    }

    // -- global declarations -----------------------------------------------

    /// Declare a global binding, creating its reference cell.
    /// Declare a `var` or function global, whose home is the global object.
    ///
    /// Writable and enumerable but not configurable: `globalThis.v` reads it,
    /// `for-in` lists it, `delete` refuses. A name that is already there — a built-in
    /// such as `print`, or a redeclaration — is left alone.
    pub fn declare_var_global(&mut self, name: &str) {
        // A strict `eval`'s `var` belongs to the eval's own scope: declaring it on the
        // global object is exactly the leak `typeof my_var === "undefined"` after
        // `eval("var my_var=2")` rules out.
        if let Some(scope) = self.eval_scope.as_mut() {
            scope.entry(name.to_string()).or_insert(Value::Undefined);
            return;
        }
        let global = self.realm.global.clone();
        if global.own_property(name).is_some() {
            return;
        }
        global.define(
            name,
            Property {
                value: Some(Value::Undefined),
                writable: true,
                enumerable: true,
                configurable: false,
                ..Property::default()
            },
        );
    }

    /// Declare a *lexical* global binding, which starts in its dead zone.
    ///
    /// Only `let`/`const`/`class` need this: a `var` global is a property of the
    /// global object and is created when its initialiser runs.
    pub fn declare_global(&mut self, name: &str, is_const: bool) {
        if is_const {
            self.realm.lexical_consts.insert(name.to_string());
        }
        if !self.realm.globals.contains_key(name) {
            // `Uninitialized` rather than `Undefined`: the binding exists but is not
            // readable until its initialiser runs, and that is what `let x` before
            // `x = 1` has to observe.
            let cell = self.new_cell(Value::Uninitialized);
            self.realm.globals.insert(name.to_string(), cell);
            self.realm.declared.push(name.to_string());
        }
    }
}

/// The bytecode and environment of a function object, if it is a bytecode function.
///
/// Extracted before a call so no borrow of the object is held across it.
fn bytecode_of(function: &Gc<JsObject>) -> Option<(Gc<CompiledCode>, Gc<ClosureEnv>)> {
    function.with(|data| match &data.kind {
        ObjectKind::Function(Callable::Bytecode { code, env }) => Some((code.clone(), env.clone())),
        _ => None,
    })
}

/// A native call target, extracted before the call so no borrow is held.
enum NativeCall {
    Native(NativeFn),
    Bytecode(Gc<CompiledCode>, Gc<ClosureEnv>),
    Bound {
        target: Value,
        this: Value,
        leading: Vec<Value>,
    },
}

/// What a store does when the object refuses the write.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum DeleteFailure {
    /// Raise a `TypeError`, which is what strict mode and every built-in does.
    Throw,
    /// Ignore the write, which is what a sloppy-mode assignment does.
    Ignore,
}

/// Whether `object` is a `Date` instance, by the class tag it carries.
///
/// Lives here rather than in `builtins::date` so `ToPrimitive` can ask without the
/// builtins module depending on the interpreter's conversion order.
fn is_date_object(object: &Gc<JsObject>) -> bool {
    object
        .own_property("_class")
        .and_then(|property| property.value)
        .is_some_and(|value| value.to_js_string() == "Date")
}

/// Which type `ToPrimitive` prefers when it converts an object.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    /// The default hint, which behaves like [`Hint::Number`].
    Default,
    /// `Number(obj)` and the arithmetic operators.
    Number,
    /// `String(obj)` and `ToPropertyKey`.
    String,
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

/// The name an accessor function gets: `get a` for a getter, `set a` for a setter,
/// and plain `a` for a method.
///
/// The reference prefixes accessors (`js_method_set_properties`), and the prefix is
/// visible: `{get a(){}}.a.name` is `"get a"`, and a stack trace from inside a getter
/// reads `at get a (file:1:2)`.
fn key_with_accessor_prefix(key: &str, flags: u32) -> String {
    match flags & 3 {
        1 => format!("get {key}"),
        2 => format!("set {key}"),
        _ => key.to_string(),
    }
}

/// Define a method or accessor property, per the `define_method` flag byte.
///
/// The low two bits pick value/getter/setter and bit 2 marks the property
/// enumerable.
///
/// A getter and a setter arrive as two separate `define_method` instructions, and the
/// second one must only fill in its own half: `{get z(){…}, set z(v){…}}` defines
/// `z` twice, and replacing the property wholesale dropped the getter, so `a.z` read
/// `undefined`. The reference passes `JS_PROP_HAS_GET`/`JS_PROP_HAS_SET` to
/// `JS_DefineProperty`, which leaves the other half alone.
fn define_function_property(object: &Gc<JsObject>, key: &str, function: Value, flags: u32) {
    let enumerable = flags & 4 != 0;
    let kind = flags & 3;
    let existing = object.own_property(key);
    let property = match kind {
        1 => Property {
            getter: Some(function),
            // The setter half, if this literal already defined one.
            setter: existing
                .filter(Property::is_accessor)
                .and_then(|p| p.setter),
            enumerable,
            configurable: true,
            ..Property::default()
        },
        2 => Property {
            getter: existing
                .filter(Property::is_accessor)
                .and_then(|p| p.getter),
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

/// How `print` renders a value: the reference's `JS_PrintValue`.
///
/// This is *not* `ToString`. `print({a:1})` shows `{ a: 1 }` and `print([1])` shows
/// `[ 1 ]`, because the host printer dumps the structure rather than converting it.
/// The format is reproduced closely because the test suite compares output text:
///
/// * an empty container has two spaces inside its brackets — `[  ]`, `{  }`;
/// * nesting stops at depth 2 and prints `[Array]` / `[Object]`;
/// * a cycle prints `[circular N]` with `N` the stack index of the object;
/// * an array with an interior hole prints as `idx: value` pairs, while holes at the
///   end print as `<N empty item(s)>`;
/// * a string is quoted inside a container but raw at the top level, which is why
///   `print("x")` and `print(["x"])` differ.
pub fn debug_display(interp: &Interp<'_>, value: &Value) -> String {
    // A string argument is printed raw: `js_print` converts it with `ToString`
    // instead of dumping it, and the quoting only appears inside a container.
    if let Value::String(text) = value {
        return text.as_str().to_string();
    }
    let mut printer = Printer {
        interp,
        out: String::new(),
        stack: Vec::new(),
    };
    printer.value(value, 0);
    printer.out
}

/// The internal class name an object carries, when it has one.
fn class_of(object: &Gc<JsObject>) -> Option<String> {
    object
        .own_property("_class")
        .and_then(|property| property.value)
        .map(|value| value.to_js_string())
}

/// A string property, looked up the way `GetProperty` does: own first, then the
/// prototype chain. Needed because the print formatter holds the interpreter
/// immutably and so cannot run getters.
fn inherited(object: &Gc<JsObject>, name: &str) -> Option<String> {
    let mut current = Some(object.clone());
    // Bounded, because a prototype cycle would otherwise loop forever.
    for _ in 0..100 {
        let node = current?;
        if let Some(property) = node.own_property(name)
            && let Some(value) = property.value
        {
            return Some(value.to_js_string());
        }
        current = node.with(|data| data.proto.clone());
    }
    None
}

/// The `JS_PrintValue` state: output, and the objects currently being printed.
struct Printer<'a, 'rt> {
    /// Kept for the class-name lookups that typed arrays and `Date` will need.
    #[allow(dead_code)]
    interp: &'a Interp<'rt>,
    out: String,
    stack: Vec<Gc<JsObject>>,
}

impl Printer<'_, '_> {
    /// Depth past which a container is summarised by its class name.
    const MAX_DEPTH: usize = 2;

    fn value(&mut self, value: &Value, level: usize) {
        match value {
            Value::Undefined
            | Value::Uninitialized
            | Value::Hole
            | Value::CatchOffset(_)
            | Value::ReturnAddress(_) => self.out.push_str("undefined"),
            Value::Null => self.out.push_str("null"),
            Value::Bool(true) => self.out.push_str("true"),
            Value::Bool(false) => self.out.push_str("false"),
            Value::Int(number) => self.out.push_str(&number.to_string()),
            Value::Float(number) => {
                self.out
                    .push_str(&crate::value::number_to_string_for_print(*number));
            }
            Value::String(text) => self.print_string(text.as_str()),
            Value::Object(object) => self.object(object, level),
        }
    }

    fn object(&mut self, object: &Gc<JsObject>, level: usize) {
        // A cycle is reported rather than followed; the index is where the object
        // already sits in the print stack, which is what the reference shows.
        if let Some(index) = self.stack.iter().position(|seen| seen.ptr_eq(object)) {
            self.out.push_str(&format!("[circular {index}]"));
            return;
        }
        if object.is_callable() {
            let name = object
                .own_property("name")
                .and_then(|property| property.value)
                .map(|value| value.to_js_string())
                .unwrap_or_default();
            if name.is_empty() {
                self.out.push_str("[Function (anonymous)]");
            } else {
                self.out.push_str(&format!("[Function {name}]"));
            }
            return;
        }
        if level >= Self::MAX_DEPTH {
            let class = if object.is_array() { "Array" } else { "Object" };
            self.out.push_str(&format!("[{class}]"));
            return;
        }
        // `Date` and `Error` print as themselves rather than as bags of properties:
        // the ISO string, and `Name: message` followed by the trace.
        if let Some(class) = class_of(object) {
            if class == "Date" {
                self.date(object);
                return;
            }
            if class == "Error" {
                self.error(object);
                return;
            }
        }
        self.stack.push(object.clone());
        if object.is_array() {
            self.array(object, level);
        } else {
            self.plain_object(object, level);
        }
        self.stack.pop();
    }

    /// A Date prints as its ISO string; an invalid one falls through to the plain
    /// object form, the way the reference falls back when `toISOString` throws.
    fn date(&mut self, object: &Gc<JsObject>) {
        let value = object
            .own_property("_date_value")
            .and_then(|property| property.value)
            .map(|value| value.to_number());
        match value.and_then(crate::builtins::date::iso_string) {
            Some(text) => self.out.push_str(&text),
            None => self.plain_object(object, 0),
        }
    }

    /// `Name: message`, then the trace with its trailing newline removed.
    fn error(&mut self, object: &Gc<JsObject>) {
        let name = inherited(object, "name").unwrap_or_else(|| "Error".to_string());
        self.out.push_str(&name);
        if let Some(message) = inherited(object, "message")
            && !message.is_empty()
        {
            self.out.push_str(": ");
            self.out.push_str(&message);
        }
        if let Some(stack) = inherited(object, "stack") {
            self.out.push('\n');
            self.out.push_str(stack.trim_end_matches('\n'));
        }
    }

    fn array(&mut self, object: &Gc<JsObject>, level: usize) {
        let elements = object.array_elements().unwrap_or_default();
        self.out.push_str("[ ");
        // An interior hole means the array is "sparse": the reference prints those
        // as `index: value` pairs and says nothing about the holes themselves, while
        // a hole run at the end becomes a single `<N empty items>` note.
        let last_present = elements
            .iter()
            .rposition(|value| !matches!(value, Value::Hole));
        let has_interior_hole = match last_present {
            Some(last) => elements[..last]
                .iter()
                .any(|value| matches!(value, Value::Hole)),
            None => false,
        };
        let mut first = true;
        if has_interior_hole {
            let mut index = 0;
            while index < elements.len() {
                let value = &elements[index];
                if matches!(value, Value::Hole) {
                    index += 1;
                    continue;
                }
                if !first {
                    self.out.push_str(", ");
                }
                first = false;
                self.out.push_str(&format!("{index}: "));
                self.value(value, level + 1);
                index += 1;
            }
        } else {
            for (index, value) in elements.iter().enumerate() {
                if matches!(value, Value::Hole) {
                    continue;
                }
                if !first {
                    self.out.push_str(", ");
                }
                first = false;
                let _ = index;
                self.value(value, level + 1);
            }
            if let Some(last) = last_present {
                let trailing = elements.len() - last - 1;
                if trailing > 0 {
                    if !first {
                        self.out.push_str(", ");
                    }
                    self.out.push_str(&format!(
                        "<{trailing} empty item{}>",
                        if trailing > 1 { "s" } else { "" }
                    ));
                }
            } else if !elements.is_empty() {
                let count = elements.len();
                self.out.push_str(&format!(
                    "<{count} empty item{}>",
                    if count > 1 { "s" } else { "" }
                ));
            }
        }
        self.out.push_str(" ]");
    }

    fn plain_object(&mut self, object: &Gc<JsObject>, level: usize) {
        self.out.push_str("{ ");
        let mut first = true;
        for key in object.own_keys() {
            let Some(property) = object.own_property(&key) else {
                continue;
            };
            if !property.enumerable {
                continue;
            }
            if !first {
                self.out.push_str(", ");
            }
            first = false;
            self.out
                .push_str(&js_compile::disasm::print_atom(&Atom::from_text(&key)));
            self.out.push_str(": ");
            // An accessor has no value to dump; the reference names the half that
            // exists instead of calling it.
            if property.is_accessor() {
                let text = match (property.getter.is_some(), property.setter.is_some()) {
                    (true, true) => "[Getter/Setter]",
                    (false, true) => "[Setter]",
                    _ => "[Getter]",
                };
                self.out.push_str(text);
            } else {
                self.value(&property.value.unwrap_or(Value::Undefined), level + 1);
            }
        }
        self.out.push_str(" }");
    }

    /// A string inside a container, quoted with the escapes `js_print_string1` uses.
    fn print_string(&mut self, text: &str) {
        self.out.push('"');
        for character in text.chars() {
            match character {
                '"' => self.out.push_str("\\\""),
                '\\' => self.out.push_str("\\\\"),
                '\n' => self.out.push_str("\\n"),
                '\r' => self.out.push_str("\\r"),
                '\t' => self.out.push_str("\\t"),
                c if (c as u32) < 0x20 => {
                    self.out.push_str(&format!("\\u{:04x}", c as u32));
                }
                c => self.out.push(c),
            }
        }
        self.out.push('"');
    }
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
        // `>>>` is the *unsigned* shift, so its result is a uint32 and can exceed
        // `i32::MAX`: `(1<<31) >>> 0` is `2147483648`, not `-2147483648`. A value that
        // fits the small-integer representation stays an `Int`; the rest become a
        // float, which is how the reference keeps `n >>> 0` non-negative.
        Shr => {
            let shifted = left.to_uint32() >> (right.to_uint32() & 31);
            match i32::try_from(shifted) {
                Ok(value) => Value::Int(value),
                Err(_) => Value::Float(shifted as f64),
            }
        }
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

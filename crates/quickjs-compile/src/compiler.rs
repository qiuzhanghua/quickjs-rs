//! The bytecode compiler.
//!
//! Compilation is a single walk over the AST that emits *symbolic* instructions
//! (jump targets are label ids, variable references are names), followed by a
//! resolution pass that turns them into final bytecode. Splitting it this way
//! keeps the emitter readable: nothing has to know slot indices or byte offsets
//! while it is still discovering them.
//!
//! Variable resolution follows the reference implementation's model:
//!
//! * every function has a flat table of arguments and locals;
//! * block scopes form a chain through [`VarDef::scope_next`], which is what makes
//!   `let`/`const` bindings visible only inside their block and lets the runtime
//!   put them back into the temporal dead zone on entry;
//! * a name that is not in the current function is looked up in the enclosing
//!   ones; if found, the variable is marked captured and reached through a
//!   closure reference;
//! * a name found nowhere is a global, reached through a closure entry as well.
//!
//! Emission order and instruction choice deliberately mirror `quickjs.c`: the
//! differential test compares disassembly with the reference opcode by opcode, so
//! an unnecessary `dup` or a differently chosen short form shows up immediately.

use crate::bytecode::LineCol;
use crate::bytecode::{
    Atom, ClosureType, ClosureVar, Constant, Function as CompiledFunction, FunctionKind, VarDef,
};
use crate::disasm;
use crate::opcode::Opcode;
use js_parse::Position;
use js_parse::Span;
use js_parse::SyntaxError;
use js_parse::ast::{self, *};

/// A label id, local to the function being compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Label(u32);

/// The `var_block` sentinel for a binding that lives at function scope.
///
/// It has to be a value a *real* block key can never take. Block keys are source
/// offsets, so `0` is a perfectly ordinary key — the first block statement of a file
/// gets it — and using `0` as the sentinel made `{ let y = 2 }` at offset zero look
/// like a function-scoped `var`, which is how a top-level block's `let` leaked out of
/// its block. The body block already uses `u64::MAX` to stay clear of offsets, so the
/// sentinel takes the slot below it.
const FUNCTION_SCOPE: u64 = u64::MAX - 3;

/// The `blocks` parent sentinel for "this block has no enclosing block".
///
/// Same reasoning as [`FUNCTION_SCOPE`]: `0` is a legal block key, and when it was
/// also the terminator the chain walk from the script body reached the file's
/// offset-zero block and treated it as an *enclosing* block — so that block's `let`
/// leaked into the body scope.
const NO_PARENT: u64 = u64::MAX - 2;

/// The block key for a script body. Distinct from both sentinels and unreachable from
/// any source offset.
const SCRIPT_BODY: u64 = u64::MAX - 1;

/// An operand of a symbolic instruction.
#[derive(Debug, Clone, PartialEq)]
enum Op {
    /// An immediate integer.
    Int(i32),
    /// A jump target.
    Label(Label),
    /// A constant pool index.
    Const(u32),
    /// An atom, before interning.
    Atom(Atom),
    /// An interned atom, as an index into [`Function::atoms`].
    AtomId(u32),
    /// A resolved slot.
    Slot(Slot),
}

/// Where a resolved variable lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    Arg(u32),
    Loc(u32),
    VarRef(u32),
    /// A global, by closure-var index.
    Global(u32),
    /// A global *declared* in this script. The reference keeps these bindings in
    /// reference cells so that the script and any closure share one variable, so
    /// a store goes through `put_var_ref` while a read uses `get_var`.
    GlobalDecl(u32),
}

/// What a symbolic variable access does, which decides the final opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VarAction {
    /// Read the value.
    Get,
    /// Read the value, yielding `undefined` for an undeclared global.
    GetUndef,
    /// Write, leaving nothing on the stack.
    Put,
    /// Write and initialise, leaving the value on the stack.
    PutInit,
    /// Write, leaving the value on the stack.
    Set,
    /// Write a binding that lives in a reference cell.
    PutRef,
}

#[derive(Debug, Clone)]
enum Insn {
    /// A fully determined instruction.
    Fixed { op: Opcode, operands: Vec<Op> },
    /// A variable access, resolved in the second pass.
    ///
    /// `block` is the lexical block the access appears in, which is what makes
    /// shadowing work: two bindings can share a name, and the one that wins is
    /// whichever scope encloses the access.
    Var {
        action: VarAction,
        name: String,
        block: u64,
    },
    /// Close a binding *if* it turned out to be captured.
    ///
    /// Whether a `for (let i ...)` loop variable needs a fresh per-iteration cell is
    /// only known once the body has been compiled — a closure created there is the
    /// thing that makes it necessary. The reference emits `close_loc` exactly in that
    /// case and nothing otherwise, so the decision is deferred to [`resolve`] instead
    /// of emitted blind.
    CloseCaptured { name: String, block: u64 },
    /// `delete name`, which can only answer `false` once the binding is known.
    ///
    /// It is not a `Var` action because `delete_var` carries an *atom*: the runtime
    /// looks the name up on the global object, which is what distinguishes an implicit
    /// global (configurable, deletable) from a declared one (a binding, always `false`).
    DeleteVar { name: String },
    /// An instruction that dead-code elimination removed. It keeps its slot so
    /// that label indices stay valid, and resolves to no bytecode.
    Dead,
}

/// Where `break` and `continue` jump to.
///
/// Targets are stacked as the compiler enters loops and labelled statements, so a
/// bare `break` needs no analysis: it is simply the innermost target. A labelled
/// one searches outward by name.
#[derive(Debug, Clone)]
struct JumpTarget {
    /// The labels attached to this target. `l: for(;;)` gives one name; a bare
    /// loop gives none; `a: b: for(;;)` gives two, because both labels name the
    /// same statement and `break a` must reach the same place as `break b`.
    names: Vec<String>,
    /// Where `break` lands. `None` for a construct `break` may only leave, not
    /// target — a `try`, whose entry exists purely to unwind.
    break_label: Option<Label>,
    /// Where `continue` lands. `None` for anything that is not a loop.
    continue_label: Option<Label>,
    /// Stack slots to discard when control leaves this construct by a jump.
    ///
    /// Both `switch` and `try` leave something on the value stack for the duration
    /// of the construct — the discriminant, or the `catch` offset — and a jump out
    /// has to take it with it. A loop leaves nothing.
    drop_count: u32,
    /// The finalizer to run when control leaves this construct.
    ///
    /// Set on a `try` even when it has no `finally`: the label is always emitted and
    /// the empty-subroutine removal cleans up afterwards, so the code that unwinds
    /// does not have to know whether a finalizer exists.
    finally_label: Option<Label>,
    /// Whether this is a labelled non-loop statement, which an unlabelled `break`
    /// does not reach.
    is_regular_stmt: bool,
    /// Whether leaving this construct has to close an iterator.
    ///
    /// A `for`/`of` loop keeps an iterator on the value stack, and leaving early has
    /// to run its `return` method. `for`/`in` keeps only an enumeration object, which
    /// is dropped rather than closed.
    has_iterator: bool,
}

/// `copy_data_properties` flags for an object-literal spread.
///
/// The byte packs three fields: the low bits are the stack offset of the source,
/// the next three the offset of the exclusion list, and bit 2 asks for symbol keys
/// as well as string keys. A spread passes a literal `null` exclusion list right
/// below the source, so that field stays zero and only the source offset and the
/// symbol bit are set — hence `{...v}` copies symbols, which the flag name says.
const COPY_ALL_PROPERTIES: i32 = 2 | (1 << 2);

/// The expression a `...` introduces.
///
/// The parser marks the spread in two places — the element/property variant and an
/// [`ExprKind::Spread`] wrapper around the argument — so the wrapper has to be
/// stepped through before the argument can be compiled.
fn spread_argument(spread: &Expr) -> &Expr {
    match &spread.kind {
        ExprKind::Spread(inner) => inner,
        _ => spread,
    }
}

/// The flag byte `define_method` takes.
///
/// The low two bits pick value/getter/setter; bit 2 marks the property
/// enumerable, which every object-literal method is.
fn define_method_flags(kind: PropertyKind) -> i32 {
    let base = match kind {
        PropertyKind::Value | PropertyKind::Method => 0,
        PropertyKind::Get => 1,
        PropertyKind::Set => 2,
    };
    base | 4
}

/// Convert a numeric property key to the text an atom would be made from.
///
/// Only the range where Rust and JavaScript agree on the spelling is accepted.
/// Both print the shortest representation that round-trips, so plain decimal
/// digits match; they diverge at the extremes, where JavaScript switches to an
/// exponent (`1e-7`, `1e+21`) and Rust does not. Rather than emit an atom that
/// differs from the reference, those keys are rejected.
fn numeric_key_text(value: f64, span: Span) -> Result<String, SyntaxError> {
    if !value.is_finite() {
        return Err(SyntaxError::new(
            "a non-finite numeric property key is not supported by the compiler yet",
            span,
        ));
    }
    if value == value.trunc() && value.abs() < 1e21 {
        return Ok(format!("{}", value as i64));
    }
    if value.abs() >= 1e-6 && value.abs() < 1e21 {
        return Ok(format!("{value}"));
    }
    Err(SyntaxError::new(
        "a numeric property key outside [1e-6, 1e21) is not supported by the compiler yet",
        span,
    ))
}

/// Collect the operands of a same-operator logical run, in source order.
///
/// Recurses down the left spine only: `((a || b) || c)` contributes `a`, `b`, then
/// the caller appends `c`. A node with a different operator is one operand.
fn collect_same_operator<'a>(op: LogicalOp, expr: &'a Expr, out: &mut Vec<&'a Expr>) {
    if let ExprKind::Logical {
        op: inner_op,
        left,
        right,
    } = &expr.kind
        && *inner_op == op
    {
        collect_same_operator(op, left, out);
        out.push(right);
        return;
    }
    out.push(expr);
}

/// The jump a logical assignment or `??` takes when the value is kept.
///
/// `||` and `&&` branch on truthiness, so the same opcode serves both the operator
/// and `??` once [`Compiler::emit_logical_test`] has pushed a boolean. `??` keeps
/// the value when it is *not* null-ish, so it is the `if_false` side.
fn logical_jump(op: AssignOp) -> Opcode {
    match op {
        AssignOp::Or => Opcode::IfTrue,
        AssignOp::And | AssignOp::Coalesce => Opcode::IfFalse,
        _ => Opcode::IfFalse,
    }
}

/// Whether a loop enumerates keys or values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForLoopKind {
    /// `for (x in o)` — the keys of an object.
    In,
    /// `for (x of o)` — the values of an iterable.
    Of,
}

/// Which part of a `try` statement a block key refers to.
#[derive(Debug, Clone, Copy)]
enum TryPart {
    /// The guarded block.
    Block = 0,
    /// The handler body, excluding the catch parameter.
    Catch = 1,
    /// The finalizer block.
    Finally = 2,
    /// The catch parameter's own scope.
    Param = 3,
}

/// A lexical scope, identified while hoisting and numbered at the end.
///
/// Scope identity is needed in two passes that must agree: declarations are
/// hoisted in source order (so slots come out in source order, as in the
/// reference), but a binding belongs to the block it was written in, which is only
/// entered later. Blocks are therefore keyed by the source position of their
/// opening brace, which is stable across both walks.
#[derive(Debug, Clone, Copy)]
struct Block {
    /// Source position of the block.
    key: u64,
    /// The block this one is nested in; `0` for the function scope.
    parent: u64,
}

/// One function under construction.
struct FnState {
    name: String,
    kind: FunctionKind,
    strict: bool,
    args: Vec<VarDef>,
    vars: Vec<VarDef>,
    closure_vars: Vec<ClosureVar>,
    cpool: Vec<Constant>,
    atoms: Vec<Atom>,
    /// Every lexical scope, in creation order.
    blocks: Vec<Block>,
    /// The block currently being parsed or compiled.
    current_block: u64,
    /// The block each local was declared in, parallel to `vars`. `0` means the
    /// function scope, where `var` bindings live.
    var_block: Vec<u64>,
    /// Set when the expression just compiled left no value on the stack.
    ///
    /// Only a store can do that: `put_field` consumes the object and the value, so
    /// an assignment whose result is unused emits nothing extra. The expression
    /// statement then has nothing to drop.
    no_value: bool,
    /// Whether a label was placed since the last instruction was emitted.
    ///
    /// The `put`/`get` fold is only valid when the two are adjacent in the final
    /// stream; a label in between means a jump can land on the read, so the store has
    /// to keep consuming its value. A flag is enough and avoids having to reason about
    /// which index a label recorded.
    label_since_insn: bool,
    /// Whether the value of the expression being compiled is thrown away.
    ///
    /// Only an expression *statement* discards its value, and only the outermost
    /// assignment in it may act on that: in `x = (o.a = 1)` the inner assignment
    /// still has to produce a value. Consumed by [`Compiler::assign`], which is
    /// the one construct whose bytecode depends on it.
    discard_value: bool,
    /// The block that holds the function body.
    body_block: u64,
    /// Head of each scope's binding chain, indexed by level. Filled in by
    /// [`rebuild_scope_chain`]; the scope chain is only needed at run time.
    block_heads: Vec<i32>,
    /// Enclosing `break`/`continue` targets, innermost last.
    jump_targets: Vec<JumpTarget>,
    insns: Vec<Insn>,
    labels: Vec<Option<usize>>,
    /// Captured locals, which need a reference cell.
    var_ref_count: u32,
    line: u32,
    column: u32,
    /// Whether this is a script body. A script's top-level declarations create
    /// global properties rather than locals, which is why they show up as
    /// closure variables with `[global_decl]`.
    is_script: bool,
    /// Whether the function is an arrow, which never gets its own `arguments`.
    is_arrow: bool,
    /// The name a *named function expression* binds inside its own body.
    self_name: Option<String>,
    /// Whether the code emitted so far leaves the program's result on the stack.
    /// The reference tracks this for the hidden `<ret>` local of an eval script.
    leaves_value: bool,
    /// Statement nesting depth, used to tell top-level statements apart.
    stmt_depth: u32,
    /// Function declarations waiting to be instantiated at scope entry, as
    /// (target slot, constant pool index). The reference emits these from
    /// `instantiate_hoisted_definitions` rather than at the statement, because a
    /// declaration is visible before it is reached.
    hoisted: Vec<(u64, HoistTarget, u32)>,
    /// The source position of the node being compiled. Every instruction emitted
    /// takes it, which is how the position table that feeds stack traces is built.
    cur_pos: Option<Position>,
    /// One position per entry in `insns`, index-aligned.
    insn_pos: Vec<Option<Position>>,
}

/// Where a hoisted function declaration is stored.
#[derive(Debug, Clone)]
enum HoistTarget {
    Arg(u32),
    Loc(u32),
    /// A global, which the resolver looks up by name.
    Global(String),
}

impl FnState {
    fn new(name: impl Into<String>, kind: FunctionKind) -> FnState {
        FnState {
            name: name.into(),
            kind,
            strict: false,
            args: Vec::new(),
            vars: Vec::new(),
            closure_vars: Vec::new(),
            cpool: Vec::new(),
            atoms: Vec::new(),
            blocks: Vec::new(),
            current_block: 0,
            var_block: Vec::new(),
            no_value: false,
            label_since_insn: false,
            discard_value: false,
            body_block: 0,
            block_heads: Vec::new(),
            jump_targets: Vec::new(),
            insns: Vec::new(),
            labels: Vec::new(),
            var_ref_count: 0,
            line: 1,
            column: 1,
            is_script: false,
            is_arrow: false,
            self_name: None,
            leaves_value: false,
            stmt_depth: 0,
            hoisted: Vec::new(),
            cur_pos: None,
            insn_pos: Vec::new(),
        }
    }

    /// The scope a declaration lands in: a script's top-level declarations are
    /// globals, everything else gets a slot.
    /// Whether a binding declared *now* becomes a property of the global object.
    ///
    /// For a script, a top-level `var` and the top-level `let`/`const` are globals.
    /// The `var` case is decided by the keyword rather than by the cursor position:
    /// `var` is function-scoped, so a `var i` in a `for` head is still the same
    /// binding as a `var i` in the body, even though the head has its own block.
    fn declares_globally(&self, def: &VarDef) -> bool {
        self.is_script && (!def.is_lexical || self.current_block == self.body_block)
    }
}

/// The compiler's entry point.
pub struct Compiler {
    module: bool,
    /// Enclosing functions, outermost first. The last entry is the current one.
    stack: Vec<FnState>,
}

/// Compile a script.
pub fn compile_script(program: &Program) -> Result<CompiledFunction, SyntaxError> {
    Compiler::new(false).compile(program)
}

/// Compile a module.
pub fn compile_module(program: &Program) -> Result<CompiledFunction, SyntaxError> {
    Compiler::new(true).compile(program)
}

impl Compiler {
    /// Create a compiler.
    pub fn new(module: bool) -> Compiler {
        Compiler {
            module,
            stack: Vec::new(),
        }
    }

    /// Compile a whole program into its top-level function.
    pub fn compile(mut self, program: &Program) -> Result<CompiledFunction, SyntaxError> {
        let mut state = FnState::new("<eval>", FunctionKind::Normal);
        state.is_script = true;
        state.strict = program.strict || self.module;
        state.line = program.span.start.line;
        state.column = program.span.start.column;
        // Locals are numbered in declaration order; `<ret>` is always slot 0.
        //
        // `var_block` has to grow in step with `vars`: it is indexed by slot, so a
        // slot that only appears in one of the two arrays shifts every later
        // binding's block and silently loses the temporal-dead-zone marking for
        // the script's own blocks. A function frame has no `<ret>`, which is why
        // the misalignment only showed up at the top level.
        state.vars.push(VarDef::var("<ret>"));
        state.var_block.push(FUNCTION_SCOPE);
        self.stack.push(state);
        // The body is its own scope, keyed by the position of the program.
        // The script body needs a key no statement can produce. Statement keys are
        // source offsets, and the program's own start offset is `0` — which is also
        // the offset of the first statement, so a top-level `{ let x }` or
        // `for (let i …)` used to share the body's block and lose its scope.
        let body_key = SCRIPT_BODY;
        self.push_block(body_key, NO_PARENT);
        self.state().body_block = body_key;
        self.hoist(&program.body, body_key)?;
        self.emit_scope_enter(body_key);
        for stmt in &program.body {
            self.stmt(stmt)?;
            if Self::terminates(stmt) {
                break;
            }
        }
        // The program's value is whatever the last expression statement stored in
        // the hidden `<ret>` local; `put_loc0`/`get_loc0` folding turns the common
        // case into a single `set_loc0`.
        self.emit(Opcode::GetLoc, &[Op::Slot(Slot::Loc(0))]);
        self.emit(Opcode::Return, &[]);
        Ok(self.finish_current())
    }

    // -- emitting ----------------------------------------------------------

    fn state(&mut self) -> &mut FnState {
        self.stack.last_mut().expect("a function is being compiled")
    }

    fn cur(&self) -> &FnState {
        self.stack.last().expect("a function is being compiled")
    }

    fn emit(&mut self, op: Opcode, operands: &[Op]) {
        self.state().label_since_insn = false;
        let pos = self.cur().cur_pos;
        self.state().insns.push(Insn::Fixed {
            op,
            operands: operands.to_vec(),
        });
        self.state().insn_pos.push(pos);
    }

    /// Reset the script's hidden return value to `undefined`.
    ///
    /// Called at the start of every statement that does not itself produce the
    /// program's value, which is how the reference keeps `if`/loop results from
    /// leaking into an eval result.
    fn reset_value(&mut self) {
        if !self.cur().is_script {
            return;
        }
        self.emit(Opcode::Undefined, &[]);
        self.emit(Opcode::PutLoc, &[Op::Slot(Slot::Loc(0))]);
        self.state().leaves_value = false;
    }

    /// Whether `name` is a local or argument slot rather than a global.
    fn is_slot(&self, name: &str) -> bool {
        self.cur().vars.iter().any(|v| v.name == name)
            || self.cur().args.iter().any(|a| a.name == name)
    }

    fn emit_var(&mut self, action: VarAction, name: &str) {
        // Peephole: `put_x v` followed by `get_x v` becomes `set_x v`, which
        // stores and keeps the value in one instruction. The reference does this
        // in a post-pass; folding it at emit time keeps instruction indices
        // stable, which matters because labels are recorded as indices.
        //
        // Only slots have a `set_` form: a global is stored with `put_var` and
        // read with `get_var`, and the reference does not fold those.
        //
        // A label between the two cancels the fold. `set_x v` leaves the value on
        // the stack, which is only equivalent when the read immediately follows the
        // store; when a jump target sits in between, the store must *consume* the
        // value and the read must fetch it afresh. A loop head is exactly that
        // case — `for (var i = 0; i < 3; i++)` stores the initialiser, then the
        // condition re-reads it — and folding across the label would make the
        // condition compare whatever the previous iteration left on the stack.
        if action == VarAction::Get {
            let is_slot = self.is_slot(name);
            let at_label = std::mem::take(&mut self.state().label_since_insn);
            //
            // `arguments` is excluded because the reference treats it as a special
            // binding: it is initialised once and read many times, and folding its
            // initialisation into a `set_` would leave the object on the stack where
            // the reference leaves nothing.
            if is_slot
                && name != "arguments"
                && !at_label
                && let Some(Insn::Var {
                    action: previous,
                    name: previous_name,
                    ..
                }) = self.state().insns.last_mut()
                && *previous == VarAction::Put
                && previous_name == name
            {
                *previous = VarAction::Set;
                return;
            }
        }
        let block = self.cur().current_block;
        self.state().label_since_insn = false;
        let pos = self.cur().cur_pos;
        self.state().insns.push(Insn::Var {
            action,
            name: name.to_string(),
            block,
        });
        self.state().insn_pos.push(pos);
    }

    fn new_label(&mut self) -> Label {
        let state = self.state();
        state.labels.push(None);
        Label(state.labels.len() as u32 - 1)
    }

    fn place_label(&mut self, label: Label) {
        let index = self.cur().insns.len();
        self.state().label_since_insn = true;
        self.state().labels[label.0 as usize] = Some(index);
    }

    fn emit_jump(&mut self, op: Opcode, label: Label, extra: &[Op]) {
        let mut operands = vec![Op::Label(label)];
        operands.extend_from_slice(extra);
        self.emit(op, &operands);
    }

    // -- scopes ------------------------------------------------------------

    /// Start a new lexical scope inside `parent`.
    fn push_block(&mut self, key: u64, parent: u64) {
        let state = self.state();
        if !state.blocks.iter().any(|b| b.key == key) {
            state.blocks.push(Block { key, parent });
        }
        state.current_block = key;
    }

    /// Emit the code that runs when a lexical scope is entered.
    ///
    /// Every `let`/`const`/`class` binding starts out in the temporal dead zone,
    /// so entering its block marks it uninitialised. Hoisted function
    /// declarations are created here instead, which is why a call before the
    /// declaration still works.
    ///
    /// The order is not obvious: bindings are emitted in *reverse* declaration
    /// order, because the reference walks its scope chain, which is built by
    /// prepending.
    fn emit_scope_enter(&mut self, block: u64) {
        let hoisted = std::mem::take(&mut self.state().hoisted);
        let mut mine: Vec<(HoistTarget, u32)> = hoisted
            .into_iter()
            .filter(|(owner, _, _)| *owner == block)
            .map(|(_, target, pool_index)| (target, pool_index))
            .collect();
        mine.sort_by_key(|(target, _)| match target {
            HoistTarget::Arg(index) | HoistTarget::Loc(index) => *index,
            HoistTarget::Global(_) => u32::MAX,
        });
        for (target, pool_index) in mine {
            self.emit(Opcode::Fclosure, &[Op::Const(pool_index)]);
            match target {
                HoistTarget::Arg(index) => self.emit(Opcode::PutArg, &[Op::Slot(Slot::Arg(index))]),
                HoistTarget::Loc(index) => self.emit(Opcode::PutLoc, &[Op::Slot(Slot::Loc(index))]),
                HoistTarget::Global(name) => {
                    // A global binding lives in a reference cell, so the store
                    // goes through `put_var_ref` while reads use `get_var`.
                    let block = self.cur().current_block;
                    let pos = self.cur().cur_pos;
                    self.state().insns.push(Insn::Var {
                        action: VarAction::PutRef,
                        name,
                        block,
                    });
                    self.state().insn_pos.push(pos);
                }
            }
        }
        for index in self.block_vars_reversed(block) {
            self.emit(
                Opcode::SetLocUninitialized,
                &[Op::Slot(Slot::Loc(index as u32))],
            );
        }
    }

    /// Emit the code that runs when a lexical scope is exited.
    ///
    /// A captured binding has to be closed: detaching its reference cell freezes
    /// the value the binding had when the block ended, which is what a closure
    /// created inside the block must keep seeing.
    fn emit_scope_leave(&mut self, block: u64) {
        for index in self.block_vars_reversed(block) {
            if self.cur().vars[index].is_captured {
                self.emit(Opcode::CloseLoc, &[Op::Slot(Slot::Loc(index as u32))]);
            }
        }
    }

    /// Locals declared in `block`, in reverse declaration order.
    fn block_vars_reversed(&self, block: u64) -> Vec<usize> {
        let state = self.cur();
        let mut out: Vec<usize> = (0..state.vars.len())
            .filter(|index| {
                state.var_block.get(*index).copied() == Some(block) && state.vars[*index].is_lexical
            })
            .collect();
        out.reverse();
        out
    }

    /// Declare a local, even at the top level of a script.
    ///
    /// A `catch` parameter is always a local: the reference puts it in the
    /// function's own variable list (`1: catch e`), never in the global object,
    /// because it only exists while the handler runs.
    fn declare_local(&mut self, _name: &str, def: VarDef) -> u32 {
        let state = self.state();
        let index = state.vars.len() as u32;
        // A `catch` parameter belongs to a scope even though it is not lexical: the
        // runtime instantiates it per handler entry, but it never sits in the
        // temporal dead zone.
        let block = if def.is_lexical || def.kind == crate::bytecode::VarKind::Catch {
            state.current_block
        } else {
            0
        };
        state.var_block.push(block);
        state.vars.push(def);
        index
    }

    fn declare(&mut self, name: &str, def: VarDef) -> u32 {
        if self.cur().declares_globally(&def) {
            self.declare_global(name, &def);
            return 0;
        }
        let state = self.state();
        let index = state.vars.len() as u32;
        let block = if def.is_lexical {
            state.current_block
        } else {
            0
        };
        state.var_block.push(block);
        state.vars.push(def);
        index
    }

    // -- statements --------------------------------------------------------

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        self.stmt_labelled(stmt, &[])
    }

    /// Compile a statement that a chain of labels names.
    ///
    /// Only a loop consumes `labels`; every other statement ignores them (the
    /// parser has already rejected a label on a statement that cannot use it).
    fn stmt_labelled(&mut self, stmt: &Stmt, labels: &[String]) -> Result<(), SyntaxError> {
        // Only a *top-level* expression statement supplies the program's value.
        let saved = self.cur().leaves_value;
        self.state().leaves_value =
            matches!(stmt.kind, StmtKind::Expr(_)) && self.cur().stmt_depth == 0;
        self.state().stmt_depth += 1;
        let result = self.stmt_inner(stmt, labels);
        self.state().stmt_depth -= 1;
        // A nested statement never contributes the program's value, so the flag
        // the wrapper set above is what survives.
        let produced = self.cur().leaves_value && self.cur().stmt_depth == 0;
        self.state().leaves_value = produced;
        let _ = saved;
        result
    }

    fn stmt_inner(&mut self, stmt: &Stmt, labels: &[String]) -> Result<(), SyntaxError> {
        match &stmt.kind {
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::Expr(expr) => {
                // An expression statement's value is thrown away — except in a
                // script, where it becomes the program's result. `assign` reads this
                // and clears it, so a nested assignment still produces a value.
                self.state().discard_value = !self.cur().is_script;
                let result = self.expr(expr);
                self.state().discard_value = false;
                result?;
                if std::mem::take(&mut self.state().no_value) {
                    // A store whose value was discarded left nothing to drop.
                } else if self.cur().is_script {
                    // The program's value lands in the hidden `<ret>` local.
                    self.emit(Opcode::PutLoc, &[Op::Slot(Slot::Loc(0))]);
                } else {
                    self.emit(Opcode::Drop, &[]);
                }
            }
            StmtKind::Block(body) => {
                // The block's declarations were hoisted with its enclosing
                // statement list, so there is nothing left to declare here.
                let key = stmt.span.start.offset as u64;
                let parent = self.cur().current_block;
                self.push_block(key, parent);
                self.emit_scope_enter(key);
                for stmt in body {
                    self.stmt(stmt)?;
                    if Self::terminates(stmt) {
                        break;
                    }
                }
                self.emit_scope_leave(key);
                self.state().current_block = parent;
            }
            StmtKind::VarDecl(decl) => self.var_decl(decl)?,
            // A function declaration was already instantiated when its scope was
            // entered, so the statement itself emits nothing.
            StmtKind::FuncDecl(_) => {}
            StmtKind::If { test, cons, alt } => {
                self.reset_value();
                let else_label = self.new_label();
                let end_label = self.new_label();
                self.expr(test)?;
                self.emit_jump(Opcode::IfFalse, else_label, &[]);
                self.stmt(cons)?;
                if alt.is_some() {
                    self.emit_jump(Opcode::Goto, end_label, &[]);
                }
                self.place_label(else_label);
                if let Some(alt) = alt {
                    self.stmt(alt)?;
                    self.place_label(end_label);
                }
            }
            StmtKind::While { test, body } => {
                self.reset_value();
                let top = self.new_label();
                let end = self.new_label();
                self.place_label(top);
                self.expr(test)?;
                // The condition's false branch and `break` share one target,
                // which is why a `while` loop needs only one end label.
                self.emit_jump(Opcode::IfFalse, end, &[]);
                self.loop_body(body, labels, top, end)?;
                self.emit_jump(Opcode::Goto, top, &[]);
                self.place_label(end);
            }
            StmtKind::DoWhile { body, test } => {
                self.reset_value();
                let top = self.new_label();
                let cont = self.new_label();
                let end = self.new_label();
                self.place_label(top);
                // `continue` in a `do…while` runs the test, so it lands on `cont`
                // rather than on the loop head.
                self.loop_body(body, labels, cont, end)?;
                self.place_label(cont);
                self.expr(test)?;
                self.emit_jump(Opcode::IfTrue, top, &[]);
                self.place_label(end);
            }
            StmtKind::For {
                init,
                test,
                update,
                body,
            } => {
                self.reset_value();
                let for_key = stmt.span.start.offset as u64;
                let for_parent = self.cur().current_block;
                self.push_block(for_key, for_parent);
                self.emit_scope_enter(for_key);
                if let Some(init) = init {
                    match init {
                        ForInit::VarDecl(decl) => {
                            self.var_decl(decl)?;
                        }
                        ForInit::Expr(expr) => {
                            // The head's value is discarded. Saying so lets an
                            // assignment store with `put` instead of `dup`+`put`, which
                            // is what the reference emits — and the difference matters:
                            // a `dup`/`drop` pair around the store gives the peephole a
                            // chance to fold the store into the *condition's* read.
                            self.state().discard_value = true;
                            let result = self.expr(expr);
                            self.state().discard_value = false;
                            result?;
                            if !std::mem::take(&mut self.state().no_value) {
                                self.emit(Opcode::Drop, &[]);
                            }
                        }
                    }
                }
                // A `let`/`const` in the head gets a *fresh binding per iteration*,
                // so a closure made in the body captures that iteration's value
                // instead of sharing one. The reference expresses this with
                // `close_loc`, which swaps in a new reference cell holding the
                // current value; the three points below are the ones it uses — after
                // the initialiser, before the update, and after the loop.
                let loop_bindings = match init {
                    Some(ForInit::VarDecl(decl)) if decl.kind != VarDeclKind::Var => {
                        let mut names: Vec<String> = Vec::new();
                        for declarator in &decl.declarators {
                            let mut idents = Vec::new();
                            declarator.target.bound_names(&mut idents);
                            names.extend(idents.into_iter().map(|ident| ident.name));
                        }
                        names
                    }
                    _ => Vec::new(),
                };
                // The reference also closes right after the initialiser, so the first
                // iteration's closures get their own cell rather than sharing the one
                // the initialiser wrote into.
                for name in &loop_bindings {
                    let pos = self.cur().cur_pos;
                    self.state().insns.push(Insn::CloseCaptured {
                        name: name.clone(),
                        block: for_key,
                    });
                    self.state().insn_pos.push(pos);
                }
                let top = self.new_label();
                let cont = self.new_label();
                let end = self.new_label();
                self.place_label(top);
                if let Some(test) = test {
                    self.expr(test)?;
                    self.emit_jump(Opcode::IfFalse, end, &[]);
                }
                // `continue` runs the update expression first, so it lands on
                // `cont`, which sits just before the update.
                self.loop_body(body, labels, cont, end)?;
                self.place_label(cont);
                // Each iteration ends with a fresh cell, so the closures the body just
                // created keep the cell they captured and the update writes to a new
                // one. Without this every closure in the loop shares one binding.
                let binding_block = for_key;
                for name in &loop_bindings {
                    let pos = self.cur().cur_pos;
                    self.state().insns.push(Insn::CloseCaptured {
                        name: name.clone(),
                        block: binding_block,
                    });
                    self.state().insn_pos.push(pos);
                }
                if let Some(update) = update {
                    self.state().discard_value = true;
                    let result = self.expr(update);
                    self.state().discard_value = false;
                    result?;
                    if !std::mem::take(&mut self.state().no_value) {
                        self.emit(Opcode::Drop, &[]);
                    }
                }
                self.emit_jump(Opcode::Goto, top, &[]);
                self.place_label(end);
                self.emit_scope_leave(for_key);
                self.state().current_block = for_parent;
            }
            StmtKind::Return(value) => self.return_stmt(value.as_deref())?,
            StmtKind::Throw(expr) => {
                self.expr(expr)?;
                self.emit(Opcode::Throw, &[]);
            }
            StmtKind::Break(_) | StmtKind::Continue(_) => {
                // Labels and loops are wired up in a later milestone; a bare
                // break/continue jumps to the innermost enclosing target, which
                // the loop helpers register.
                self.control_transfer(stmt)?;
            }
            StmtKind::Labeled { label, body } => {
                self.labeled(label, body, stmt)?;
            }
            StmtKind::Switch {
                discriminant,
                cases,
            } => {
                self.switch(discriminant, cases, stmt)?;
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                self.try_stmt(stmt, block, handler, finalizer.as_deref())?;
            }
            StmtKind::ForIn { left, right, body } => {
                self.for_in_of(stmt, left, right, body, ForLoopKind::In)?;
            }
            StmtKind::ForOf {
                left,
                right,
                body,
                is_await,
            } => {
                if *is_await {
                    return Err(SyntaxError::new(
                        "for await is not supported by the compiler yet",
                        stmt.span,
                    ));
                }
                self.for_in_of(stmt, left, right, body, ForLoopKind::Of)?;
            }
            StmtKind::With { .. } => {
                // `with` is sloppy-mode only and needs the object scope chain at
                // run time; left for the milestone that adds it.
                return Err(SyntaxError::new(
                    "'with' is not supported by the compiler yet",
                    stmt.span,
                ));
            }
            StmtKind::ClassDecl(_) => {
                return Err(SyntaxError::new(
                    "class declarations are not supported by the compiler yet",
                    stmt.span,
                ));
            }
            StmtKind::Import(_) | StmtKind::Export(_) => {
                return Err(SyntaxError::new(
                    "modules are not supported by the compiler yet",
                    stmt.span,
                ));
            }
        }
        Ok(())
    }

    /// Compile `return`, running every enclosing finalizer on the way out.
    ///
    /// `nip_catch` is what makes this possible: it discards everything on the stack
    /// down to and including the `catch` offset while keeping the return value on
    /// top, so the frame the finalizer sees has the same shape as on the normal
    /// path.
    fn return_stmt(&mut self, value: Option<&Expr>) -> Result<(), SyntaxError> {
        let mut has_value = value.is_some();
        if let Some(expr) = value {
            // `return f(x)` is a tail call *only* when nothing has to happen to the
            // value afterwards. A `finally` or an open iterator in scope forces the
            // normal path, because the cleanup has to run after the call returns —
            // which is exactly what a tail call refuses to do.
            let plain = self
                .cur()
                .jump_targets
                .iter()
                .all(|target| target.finally_label.is_none() && !target.has_iterator);
            if plain && self.expr_if_tail_call(expr)? {
                return Ok(());
            }
            self.expr(expr)?;
        }
        let targets = self.cur().jump_targets.clone();
        for target in targets.iter().rev() {
            if target.finally_label.is_none() && !target.has_iterator {
                continue;
            }
            if !has_value {
                // The cleanup expects a value to pass through; with nothing to
                // return, `undefined` stands in.
                self.emit(Opcode::Undefined, &[]);
                has_value = true;
            }
            self.emit(Opcode::NipCatch, &[]);
            if target.has_iterator {
                // Rewind to the loop's own slots, then rotate the return value out
                // of the way and hand `iterator_close` a dummy catch offset: the
                // loop is being abandoned, so there is nothing left to catch.
                self.emit(Opcode::Rot3r, &[]);
                self.emit(Opcode::Undefined, &[]);
                self.emit(Opcode::IteratorClose, &[]);
            } else if let Some(finally) = target.finally_label {
                self.emit_jump(Opcode::Gosub, finally, &[]);
            }
        }
        if has_value {
            self.emit(Opcode::Return, &[]);
        } else {
            self.emit(Opcode::ReturnUndef, &[]);
        }
        Ok(())
    }

    /// Compile a statement list, stopping at the first statement that cannot
    /// fall through.
    fn stmts(&mut self, body: &[Stmt]) -> Result<(), SyntaxError> {
        for stmt in body {
            self.stmt(stmt)?;
            if Self::terminates(stmt) {
                break;
            }
        }
        Ok(())
    }

    /// Declare every name a statement list introduces, before compiling any of
    /// it, so a reference that appears textually before a declaration still
    /// resolves.
    ///
    /// Declarations are walked in source order because that is the order the
    /// reference numbers its slots in: `let a; var b;` gives `a` slot 0 and `b`
    /// slot 1, while `var b; let a;` gives the opposite. `var` and function
    /// declarations hoist out to the function scope; lexical names belong to the
    /// block they were written in, which `block` records.
    fn hoist(&mut self, body: &[Stmt], block: u64) -> Result<(), SyntaxError> {
        let saved = self.cur().current_block;
        self.state().current_block = block;
        let result = self.hoist_list(body);
        // `current_block` is restored by the caller for a block statement, since
        // it needs it set while compiling; only clean up on the hoist path.
        self.state().current_block = saved;
        result
    }

    fn hoist_list(&mut self, body: &[Stmt]) -> Result<(), SyntaxError> {
        for stmt in body {
            self.hoist_stmt(stmt)?;
        }
        Ok(())
    }

    /// Whether a statement always transfers control away, making everything after
    /// it in the same list unreachable.
    ///
    /// The reference drops such statements in `skip_dead_code`, so `return 1;
    /// let x = 2;` compiles to the same bytecode as `return 1;`.
    fn terminates(stmt: &Stmt) -> bool {
        matches!(
            stmt.kind,
            StmtKind::Return(_) | StmtKind::Throw(_) | StmtKind::Break(_) | StmtKind::Continue(_)
        )
    }

    fn hoist_stmt(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        match &stmt.kind {
            StmtKind::VarDecl(decl) => {
                for declarator in &decl.declarators {
                    let mut names = Vec::new();
                    declarator.target.bound_names(&mut names);
                    for name in names {
                        let text = name.name.clone();
                        if decl.kind == VarDeclKind::Var {
                            let def = VarDef::var(text.clone());
                            self.declare_var(&text, def);
                        } else {
                            let def = if decl.kind == VarDeclKind::Const {
                                VarDef::constant(text.clone())
                            } else {
                                VarDef::lexical(text.clone())
                            };
                            self.declare(&text, def);
                        }
                    }
                }
            }
            StmtKind::FuncDecl(func) => {
                if let Some(name) = &func.name {
                    // A function declaration is a `var` binding that happens to
                    // hold a function, not a lexical one.
                    let def = VarDef::var(name.name.clone());
                    self.declare_var(&name.name.clone(), def);
                }
                // The function object is created when the scope is entered, so it
                // exists before the declaration is reached.
                self.func_decl(func)?;
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = &class.name {
                    self.declare(&name.name.clone(), VarDef::lexical(name.name.clone()));
                }
            }
            StmtKind::Block(inner) => self.hoist_block(stmt.span.start.offset as u64, inner)?,
            StmtKind::If { cons, alt, .. } => {
                self.hoist_stmt(cons)?;
                if let Some(alt) = alt {
                    self.hoist_stmt(alt)?;
                }
            }
            StmtKind::While { body, .. }
            | StmtKind::DoWhile { body, .. }
            | StmtKind::Labeled { body, .. }
            | StmtKind::With { body, .. } => self.hoist_stmt(body)?,
            StmtKind::For { init, body, .. } => {
                // A `for` head gets its own scope even when it declares nothing,
                // because `let` in the head is scoped to the loop.
                let key = stmt.span.start.offset as u64;
                let parent = self.cur().current_block;
                self.push_block(key, parent);
                if let Some(init) = init
                    && let ForInit::VarDecl(decl) = init
                {
                    self.hoist_stmt(&Stmt::new(StmtKind::VarDecl(decl.clone()), stmt.span))?;
                }
                self.hoist_stmt(body)?;
                self.state().current_block = parent;
            }
            StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
                let key = stmt.span.start.offset as u64;
                let parent = self.cur().current_block;
                self.push_block(key, parent);
                if let ForHead::VarDecl(decl) = left {
                    self.hoist_stmt(&Stmt::new(StmtKind::VarDecl(decl.clone()), stmt.span))?;
                }
                self.hoist_stmt(body)?;
                self.state().current_block = parent;
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                self.hoist_block(Self::try_block_key(stmt, TryPart::Block), block)?;
                let try_parent = self.cur().current_block;
                if let Some(handler) = handler {
                    // The catch parameter belongs to the function scope, not to a
                    // block: it is a `catch` binding rather than a `let`, so it is
                    // never in the temporal dead zone and `emit_scope_enter` must
                    // not mark it uninitialised. It still has to be declared before
                    // the handler body is hoisted, so it takes the slot the
                    // reference gives it (bindings are numbered in source order).
                    if let Some(Pattern {
                        kind: PatternKind::Ident(name),
                        ..
                    }) = &handler.param
                    {
                        // The parameter gets its own scope, so entering the handler
                        // does not have to re-declare it. It is not lexical, so it
                        // is never marked uninitialised.
                        let key = Self::try_block_key(stmt, TryPart::Param);
                        let parent = self.cur().current_block;
                        self.push_block(key, parent);
                        let def = VarDef {
                            name: name.name.clone(),
                            is_const: false,
                            is_lexical: false,
                            kind: crate::bytecode::VarKind::Catch,
                            is_captured: false,
                            scope_level: 0,
                            scope_next: None,
                        };
                        self.declare_local(&name.name.clone(), def);
                        // The handler body nests inside the parameter's scope, so the
                        // body's bindings chain on to the parameter (the reference
                        // prints `let y [next:1]`, pointing at `e`).
                        self.state().current_block = key;
                    }
                    self.hoist_block(Self::try_block_key(stmt, TryPart::Catch), &handler.body)?;
                    // Back to the block the `try` itself lives in, so the finalizer
                    // is a sibling of the handler rather than nested in it.
                    self.state().current_block = try_parent;
                }
                if let Some(finalizer) = finalizer {
                    self.hoist_block(Self::try_block_key(stmt, TryPart::Finally), finalizer)?;
                }
            }
            StmtKind::Switch { cases, .. } => {
                let key = stmt.span.start.offset as u64;
                let parent = self.cur().current_block;
                self.push_block(key, parent);
                for case in cases {
                    self.hoist_list(&case.body)?;
                }
                self.state().current_block = parent;
            }
            _ => {}
        }
        Ok(())
    }

    /// Hoist a nested statement list, which is its own lexical scope.
    fn hoist_block(&mut self, key: u64, body: &[Stmt]) -> Result<(), SyntaxError> {
        let parent = self.cur().current_block;
        self.push_block(key, parent);
        self.hoist_list(body)?;
        self.state().current_block = parent;
        Ok(())
    }

    /// A block key for one part of a `try` statement.
    ///
    /// The three parts are three separate scopes, so they cannot share the key a
    /// statement's own offset would give them: the try block, the handler body and
    /// the finalizer would then look like one scope, and their `let` bindings would
    /// be marked uninitialised only on entering the first of them.
    fn try_block_key(stmt: &Stmt, part: TryPart) -> u64 {
        (stmt.span.start.offset as u64) * 4 + part as u64
    }

    /// Declare a `var`: it lives in the function's own scope, not the block's.
    fn declare_var(&mut self, name: &str, def: VarDef) {
        if self.cur().declares_globally(&def) {
            self.declare_global(name, &def);
            return;
        }
        let state = self.state();
        if state.vars.iter().any(|v| v.name == name) {
            return;
        }
        // A `var` belongs to the function scope, never to the enclosing block.
        state.var_block.push(FUNCTION_SCOPE);
        state.vars.push(def);
    }

    /// Give the function an `arguments` binding if its body mentions one.
    ///
    /// The check runs on the *instruction* stream rather than the AST: by this point
    /// every reference has become an `Insn::Var`, so scanning is exact and needs no
    /// AST walker. Declaring lazily matters for more than tidiness — always declaring
    /// would renumber every later local slot and stop the bytecode matching the
    /// reference, which only adds the binding when the body uses it.
    ///
    /// `arguments` is a local, initialised at the top of the body by
    /// `special_object`; that is the reference's shape, so the disassembly agrees.
    fn provide_arguments(&mut self) {
        let current = self.stack.len() - 1;
        if self.stack[current].is_script || self.stack[current].is_arrow {
            return;
        }
        if !self.mentions("arguments") {
            return;
        }
        // 0 is strict `arguments`, 1 is sloppy (mapped) `arguments`. The mapping
        // itself is not implemented, so the two behave alike for now.
        let magic = if self.stack[current].strict { 0 } else { 1 };
        self.prepend_binding("arguments", magic);
    }

    /// Whether the function body's instruction stream refers to `name`.
    fn mentions(&self, name: &str) -> bool {
        let current = self.stack.len() - 1;
        if self.stack[current].vars.iter().any(|v| v.name == name) {
            return false;
        }
        self.stack[current]
            .insns
            .iter()
            .any(|insn| matches!(insn, Insn::Var { name: used, .. } if used == name))
    }

    /// Bind the name a named function expression uses to refer to itself.
    ///
    /// The value is `special_object 2`, the function currently running, so the binding
    /// is the closure itself and not a copy. It is a local of this function, which is
    /// what keeps `g` out of the enclosing scope.
    fn provide_self_name(&mut self) {
        let current = self.stack.len() - 1;
        let Some(name) = self.stack[current].self_name.clone() else {
            return;
        };
        if !self.mentions(&name) {
            return;
        }
        self.prepend_binding(&name, 2);
    }

    /// Insert `special_object magic` plus the store to `name` at the top of the body.
    ///
    /// Prepending shifts every label that has been placed, so they are all moved by
    /// the number of instructions inserted.
    fn prepend_binding(&mut self, name: &str, magic: i32) {
        let current = self.stack.len() - 1;
        self.declare_var(name, VarDef::var(name));
        let init = [
            Insn::Fixed {
                op: Opcode::SpecialObject,
                operands: vec![Op::Int(magic)],
            },
            Insn::Var {
                action: VarAction::Put,
                name: name.to_string(),
                block: 0,
            },
        ];
        let inserted = init.len();
        for (index, instruction) in init.into_iter().enumerate() {
            self.stack[current].insns.insert(index, instruction);
        }
        for placed in self.stack[current].labels.iter_mut().flatten() {
            *placed += inserted;
        }
        // A store sits at the insertion point now, so the `put`/`get` fold must not
        // reach across it.
        self.stack[current].label_since_insn = true;
    }

    /// Record a top-level script declaration as a global.
    ///
    /// A script's top-level `var`/`let`/`function` create properties of the
    /// global object, not slots in the script's frame, so they show up as closure
    /// variables instead.
    fn declare_global(&mut self, name: &str, def: &VarDef) {
        let state = self.state();
        if let Some(existing) = state.closure_vars.iter_mut().find(|c| c.name == name) {
            // The name was already referenced, from a function compiled before this
            // declaration was reached — `function a(){ b() } function b(){}` resolves
            // `b` while `b` is still unknown, and that guess has to be corrected here.
            // Leaving it as a plain global meant the declaration was never registered,
            // so the binding did not exist at run time and `a()` threw
            // `b is not defined`.
            if matches!(existing.closure_type, ClosureType::Global) {
                existing.closure_type = ClosureType::GlobalDecl;
                existing.is_const = def.is_const;
                existing.is_lexical = def.is_lexical;
            }
            return;
        }
        state.closure_vars.push(ClosureVar {
            name: name.to_string(),
            is_const: def.is_const,
            is_lexical: def.is_lexical,
            closure_type: ClosureType::GlobalDecl,
            var_idx: 0,
        });
    }

    /// Compile a loop body with a `break`/`continue` target pushed for it.
    fn loop_body(
        &mut self,
        body: &Stmt,
        labels: &[String],
        continue_label: Label,
        break_label: Label,
    ) -> Result<(), SyntaxError> {
        self.push_jump_target(JumpTarget {
            names: labels.to_vec(),
            break_label: Some(break_label),
            continue_label: Some(continue_label),
            // A loop leaves nothing of its own on the value stack.
            drop_count: 0,
            finally_label: None,
            is_regular_stmt: false,
            has_iterator: false,
        });
        let result = self.stmt(body);
        self.pop_jump_target();
        result
    }

    fn push_jump_target(&mut self, target: JumpTarget) {
        self.state().jump_targets.push(target);
    }

    fn pop_jump_target(&mut self) {
        self.state().jump_targets.pop();
    }

    fn control_transfer(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        let (label, is_break) = match &stmt.kind {
            StmtKind::Break(label) => (label.as_ref(), true),
            StmtKind::Continue(label) => (label.as_ref(), false),
            _ => unreachable!("control_transfer is only called for break and continue"),
        };

        // Walk outward from the innermost construct. Everything considered before
        // the match is a construct control is leaving, and leaving it has a cost:
        // its stack slots go, and its finalizer runs.
        let targets = self.cur().jump_targets.clone();
        for target in targets.iter().rev() {
            let destination = match (label, is_break) {
                // An unlabelled `break` skips a labelled non-loop statement: `break`
                // may name one, but the bare form only means "leave the loop".
                (None, true) => target.break_label.filter(|_| !target.is_regular_stmt),
                (None, false) => target.continue_label,
                (Some(name), true) => target
                    .break_label
                    .filter(|_| target.names.contains(&name.name)),
                (Some(name), false) => target
                    .continue_label
                    .filter(|_| target.names.contains(&name.name)),
            };

            if let Some(destination) = destination {
                self.emit_jump(Opcode::Goto, destination, &[]);
                return Ok(());
            }

            // Closing the iterator consumes the three slots the loop keeps on the
            // stack (iterator, next method, catch offset), so those are not dropped
            // again below.
            let mut remaining = target.drop_count;
            if target.has_iterator {
                self.emit(Opcode::IteratorClose, &[]);
                remaining = remaining.saturating_sub(3);
            }
            for _ in 0..remaining {
                self.emit(Opcode::Drop, &[]);
            }
            if let Some(finally) = target.finally_label {
                // The finalizer runs as a subroutine and is handed a dummy return
                // value, which is dropped afterwards to keep the stack the depth it
                // had before.
                self.emit(Opcode::Undefined, &[]);
                self.emit_jump(Opcode::Gosub, finally, &[]);
                self.emit(Opcode::Drop, &[]);
            }
        }

        // The parser rejects a break/continue with no target, so reaching here
        // means the compiler's own bookkeeping is wrong.
        Err(SyntaxError::new(
            if is_break {
                "break has no enclosing loop or switch"
            } else {
                "continue has no enclosing loop"
            },
            stmt.span,
        ))
    }

    /// Compile a labelled statement.
    ///
    /// `a: b: for(…)` is a chain of labels on one loop; every name has to reach
    /// that loop's targets, so the chain is collected and handed to the loop. A
    /// label on a non-loop statement only provides a `break` target, because
    /// `continue` may not target it.
    fn labeled(&mut self, label: &Ident, body: &Stmt, stmt: &Stmt) -> Result<(), SyntaxError> {
        let mut names = vec![label.name.clone()];
        let mut inner = body;
        while let StmtKind::Labeled {
            label: next,
            body: deeper,
        } = &inner.kind
        {
            names.push(next.name.clone());
            inner = deeper;
        }
        if Self::is_loop(inner) {
            return self.stmt_labelled(inner, &names);
        }
        let end = self.new_label();
        self.push_jump_target(JumpTarget {
            names,
            break_label: Some(end),
            continue_label: None,
            drop_count: 0,
            finally_label: None,
            is_regular_stmt: true,
            has_iterator: false,
        });
        let result = self.stmt(inner);
        self.pop_jump_target();
        result?;
        self.place_label(end);
        let _ = stmt;
        Ok(())
    }

    /// Whether a statement is a loop, i.e. a valid target for `continue`.
    fn is_loop(stmt: &Stmt) -> bool {
        matches!(
            stmt.kind,
            StmtKind::While { .. }
                | StmtKind::DoWhile { .. }
                | StmtKind::For { .. }
                | StmtKind::ForIn { .. }
                | StmtKind::ForOf { .. }
        )
    }

    /// Compile `switch`.
    ///
    /// The discriminant is evaluated once and stays on the value stack while the
    /// clauses compare against copies of it, so it is dropped at the end — which is
    /// why `switch (x) { }` still leaves `x` behind. Fallthrough is real: a
    /// clause's body jumps over the *next* clause's tests straight into its body.
    ///
    /// The tests form a chain: each `case` run compares and, on a miss, hands over to
    /// the next run's tests. Only the last run's miss reaches the `default` body, or
    /// the end of the switch when there is no `default`. That failure path can point
    /// backwards (`default` written first), so the runs and their labels are worked
    /// out before any code is emitted.
    fn switch(
        &mut self,
        discriminant: &Expr,
        cases: &[SwitchCase],
        stmt: &Stmt,
    ) -> Result<(), SyntaxError> {
        self.reset_value();
        // The discriminant is evaluated outside the switch's block scope, before
        // any `case` clause exists.
        self.expr(discriminant)?;

        let switch_key = stmt.span.start.offset as u64;
        let switch_parent = self.cur().current_block;
        self.push_block(switch_key, switch_parent);
        self.emit_scope_enter(switch_key);

        let break_label = self.new_label();
        self.push_jump_target(JumpTarget {
            names: Vec::new(),
            break_label: Some(break_label),
            // `switch` is a valid `break` target but not a `continue` target.
            continue_label: None,
            // The discriminant stays on the value stack for the whole statement, so
            // a `break` aimed further out has to discard it.
            drop_count: 1,
            finally_label: None,
            is_regular_stmt: false,
            has_iterator: false,
        });

        // Runs of tests. `case 1: case 2: body` is one run: the empty clause hands
        // its label to the clause after it, so both tests reach the same body.
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut scan = 0;
        while scan < cases.len() {
            if cases[scan].test.is_some() {
                let mut last = scan;
                while last + 1 < cases.len()
                    && cases[last].test.is_some()
                    && cases[last].body.is_empty()
                    && cases[last + 1].test.is_some()
                {
                    last += 1;
                }
                runs.push((scan, last));
                scan = last + 1;
            } else {
                scan += 1;
            }
        }

        let body_labels: Vec<Label> = cases.iter().map(|_| self.new_label()).collect();
        let run_labels: Vec<Label> = runs.iter().map(|_| self.new_label()).collect();
        let default_at = cases.iter().position(|case| case.test.is_none());
        // Where the last run sends control when nothing matched.
        let miss_target = match default_at {
            Some(index) => body_labels[index],
            None => break_label,
        };

        // When `default` comes first, control has to jump over it to reach the
        // tests. It only needs to when some clause actually has a test; otherwise
        // the default body is the entry point.
        let mut entry = None;
        if default_at == Some(0) && !runs.is_empty() {
            self.emit_jump(Opcode::Goto, run_labels[0], &[]);
            entry = Some(run_labels[0]);
        }

        let mut index = 0;
        let mut run = 0;
        let result = (|| -> Result<(), SyntaxError> {
            while index < cases.len() {
                if cases[index].test.is_some() {
                    let (first, last) = runs[run];
                    // Reaching these tests from the preceding body is a
                    // fallthrough, which continues inside the body and skips them.
                    if index > 0 {
                        self.emit_jump(Opcode::Goto, body_labels[index], &[]);
                    }
                    if let Some(label) = entry.take() {
                        self.place_label(label);
                    }
                    self.place_label(run_labels[run]);

                    for (offset, case) in cases[first..=last].iter().enumerate() {
                        let test = case.test.as_ref().unwrap();
                        self.emit(Opcode::Dup, &[]);
                        self.expr(test)?;
                        self.emit(Opcode::StrictEq, &[]);
                        if first + offset < last {
                            self.emit_jump(Opcode::IfTrue, body_labels[index], &[]);
                        } else {
                            // Hand over to the next run's tests, or give up.
                            let next = match runs.get(run + 1) {
                                Some(_) => run_labels[run + 1],
                                None => miss_target,
                            };
                            self.emit_jump(Opcode::IfFalse, next, &[]);
                        }
                    }

                    self.place_label(body_labels[index]);
                    for case in &cases[first..=last] {
                        self.stmts(&case.body)?;
                    }
                    index = last + 1;
                    run += 1;
                } else {
                    // `default`: control arrives here either by falling through or
                    // because every test missed.
                    self.place_label(body_labels[index]);
                    self.stmts(&cases[index].body)?;
                    index += 1;
                }
            }
            Ok(())
        })();

        if result.is_ok() {
            self.place_label(break_label);
            // Discard the discriminant now that no test needs it.
            self.emit(Opcode::Drop, &[]);
        }

        self.pop_jump_target();
        self.emit_scope_leave(switch_key);
        self.state().current_block = switch_parent;
        result
    }

    /// Compile `try`/`catch`/`finally`.
    ///
    /// The shape is an exception table built out of `catch` instructions, each of
    /// which records where control goes if the instructions that follow raise:
    ///
    /// ```text
    /// catch HANDLER            <- if the guarded block throws, jump to HANDLER
    ///   <guarded block>
    ///   drop                   <- discard the catch offset
    ///   undefined; gosub FINALLY; drop
    ///   goto END
    /// HANDLER:
    ///   <bind the catch parameter>
    ///   catch CATCH2           <- if the handler itself throws, go to CATCH2
    ///   <handler body>
    ///   drop; undefined; gosub FINALLY; drop; goto END
    /// CATCH2:
    ///   gosub FINALLY; throw   <- run the finalizer, then let the error out
    /// FINALLY:
    ///   <finalizer>
    ///   ret                    <- return to the `gosub`
    /// END:
    /// ```
    ///
    /// `gosub` is a subroutine call and `ret` returns to it, which is how one copy
    /// of the finalizer serves the normal path, the handler path and the
    /// propagating-exception path.
    ///
    /// The `undefined` before each `gosub` is a dummy return value: the finalizer
    /// runs with `ret_value` on the stack, and the normal path has no value to
    /// keep, so it pushes one to keep the stack depth identical on every path.
    fn try_stmt(
        &mut self,
        stmt: &Stmt,
        block: &[Stmt],
        handler: &Option<CatchClause>,
        finalizer: Option<&[Stmt]>,
    ) -> Result<(), SyntaxError> {
        self.reset_value();

        let catch_label = self.new_label();
        let catch2_label = self.new_label();
        let finally_label = self.new_label();
        let end_label = self.new_label();

        self.emit_jump(Opcode::Catch, catch_label, &[]);

        // The guarded block. `break` may not target it, so its entry exists purely
        // to unwind: a jump out has to discard the catch offset and run the
        // finalizer.
        self.push_jump_target(JumpTarget {
            names: Vec::new(),
            break_label: None,
            continue_label: None,
            drop_count: 1,
            finally_label: Some(finally_label),
            is_regular_stmt: false,
            has_iterator: false,
        });

        let block_key = Self::try_block_key(stmt, TryPart::Block);
        let block_parent = self.cur().current_block;
        self.push_block(block_key, block_parent);
        self.emit_scope_enter(block_key);
        self.stmts(block)?;
        self.emit_scope_leave(block_key);
        self.state().current_block = block_parent;
        // A guarded block that ends in a `return`/`throw` never reaches the
        // epilogue, so the reference does not emit one.
        self.pop_jump_target();
        let block_live = !block.last().is_some_and(Self::terminates);

        if block_live {
            self.emit(Opcode::Drop, &[]);
            self.emit(Opcode::Undefined, &[]);
            self.emit_jump(Opcode::Gosub, finally_label, &[]);
            self.emit(Opcode::Drop, &[]);
            self.emit_jump(Opcode::Goto, end_label, &[]);
        }

        if let Some(handler) = handler {
            self.place_label(catch_label);
            // The thrown value is on top of the stack: bind it, or discard it for
            // `catch { }`.
            match &handler.param {
                Some(Pattern {
                    kind: PatternKind::Ident(name),
                    ..
                }) => {
                    // The store resolves the name, so it has to run inside the
                    // parameter's own scope; otherwise a same-named `catch` further
                    // out could win.
                    let param_key = Self::try_block_key(stmt, TryPart::Param);
                    let param_parent = self.cur().current_block;
                    self.push_block(param_key, param_parent);
                    self.emit_var(VarAction::Put, &name.name);
                    self.state().current_block = param_parent;
                }
                Some(other) => {
                    return Err(SyntaxError::new(
                        "destructuring catch parameters are not supported by the compiler yet",
                        other.span,
                    ));
                }
                None => self.emit(Opcode::Drop, &[]),
            }
            self.emit_jump(Opcode::Catch, catch2_label, &[]);

            self.push_jump_target(JumpTarget {
                names: Vec::new(),
                break_label: None,
                continue_label: None,
                drop_count: 1,
                finally_label: Some(finally_label),
                is_regular_stmt: false,
                has_iterator: false,
            });
            let catch_key = Self::try_block_key(stmt, TryPart::Catch);
            let catch_parent = self.cur().current_block;
            self.push_block(catch_key, catch_parent);
            self.emit_scope_enter(catch_key);
            self.stmts(&handler.body)?;
            self.emit_scope_leave(catch_key);
            self.state().current_block = catch_parent;
            self.pop_jump_target();
            let handler_live = !handler.body.last().is_some_and(Self::terminates);

            if handler_live {
                self.emit(Opcode::Drop, &[]);
                self.emit(Opcode::Undefined, &[]);
                self.emit_jump(Opcode::Gosub, finally_label, &[]);
                self.emit(Opcode::Drop, &[]);
                self.emit_jump(Opcode::Goto, end_label, &[]);
            }

            // An error raised inside the handler still has to run the finalizer.
            self.place_label(catch2_label);
            self.emit_jump(Opcode::Gosub, finally_label, &[]);
            self.emit(Opcode::Throw, &[]);
        } else {
            // `try`/`finally` with no handler: the finalizer runs, then the error
            // keeps propagating.
            self.place_label(catch_label);
            self.emit_jump(Opcode::Gosub, finally_label, &[]);
            self.emit(Opcode::Throw, &[]);
        }

        self.place_label(finally_label);
        // Inside the finalizer, leaving has a different cost: the return value and
        // the `gosub` return address are both on the stack, and neither finalizer nor
        // any outer one may run before the block has finished.
        self.push_jump_target(JumpTarget {
            names: Vec::new(),
            break_label: None,
            continue_label: None,
            drop_count: 2,
            finally_label: None,
            is_regular_stmt: false,
            has_iterator: false,
        });
        if let Some(finalizer) = finalizer {
            let finally_key = Self::try_block_key(stmt, TryPart::Finally);
            let finally_parent = self.cur().current_block;
            self.push_block(finally_key, finally_parent);
            self.emit_scope_enter(finally_key);
            self.stmts(finalizer)?;
            self.emit_scope_leave(finally_key);
            self.state().current_block = finally_parent;
        }
        self.pop_jump_target();
        self.emit(Opcode::Ret, &[]);
        self.place_label(end_label);
        Ok(())
    }

    /// Compile `for`/`in` and `for`/`of`.
    ///
    /// Both keep an enumeration record on the value stack for the whole loop, which
    /// is what makes leaving early expensive: the record has to be dropped, or the
    /// iterator closed. The layout is
    ///
    /// ```text
    ///   <iterated expression>
    ///   for_in_start | for_of_start      <- pushes the record
    ///   goto TEST
    /// NEXT:
    ///   <bind the loop variable>
    ///   <body>
    /// TEST:
    ///   for_in_next | for_of_next 0
    ///   if_false NEXT                    <- not finished: next iteration
    ///   drop                             <- finished: discard the leftover value
    /// BREAK:
    ///   drop | iterator_close            <- the one cleanup `break` also uses
    /// ```
    ///
    /// `BREAK` sits *on* the cleanup rather than after it, so an early exit and the
    /// normal exit share one copy — which is why `break` inside the loop emits nothing
    /// of its own, and only a jump that passes *through* the loop pays for its record.
    fn for_in_of(
        &mut self,
        stmt: &Stmt,
        left: &ForHead,
        right: &Expr,
        body: &Stmt,
        kind: ForLoopKind,
    ) -> Result<(), SyntaxError> {
        self.reset_value();

        let loop_key = stmt.span.start.offset as u64;
        let loop_parent = self.cur().current_block;
        self.push_block(loop_key, loop_parent);
        self.emit_scope_enter(loop_key);

        // The iterated expression is evaluated inside the loop's scope, after the
        // bindings exist in the temporal dead zone.
        self.expr(right)?;

        let next = self.new_label();
        let test = self.new_label();
        let break_target = self.new_label();

        let has_iterator = kind == ForLoopKind::Of;
        match kind {
            ForLoopKind::In => self.emit(Opcode::ForInStart, &[]),
            ForLoopKind::Of => self.emit(Opcode::ForOfStart, &[]),
        }
        self.emit_jump(Opcode::Goto, test, &[]);

        self.push_jump_target(JumpTarget {
            names: Vec::new(),
            break_label: Some(break_target),
            continue_label: Some(test),
            // Passing through the loop has to dispose of its record. A `for`/`of`
            // record is three slots, and closing the iterator consumes all of them.
            drop_count: if has_iterator { 3 } else { 1 },
            finally_label: None,
            is_regular_stmt: false,
            has_iterator,
        });

        self.place_label(next);
        self.bind_loop_variable(left)?;
        // The reference marks the body with its own label here. Nothing jumps to it
        // once the loop is laid out, but it is not decoration: a label between the
        // store and the body is why `put_loc v; get_loc v` does *not* fold into
        // `set_loc v` when the body starts by reading the loop variable.
        let body_label = self.new_label();
        self.place_label(body_label);
        self.stmt(body)?;

        self.place_label(test);
        match kind {
            ForLoopKind::In => self.emit(Opcode::ForInNext, &[]),
            ForLoopKind::Of => self.emit(Opcode::ForOfNext, &[Op::Int(0)]),
        }
        self.emit_jump(Opcode::IfFalse, next, &[]);
        // The loop is finished; `for_xx_next` left an `undefined` behind.
        self.emit(Opcode::Drop, &[]);

        self.pop_jump_target();
        self.place_label(break_target);
        if has_iterator {
            self.emit(Opcode::IteratorClose, &[]);
        } else {
            self.emit(Opcode::Drop, &[]);
        }

        self.emit_scope_leave(loop_key);
        self.state().current_block = loop_parent;
        Ok(())
    }

    /// Store the value `for_xx_next` left on the stack into the loop variable.
    fn bind_loop_variable(&mut self, left: &ForHead) -> Result<(), SyntaxError> {
        // A declaration *initialises* the binding, which is what takes a `let` out of
        // the temporal dead zone, so the store is the unchecked form. Assigning to a
        // binding declared elsewhere is an ordinary write and keeps its check.
        let (pattern, action) = match left {
            ForHead::VarDecl(decl) => (&decl.declarators[0].target, VarAction::PutInit),
            ForHead::Assign(pattern) => (pattern.as_ref(), VarAction::Put),
        };
        match &pattern.kind {
            PatternKind::Ident(name) => {
                self.emit_var(action, &name.name);
                Ok(())
            }
            _ => Err(SyntaxError::new(
                "destructuring loop targets are not supported by the compiler yet",
                pattern.span,
            )),
        }
    }

    fn var_decl(&mut self, decl: &VarDecl) -> Result<(), SyntaxError> {
        for declarator in &decl.declarators {
            let Some(init) = &declarator.init else {
                // `var x;` stays `undefined`. A lexical binding is different: it
                // has to be *initialised* to leave the temporal dead zone, so the
                // compiler supplies an explicit `undefined`.
                if decl.kind.is_lexical()
                    && let PatternKind::Ident(name) = &declarator.target.kind
                {
                    self.emit(Opcode::Undefined, &[]);
                    self.emit_var(VarAction::PutInit, &name.name);
                }
                continue;
            };
            match &declarator.target.kind {
                PatternKind::Ident(name) => {
                    self.expr(init)?;
                    self.set_name_if_anonymous(init, &name.name);
                    // A declaration statement produces no value, so nothing is
                    // duplicated onto the stack.
                    match decl.kind {
                        VarDeclKind::Var => self.emit_var(VarAction::Put, &name.name),
                        _ => self.emit_var(VarAction::PutInit, &name.name),
                    }
                }
                _ => {
                    return Err(SyntaxError::new(
                        "destructuring declarations are not supported by the compiler yet",
                        declarator.span,
                    ));
                }
            }
        }
        Ok(())
    }

    /// Compile a function declaration and queue it for instantiation at scope
    /// entry.
    ///
    /// A declaration is visible before the statement that declares it, so the
    /// code that creates the function object belongs at the top of the enclosing
    /// scope, not at the declaration.
    fn func_decl(&mut self, func: &ast::Function) -> Result<(), SyntaxError> {
        let name = func.name.as_ref().expect("a declaration has a name");
        let function = self.compile_function(func)?;
        let pool_index = self.push_constant(Constant::Function(Box::new(function)));
        let target = self.hoist_target(&name.name);
        let block = self.cur().current_block;
        self.state().hoisted.push((block, target, pool_index));
        Ok(())
    }

    /// Where a name lives, for a hoisted declaration.
    fn hoist_target(&self, name: &str) -> HoistTarget {
        let state = self.cur();
        if let Some(index) = state.args.iter().position(|a| a.name == name) {
            return HoistTarget::Arg(index as u32);
        }
        if let Some(index) = state.vars.iter().position(|v| v.name == name) {
            return HoistTarget::Loc(index as u32);
        }
        // A global function declaration is not a slot at all.
        HoistTarget::Global(name.to_string())
    }

    // -- expressions -------------------------------------------------------

    /// Compile one expression, recording where its instructions sit.
    ///
    /// The position is the node's operator when it has one and its first token
    /// otherwise, and it is restored on the way out so a parent's own instructions —
    /// which are emitted *after* its children — are blamed on the parent. Without the
    /// restore the `add` in `a + b` would inherit `b`'s position instead of the `+`.
    fn expr(&mut self, expr: &Expr) -> Result<(), SyntaxError> {
        let saved = self.cur().cur_pos;
        self.state().cur_pos = Some(expr.op_pos.unwrap_or(expr.span.start));
        let result = self.expr_inner(expr);
        self.state().cur_pos = saved;
        result
    }

    fn expr_inner(&mut self, expr: &Expr) -> Result<(), SyntaxError> {
        match &expr.kind {
            ExprKind::Number(value) => self.push_number(*value),
            ExprKind::Bool(true) => self.emit(Opcode::PushTrue, &[]),
            ExprKind::Bool(false) => self.emit(Opcode::PushFalse, &[]),
            ExprKind::Null => self.emit(Opcode::Null, &[]),
            ExprKind::String(text) => self.push_string(text),
            ExprKind::Ident(name) => self.emit_var(VarAction::Get, &name.name),
            ExprKind::This => self.push_this(),
            ExprKind::Paren(inner) => self.expr(inner)?,
            ExprKind::Binary { op, left, right } => {
                self.expr(left)?;
                self.expr(right)?;
                self.emit(binary_opcode(*op), &[]);
            }
            ExprKind::Logical { op, left, right } => self.logical(*op, left, right)?,
            ExprKind::Unary {
                op: UnaryOp::Minus,
                argument,
            } if matches!(argument.unparen().kind, ExprKind::Number(_)) => {
                let ExprKind::Number(value) = argument.unparen().kind else {
                    unreachable!("matched above")
                };
                // The reference folds a negative literal only when the negation is
                // exact in the integer range: `-1` becomes `push_minus1` and
                // `-1.5e3` becomes `push_i16 -1500`, but `-0` and `-2.5` keep a
                // runtime `neg`. Folding `-0` would lose the sign — `1/-0` is
                // `-Infinity`, not `Infinity` — and folding a fraction would change
                // which instruction the reference emits.
                let negated = -value;
                // `-2147483648` is deliberately excluded: its magnitude is one past
                // `i32::MAX`, so the reference sends it through the constant pool and
                // negates at run time instead.
                if value > 0.0
                    && negated == negated.trunc()
                    && negated >= -(i32::MAX as f64)
                    && negated < 0.0
                {
                    self.push_number(negated);
                } else {
                    self.push_number(value);
                    self.emit(Opcode::Neg, &[]);
                }
            }
            ExprKind::Unary { op, argument } => self.unary(*op, argument)?,
            ExprKind::Conditional { test, cons, alt } => {
                let else_label = self.new_label();
                let end_label = self.new_label();
                self.expr(test)?;
                self.emit_jump(Opcode::IfFalse, else_label, &[]);
                self.expr(cons)?;
                self.emit_jump(Opcode::Goto, end_label, &[]);
                self.place_label(else_label);
                self.expr(alt)?;
                self.place_label(end_label);
            }
            ExprKind::Assign { op, target, value } => self.assign(*op, target, value)?,
            ExprKind::Update {
                op,
                argument,
                prefix,
            } => self.update(*op, argument, *prefix)?,
            ExprKind::Sequence(items) => {
                for (index, item) in items.iter().enumerate() {
                    self.expr(item)?;
                    if index + 1 != items.len() {
                        self.emit(Opcode::Drop, &[]);
                    }
                }
            }
            ExprKind::Call { .. } => self.chain_link_top(expr)?,
            ExprKind::Member { .. } => self.chain_link_top(expr)?,
            ExprKind::OptionalChain(inner) => self.optional_chain(inner)?,
            ExprKind::Array { elements } => self.array_literal(elements, expr.span)?,
            ExprKind::Object(properties) => self.object_literal(properties, expr.span)?,
            ExprKind::Template(template) => self.template_literal(template)?,
            ExprKind::New { callee, arguments } => {
                // `new C(args)` compiles to `C; dup; args…; call_constructor n`.
                // The `dup` is not redundant: `call_constructor` takes the
                // constructor twice, once as the target and once as `new.target`,
                // which is what makes `new.target === C` inside the body.
                self.expr(callee)?;
                self.emit(Opcode::Dup, &[]);
                let args = arguments.as_deref().unwrap_or(&[]);
                if Self::has_spread(args) {
                    // `C C array`: the duplicated callee is already both the target
                    // and `new.target`, so no reordering is needed before `apply 1`.
                    self.argument_array(args)?;
                    self.emit(Opcode::Apply, &[Op::Int(1)]);
                    return Ok(());
                }
                for argument in args {
                    self.expr(argument)?;
                }
                self.emit(Opcode::CallConstructor, &[Op::Int(args.len() as i32)]);
            }
            ExprKind::Arrow(func) => {
                let function = self.compile_function(func)?;
                let index = self.push_constant(Constant::Function(Box::new(function)));
                self.emit(Opcode::Fclosure, &[Op::Const(index)]);
            }
            ExprKind::Function(func) => {
                let function = self.compile_function_with_name(func, true)?;
                let index = self.push_constant(Constant::Function(Box::new(function)));
                self.emit(Opcode::Fclosure, &[Op::Const(index)]);
            }
            _ => {
                return Err(SyntaxError::new(
                    "this expression form is not supported by the compiler yet",
                    expr.span,
                ));
            }
        }
        Ok(())
    }

    /// Build one array holding the evaluated call arguments, which is how a call
    /// containing a spread is compiled: the reference does the same, building the
    /// list on the stack and handing it to `apply`.
    fn argument_array(&mut self, arguments: &[Expr]) -> Result<(), SyntaxError> {
        let elements: Vec<Option<ArrayElement>> = arguments
            .iter()
            .map(|argument| match &argument.unparen().kind {
                ExprKind::Spread(inner) => Some(ArrayElement::Spread(inner.clone())),
                _ => Some(ArrayElement::Expr(Box::new(argument.clone()))),
            })
            .collect();
        self.array_literal(&elements, Span::default())
    }

    /// Whether any argument is a spread, which is what selects the `apply` form.
    fn has_spread(arguments: &[Expr]) -> bool {
        arguments
            .iter()
            .any(|argument| matches!(argument.unparen().kind, ExprKind::Spread(_)))
    }

    /// Compile `a${x}b`.
    ///
    /// A template is a `+` chain built left to right: the leading chunk (or an empty
    /// string when the template starts with a substitution), then for each
    /// substitution its value, `to_propkey` and `add`, then the following chunk when
    /// it is not empty. `to_propkey` is what makes a number or object substitute the
    /// way `ToString` would while leaving the result a string for `add` to
    /// concatenate.
    ///
    /// The chunk is an atom rather than a constant-pool string, because a non-empty
    /// chunk always lands where a value is pushed and `push_atom_value` is the
    /// instruction the reference uses for it; the empty first chunk has its own
    /// zero-byte form.
    fn template_literal(&mut self, template: &TemplateLiteral) -> Result<(), SyntaxError> {
        let first = template.quasis.first();
        match first.and_then(|quasi| quasi.cooked.clone()) {
            Some(text) if !text.is_empty() => {
                self.emit(Opcode::PushAtomValue, &[Op::Atom(Atom::from_text(&text))]);
            }
            // Either the template begins with a substitution, or the chunk is
            // empty — in both cases the chain needs a string to start from.
            _ => self.emit(Opcode::PushEmptyString, &[]),
        }
        for (index, expression) in template.expressions.iter().enumerate() {
            self.expr(expression)?;
            self.emit(Opcode::ToPropkey, &[]);
            self.emit(Opcode::Add, &[]);
            if let Some(text) = template
                .quasis
                .get(index + 1)
                .and_then(|quasi| quasi.cooked.clone())
                && !text.is_empty()
            {
                self.emit(Opcode::PushAtomValue, &[Op::Atom(Atom::from_text(&text))]);
                self.emit(Opcode::Add, &[]);
            }
        }
        Ok(())
    }

    /// Compile an object literal.
    ///
    /// The object is built in place: `object` pushes it, and each property leaves it
    /// on the stack with one field more, so no temporary is needed and later
    /// properties can overwrite earlier ones.
    ///
    /// The instruction depends on *how* the property was written, not on what kind
    /// of value it holds:
    ///
    /// | source | instruction |
    /// |---|---|
    /// | `{ a: v }` | `define_field a` |
    /// | `{ __proto__: v }` | `set_proto` |
    /// | `{ [k]: v }` | `to_propkey` … `define_array_el` + `drop` |
    /// | `{ m() {} }` | `define_method m,4` |
    /// | `{ get m() {} }` / `{ set m(v) {} }` | `define_method m,5` / `,6` |
    /// | `{ [k]() {} }` | `define_method_computed 4` |
    /// | `{ ...v }` | `null` … `copy_data_properties 6` + two `drop`s |
    ///
    /// A computed *method* skips `to_propkey`: `define_method_computed` converts the
    /// key itself, which is why the trailing `drop` is absent there too.
    fn object_literal(
        &mut self,
        properties: &[ObjectProperty],
        span: Span,
    ) -> Result<(), SyntaxError> {
        self.emit(Opcode::Object, &[]);
        for property in properties {
            match property {
                ObjectProperty::Spread(value) => {
                    // `copy_data_properties` takes the target, the source and an
                    // exclusion list; a literal `null` means "exclude nothing".
                    // Flag bit 1 copies symbols, bits 2/5 pick the source and the
                    // exclusion list by stack offset.
                    self.expr(spread_argument(value))?;
                    self.emit(Opcode::Null, &[]);
                    self.emit(Opcode::CopyDataProperties, &[Op::Int(COPY_ALL_PROPERTIES)]);
                    self.emit(Opcode::Drop, &[]);
                    self.emit(Opcode::Drop, &[]);
                }
                ObjectProperty::Property {
                    key,
                    value,
                    kind,
                    shorthand,
                    span: property_span,
                } => match key {
                    PropName::Computed(key_expr) => {
                        self.expr(key_expr)?;
                        match kind {
                            PropertyKind::Value => {
                                self.emit(Opcode::ToPropkey, &[]);
                                self.expr(value)?;
                                self.set_name_if_anonymous_computed(value);
                                self.emit(Opcode::DefineArrayEl, &[]);
                                // `define_array_el` leaves the object and the key;
                                // the key is no longer needed.
                                self.emit(Opcode::Drop, &[]);
                            }
                            PropertyKind::Method | PropertyKind::Get | PropertyKind::Set => {
                                self.emit_method_function(value)?;
                                self.emit(
                                    Opcode::DefineMethodComputed,
                                    &[Op::Int(define_method_flags(*kind))],
                                );
                            }
                        }
                    }
                    PropName::Literal(key) => {
                        let name = self.property_key_text(key, *property_span)?;
                        match kind {
                            PropertyKind::Method | PropertyKind::Get | PropertyKind::Set => {
                                self.emit_method_function(value)?;
                                self.emit(
                                    Opcode::DefineMethod,
                                    &[
                                        Op::Atom(Atom::from_text(&name)),
                                        Op::Int(define_method_flags(*kind)),
                                    ],
                                );
                            }
                            PropertyKind::Value => {
                                self.expr(value)?;
                                // `{__proto__: x}` sets the prototype instead of
                                // creating a property; the shorthand `{__proto__}`
                                // is an ordinary property and does not.
                                if name == "__proto__" && !shorthand {
                                    self.emit(Opcode::SetProto, &[]);
                                } else {
                                    self.set_name_if_anonymous(value, &name);
                                    self.emit(
                                        Opcode::DefineField,
                                        &[Op::Atom(Atom::from_text(&name))],
                                    );
                                }
                            }
                        }
                    }
                    PropName::Private(_) => {
                        return Err(SyntaxError::new(
                            "private names are not valid in object literals",
                            *property_span,
                        ));
                    }
                },
            }
        }
        let _ = span;
        Ok(())
    }

    /// Emit the function a method/accessor property defines.
    fn emit_method_function(&mut self, value: &Expr) -> Result<(), SyntaxError> {
        self.expr(value)
    }

    /// The name a literal property key contributes.
    ///
    /// String keys are used as written. A numeric key is converted the way
    /// `ToString` would convert it, because the atom is made from that spelling:
    /// `{ 1e2: x }` and `{ 0x10: x }` both become `define_field 100` / `16`.
    fn property_key_text(&self, key: &PropKey, span: Span) -> Result<String, SyntaxError> {
        match key {
            PropKey::Str(text) => Ok(text.clone()),
            PropKey::Num(value) => numeric_key_text(*value, span),
        }
    }

    /// Compile an optional chain.
    ///
    /// `?.` short-circuits the *whole remaining spine*, not just its own link:
    /// `a?.b.c` yields `undefined` when `a` is null, it does not read `.c` of
    /// `undefined`. So the chain is compiled with one end label and every guard
    /// jumps to it. The parser hands us the extent of that spine as an
    /// [`ExprKind::OptionalChain`] wrapper around the outermost link, which is
    /// exactly the scope the label has to cover.
    fn optional_chain(&mut self, inner: &Expr) -> Result<(), SyntaxError> {
        let end = self.new_label();
        self.chain_link(inner, end)?;
        self.place_label(end);
        Ok(())
    }

    /// A member access or call reached from outside any optional chain.
    ///
    /// Such a link cannot contain a `?.` — if it did, the parser would have wrapped
    /// it in [`ExprKind::OptionalChain`] — so there is no guard and no label; one is
    /// still created because [`Self::chain_link`] takes it, and placing it at the
    /// end is a no-op that keeps a single code path.
    fn chain_link_top(&mut self, expr: &Expr) -> Result<(), SyntaxError> {
        let end = self.new_label();
        self.chain_link(expr, end)?;
        self.place_label(end);
        Ok(())
    }

    /// One link of a chain, where `end` is the label a `?.` guard jumps to.
    fn chain_link(&mut self, expr: &Expr, end: Label) -> Result<(), SyntaxError> {
        match &expr.kind {
            ExprKind::Member {
                object,
                property,
                optional,
            } => {
                self.chain_link(object, end)?;
                if *optional {
                    self.optional_guard(end);
                }
                match property {
                    // `.length` has its own opcode. The reference emits it for every
                    // object, not just arrays, so a name-based rule is right and
                    // cheaper than the generic field read.
                    MemberProp::Ident(name) if name.name == "length" => {
                        self.emit(Opcode::GetLength, &[]);
                    }
                    MemberProp::Ident(name) => {
                        self.emit(Opcode::GetField, &[Op::Atom(Atom::from_text(&name.name))]);
                    }
                    MemberProp::Computed(index) => {
                        self.expr(index)?;
                        self.emit(Opcode::GetArrayEl, &[]);
                    }
                    MemberProp::Private(_) => {
                        return Err(SyntaxError::new(
                            "private fields are not supported by the compiler yet",
                            expr.span,
                        ));
                    }
                }
                Ok(())
            }
            ExprKind::Call {
                callee,
                arguments,
                optional,
            } => {
                // `o.m(args)` has to pass `o` as `this`, which a plain `call`
                // cannot: it reads the function and then calls it with no
                // receiver at all. The reference's answer is to keep the object
                // under the function value — `get_field2` reads a field and leaves
                // its base in place — and to use `call_method`, which takes the
                // receiver from the stack. Missing this does not just cost an
                // instruction, it loses the `this` binding.
                if let ExprKind::Member {
                    object,
                    property,
                    optional: member_optional,
                } = &callee.unparen().kind
                {
                    self.chain_link(object, end)?;
                    if *member_optional {
                        self.optional_guard(end);
                    }
                    match property {
                        MemberProp::Ident(name) => {
                            self.emit(Opcode::GetField2, &[Op::Atom(Atom::from_text(&name.name))])
                        }
                        MemberProp::Computed(index) => {
                            self.expr(index)?;
                            // The key is not needed after the read, so this is the
                            // two-output form: object, value.
                            self.emit(Opcode::GetArrayEl2, &[]);
                        }
                        MemberProp::Private(_) => {
                            return Err(SyntaxError::new(
                                "private fields are not supported by the compiler yet",
                                expr.span,
                            ));
                        }
                    }
                    if Self::has_spread(arguments) {
                        // Stack is `obj func`; the array goes on top, then `perm3`
                        // reorders to the `func this array` that `apply` wants.
                        self.argument_array(arguments)?;
                        self.emit(Opcode::Perm3, &[]);
                        self.emit(Opcode::Apply, &[Op::Int(0)]);
                        return Ok(());
                    }
                    for argument in arguments {
                        self.expr(argument)?;
                    }
                    self.emit(Opcode::CallMethod, &[Op::Int(arguments.len() as i32)]);
                    return Ok(());
                }
                self.chain_link(callee, end)?;
                if *optional {
                    self.optional_guard(end);
                }
                if Self::has_spread(arguments) {
                    // `func array -> func undefined array`, which is the plain-call
                    // form of `apply`: the receiver is `undefined`.
                    self.argument_array(arguments)?;
                    self.emit(Opcode::Undefined, &[]);
                    self.emit(Opcode::Swap, &[]);
                    self.emit(Opcode::Apply, &[Op::Int(0)]);
                    return Ok(());
                }
                for argument in arguments {
                    self.expr(argument)?;
                }
                self.emit(Opcode::Call, &[Op::Int(arguments.len() as i32)]);
                Ok(())
            }
            _ => self.expr(expr),
        }
    }

    /// Compile `expr` as a tail call when it is one, reporting whether it was.
    ///
    /// `tail_call` reuses the current frame instead of pushing a new one, so a
    /// tail-recursive function runs in constant stack space. It also has to be the
    /// *last* thing the function does, which is why only a `return` reaches here.
    fn expr_if_tail_call(&mut self, expr: &Expr) -> Result<bool, SyntaxError> {
        let ExprKind::Call {
            callee,
            arguments,
            optional: false,
        } = &expr.unparen().kind
        else {
            return Ok(false);
        };
        let end = self.new_label();
        if let ExprKind::Member {
            object,
            property,
            optional: false,
        } = &callee.unparen().kind
        {
            self.chain_link(object, end)?;
            match property {
                MemberProp::Ident(name) => {
                    self.emit(Opcode::GetField2, &[Op::Atom(Atom::from_text(&name.name))])
                }
                MemberProp::Computed(index) => {
                    self.expr(index)?;
                    self.emit(Opcode::GetArrayEl2, &[]);
                }
                MemberProp::Private(_) => return Ok(false),
            }
            for argument in arguments {
                self.expr(argument)?;
            }
            self.emit(Opcode::TailCallMethod, &[Op::Int(arguments.len() as i32)]);
            self.place_label(end);
            return Ok(true);
        }
        self.chain_link(callee, end)?;
        for argument in arguments {
            self.expr(argument)?;
        }
        self.emit(Opcode::TailCall, &[Op::Int(arguments.len() as i32)]);
        self.place_label(end);
        Ok(true)
    }

    /// The `?.` guard: keep the value if it is neither null nor undefined,
    /// otherwise replace it with `undefined` and leave the whole chain.
    fn optional_guard(&mut self, end: Label) {
        let cont = self.new_label();
        self.emit(Opcode::Dup, &[]);
        self.emit(Opcode::IsUndefinedOrNull, &[]);
        self.emit_jump(Opcode::IfFalse, cont, &[]);
        self.emit(Opcode::Drop, &[]);
        self.emit(Opcode::Undefined, &[]);
        self.emit_jump(Opcode::Goto, end, &[]);
        self.place_label(cont);
    }

    fn push_number(&mut self, value: f64) {
        // `push_N` / `push_i8` / `push_i32` cover small integers; everything else
        // goes into the constant pool.
        if value == value.trunc() && value.abs() <= i32::MAX as f64 {
            let int = value as i32;
            if (-1..=7).contains(&int) {
                self.emit(Opcode::PushI32, &[Op::Int(int)]);
                return;
            }
            if (-128..=127).contains(&int) {
                self.emit(Opcode::PushI8, &[Op::Int(int)]);
                return;
            }
            if (-32768..=32767).contains(&int) {
                self.emit(Opcode::PushI16, &[Op::Int(int)]);
                return;
            }
            self.emit(Opcode::PushI32, &[Op::Int(int)]);
            return;
        }
        let index = self.push_constant(Constant::Number(value));
        self.emit(Opcode::PushConst, &[Op::Const(index)]);
    }

    fn push_string(&mut self, text: &str) {
        if text.is_empty() {
            self.emit(Opcode::PushEmptyString, &[]);
            return;
        }
        self.emit(Opcode::PushAtomValue, &[Op::Atom(Atom::from_text(text))]);
    }

    /// Name an anonymous function from the binding it is assigned to.
    ///
    /// `var f = function(){}` produces a function whose `name` is `f`, which the
    /// runtime cannot know without being told: the function object is created
    /// before the store. `set_name` carries the name across.
    fn set_name_if_anonymous(&mut self, value: &Expr, name: &str) {
        if value.is_anonymous_function_definition() {
            self.emit(Opcode::SetName, &[Op::Atom(Atom::from_text(name))]);
        }
    }

    /// The same, for a computed key: the name comes from the value on the stack
    /// rather than from an atom, so a different instruction carries it.
    fn set_name_if_anonymous_computed(&mut self, value: &Expr) {
        if value.is_anonymous_function_definition() {
            self.emit(Opcode::SetNameComputed, &[]);
        }
    }

    fn push_this(&mut self) {
        self.emit(Opcode::PushThis, &[]);
    }

    fn push_constant(&mut self, constant: Constant) -> u32 {
        let state = self.state();
        let index = state.cpool.len() as u32;
        state.cpool.push(constant);
        index
    }

    /// Compile `a && b`, `a || b`, `a ?? b`.
    ///
    /// A run of the *same* operator shares one end label, so `a || b || c` tests
    /// `a` once and jumps past everything when it is truthy, instead of falling
    /// into `b`'s test. The parser builds such a run left-associated —
    /// `((a || b) || c)` — so the operands are collected off the left spine.
    ///
    /// A *different* operator starts its own chain: `(a || b) && c` keeps the inner
    /// `||`'s label, because the inner chain really does end before `&&` begins.
    fn logical(&mut self, op: LogicalOp, left: &Expr, right: &Expr) -> Result<(), SyntaxError> {
        let mut operands = Vec::new();
        collect_same_operator(op, left, &mut operands);
        operands.push(right);

        let end = self.new_label();
        // Every operand but the last decides whether the chain stops here.
        let last = operands.pop().expect("the right operand is always present");
        for operand in operands {
            self.expr(operand)?;
            self.emit(Opcode::Dup, &[]);
            match op {
                LogicalOp::And => self.emit_jump(Opcode::IfFalse, end, &[]),
                LogicalOp::Or => self.emit_jump(Opcode::IfTrue, end, &[]),
                // `??` cannot reuse the truthiness jumps: `0 ?? b` is `0` while
                // `0 || b` is `b`. The dedicated test keeps the value unless it is
                // null-ish, so the jump is taken on `false`.
                LogicalOp::Coalesce => {
                    self.emit(Opcode::IsUndefinedOrNull, &[]);
                    self.emit_jump(Opcode::IfFalse, end, &[]);
                }
            }
            self.emit(Opcode::Drop, &[]);
        }
        self.expr(last)?;
        self.place_label(end);
        Ok(())
    }

    fn unary(&mut self, op: UnaryOp, argument: &Expr) -> Result<(), SyntaxError> {
        match op {
            UnaryOp::Typeof => {
                // `typeof x` must not throw on an undeclared name.
                if let ExprKind::Ident(name) = &argument.kind {
                    self.emit_var(VarAction::GetUndef, &name.name);
                } else {
                    self.expr(argument)?;
                }
                self.emit(Opcode::Typeof, &[]);
            }
            UnaryOp::Delete => {
                // `delete obj.prop` really deletes, and answers whether it could.
                // Deleting a *binding* is always `false`: a local, an argument and a
                // declared global are all non-configurable, which the reference encodes
                // as a constant `push_false` rather than a runtime check. Only a
                // property access has anything to do at run time.
                match &argument.kind {
                    ExprKind::Member {
                        object, property, ..
                    } => {
                        self.expr(object)?;
                        match property {
                            MemberProp::Ident(name) => {
                                self.emit(
                                    Opcode::PushAtomValue,
                                    &[Op::Atom(Atom::from_text(&name.name))],
                                );
                            }
                            MemberProp::Computed(index) => self.expr(index)?,
                            MemberProp::Private(_) => {
                                return Err(SyntaxError::new(
                                    "deleting a private field is not allowed",
                                    argument.span,
                                ));
                            }
                        }
                        self.emit(Opcode::Delete, &[]);
                    }
                    ExprKind::Ident(name) => {
                        // `delete name`: a declared global, a local and an argument all
                        // answer `false`, but an *implicit* global (`x = 1` with no
                        // declaration) is a configurable property of the global object
                        // and really goes away. Which of those a name is only the
                        // resolver knows, so the decision waits for it.
                        let pos = self.cur().cur_pos;
                        self.state().insns.push(Insn::DeleteVar {
                            name: name.name.clone(),
                        });
                        self.state().insn_pos.push(pos);
                    }
                    _ => self.emit(Opcode::PushFalse, &[]),
                }
            }
            UnaryOp::Void => {
                self.expr(argument)?;
                self.emit(Opcode::Drop, &[]);
                self.emit(Opcode::Undefined, &[]);
            }
            UnaryOp::Minus => {
                self.expr(argument)?;
                self.emit(Opcode::Neg, &[]);
            }
            UnaryOp::Plus => {
                self.expr(argument)?;
                self.emit(Opcode::Plus, &[]);
            }
            UnaryOp::Not => {
                self.expr(argument)?;
                self.emit(Opcode::Lnot, &[]);
            }
            UnaryOp::BitNot => {
                self.expr(argument)?;
                self.emit(Opcode::Not, &[]);
            }
        }
        Ok(())
    }

    /// Compile an assignment.
    ///
    /// Three operator families, three shapes:
    ///
    /// * **plain** `=` — evaluate, store. A member needs `insert2`/`insert3` when
    ///   the value is kept, because `put_field`/`put_array_el` consume both the
    ///   object and the value: `insert2` is `obj a -> a obj a`, so the store sees
    ///   the pair it wants and leaves a copy on top.
    /// * **compound** `+=` … — read the place, apply the operator, store. A member
    ///   reads through `get_field2`/`get_array_el3`, which leave the object (and
    ///   index) underneath the value precisely so the store can follow.
    /// * **logical** `||=`, `&&=`, `??=` — read, test, and only then evaluate the
    ///   right-hand side. The test *keeps* the current value and skips the store
    ///   entirely, which is the whole point: `a ||= b` must not evaluate `b` when
    ///   `a` is already truthy.
    fn assign(&mut self, op: AssignOp, target: &Pattern, value: &Expr) -> Result<(), SyntaxError> {
        // Reading the flag clears it: only the outermost assignment of an
        // expression statement may drop its value.
        let keep = !std::mem::take(&mut self.state().discard_value);
        match &target.kind {
            PatternKind::Ident(name) => {
                // A *simple* store is blamed on the identifier, not on the `=`: a
                // strict-mode `not_def = 1` raises when the name is written, which the
                // reference reports at the name's column. A compound store is blamed on
                // the operator instead (`a += poisoned` raises inside `ToPrimitive`,
                // which the reference reports at the `+=`), so only the simple case
                // moves the position.
                let saved = self.cur().cur_pos;
                let name_pos = Some(name.span.start);
                if matches!(op, AssignOp::Assign) {
                    self.state().cur_pos = name_pos;
                }
                // A *compound* store reads the binding before it combines, and that read
                // is blamed on the identifier: `not_def += 2` fails while reading
                // `not_def`, so the position is the name's, not the `+=`'s. The
                // combination that follows is blamed on the operator, which is why
                // `cur_pos` is put back to the operator position before it is emitted.
                self.state().cur_pos = name_pos;
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                        self.set_name_if_anonymous(value, &name.name);
                        // A discarded store to a *slot* consumes its value, so no copy
                        // is needed. A global store goes through `put_var`, which the
                        // reference always pairs with a `dup` — matching it keeps the
                        // bytecode identical. Getting this wrong is not only an extra
                        // instruction: a stray copy gives the peephole a chance to fold
                        // the store into an unrelated later read.
                        let slot = self.is_slot(&name.name);
                        if keep || !slot {
                            self.emit(Opcode::Dup, &[]);
                        }
                        self.state().no_value = !keep && slot;
                        self.emit_var(VarAction::Put, &name.name);
                    }
                    AssignOp::And | AssignOp::Or | AssignOp::Coalesce => {
                        let end = self.new_label();
                        self.emit_var(VarAction::Get, &name.name);
                        self.state().cur_pos = saved;
                        self.emit(Opcode::Dup, &[]);
                        self.emit_logical_test(op);
                        self.emit_jump(logical_jump(op), end, &[]);
                        self.emit(Opcode::Drop, &[]);
                        self.expr(value)?;
                        self.emit(Opcode::Dup, &[]);
                        self.emit_var(VarAction::Put, &name.name);
                        self.place_label(end);
                    }
                    _ => {
                        self.emit_var(VarAction::Get, &name.name);
                        self.expr(value)?;
                        self.state().cur_pos = saved;
                        self.emit(compound_opcode(op), &[]);
                        self.emit(Opcode::Dup, &[]);
                        self.emit_var(VarAction::Put, &name.name);
                    }
                }
                self.state().cur_pos = saved;
                Ok(())
            }
            PatternKind::Member(member) => {
                // A store is blamed on the selector punctuator (`.` or `[`), the same
                // token a load is blamed on — the reference records `put_field obj.a`
                // at the `.`, so a setter that throws reports that column and not the
                // `=` of the assignment it sits in. The inner expressions restore their
                // own positions, so the selector's sticks for the store instructions.
                let saved = self.cur().cur_pos;
                self.state().cur_pos = member.op_pos.or(Some(member.span.start));
                self.expr(&member.object)?;
                match &member.property {
                    MemberProp::Ident(property) => {
                        let atom = Atom::from_text(&property.name);
                        match op {
                            AssignOp::Assign => {
                                self.expr(value)?;
                                // `put_field` consumes the object and the value, so
                                // without `insert2` nothing is left over for the
                                // statement to drop.
                                self.state().no_value = !keep;
                                if keep {
                                    self.emit(Opcode::Insert2, &[]);
                                }
                                self.emit(Opcode::PutField, &[Op::Atom(atom.clone())]);
                            }
                            AssignOp::And | AssignOp::Or | AssignOp::Coalesce => {
                                // `get_field2` leaves the object under the value, so
                                // the keep path has an object to dispose of. The store
                                // path must jump *over* that cleanup: falling into it
                                // would `nip` a stack that only holds the value.
                                let (keep_label, end) = (self.new_label(), self.new_label());
                                self.emit(Opcode::GetField2, &[Op::Atom(atom.clone())]);
                                self.emit(Opcode::Dup, &[]);
                                self.emit_logical_test(op);
                                self.emit_jump(logical_jump(op), keep_label, &[]);
                                self.emit(Opcode::Drop, &[]);
                                self.expr(value)?;
                                self.emit(Opcode::Insert2, &[]);
                                self.emit(Opcode::PutField, &[Op::Atom(atom.clone())]);
                                self.emit_jump(Opcode::Goto, end, &[]);
                                self.place_label(keep_label);
                                self.emit(Opcode::Nip, &[]);
                                self.place_label(end);
                            }
                            _ => {
                                self.emit(Opcode::GetField2, &[Op::Atom(atom.clone())]);
                                self.expr(value)?;
                                self.emit(compound_opcode(op), &[]);
                                self.state().no_value = !keep;
                                if keep {
                                    self.emit(Opcode::Insert2, &[]);
                                }
                                self.emit(Opcode::PutField, &[Op::Atom(atom.clone())]);
                            }
                        }
                    }
                    MemberProp::Computed(index) => {
                        self.expr(index)?;
                        match op {
                            AssignOp::Assign => {
                                self.expr(value)?;
                                self.state().no_value = !keep;
                                if keep {
                                    self.emit(Opcode::Insert3, &[]);
                                }
                                self.emit(Opcode::PutArrayEl, &[]);
                            }
                            AssignOp::And | AssignOp::Or | AssignOp::Coalesce => {
                                let (keep_label, end) = (self.new_label(), self.new_label());
                                self.emit(Opcode::GetArrayEl3, &[]);
                                self.emit(Opcode::Dup, &[]);
                                self.emit_logical_test(op);
                                self.emit_jump(logical_jump(op), keep_label, &[]);
                                self.emit(Opcode::Drop, &[]);
                                self.expr(value)?;
                                self.emit(Opcode::Insert3, &[]);
                                self.emit(Opcode::PutArrayEl, &[]);
                                self.emit_jump(Opcode::Goto, end, &[]);
                                self.place_label(keep_label);
                                // Both the object and the index are left over.
                                self.emit(Opcode::Nip, &[]);
                                self.emit(Opcode::Nip, &[]);
                                self.place_label(end);
                            }
                            _ => {
                                self.emit(Opcode::GetArrayEl3, &[]);
                                self.expr(value)?;
                                self.emit(compound_opcode(op), &[]);
                                self.state().no_value = !keep;
                                if keep {
                                    self.emit(Opcode::Insert3, &[]);
                                }
                                self.emit(Opcode::PutArrayEl, &[]);
                            }
                        }
                    }
                    MemberProp::Private(_) => {
                        return Err(SyntaxError::new(
                            "private fields are not supported by the compiler yet",
                            member.span,
                        ));
                    }
                }
                self.state().cur_pos = saved;
                Ok(())
            }
            _ => Err(SyntaxError::new(
                "this assignment target is not supported by the compiler yet",
                target.span,
            )),
        }
    }

    /// The test a logical assignment applies to the current value.
    ///
    /// `??=` needs a real opcode rather than a truthiness jump: `0 ??= b` must
    /// assign, while `0 ||= b` must not.
    fn emit_logical_test(&mut self, op: AssignOp) {
        if op == AssignOp::Coalesce {
            self.emit(Opcode::IsUndefinedOrNull, &[]);
        }
    }

    fn update(&mut self, op: UpdateOp, argument: &Expr, prefix: bool) -> Result<(), SyntaxError> {
        let ExprKind::Ident(name) = &argument.kind else {
            return Err(SyntaxError::new(
                "increment of a non-variable is not supported by the compiler yet",
                argument.span,
            ));
        };
        // A discarded `i++` keeps the plain `get`/`post_inc`/`put` shape. The
        // reference's `inc_loc` folds those three into one, but it does so only in some
        // positions (a `for` update and not a bare statement, and never for a lexical
        // binding), so emitting it unconditionally would move *away* from its output.
        // The instruction is equivalent either way; matching the reference matters more
        // than saving two instructions.
        let _ = !std::mem::take(&mut self.state().discard_value);
        self.emit_var(VarAction::Get, &name.name);
        let instruction = match (op, prefix) {
            (UpdateOp::Increment, true) => Opcode::Inc,
            (UpdateOp::Increment, false) => Opcode::PostInc,
            (UpdateOp::Decrement, true) => Opcode::Dec,
            (UpdateOp::Decrement, false) => Opcode::PostDec,
        };
        self.emit(instruction, &[]);
        if prefix {
            self.emit(Opcode::Dup, &[]);
        }
        self.emit_var(VarAction::Put, &name.name);
        Ok(())
    }

    /// Compile an array literal.
    ///
    /// The reference splits this into three phases, and the split is load-bearing
    /// rather than an optimisation detail:
    ///
    /// 1. **A leading run of plain elements** becomes `array_from n`, which builds
    ///    the array in one instruction.
    /// 2. **Everything up to the first spread** is appended with explicit indices
    ///    (`define_field <index>`), because a hole has no element to pass to
    ///    `array_from`. A hole here is what makes `length` need fixing up: an
    ///    explicit index does not extend the array the way a pushed element does.
    /// 3. **From the first spread on** the index lives on the stack, so `append` and
    ///    `define_array_el` can consume it and `inc` advances it.
    ///
    /// So `[1,,3]` and `[1,2,3]` are not two spellings of one program: the hole
    /// forces phase 2, and `[...a]` forces phase 3.
    fn array_literal(
        &mut self,
        elements: &[Option<ArrayElement>],
        span: Span,
    ) -> Result<(), SyntaxError> {
        let _ = span;
        let mut index: u32 = 0;
        let mut cursor = 0usize;

        // Phase 1: a leading run of plain elements, at most 32 of them.
        while let Some(Some(ArrayElement::Expr(element))) = elements.get(cursor) {
            if index >= 32 {
                break;
            }
            self.expr(element)?;
            index += 1;
            cursor += 1;
        }
        self.emit(Opcode::ArrayFrom, &[Op::Int(index as i32)]);

        // Phase 2: explicit indices, up to the first spread.
        let mut need_length = false;
        while let Some(element) = elements.get(cursor) {
            if matches!(element, Some(ArrayElement::Spread(_))) {
                break;
            }
            need_length = true;
            if let Some(ArrayElement::Expr(expression)) = element {
                self.expr(expression)?;
                self.emit_define_index(index);
                need_length = false;
            }
            index += 1;
            cursor += 1;
        }

        if cursor >= elements.len() {
            if need_length {
                // `define_field` cannot set `length`, which is not configurable.
                self.emit(Opcode::Dup, &[]);
                self.push_number(index as f64);
                self.emit(Opcode::PutField, &[Op::Atom(Atom::from_text("length"))]);
            }
            return Ok(());
        }

        // Phase 3: the index moves onto the stack for the spreading tail.
        //
        // `need_length` deliberately carries over from phase 2: a hole before the
        // first spread still has to be reflected in `length`, and only the final
        // check can do it because the array is not finished until the tail is
        // appended.
        self.push_number(index as f64);
        while let Some(element) = elements.get(cursor) {
            match element {
                Some(ArrayElement::Spread(expression)) => {
                    // `append` copies an iterable and leaves the next index behind,
                    // so no `inc` is needed here.
                    self.expr(spread_argument(expression))?;
                    self.emit(Opcode::Append, &[]);
                }
                None | Some(ArrayElement::Expr(_)) => {
                    need_length = true;
                    if let Some(ArrayElement::Expr(expression)) = element {
                        self.expr(expression)?;
                        self.emit(Opcode::DefineArrayEl, &[]);
                        need_length = false;
                    }
                    self.emit(Opcode::Inc, &[]);
                }
            }
            cursor += 1;
        }
        if need_length {
            self.emit(Opcode::Dup1, &[]);
            self.emit(Opcode::PutField, &[Op::Atom(Atom::from_text("length"))]);
        } else {
            self.emit(Opcode::Drop, &[]);
        }
        Ok(())
    }

    /// `define_field` with an array-index atom, which is how phase 2 stores elements.
    fn emit_define_index(&mut self, index: u32) {
        self.emit(
            Opcode::DefineField,
            &[Op::Atom(Atom::from_text(&index.to_string()))],
        );
    }

    // -- functions ---------------------------------------------------------

    fn compile_function(&mut self, func: &ast::Function) -> Result<CompiledFunction, SyntaxError> {
        self.compile_function_with_name(func, false)
    }

    /// Compile a function, recording whether it is a *named function expression*.
    ///
    /// `var f = function g(){ return g }` has to see `g` inside its own body, but `g`
    /// must not leak outside. The reference does that by declaring `g` as a local of
    /// the function itself and initialising it with `special_object 2` (the current
    /// function), which is why this has to be known at compile time.
    fn compile_function_with_name(
        &mut self,
        func: &ast::Function,
        is_expression: bool,
    ) -> Result<CompiledFunction, SyntaxError> {
        if func.is_generator || func.is_async {
            return Err(SyntaxError::new(
                "generators and async functions are not supported by the compiler yet",
                func.span,
            ));
        }
        let kind = FunctionKind::Normal;
        let mut state = FnState::new(
            func.name
                .as_ref()
                .map(|n| n.name.clone())
                .unwrap_or_default(),
            kind,
        );
        state.strict = self.cur().strict;
        state.is_arrow = func.is_arrow;
        // A named *expression* binds its own name; a declaration's name is already
        // bound in the enclosing scope.
        state.self_name = if is_expression && !func.is_arrow {
            func.name.as_ref().map(|n| n.name.clone())
        } else {
            None
        };
        state.line = func.span.start.line;
        state.column = func.span.start.column;
        // A function body has no hidden `<ret>` local; the value comes back on
        // the stack. Only a script keeps one.
        for param in &func.params {
            let PatternKind::Ident(name) = &param.kind else {
                return Err(SyntaxError::new(
                    "destructuring parameters are not supported by the compiler yet",
                    param.span,
                ));
            };
            state.args.push(VarDef::var(name.name.clone()));
        }
        let depth = self.stack.len();
        self.stack.push(state);
        let result = self.compile_function_body(func);
        if result.is_err() {
            // A failed compile must not leave its `FnState` behind. Every caller
            // reads `self.state()` as "the function being compiled", so a leftover
            // frame silently retargets all later bookkeeping at a dead function.
            self.stack.truncate(depth);
        }
        result
    }

    /// The body of a function, with its frame already pushed.
    fn compile_function_body(
        &mut self,
        func: &ast::Function,
    ) -> Result<CompiledFunction, SyntaxError> {
        let body_key = func.params_span.end.offset as u64;
        self.push_block(body_key, NO_PARENT);
        self.state().body_block = body_key;
        // An arrow with an expression body has no statements; the expression is
        // its `return`. Building the statement here keeps one code path below, and
        // matters for correctness beyond convenience: returning early with the new
        // `FnState` still on the stack would leave the compiler looking at the
        // wrong function, which surfaces as an unrelated underflow later.
        let concise = match &func.body {
            js_parse::ast::FunctionBody::Expr(value) => Some(Stmt::new(
                StmtKind::Return(Some(Box::new((**value).clone()))),
                value.span,
            )),
            js_parse::ast::FunctionBody::Block(_) => None,
        };
        let body: &[Stmt] = match &concise {
            Some(stmt) => std::slice::from_ref(stmt),
            None => func
                .body
                .statements()
                .ok_or_else(|| SyntaxError::new("unsupported function body", func.span))?,
        };
        self.hoist(body, body_key)?;
        self.emit_scope_enter(body_key);
        for stmt in body {
            self.stmt(stmt)?;
            if Self::terminates(stmt) {
                break;
            }
        }
        // The implicit return only exists if control can actually get here: an
        // infinite loop never falls off the end.
        if self.end_is_reachable() {
            self.emit(Opcode::ReturnUndef, &[]);
        }
        // Resolution runs while this function is still on the stack: capturing a
        // variable from an enclosing function means reaching into that
        // function's state, which requires the whole chain to be present.
        Ok(self.finish_current())
    }

    /// Whether the current position in the instruction stream can be reached.
    ///
    /// A walk over the control-flow graph, using labels as instruction indices.
    /// The reference does the same thing to decide whether an implicit
    /// `return_undef` is needed.
    fn end_is_reachable(&self) -> bool {
        let state = self.cur();
        reachable_instructions(state, state.insns.len())[state.insns.len()]
    }

    /// Resolve and assemble the function on top of the compiler stack, then pop
    /// it.
    fn finish_current(&mut self) -> CompiledFunction {
        self.provide_arguments();
        self.provide_self_name();
        let (mut resolved, resolved_pos) = resolve(&mut self.stack);
        // Captured bindings need a reference cell; the count goes in the header so
        // the runtime knows how many cells to allocate on entry.
        let current = self.stack.len() - 1;
        let captured = self.stack[current]
            .args
            .iter()
            .filter(|a| a.is_captured)
            .count()
            + self.stack[current]
                .vars
                .iter()
                .filter(|v| v.is_captured)
                .count();
        self.stack[current].var_ref_count = captured as u32;
        // The scope chain is only final now that every binding is known.
        rebuild_scope_chain(&mut self.stack[current]);
        let (code, stack_size, positions) =
            assemble(&mut resolved, &resolved_pos, &mut self.stack[current]);
        let _ = &mut resolved;
        let state = self.stack.pop().expect("a function is being compiled");
        CompiledFunction {
            name: state.name,
            kind: state.kind,
            strict: state.strict,
            args: state.args,
            vars: state.vars,
            closure_vars: state.closure_vars,
            cpool: state.cpool,
            atoms: state.atoms,
            code,
            stack_size,
            var_ref_count: state.var_ref_count,
            positions,
            source: None,
            line: state.line,
            column: state.column,
            filename: None,
        }
    }
}

/// Assign each binding a scope level and rebuild the scope chains.
///
/// Levels are numbered in the order the blocks appear, with 0 reserved for the
/// function scope where `var` and hoisted functions live. Every binding outside
/// level 0 gets a `scope_next` link to the next binding visible from the same
/// point, so walking a block's chain enumerates exactly the bindings live there.
///
/// The links cannot be filled in while hoisting: `{ let a } let b` needs `a` to
/// point at `b`, which is declared later in the source but earlier in the chain.
fn rebuild_scope_chain(state: &mut FnState) {
    // Block key -> level. Level 0 is the function scope, keyed by the sentinel so a
    // real block that happens to start at offset zero keeps its own level instead of
    // being merged into the function scope.
    let mut levels: Vec<u64> = vec![FUNCTION_SCOPE];
    for block in &state.blocks {
        if !levels.contains(&block.key) {
            levels.push(block.key);
        }
    }
    let level_of = |key: u64| -> usize { levels.iter().position(|k| *k == key).unwrap_or(0) };
    let parent_level = |level: usize| -> usize {
        if level == 0 {
            return 0;
        }
        let key = levels[level];
        let parent = state
            .blocks
            .iter()
            .find(|b| b.key == key)
            .map(|b| b.parent)
            .unwrap_or(FUNCTION_SCOPE);
        level_of(parent)
    };

    // Own bindings per level, in declaration order.
    let mut own: Vec<Vec<usize>> = vec![Vec::new(); levels.len()];
    let keys: Vec<u64> = (0..state.vars.len())
        .map(|index| {
            state
                .var_block
                .get(index)
                .copied()
                .unwrap_or(FUNCTION_SCOPE)
        })
        .collect();
    for (index, key) in keys.into_iter().enumerate() {
        let level = level_of(key);
        state.vars[index].scope_level = level as u32;
        // A `catch` parameter is part of the runtime scope chain even though it is
        // not lexical: the handler's own bindings chain on to it, which is why the
        // reference prints `let y [next:1]` with `e` at 1.
        if state.vars[index].is_lexical || state.vars[index].kind == crate::bytecode::VarKind::Catch
        {
            own[level].push(index);
        }
    }

    // A scope's chain is its own bindings in reverse declaration order followed
    // by the enclosing scope's chain.
    let mut head = vec![-1i32; levels.len()];
    for level in 0..levels.len() {
        let inherited = if level == 0 {
            -1
        } else {
            head[parent_level(level)]
        };
        let chain: Vec<usize> = own[level].iter().rev().copied().collect();
        head[level] = chain.first().map_or(inherited, |first| *first as i32);
        for (position, &index) in chain.iter().enumerate() {
            let next = chain
                .get(position + 1)
                .map_or(inherited, |next| *next as i32);
            state.vars[index].scope_next = if next < 0 { None } else { Some(next) };
        }
    }
    state.block_heads = head;
}

/// This is where names become slots and where the temporal dead zone shows up in
/// the instruction stream: a read of a `let`/`const` binding compiles to
/// `get_loc_check`, which throws if the binding is still uninitialised.
///
/// The peephole pass from the reference implementation is applied at the end,
/// for the same reason it is applied to resolved bytecode there: the patterns are
/// stated in terms of `put_loc`/`set_loc` pairs, which only exist after
/// resolution. Deleted instructions stay in the list as `None` so that label
/// indices remain valid; [`Assembler`] maps them to byte offsets.
/// A resolved instruction and the source position it came from.
type ResolvedInsn = Option<(Opcode, Vec<Op>)>;

/// A resolved instruction paired with its position, which is what `assemble` consumes.
type PositionedInsn = (Opcode, Vec<Op>);

fn resolve(stack: &mut [FnState]) -> (Vec<ResolvedInsn>, Vec<Option<Position>>) {
    let current = stack.len() - 1;
    // Dead-code elimination, on the symbolic form where labels are still
    // instruction indices. A `break` inside a loop leaves the trailing `goto`
    // unreachable, and a `return` leaves everything after it unreachable; the
    // reference drops both. Doing it here means the emitter never has to think
    // about reachability.
    strip_empty_gosubs(stack, current);
    eliminate_dead_code(stack, current);
    let insns = std::mem::take(&mut stack[current].insns);
    // One entry per instruction, carried alongside: `insn_pos` is index-aligned with
    // `insns`, and every arm below pushes exactly one instruction, so the positions
    // stay aligned with `out`.
    let positions = std::mem::take(&mut stack[current].insn_pos);
    let mut out: Vec<Option<(Opcode, Vec<Op>)>> = Vec::with_capacity(insns.len());
    let mut out_pos: Vec<Option<Position>> = Vec::with_capacity(insns.len());
    for (index, insn) in insns.iter().enumerate() {
        let pos = positions.get(index).copied().flatten();
        out_pos.push(pos);
        match insn {
            Insn::Fixed { op, operands } => {
                let operands = operands
                    .iter()
                    .map(|operand| match operand {
                        Op::Atom(atom) => {
                            Op::AtomId(intern_atom(&mut stack[current], &atom.to_string()))
                        }
                        other => other.clone(),
                    })
                    .collect();
                out.push(Some((*op, operands)));
            }
            Insn::Var {
                action,
                name,
                block,
            } => {
                let slot = resolve_slot(stack, current, name, *block);
                out.push(Some(variable_instruction(stack, current, *action, slot)));
            }
            Insn::CloseCaptured { name, block } => {
                // The block matters: a loop variable is scoped to the `for` head's own
                // block, so resolving it against block 0 would miss it entirely and
                // silently emit nothing.
                let slot = resolve_slot(stack, current, name, *block);
                // Only a local with a reference cell has anything to close. A global
                // or a never-captured local resolves to no instruction at all, which
                // keeps the bytecode identical to the reference's.
                if let Slot::Loc(index) = slot
                    && stack[current]
                        .vars
                        .get(index as usize)
                        .is_some_and(|v| v.is_captured)
                {
                    out.push(Some((Opcode::CloseLoc, vec![Op::Slot(slot)])));
                } else {
                    out.push(None);
                }
            }
            Insn::DeleteVar { name } => {
                // A binding — a local, an argument, or a declared global — cannot be
                // deleted, and the reference encodes that as a constant `push_false`.
                // Only an implicit global needs the runtime lookup.
                if matches!(resolve_slot(stack, current, name, 0), Slot::Global(_)) {
                    let atom = intern_atom(&mut stack[current], name);
                    out.push(Some((Opcode::DeleteVar, vec![Op::AtomId(atom)])));
                } else {
                    out.push(Some((Opcode::PushFalse, Vec::new())));
                }
            }
            Insn::Dead => out.push(None),
        }
    }
    // Which label (if any) sits on each instruction, so the jump patterns can
    // tell whether they would cross a jump target.
    let mut label_at: Vec<Option<u32>> = vec![None; out.len() + 1];
    for (id, placed) in stack[current].labels.iter().enumerate() {
        if let Some(index) = *placed
            && index < label_at.len()
            && label_at[index].is_none()
        {
            label_at[index] = Some(id as u32);
        }
    }
    if std::env::var_os("QJS_DEBUG").is_some() {
        eprintln!("--- peephole entry ---");
        for (i, e) in out.iter().enumerate() {
            eprintln!("{i}: {e:?}");
        }
        eprintln!("label_at: {label_at:?}");
    }
    peephole(&mut out, &label_at, &stack[current].labels);
    (out, out_pos)
}

/// Find where `name` lives, capturing it in an enclosing function when needed.
///
/// The search runs outward through the enclosing functions' states, which are
/// still on the compiler stack while a nested function is resolved. Finding the
/// name in an enclosing function is what creates a closure reference, and every
/// function in between has to forward it — a function can only reach its own
/// parent's frame.
///
/// A name declared nowhere is a global. The reference does *not* look it up by
/// name from the inner function: the top-level function gets one `[global]`
/// entry and every nested function gets a `[global_ref<n>]` pointing at its
/// parent's entry, which is why `function f(){ g(); }` shows `[global_ref0]`
/// rather than `[global]`.
/// The lexical blocks enclosing `block`, innermost first.
fn block_chain(state: &FnState, block: u64) -> Vec<u64> {
    let mut chain = Vec::new();
    let mut current = Some(block);
    while let Some(key) = current {
        // `NO_PARENT` is not a block, so the walk ends here. Stopping on the sentinel
        // explicitly (rather than on "no block with this key") is what keeps a block
        // that happens to sit at offset zero from being mistaken for the terminator.
        if key == NO_PARENT || chain.contains(&key) {
            break;
        }
        chain.push(key);
        current = state.blocks.iter().find(|b| b.key == key).map(|b| b.parent);
    }
    chain
}

fn resolve_slot(stack: &mut [FnState], current: usize, name: &str, block: u64) -> Slot {
    if let Some(index) = stack[current].args.iter().position(|a| a.name == name) {
        return Slot::Arg(index as u32);
    }
    // A name can be bound more than once: two `catch` parameters in nested
    // handlers are two slots. The binding that wins is the one whose scope encloses
    // the access, and among those the innermost — so the blocks are walked outward
    // and the first binding found on the way is the one that counts. A binding in
    // no block (`var`) is reachable from anywhere, which is why the function scope
    // is the end of the walk.
    let chain = block_chain(&stack[current], block);
    for scope in &chain {
        let found = stack[current]
            .vars
            .iter()
            .enumerate()
            .find(|(index, var)| {
                var.name == name && stack[current].var_block.get(*index).copied() == Some(*scope)
            })
            .map(|(index, _)| index);
        if let Some(index) = found {
            return Slot::Loc(index as u32);
        }
    }
    // Function-scope bindings — `var` and function declarations — are recorded with
    // the sentinel block `0`, which no real scope uses, so the walk above cannot see
    // them. They are reachable from anywhere in the function, which is exactly what
    // the sentinel means.
    //
    // This pass is *only* for those. A lexical binding has to be found through the
    // chain or not at all, and looking a name up without its block is precisely how
    // `var x = 1; { let x = 2; } x` used to see the inner `x`, and how a `let` in a
    // block leaked out of it.
    let function_scoped = stack[current]
        .vars
        .iter()
        .enumerate()
        .find(|(index, var)| {
            var.name == name
                && stack[current].var_block.get(*index).copied() == Some(FUNCTION_SCOPE)
        })
        .map(|(index, _)| index);
    if let Some(index) = function_scoped {
        return Slot::Loc(index as u32);
    }
    if let Some(index) = stack[current]
        .closure_vars
        .iter()
        .position(|c| c.name == name)
    {
        return slot_for_closure(&stack[current].closure_vars[index], index as u32);
    }

    for owner in (0..current).rev() {
        let found = if let Some(index) = stack[owner].args.iter().position(|a| a.name == name) {
            Some(Found::Local(ClosureType::Arg, index as u32, false, false))
        } else if let Some(index) = stack[owner].vars.iter().position(|v| v.name == name) {
            let var = &stack[owner].vars[index];
            Some(Found::Local(
                ClosureType::Local,
                index as u32,
                var.is_const,
                var.is_lexical,
            ))
        } else if let Some(index) = stack[owner]
            .closure_vars
            .iter()
            .position(|c| c.name == name)
        {
            let closure = &stack[owner].closure_vars[index];
            Some(match closure.closure_type {
                ClosureType::Global
                | ClosureType::GlobalDecl
                | ClosureType::ModuleDecl
                | ClosureType::ModuleImport => Found::Global(index as u32),
                _ => Found::Local(
                    ClosureType::Ref,
                    index as u32,
                    closure.is_const,
                    closure.is_lexical,
                ),
            })
        } else {
            None
        };
        let Some(found) = found else { continue };

        let (is_const, is_lexical) = match found {
            Found::Local(_, _, is_const, is_lexical) => (is_const, is_lexical),
            Found::Global(_) => (false, false),
        };
        // Mark the owner's binding as captured so it moves into a reference cell.
        if let Found::Local(ClosureType::Arg | ClosureType::Local, index, _, _) = found {
            let slot = match found {
                Found::Local(ClosureType::Arg, _, _, _) => &mut stack[owner].args[index as usize],
                _ => &mut stack[owner].vars[index as usize],
            };
            slot.is_captured = true;
        }
        // Forward the reference through every function in between.
        let mut previous = match found {
            Found::Local(kind, index, _, _) => (kind, index),
            Found::Global(index) => (ClosureType::Global, index),
        };
        for level in owner + 1..=current {
            let kind = match (previous.0, level) {
                // A global stays a global at *every* level. Without `GlobalRef` in
                // this arm the second nested function over the same global got a
                // `Ref` to a cell no frame ever held, so the innermost closure read
                // `undefined` — which is how a two-deep getter lost its outer
                // variable.
                (ClosureType::Global | ClosureType::GlobalDecl | ClosureType::GlobalRef, _) => {
                    ClosureType::GlobalRef
                }
                (ClosureType::Arg | ClosureType::Local, level) if level == owner + 1 => previous.0,
                _ => ClosureType::Ref,
            };
            let index = ensure_closure(stack, level, name, is_const, is_lexical, kind, previous.1);
            previous = (kind, index);
        }
        let index = previous.1;
        return slot_for_closure(&stack[current].closure_vars[index as usize], index);
    }

    // Declared nowhere: a dynamic global lookup, which sloppy mode may satisfy by
    // creating a property. The top-level function holds the entry.
    let mut previous = (
        ClosureType::Global,
        ensure_closure(stack, 0, name, false, false, ClosureType::Global, 0),
    );
    for level in 1..=current {
        let index = ensure_closure(
            stack,
            level,
            name,
            false,
            false,
            ClosureType::GlobalRef,
            previous.1,
        );
        previous = (ClosureType::GlobalRef, index);
    }
    Slot::Global(previous.1)
}

/// What an outward search turned up.
#[derive(Debug, Clone, Copy)]
enum Found {
    /// A binding in an enclosing function, reachable through a closure of this
    /// type, with its const/lexical flags.
    Local(ClosureType, u32, bool, bool),
    /// A global, reached through the enclosing function's closure entry.
    Global(u32),
}

/// Add a closure entry to `level` if it is not there yet.
fn ensure_closure(
    stack: &mut [FnState],
    level: usize,
    name: &str,
    is_const: bool,
    is_lexical: bool,
    closure_type: ClosureType,
    var_idx: u32,
) -> u32 {
    if let Some(index) = stack[level]
        .closure_vars
        .iter()
        .position(|c| c.name == name)
    {
        return index as u32;
    }
    stack[level].closure_vars.push(ClosureVar {
        name: name.to_string(),
        is_const,
        is_lexical,
        closure_type,
        var_idx: var_idx as u16,
    });
    stack[level].closure_vars.len() as u32 - 1
}

fn slot_for_closure(closure: &ClosureVar, index: u32) -> Slot {
    match closure.closure_type {
        // A `global_ref` only says *how* the binding is found (through the
        // parent's closure table); the access itself is an ordinary global one.
        ClosureType::Global | ClosureType::GlobalRef => Slot::Global(index),
        ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport => {
            Slot::GlobalDecl(index)
        }
        _ => Slot::VarRef(index),
    }
}

/// Pick the opcode for one variable access.
///
/// The `_check` variants are what enforce the temporal dead zone. They are used
/// for every access to a `let`/`const` binding except its own initialisation —
/// which is exactly the operation that takes the binding out of the dead zone.
fn variable_instruction(
    stack: &[FnState],
    current: usize,
    action: VarAction,
    slot: Slot,
) -> (Opcode, Vec<Op>) {
    let lexical = match slot {
        Slot::Loc(index) => stack[current]
            .vars
            .get(index as usize)
            .is_some_and(|v| v.needs_check()),
        Slot::VarRef(index) => stack[current]
            .closure_vars
            .get(index as usize)
            .is_some_and(|c| c.is_lexical),
        _ => false,
    };
    let operand = vec![Op::Slot(slot)];
    match (action, slot) {
        (VarAction::Get, Slot::Loc(index)) => (
            if lexical {
                Opcode::GetLocCheck
            } else {
                Opcode::GetLoc
            },
            vec![Op::Slot(Slot::Loc(index))],
        ),
        (VarAction::Get, Slot::Arg(index)) => (Opcode::GetArg, vec![Op::Slot(Slot::Arg(index))]),
        (VarAction::Get, Slot::VarRef(index)) => (
            if lexical {
                Opcode::GetVarRefCheck
            } else {
                Opcode::GetVarRef
            },
            vec![Op::Slot(Slot::VarRef(index))],
        ),
        (VarAction::Get, Slot::Global(_)) => (Opcode::GetVar, operand),
        (VarAction::Get, Slot::GlobalDecl(_)) => (Opcode::GetVar, operand),
        (VarAction::GetUndef, Slot::Loc(_)) => (Opcode::GetLoc, operand),
        (VarAction::GetUndef, Slot::Arg(_)) => (Opcode::GetArg, operand),
        (VarAction::GetUndef, Slot::VarRef(_)) => (Opcode::GetVarRef, operand),
        (VarAction::GetUndef, Slot::Global(_) | Slot::GlobalDecl(_)) => {
            (Opcode::GetVarUndef, operand)
        }

        (VarAction::Put, Slot::Loc(_)) => (
            if lexical {
                Opcode::PutLocCheck
            } else {
                Opcode::PutLoc
            },
            operand,
        ),
        (VarAction::Put, Slot::Arg(_)) => (Opcode::PutArg, operand),
        (VarAction::Put, Slot::VarRef(_)) => (
            if lexical {
                Opcode::PutVarRefCheck
            } else {
                Opcode::PutVarRef
            },
            operand,
        ),
        (VarAction::Put, Slot::Global(_)) => (Opcode::PutVar, operand),
        (VarAction::Put, Slot::GlobalDecl(_)) => (Opcode::PutVar, operand),
        // Initialisation is what ends the dead zone, so it never checks.
        (VarAction::PutInit, Slot::Loc(_)) => (Opcode::PutLoc, operand),
        (VarAction::PutInit, Slot::Arg(_)) => (Opcode::PutArg, operand),
        (VarAction::PutInit, Slot::VarRef(_)) => (Opcode::PutVarRef, operand),
        (VarAction::PutInit, Slot::Global(_) | Slot::GlobalDecl(_)) => {
            (Opcode::PutVarInit, operand)
        }
        (VarAction::Set, Slot::Loc(_)) => (
            if lexical {
                Opcode::SetLocCheck
            } else {
                Opcode::SetLoc
            },
            operand,
        ),
        (VarAction::Set, Slot::Arg(_)) => (Opcode::SetArg, operand),
        (VarAction::Set, Slot::VarRef(_)) => (Opcode::SetVarRef, operand),
        (VarAction::Set, Slot::Global(_)) => (Opcode::PutVar, operand),
        (VarAction::Set, Slot::GlobalDecl(_)) => (Opcode::SetVarRef, operand),
        (VarAction::PutRef, Slot::Loc(_)) => (Opcode::PutLoc, operand),
        (VarAction::PutRef, Slot::Arg(_)) => (Opcode::PutArg, operand),
        (VarAction::PutRef, Slot::VarRef(_)) => (Opcode::PutVarRef, operand),
        (VarAction::PutRef, Slot::Global(_) | Slot::GlobalDecl(_)) => (Opcode::PutVarRef, operand),
    }
}

/// The instruction-rewriting pass.
///
/// These are the reference implementation's patterns, restricted to the ones the
/// constructs this compiler supports can produce. Each entry is a comment on the
/// bytecode it removes, because the whole point is that the result is one
/// instruction shorter than the literal translation of the AST.
fn peephole(
    insns: &mut [Option<(Opcode, Vec<Op>)>],
    label_at: &[Option<u32>],
    placed: &[Option<usize>],
) {
    fold_dup_patterns(insns, label_at);

    // `drop` before `return_undef` is dead: the return discards the whole frame
    // anyway. This runs *after* the `dup` folds, because a `dup; put; drop` there
    // consumes its own `drop`; removing the drop first would leave the `dup` and
    // produce a `set_` where the reference has a `put_`.
    for index in 0..insns.len() {
        if opcode_at(insns, index) != Some(Opcode::Drop) {
            continue;
        }
        // Skip over a run of drops, but stop at a jump target: removing code on
        // one side of a label would silently retarget the jump.
        let mut next = index + 1;
        while opcode_at(insns, next) == Some(Opcode::Drop) && !is_label_at(label_at, next) {
            next += 1;
        }
        if is_label_at(label_at, next) {
            continue;
        }
        if opcode_at(insns, next) == Some(Opcode::ReturnUndef) {
            for entry in &mut insns[index..next] {
                *entry = None;
            }
        }
    }

    // The rewrites below all decide against one fixed snapshot, which is what the
    // reference sees: `code_match` reads the bytecode buffer as it was on entry to
    // the pass, so a rule never observes what an earlier rule removed. Gathering
    // every deletion first and applying them together reproduces that.
    let snapshot: Vec<Option<Opcode>> = insns
        .iter()
        .map(|entry| entry.as_ref().map(|(op, _)| *op))
        .collect();
    // The next instruction that is still there. Holes left by dead-code
    // elimination are not instructions, so a rule looks past them.
    let next_op = |index: usize| -> Option<usize> {
        (index + 1..snapshot.len()).find(|position| snapshot[*position].is_some())
    };

    let mut doomed = vec![false; insns.len()];
    for index in 0..insns.len() {
        // A constant pushed only to be dropped does nothing at all. This is what a
        // comma expression with a literal left operand compiles to — `(0, f)()` — and
        // the reference removes the pair rather than making the reader step over it.
        let constant = matches!(
            snapshot[index],
            Some(
                Opcode::Undefined
                    | Opcode::Null
                    | Opcode::PushMinus1
                    | Opcode::Push0
                    | Opcode::Push1
                    | Opcode::Push2
                    | Opcode::Push3
                    | Opcode::Push4
                    | Opcode::Push5
                    | Opcode::Push6
                    | Opcode::Push7
                    | Opcode::PushTrue
                    | Opcode::PushFalse
                    | Opcode::PushI32
            )
        );
        if constant
            && let Some(next) = next_op(index)
            && snapshot[next] == Some(Opcode::Drop)
        {
            doomed[index] = true;
            doomed[next] = true;
        }
    }
    for (index, entry) in insns.iter_mut().enumerate() {
        if doomed[index] {
            *entry = None;
        }
    }

    // A conditional whose destination is the very next instruction just pops its
    // condition, so it becomes a plain `drop`. This is what an empty `case` clause
    // compiles to.
    //
    // Order matters, and it is the reference's order: this runs *before* the
    // folding below, on the instruction the conditional originally had. Folding
    // moves a conditional's destination, and the reference never re-examines it
    // afterwards — which is why `if (c) continue;` at the end of a `do` body stays
    // a conditional even though folding leaves it pointing at the next
    // instruction.
    for (index, entry) in insns.iter_mut().enumerate() {
        let Some((op, operands)) = entry.clone() else {
            continue;
        };
        if !matches!(op, Opcode::IfTrue | Opcode::IfFalse) {
            continue;
        }
        let Some(id) = label_id(&operands) else {
            continue;
        };
        if placed.get(id as usize).copied().flatten() == Some(index + 1) {
            *entry = Some((Opcode::Drop, Vec::new()));
        }
    }

    // A constant condition is decided here rather than at run time.
    fold_constant_conditions(insns);
    // The conditional folding below is about pairs of *real* branches, so it runs
    // before a jump to the next instruction is dropped: `do { … if (c) continue }
    // while (t)` puts `continue` exactly on the condition, and the reference inverts
    // the branch first and only then notices the jump goes nowhere.
    fold_conditional_jumps(insns, label_at, placed);

    // A jump whose destination is the very next instruction does nothing, which is
    // what constant folding leaves behind for `while (1) { … }`.
    remove_jump_to_next(insns, placed);

    // `put_x(n) get_x(n)` -> `set_x(n)`, which is what a declaration followed by
    // a use of the same variable boils down to.
    for index in 0..insns.len() {
        let Some((put_op, _)) = insns[index].clone() else {
            continue;
        };
        if !is_foldable_put(put_op) {
            continue;
        }
        if is_label_at(label_at, index + 1) {
            continue;
        }
        let Some((get_op, get_operands)) = insns.get(index + 1).cloned().flatten() else {
            continue;
        };
        if get_op != get_form_of(put_op) {
            continue;
        }
        let put_slot = insns[index].as_ref().and_then(|(_, o)| o.first().cloned());
        if put_slot.is_none() || put_slot != get_operands.first().cloned() {
            continue;
        }
        insns[index] = Some((set_form_of(put_op), get_operands));
        insns[index + 1] = None;
    }
}

/// Drop `gosub`s that call an empty subroutine.
///
/// A `gosub` whose target starts with `ret` returns immediately, so the reference
/// drops the call. This is how `try`/`catch` without a `finally` loses its
/// finalizer plumbing: the finalizer block is empty, its first instruction is
/// `ret`, and every `gosub` to it disappears — leaving the `ret` unreachable, which
/// [`eliminate_dead_code`] then removes.
fn strip_empty_gosubs(stack: &mut [FnState], current: usize) {
    let state = &stack[current];
    let mut targets = Vec::new();
    for insn in &state.insns {
        if let Insn::Fixed { op, operands } = insn
            && *op == Opcode::Gosub
            && let Some(id) = label_id(operands)
            && let Some(target) = state.labels.get(id as usize).copied().flatten()
        {
            targets.push(target);
        }
    }
    let empty: Vec<usize> = targets
        .into_iter()
        .filter(|target| {
            stack[current]
                .insns
                .get(*target)
                .map(|insn| {
                    matches!(
                        insn,
                        Insn::Fixed {
                            op: Opcode::Ret,
                            ..
                        }
                    )
                })
                .unwrap_or(false)
        })
        .collect();
    if empty.is_empty() {
        return;
    }
    for insn in stack[current].insns.iter_mut() {
        if let Insn::Fixed { op, operands } = insn
            && *op == Opcode::Gosub
            && let Some(id) = label_id(operands)
            && let Some(target) = stack[current].labels.get(id as usize).copied().flatten()
            && empty.contains(&target)
        {
            *insn = Insn::Dead;
        }
    }
}

/// Replace unreachable instructions with [`Insn::Dead`].
fn eliminate_dead_code(stack: &mut [FnState], current: usize) {
    let reachable = reachable_instructions(&stack[current], stack[current].insns.len());
    for (index, insn) in stack[current].insns.iter_mut().enumerate() {
        if !reachable[index] {
            *insn = Insn::Dead;
        }
    }
}

/// Mark every instruction index reachable from the entry, plus the index one past
/// the last instruction.
///
/// `seen[len]` answers "can control reach the end of this function", and the
/// entries before it identify dead code. Both DCE and the implicit-return decision
/// read the same result, which keeps them from disagreeing.
fn reachable_instructions(state: &FnState, count: usize) -> Vec<bool> {
    let mut seen = vec![false; count + 1];
    let mut worklist = vec![0usize];
    while let Some(index) = worklist.pop() {
        if index > count || seen[index] {
            continue;
        }
        seen[index] = true;
        if index == count {
            continue;
        }
        let (op, operands) = match &state.insns[index] {
            Insn::Fixed { op, operands } => (*op, operands.as_slice()),
            // A variable access never transfers control, and a removed
            // instruction is only reachable if it is already marked.
            Insn::Var { .. } | Insn::CloseCaptured { .. } | Insn::DeleteVar { .. } | Insn::Dead => {
                worklist.push(index + 1);
                continue;
            }
        };
        match op {
            // A return, a throw and a `ret` all leave this instruction stream: the
            // first two give control to the caller or the handler, and `ret`
            // returns to a `gosub` elsewhere.
            // A tail call leaves the function too: the callee's result becomes this
            // function's result, so nothing after it runs.
            Opcode::Return
            | Opcode::ReturnUndef
            | Opcode::TailCall
            | Opcode::TailCallMethod
            | Opcode::Throw
            | Opcode::ThrowError
            | Opcode::Ret => {}
            // A `catch` records where exceptions from the code that follows go.
            // That code is not reachable through normal control flow, but it is
            // reachable — through the exception edge — so the walk has to follow
            // it, or dead-code elimination would delete every handler.
            Opcode::Catch => {
                if let Some(target) = placed_label(state, operands) {
                    worklist.push(target);
                }
                worklist.push(index + 1);
            }
            // A `gosub` calls a subroutine and then continues, so both edges count.
            Opcode::Gosub => {
                if let Some(target) = placed_label(state, operands) {
                    worklist.push(target);
                }
                worklist.push(index + 1);
            }
            Opcode::Goto => {
                if let Some(target) = placed_label(state, operands) {
                    worklist.push(target);
                }
            }
            Opcode::IfTrue | Opcode::IfFalse => {
                if let Some(target) = placed_label(state, operands) {
                    worklist.push(target);
                }
                worklist.push(index + 1);
            }
            _ => worklist.push(index + 1),
        }
    }
    seen
}

/// The instruction index a jump operand points at, resolved through the label
/// placement table.
fn placed_label(state: &FnState, operands: &[Op]) -> Option<usize> {
    let id = label_id(operands)?;
    state.labels.get(id as usize).copied().flatten()
}

/// The label an instruction refers to.
fn label_id(operands: &[Op]) -> Option<u32> {
    operands.iter().find_map(|operand| match operand {
        Op::Label(label) => Some(label.0),
        _ => None,
    })
}

/// Turn `if_false L; goto M; L:` into `if_true M`.
///
/// A conditional whose alternate branch is an unconditional jump needs one jump,
/// not two. This is the shape `if (c) break;` produces, so folding it removes an
/// instruction and a label from every conditional `break`.
/// The boolean a constant-push instruction produces, or `None` if it is not one.
///
/// Only the operators whose truthiness is decided by the opcode are covered; a
/// constant that needs the constant pool (a string) is left alone, because reading it
/// would mean threading the pool through the peephole for a case the reference also
/// skips.
fn constant_truth(op: Opcode, operands: &[Op]) -> Option<bool> {
    match op {
        // `push_0` returns -1 for zero, `push_1` 1, and so on.
        Opcode::PushMinus1 => Some(true),
        Opcode::Push0 => Some(false),
        Opcode::Push1
        | Opcode::Push2
        | Opcode::Push3
        | Opcode::Push4
        | Opcode::Push5
        | Opcode::Push6
        | Opcode::Push7 => Some(true),
        Opcode::PushTrue => Some(true),
        Opcode::PushFalse => Some(false),
        Opcode::Undefined | Opcode::Null => Some(false),
        Opcode::PushI32 => match operands.first() {
            Some(Op::Int(value)) => Some(*value != 0),
            _ => None,
        },
        _ => None,
    }
}

/// Fold a branch on a constant condition.
///
/// `if (0) a() else b()` must not test anything at run time, and the reference
/// compiles it to a plain `goto`. Removing the test also removes the unreachable arm,
/// which is why the bytecode for a `while (1)` loop has no condition at all.
///
/// Two instructions become one, so the slot the test occupied is left empty rather
/// than shifting everything after it: labels have already been resolved to indices.
fn fold_constant_conditions(insns: &mut [Option<(Opcode, Vec<Op>)>]) {
    for index in 0..insns.len() {
        let Some((push_op, push_operands)) = insns[index].clone() else {
            continue;
        };
        let Some(truth) = constant_truth(push_op, &push_operands) else {
            continue;
        };
        let Some((branch_op, branch_operands)) = insns.get(index + 1).cloned().flatten() else {
            continue;
        };
        let (destination, taken) = match branch_op {
            Opcode::IfTrue => (branch_operands.first().cloned(), truth),
            Opcode::IfFalse => (branch_operands.first().cloned(), !truth),
            _ => continue,
        };
        let Some(destination) = destination else {
            continue;
        };
        insns[index] = if taken {
            // The branch is always taken, so it becomes an unconditional jump.
            Some((Opcode::Goto, vec![destination]))
        } else {
            // The branch never runs: both instructions disappear and control falls
            // through to what came after.
            None
        };
        insns[index + 1] = None;
    }
}

/// Remove a jump whose destination is the instruction right after it.
///
/// Folding a constant condition leaves these behind often: `while (1) { ... }` turns
/// its test into nothing, and the loop-end jump then points at the following
/// instruction. The reference drops them, so `stack_size` and the listing agree.
fn remove_jump_to_next(insns: &mut [Option<(Opcode, Vec<Op>)>], placed: &[Option<usize>]) {
    for index in 0..insns.len() {
        let Some((op, operands)) = insns[index].clone() else {
            continue;
        };
        if !is_goto(op) {
            continue;
        }
        let Some(id) = label_id(&operands) else {
            continue;
        };
        // The next instruction that still exists, which is what "the instruction
        // after this jump" means once earlier folds have punched holes.
        let next = (index + 1..insns.len()).find(|position| insns[*position].is_some());
        if placed.get(id as usize).copied().flatten() == next {
            insns[index] = None;
        }
    }
}

fn fold_conditional_jumps(
    insns: &mut [Option<(Opcode, Vec<Op>)>],
    label_at: &[Option<u32>],
    placed: &[Option<usize>],
) {
    for index in 0..insns.len() {
        let Some((op, operands)) = insns[index].clone() else {
            continue;
        };
        let inverted = match op {
            Opcode::IfFalse => Opcode::IfTrue,
            Opcode::IfTrue => Opcode::IfFalse,
            _ => continue,
        };
        // Next must be an unconditional jump that is not itself a target, and the
        // instruction after it must be exactly where the conditional goes.
        let Some((next_op, next_operands)) = insns.get(index + 1).cloned().flatten() else {
            continue;
        };
        if !is_goto(next_op) || is_label_at(label_at, index + 1) {
            continue;
        }
        // The conditional must land on the instruction *after* the jump. Several
        // labels can share a position, so this asks where *this* label was put
        // rather than what label happens to sit there.
        let Some(taken) = label_id(&operands) else {
            continue;
        };
        if placed.get(taken as usize).copied().flatten() != Some(index + 2) {
            continue;
        }
        let Some(destination) = next_operands.first().cloned() else {
            continue;
        };
        insns[index] = Some((inverted, vec![destination]));
        insns[index + 1] = None;
    }
}

/// Whether a jump target is placed at this instruction index.
fn is_label_at(label_at: &[Option<u32>], index: usize) -> bool {
    label_at.get(index).copied().flatten().is_some()
}

fn fold_dup_patterns(insns: &mut [Option<(Opcode, Vec<Op>)>], label_at: &[Option<u32>]) {
    // `dup put_x(n)` -> `set_x(n)`
    // `dup put_x(n) drop` -> `put_x(n)`
    // `dup put_x(n) drop get_x(n)` -> `set_x(n)`
    for index in 0..insns.len() {
        if opcode_at(insns, index) != Some(Opcode::Dup) {
            continue;
        }
        if is_label_at(label_at, index + 1) {
            continue;
        }
        let Some((put_op, put_operands)) = insns[index + 1].clone() else {
            continue;
        };
        if !is_foldable_put(put_op) {
            continue;
        }
        let Some(Op::Slot(slot)) = put_operands.first().cloned() else {
            continue;
        };
        let mut new_op = set_form_of(put_op);
        let mut end = index + 2;
        if is_label_at(label_at, end) {
            // The `put` has to stay as it is; only the `dup` can be folded.
            insns[index] = Some((set_form_of(put_op), vec![Op::Slot(slot)]));
            insns[index + 1] = None;
            continue;
        }
        if opcode_at(insns, end) == Some(Opcode::Drop) {
            new_op = put_op;
            end += 1;
            if opcode_at(insns, end) == Some(get_form_of(put_op))
                && insns[end]
                    .as_ref()
                    .is_some_and(|(_, operands)| operands.first() == Some(&Op::Slot(slot)))
            {
                new_op = set_form_of(put_op);
                end += 1;
            }
        }
        insns[index] = Some((new_op, vec![Op::Slot(slot)]));
        for entry in &mut insns[index + 1..end] {
            *entry = None;
        }
    }
}

fn opcode_at(insns: &[Option<(Opcode, Vec<Op>)>], index: usize) -> Option<Opcode> {
    insns
        .get(index)
        .and_then(|entry| entry.as_ref())
        .map(|(op, _)| *op)
}

/// The four `put_` families that have a `set_` twin. Deliberately not including
/// the global forms or `put_var_ref_check`: the reference's peephole lists exactly
/// these, and folding the others would produce instructions that do not exist.
fn is_foldable_put(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::PutLoc | Opcode::PutLocCheck | Opcode::PutArg | Opcode::PutVarRef
    )
}

fn set_form_of(op: Opcode) -> Opcode {
    match op {
        Opcode::PutLoc => Opcode::SetLoc,
        Opcode::PutLocCheck => Opcode::SetLocCheck,
        Opcode::PutArg => Opcode::SetArg,
        Opcode::PutVarRef => Opcode::SetVarRef,
        other => other,
    }
}

fn get_form_of(op: Opcode) -> Opcode {
    match op {
        Opcode::PutLoc => Opcode::GetLoc,
        Opcode::PutLocCheck => Opcode::GetLocCheck,
        Opcode::PutArg => Opcode::GetArg,
        Opcode::PutVarRef => Opcode::GetVarRef,
        other => other,
    }
}

/// Atom interning, per function.
///
/// Everything the reference interns goes through `JS_NewAtomStr`, including plain
/// identifiers, so the array-index rule applies here rather than at the handful of
/// call sites that build property keys.
fn intern_atom(state: &mut FnState, name: &str) -> u32 {
    let atom = Atom::from_text(name);
    if let Some(index) = state.atoms.iter().position(|a| *a == atom) {
        return index as u32;
    }
    state.atoms.push(atom);
    state.atoms.len() as u32 - 1
}

fn binary_opcode(op: BinaryOp) -> Opcode {
    match op {
        BinaryOp::Add => Opcode::Add,
        BinaryOp::Sub => Opcode::Sub,
        BinaryOp::Mul => Opcode::Mul,
        BinaryOp::Div => Opcode::Div,
        BinaryOp::Mod => Opcode::Mod,
        BinaryOp::Pow => Opcode::Pow,
        BinaryOp::BitOr => Opcode::Or,
        BinaryOp::BitXor => Opcode::Xor,
        BinaryOp::BitAnd => Opcode::And,
        BinaryOp::Shl => Opcode::Shl,
        BinaryOp::Shr => Opcode::Sar,
        BinaryOp::UShr => Opcode::Shr,
        BinaryOp::Lt => Opcode::Lt,
        BinaryOp::LtEq => Opcode::Lte,
        BinaryOp::Gt => Opcode::Gt,
        BinaryOp::GtEq => Opcode::Gte,
        BinaryOp::Eq => Opcode::Eq,
        BinaryOp::NotEq => Opcode::Neq,
        BinaryOp::StrictEq => Opcode::StrictEq,
        BinaryOp::StrictNotEq => Opcode::StrictNeq,
        BinaryOp::In => Opcode::In,
        BinaryOp::Instanceof => Opcode::Instanceof,
    }
}

fn compound_opcode(op: AssignOp) -> Opcode {
    match op {
        AssignOp::Add => Opcode::Add,
        AssignOp::Sub => Opcode::Sub,
        AssignOp::Mul => Opcode::Mul,
        AssignOp::Div => Opcode::Div,
        AssignOp::Mod => Opcode::Mod,
        AssignOp::Pow => Opcode::Pow,
        AssignOp::Shl => Opcode::Shl,
        AssignOp::Shr => Opcode::Sar,
        AssignOp::UShr => Opcode::Shr,
        AssignOp::BitOr => Opcode::Or,
        AssignOp::BitXor => Opcode::Xor,
        AssignOp::BitAnd => Opcode::And,
        AssignOp::Assign | AssignOp::And | AssignOp::Or | AssignOp::Coalesce => Opcode::Nop,
    }
}

/// Encode the resolved instruction stream, choosing short forms where possible.
///
/// The peephole pass leaves holes in the instruction list so that label indices
/// stay valid. A label names an *instruction index*, and an index that was
/// deleted has to resolve to the next surviving instruction, which is what
/// [`resolve_label`] does before the byte offsets are known.
fn assemble(
    insns: &mut [Option<(Opcode, Vec<Op>)>],
    insn_pos: &[Option<Position>],
    state: &mut FnState,
) -> (Vec<u8>, u32, Vec<LineCol>) {
    // Compaction: surviving instructions in order, plus the index mapping.
    //
    // `spans` runs parallel to `kept` and is compacted in lock step with it, at every
    // point below where instructions are removed — otherwise a stack trace would point
    // at the wrong source line as soon as dead-code elimination kicked in.
    let mut kept: Vec<(Opcode, Vec<Op>)> = Vec::with_capacity(insns.len());
    let mut spans: Vec<Option<Position>> = Vec::with_capacity(insns.len());
    let mut original_to_kept: Vec<Option<usize>> = vec![None; insns.len()];
    for (original, entry) in insns.iter_mut().enumerate() {
        if let Some((op, operands)) = entry.take() {
            original_to_kept[original] = Some(kept.len());
            kept.push((op, operands));
            spans.push(insn_pos.get(original).copied().flatten());
        }
    }

    if std::env::var_os("QJS_DEBUG").is_some() {
        eprintln!("--- assemble entry --- {kept:?}");
    }
    // How many labels are real, as opposed to the per-instruction ones allocated
    // just below. Only a real label marks a spot in the source program.
    let real_labels = state.labels.len();

    // Give every surviving instruction a label of its own. Jump cleanup below
    // redirects jumps to arbitrary instructions, and a jump target has to be a
    // label; pre-allocating one per index means a redirect never has to invent
    // anything.
    let existing = state.labels.len();
    let base = kept.len();
    let mut index_label: Vec<Label> = Vec::with_capacity(kept.len() + 1);
    for offset in 0..=kept.len() {
        state.labels.push(Some(0));
        index_label.push(Label((existing + offset) as u32));
    }
    // Point each pre-allocated label at its instruction, reusing the count from
    // `original_to_kept` so the mapping survives compaction.
    let place_index_labels = |state: &mut FnState,
                              original_to_kept: &[Option<usize>],
                              kept_len: usize,
                              index_label: &[Label]| {
        for (original, mapped) in original_to_kept.iter().enumerate() {
            if let Some(kept_index) = mapped {
                let label = index_label[*kept_index].0 as usize;
                state.labels[label] = Some(original);
            }
        }
        state.labels[index_label[kept_len].0 as usize] = Some(usize::MAX);
    };
    place_index_labels(state, &original_to_kept, base, &index_label);

    // Resolve every label to a kept index once; the placement table maps labels
    // to original indices, so a second hop lands in kept space.
    let mut label_kept: Vec<Option<usize>> = vec![None; state.labels.len()];
    for (id, placed) in state.labels.iter().enumerate() {
        let Some(original) = placed else { continue };
        label_kept[id] = if *original == usize::MAX {
            Some(base)
        } else {
            (*original..original_to_kept.len()).find_map(|i| original_to_kept[i])
        };
    }

    // Code after an unconditional transfer is unreachable, and the reference drops it
    // as it walks past the transfer — the `skip_dead_code` in its first pass. It
    // scans forward until it finds a label something still jumps to, so a run of
    // dead instructions between a `throw` and the next live target disappears.
    //
    // The scan repeats because dropping a jump lowers the reference count of the
    // label it pointed at, which can strand what follows. Only a real label keeps an
    // instruction alive here; the per-instruction labels allocated above belong to
    // this pass, not to the program.
    loop {
        let mut references = vec![false; kept.len() + 1];
        for (_, operands) in &kept {
            if let Some(target) = target_kept_index(operands, &label_kept)
                && target < references.len()
            {
                references[target] = true;
            }
        }
        let mut doomed = vec![false; kept.len()];
        let mut any = false;
        for (index, (op, _)) in kept.iter().enumerate() {
            if !is_transfer(*op) {
                continue;
            }
            for next in index + 1..kept.len() {
                if references[next] {
                    break;
                }
                doomed[next] = true;
                any = true;
            }
        }
        if !any {
            break;
        }
        (kept, spans) = drop_instructions(kept, spans, &mut label_kept, &doomed);
    }

    // A label that lands on an unconditional jump stands for wherever that jump
    // goes, so retarget it once and every jump to the label follows for free.
    // This is what makes `continue` in a `for` head reach the loop top instead of
    // stopping at the update's trailing jump.
    let folded: Vec<Option<usize>> = label_kept
        .iter()
        .map(|slot| slot.map(|target| follow_jump_chain(&kept, &label_kept, target)))
        .collect();
    label_kept = folded;

    // Jump cleanup, in compacted index space. Two rewrites the reference also
    // applies while it resolves labels:
    //   * a jump whose destination is the very next instruction does nothing;
    //   * a jump to a jump is redirected to its final destination.
    // The second can expose the first, so this repeats until it settles.
    loop {
        let mut delete = vec![false; kept.len()];
        let mut changed = false;
        for index in 0..kept.len() {
            let (op, operands) = &kept[index];
            if !is_goto(*op) {
                continue;
            }
            let Some(immediate) = target_kept_index(operands, &label_kept) else {
                continue;
            };
            let target = follow_jump_chain(&kept, &label_kept, immediate);
            if target == index + 1 {
                delete[index] = true;
                changed = true;
            } else if matches!(
                kept.get(target).map(|(op, _)| *op),
                Some(Opcode::Return | Opcode::ReturnUndef | Opcode::Throw)
            ) {
                // Jumping straight to a return or throw is the same as doing it
                // here, so the jump becomes that instruction.
                kept[index] = (kept[target].0, Vec::new());
                changed = true;
            } else if target != immediate {
                // Redirect to the end of the chain.
                let label = index_label[target];
                kept[index].1 = vec![Op::Label(label)];
                changed = true;
            }
        }
        if !changed {
            break;
        }
        if !delete.iter().any(|deleted| *deleted) {
            break;
        }
        // Drop the deleted instructions and renumber everything.
        let mut next: Vec<(Opcode, Vec<Op>)> = Vec::with_capacity(kept.len());
        let mut next_spans: Vec<Option<Position>> = Vec::with_capacity(kept.len());
        let mut moved: Vec<Option<usize>> = vec![None; kept.len()];
        for (old, entry) in kept.into_iter().enumerate() {
            if delete[old] {
                continue;
            }
            moved[old] = Some(next.len());
            next.push(entry);
            next_spans.push(spans[old]);
        }
        for (id, slot) in label_kept.iter_mut().enumerate() {
            let Some(old) = *slot else { continue };
            *slot = match moved.get(old).copied().flatten() {
                Some(new) => Some(new),
                // The target was deleted: the jump now lands on whatever follows.
                None => (old..moved.len())
                    .find_map(|i| moved[i])
                    .or(Some(next.len())),
            };
            let _ = id;
        }
        kept = next;
        spans = next_spans;
    }

    // The same walk at the end of the function: `try`/`catch`'s `goto end` becomes a
    // jump to the next instruction and is deleted, leaving the label at the end with
    // nothing pointing at it. If that also strands the last instruction, it goes
    // too — which is how a `try` whose body always returns ends on its `ret` rather
    // than an implicit `return_undef`.
    // Which instructions something still jumps to. Reading the surviving jumps is
    // what the reference's reference counts amount to: once a jump has been folded
    // away the label it named is unreferenced, even though the program still has a
    // label there.
    let mut references = vec![false; kept.len() + 1];
    let _ = real_labels;
    for (_, operands) in &kept {
        if let Some(target) = target_kept_index(operands, &label_kept)
            && target < references.len()
        {
            references[target] = true;
        }
    }
    let mut end = kept.len();
    while end > 0 {
        let last = end - 1;
        let reachable = references[last] || end < kept.len();
        if reachable {
            break;
        }
        let after_transfer = last
            .checked_sub(1)
            .map(|previous| {
                matches!(
                    kept[previous].0,
                    Opcode::Goto | Opcode::Return | Opcode::ReturnUndef | Opcode::Throw
                )
            })
            .unwrap_or(false);
        if !after_transfer {
            break;
        }
        end = last;
    }
    kept.truncate(end);
    spans.truncate(end);

    // Choose an opcode for each instruction and its size, so label offsets can be
    // computed.
    let mut chosen = Vec::with_capacity(kept.len());
    let mut offsets = Vec::with_capacity(kept.len() + 1);
    let mut offset = 0usize;
    for (op, operands) in &kept {
        let op = choose_short_form(*op, operands);
        offsets.push(offset);
        chosen.push(op);
        offset += op.size() as usize;
    }
    offsets.push(offset);

    let mut code = Vec::with_capacity(offset);
    for (position, (_, operands)) in kept.iter().enumerate() {
        let op = chosen[position];
        code.push(op as u8);
        let operands = retarget_labels(operands, &label_kept, kept.len());
        encode_operands(&mut code, op, &operands, offsets[position], &offsets);
    }
    let stack_size = compute_stack_size(&code);
    // Keep only the offsets where the position actually changes: a run of
    // instructions on one line is one table entry, and the runtime binary searches
    // the table for the frame's pc.
    let mut positions: Vec<LineCol> = Vec::new();
    let mut last: Option<Position> = None;
    for (index, span) in spans.iter().enumerate() {
        let Some(pos) = *span else { continue };
        if last == Some(pos) {
            continue;
        }
        last = Some(pos);
        positions.push(LineCol {
            pc: offsets[index] as u32,
            line: pos.line,
            column: pos.column,
        });
    }
    (code, stack_size, positions)
}

/// Whether an instruction always transfers control away, so anything after it in the
/// same stream is unreachable.
fn is_transfer(op: Opcode) -> bool {
    matches!(
        op,
        Opcode::Goto
            | Opcode::Goto8
            | Opcode::Goto16
            | Opcode::Return
            | Opcode::ReturnUndef
            | Opcode::Throw
            | Opcode::ThrowError
            | Opcode::Ret
    )
}

/// Remove the marked instructions and renumber the label table so it keeps pointing
/// at the same instructions. A label whose instruction went away moves to whatever
/// follows it, since a jump is never left with nowhere to land.
fn drop_instructions(
    kept: Vec<(Opcode, Vec<Op>)>,
    spans: Vec<Option<Position>>,
    label_kept: &mut [Option<usize>],
    doomed: &[bool],
) -> (Vec<PositionedInsn>, Vec<Option<Position>>) {
    let mut result: Vec<(Opcode, Vec<Op>)> = Vec::with_capacity(kept.len());
    let mut result_spans: Vec<Option<Position>> = Vec::with_capacity(kept.len());
    let mut moved: Vec<Option<usize>> = vec![None; kept.len()];
    for (old, entry) in kept.into_iter().enumerate() {
        if doomed.get(old).copied().unwrap_or(false) {
            continue;
        }
        moved[old] = Some(result.len());
        result.push(entry);
        result_spans.push(spans[old]);
    }
    for slot in label_kept.iter_mut() {
        let Some(old) = *slot else { continue };
        *slot = match moved.get(old).copied().flatten() {
            Some(new) => Some(new),
            None => (old..moved.len())
                .find_map(|i| moved[i])
                .or(Some(result.len())),
        };
    }
    (result, result_spans)
}

fn is_goto(op: Opcode) -> bool {
    matches!(op, Opcode::Goto | Opcode::Goto8 | Opcode::Goto16)
}

/// The kept index a jump operand points at.
fn target_kept_index(operands: &[Op], label_kept: &[Option<usize>]) -> Option<usize> {
    let id = label_id(operands)?;
    label_kept.get(id as usize).copied().flatten()
}

/// Follow a chain of unconditional jumps to its first real destination.
fn follow_jump_chain(
    kept: &[(Opcode, Vec<Op>)],
    label_kept: &[Option<usize>],
    start: usize,
) -> usize {
    let mut current = start;
    // `for(;;)` jumps to itself, so the walk has to be bounded.
    for _ in 0..kept.len() {
        let Some((op, operands)) = kept.get(current) else {
            return current;
        };
        if !is_goto(*op) {
            return current;
        }
        let Some(next) = target_kept_index(operands, label_kept) else {
            return current;
        };
        if next == current {
            return current;
        }
        current = next;
    }
    current
}

/// Rewrite label operands from label ids to kept instruction indices.
fn retarget_labels(operands: &[Op], label_kept: &[Option<usize>], kept_len: usize) -> Vec<Op> {
    operands
        .iter()
        .map(|operand| match operand {
            Op::Label(label) => {
                let target = label_kept
                    .get(label.0 as usize)
                    .copied()
                    .flatten()
                    .unwrap_or(kept_len);
                Op::Label(Label(target as u32))
            }
            other => other.clone(),
        })
        .collect()
}

/// Encode the resolved instruction stream, choosing short forms where possible.
///
/// Compute the maximum value-stack depth by exploring the control-flow graph.
///
/// This is a breadth-first walk from the first instruction, tracking the stack
/// depth on entry to each instruction. Every instruction has at most two
/// successors (fall through, and a jump target), so a worklist over byte offsets
/// is enough. The reference does the same thing in `compute_stack_size`, which is
/// why the resulting numbers match exactly.
///
/// Instructions that pop a variable number of values (`call`, `array_from`, …)
/// encode that count in their operand, which is added to the static `n_pop`.
fn compute_stack_size(code: &[u8]) -> u32 {
    let mut depth_at = vec![u16::MAX; code.len()];
    // Each entry carries the enclosing `catch` levels, innermost last, because
    // `nip_catch` needs them: it rewinds the stack to just above the innermost catch
    // offset and forgets that level.
    let mut worklist: Vec<(usize, u32, Vec<u32>)> = vec![(0, 0, Vec::new())];
    let mut max_depth = 0u32;

    while let Some((pc, depth, catch_levels)) = worklist.pop() {
        if pc >= code.len() {
            continue;
        }
        if depth_at[pc] != u16::MAX {
            continue;
        }
        force_depth(&mut depth_at, pc, depth);

        let Some(op) = Opcode::from_u8(code[pc]) else {
            break;
        };
        let info = op.info();
        let next = pc + info.size as usize;
        if next > code.len() {
            break;
        }
        let mut n_pop = info.n_pop as u32;
        let mut n_push = info.n_push as u32;
        // Variable-length forms carry their count in the operand.
        match info.format {
            crate::opcode::OpFormat::Npop | crate::opcode::OpFormat::NpopU16 => {
                n_pop += read_u16(code, pc + 1) as u32;
            }
            crate::opcode::OpFormat::Npopx => {
                n_pop += op.specialised_index().unwrap_or(0).max(0) as u32;
            }
            _ => {}
        }
        // `put_loc`/`put_arg`/`put_var_ref` keep the value on the stack when they
        // are the `set_` variant; the table already records that, so nothing to
        // adjust here.
        let _ = &mut n_push;

        // The depth before this instruction's own push, which is what a `catch`
        // records as its level.
        let depth_before = depth;
        let mut depth = depth.saturating_sub(n_pop) + n_push;
        let mut catch_levels = catch_levels;
        if op == Opcode::NipCatch {
            // Discard everything above the innermost catch offset, keeping the value
            // on top: the frame the code after this sees is the same shape it would
            // have had if the `try` were not there.
            if let Some(base) = catch_levels.pop() {
                depth = base + 1;
            }
        }
        max_depth = max_depth.max(depth);

        // Successors.
        let target = jump_target_of(code, pc, op);
        match op {
            Opcode::Goto | Opcode::Goto8 | Opcode::Goto16 => {
                if let Some(target) = target {
                    worklist.push((target, depth, catch_levels));
                }
            }
            Opcode::IfFalse | Opcode::IfFalse8 | Opcode::IfTrue | Opcode::IfTrue8 => {
                if let Some(target) = target {
                    worklist.push((target, depth, catch_levels.clone()));
                }
                worklist.push((next, depth, catch_levels));
            }
            // A `catch` records an exception edge to the handler. The handler is
            // entered with the thrown value on top of the stack, in the slot the
            // catch offset occupies on the normal path — which is why the handler's
            // first instruction can `put_loc` the exception away, and why the depth
            // is the same on both paths.
            Opcode::Catch => {
                if let Some(target) = target {
                    // The handler is protected by the *enclosing* catch, not by this
                    // one: an error raised inside it is what the handler's own
                    // `catch` catches.
                    worklist.push((target, depth, catch_levels.clone()));
                }
                catch_levels.push(depth_before);
                worklist.push((next, depth, catch_levels));
            }
            // A `for`/`of` loop keeps its own catch offset, and `nip_catch` has to
            // rewind past it too. Its level sits one slot higher than a `catch`'s:
            // the iterator and next-method slots occupy the offset's place.
            Opcode::ForOfStart | Opcode::ForAwaitOfStart => {
                if let Some(target) = target {
                    worklist.push((target, depth, catch_levels.clone()));
                }
                catch_levels.push(depth_before + 1);
                worklist.push((next, depth, catch_levels));
            }
            // `gosub` calls a subroutine with the return address on top. The opcode
            // table records no push for it — `ret` is what pops the slot — so the
            // extra one is added here, where the depth of the subroutine's first
            // instruction is being computed.
            Opcode::Gosub => {
                if let Some(target) = target {
                    worklist.push((target, depth + 1, catch_levels.clone()));
                }
                worklist.push((next, depth, catch_levels));
            }
            // `ret` goes back to a `gosub`; nothing here follows it.
            Opcode::Ret
            | Opcode::Return
            | Opcode::ReturnUndef
            | Opcode::Throw
            | Opcode::ThrowError => {}
            _ => worklist.push((next, depth, catch_levels)),
        }
    }
    max_depth
}

/// Record the stack depth on entry to `pc`, keeping the deeper value if the
/// instruction is reachable more than once.
fn force_depth(depth_at: &mut [u16], pc: usize, depth: u32) {
    let clamped = depth.min(u16::MAX as u32 - 1) as u16;
    if depth_at[pc] == u16::MAX || depth_at[pc] < clamped {
        depth_at[pc] = clamped;
    }
}

fn read_u16(code: &[u8], at: usize) -> u16 {
    let mut value = 0u16;
    for i in 0..2 {
        value = (value << 8) | code.get(at + i).copied().unwrap_or(0) as u16;
    }
    value
}

/// The absolute target of a jump instruction, if it has one.
fn jump_target_of(code: &[u8], pc: usize, op: Opcode) -> Option<usize> {
    let info = op.info();
    let position = pc + 1;
    match info.format {
        crate::opcode::OpFormat::Label8 => {
            let offset = *code.get(position)? as i8 as i64;
            Some((position as i64 + offset) as usize)
        }
        crate::opcode::OpFormat::Label16 => {
            let offset = read_u16(code, position) as i16 as i64;
            Some((position as i64 + offset) as usize)
        }
        crate::opcode::OpFormat::Label => {
            let mut offset = 0i32;
            for i in 0..4 {
                offset = (offset << 8) | *code.get(position + i).unwrap_or(&0) as i32;
            }
            Some((position as i64 + offset as i64) as usize)
        }
        _ => None,
    }
}

fn choose_short_form(op: Opcode, operands: &[Op]) -> Opcode {
    use Opcode::*;
    match (op, operands.first()) {
        (PushI32, Some(Op::Int(value))) if (-1..=7).contains(value) => {
            op.short_form(*value as u32).unwrap_or(op)
        }
        (PushConst, Some(Op::Const(index))) if *index < 256 => Opcode::PushConst8,
        (Fclosure, Some(Op::Const(index))) if *index < 256 => Opcode::Fclosure8,
        (GetLoc | PutLoc | SetLoc, Some(Op::Slot(Slot::Loc(index))))
        | (GetArg | PutArg | SetArg, Some(Op::Slot(Slot::Arg(index))))
        | (GetVarRef | PutVarRef | SetVarRef, Some(Op::Slot(Slot::VarRef(index))))
        // A declared global is reached through its reference cell, so it shares
        // the `var_ref` encodings, short forms included.
        | (GetVarRef | PutVarRef | SetVarRef, Some(Op::Slot(Slot::GlobalDecl(index)))) => {
            op.short_form(*index).unwrap_or(op)
        }
        (Call, Some(Op::Int(count))) if (0..=3).contains(count) => {
            op.short_form(*count as u32).unwrap_or(op)
        }
        (IfFalse, _) => IfFalse8,
        (IfTrue, _) => IfTrue8,
        (Goto, _) => Goto8,
        _ => op,
    }
}

fn encode_operands(
    code: &mut Vec<u8>,
    op: Opcode,
    operands: &[Op],
    position: usize,
    offsets: &[usize],
) {
    use crate::opcode::OpFormat::*;
    let info = op.info();
    let write_slot = |code: &mut Vec<u8>, slot: Slot| {
        let index = match slot {
            Slot::Arg(i)
            | Slot::Loc(i)
            | Slot::VarRef(i)
            | Slot::Global(i)
            | Slot::GlobalDecl(i) => i,
        };
        code.extend_from_slice(&(index as u16).to_be_bytes());
    };
    match info.format {
        None | NoneInt | NoneLoc | NoneArg | NoneVarRef | Npopx => {}
        U8 | Const8 => {
            if let Some(value) = operands.first().and_then(operand_uint) {
                code.push(value as u8);
            }
        }
        // The narrow slot forms (`inc_loc8` and friends) take a slot operand just
        // like their wide counterparts, only one byte wide.
        Loc8 => {
            if let Some(Op::Slot(slot)) = operands.first() {
                let index = match slot {
                    Slot::Arg(i)
                    | Slot::Loc(i)
                    | Slot::VarRef(i)
                    | Slot::Global(i)
                    | Slot::GlobalDecl(i) => *i,
                };
                code.push(index as u8);
            }
        }
        I8 => {
            if let Some(value) = operands.first().and_then(operand_int) {
                code.push(value as i8 as u8);
            }
        }
        U16 | Npop => {
            if let Some(value) = operands.first().and_then(operand_int) {
                code.extend_from_slice(&(value as u16).to_be_bytes());
            }
        }
        I16 => {
            if let Some(value) = operands.first().and_then(operand_int) {
                code.extend_from_slice(&(value as i16 as u16).to_be_bytes());
            }
        }
        I32 => {
            if let Some(value) = operands.first().and_then(operand_int) {
                code.extend_from_slice(&value.to_be_bytes());
            }
        }
        U32 | Const => {
            if let Some(value) = operands.first().and_then(operand_uint) {
                code.extend_from_slice(&value.to_be_bytes());
            }
        }
        Loc | Arg | VarRef => {
            if let Some(Op::Slot(slot)) = operands.first() {
                write_slot(code, *slot);
            }
        }
        Atom | AtomU8 | AtomU16 | AtomLabelU8 | AtomLabelU16 => {
            if let Some(Op::AtomId(index)) = operands.first() {
                code.extend_from_slice(&index.to_be_bytes());
            }
            // The trailing immediate means different things per format, and the
            // atom forms that carry a *label* have to resolve it like any other
            // jump, so they cannot share the `U8`/`U16` arms above.
            match info.format {
                AtomU8 | AtomLabelU8 => {
                    if let Some(value) = operands.get(1).and_then(operand_uint) {
                        code.push(value as u8);
                    }
                }
                AtomU16 | AtomLabelU16 => {
                    if let Some(value) = operands.get(1).and_then(operand_uint) {
                        code.extend_from_slice(&(value as u16).to_be_bytes());
                    }
                }
                _ => {}
            }
        }
        NpopU16 => {
            for operand in operands.iter().take(2) {
                if let Some(value) = operand_uint(operand) {
                    code.extend_from_slice(&(value as u16).to_be_bytes());
                }
            }
        }
        Label | Label8 | Label16 | LabelU16 => {
            if let Some(Op::Label(label)) = operands.first() {
                // Labels were already remapped to kept-instruction indices.
                let Some(&target) = offsets.get(label.0 as usize) else {
                    debug_assert!(false, "label {} is past the end", label.0);
                    return;
                };
                let relative = target as i64 - (position as i64 + 1);
                match info.format {
                    Label8 => code.push(relative as i8 as u8),
                    Label16 => code.extend_from_slice(&(relative as i16 as u16).to_be_bytes()),
                    // `label_u16` stores the target followed by an unrelated u16.
                    LabelU16 => {
                        code.extend_from_slice(&(relative as i32).to_be_bytes());
                        if let Some(value) = operands.get(1).and_then(operand_uint) {
                            code.extend_from_slice(&(value as u16).to_be_bytes());
                        }
                    }
                    _ => code.extend_from_slice(&(relative as i32).to_be_bytes()),
                }
            }
        }
    }
}

fn operand_int(op: &Op) -> Option<i32> {
    match op {
        Op::Int(value) => Some(*value),
        _ => None,
    }
}

fn operand_uint(op: &Op) -> Option<u32> {
    match op {
        Op::Const(value) | Op::AtomId(value) => Some(*value),
        Op::Int(value) => Some(*value as u32),
        _ => None,
    }
}

/// Disassemble a compiled function.
pub fn disassemble(function: &CompiledFunction) -> String {
    disasm::disassemble_program(function)
}

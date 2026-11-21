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

use crate::bytecode::{
    ClosureType, ClosureVar, Constant, Function as CompiledFunction, FunctionKind, VarDef,
};
use crate::disasm;
use crate::opcode::Opcode;
use js_parse::Span;
use js_parse::SyntaxError;
use js_parse::ast::{self, *};

/// A label id, local to the function being compiled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Label(u32);

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
    Atom(String),
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
    Var { action: VarAction, name: String },
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
    /// Where `break` lands.
    break_label: Label,
    /// Where `continue` lands. `None` for a labelled non-loop statement, which
    /// `break` may leave but `continue` may not target.
    continue_label: Option<Label>,
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
    atoms: Vec<String>,
    /// Every lexical scope, in creation order.
    blocks: Vec<Block>,
    /// The block currently being parsed or compiled.
    current_block: u64,
    /// The block each local was declared in, parallel to `vars`. `0` means the
    /// function scope, where `var` bindings live.
    var_block: Vec<u64>,
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
            body_block: 0,
            block_heads: Vec::new(),
            jump_targets: Vec::new(),
            insns: Vec::new(),
            labels: Vec::new(),
            var_ref_count: 0,
            line: 1,
            column: 1,
            is_script: false,
            leaves_value: false,
            stmt_depth: 0,
            hoisted: Vec::new(),
        }
    }

    /// The scope a declaration lands in: a script's top-level declarations are
    /// globals, everything else gets a slot.
    fn declares_globally(&self) -> bool {
        self.is_script && self.current_block == self.body_block
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
        state.vars.push(VarDef::var("<ret>"));
        self.stack.push(state);
        // The body is its own scope, keyed by the position of the program.
        let body_key = program.span.start.offset as u64;
        self.push_block(body_key, 0);
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
        self.state().insns.push(Insn::Fixed {
            op,
            operands: operands.to_vec(),
        });
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

    fn emit_var(&mut self, action: VarAction, name: &str) {
        // Peephole: `put_x v` followed by `get_x v` becomes `set_x v`, which
        // stores and keeps the value in one instruction. The reference does this
        // in a post-pass; folding it at emit time keeps instruction indices
        // stable, which matters because labels are recorded as indices.
        //
        // Only slots have a `set_` form: a global is stored with `put_var` and
        // read with `get_var`, and the reference does not fold those.
        if action == VarAction::Get {
            let is_slot = self.cur().vars.iter().any(|v| v.name == name)
                || self.cur().args.iter().any(|a| a.name == name);
            if is_slot
                && let Some(Insn::Var {
                    action: previous,
                    name: previous_name,
                }) = self.state().insns.last_mut()
                && *previous == VarAction::Put
                && previous_name == name
            {
                *previous = VarAction::Set;
                return;
            }
        }
        self.state().insns.push(Insn::Var {
            action,
            name: name.to_string(),
        });
    }

    fn new_label(&mut self) -> Label {
        let state = self.state();
        state.labels.push(None);
        Label(state.labels.len() as u32 - 1)
    }

    fn place_label(&mut self, label: Label) {
        let index = self.cur().insns.len();
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
                    self.state().insns.push(Insn::Var {
                        action: VarAction::PutRef,
                        name,
                    });
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

    fn declare(&mut self, name: &str, def: VarDef) -> u32 {
        if self.cur().declares_globally() {
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
                self.expr(expr)?;
                if self.cur().is_script {
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
                            self.expr(expr)?;
                            self.emit(Opcode::Drop, &[]);
                        }
                    }
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
                if let Some(update) = update {
                    self.expr(update)?;
                    self.emit(Opcode::Drop, &[]);
                }
                self.emit_jump(Opcode::Goto, top, &[]);
                self.place_label(end);
                self.emit_scope_leave(for_key);
                self.state().current_block = for_parent;
            }
            StmtKind::Return(value) => match value {
                Some(expr) => {
                    self.expr(expr)?;
                    self.emit(Opcode::Return, &[]);
                }
                None => {
                    self.emit(Opcode::ReturnUndef, &[]);
                }
            },
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
                self.try_stmt(block, handler, finalizer.as_deref())?;
            }
            StmtKind::ForIn { left, right, body } => {
                self.for_in_of(left, right, body, false, false)?;
            }
            StmtKind::ForOf {
                left,
                right,
                body,
                is_await,
            } => {
                self.for_in_of(left, right, body, true, *is_await)?;
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
            StmtKind::Block(inner) => self.hoist_block(stmt, inner)?,
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
                self.hoist_block(stmt, block)?;
                if let Some(handler) = handler {
                    self.hoist_block(stmt, &handler.body)?;
                }
                if let Some(finalizer) = finalizer {
                    self.hoist_block(stmt, finalizer)?;
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
    fn hoist_block(&mut self, owner: &Stmt, body: &[Stmt]) -> Result<(), SyntaxError> {
        let key = owner.span.start.offset as u64;
        let parent = self.cur().current_block;
        self.push_block(key, parent);
        self.hoist_list(body)?;
        self.state().current_block = parent;
        Ok(())
    }

    /// Declare a `var`: it lives in the function's own scope, not the block's.
    fn declare_var(&mut self, name: &str, def: VarDef) {
        if self.cur().declares_globally() {
            self.declare_global(name, &def);
            return;
        }
        let state = self.state();
        if state.vars.iter().any(|v| v.name == name) {
            return;
        }
        // A `var` belongs to the function scope, never to the enclosing block.
        state.var_block.push(0);
        state.vars.push(def);
    }

    /// Record a top-level script declaration as a global.
    ///
    /// A script's top-level `var`/`let`/`function` create properties of the
    /// global object, not slots in the script's frame, so they show up as closure
    /// variables instead.
    fn declare_global(&mut self, name: &str, def: &VarDef) {
        let state = self.state();
        if state.closure_vars.iter().any(|c| c.name == name) {
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
            break_label,
            continue_label: Some(continue_label),
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

    /// The target a `break`/`continue` refers to: the innermost one, or the
    /// nearest enclosing one with that label.
    fn jump_target(&self, label: Option<&Ident>) -> Option<&JumpTarget> {
        let targets = &self.cur().jump_targets;
        match label {
            None => targets.last(),
            Some(label) => targets
                .iter()
                .rev()
                .find(|target| target.names.contains(&label.name)),
        }
    }

    fn control_transfer(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        let (label, is_break) = match &stmt.kind {
            StmtKind::Break(label) => (label.as_ref(), true),
            StmtKind::Continue(label) => (label.as_ref(), false),
            _ => unreachable!("control_transfer is only called for break and continue"),
        };
        let Some(target) = self.jump_target(label) else {
            // The parser rejects a break/continue with no target, so reaching
            // here means the compiler's own bookkeeping is wrong.
            return Err(SyntaxError::new(
                "break/continue has no enclosing loop",
                stmt.span,
            ));
        };
        let destination = if is_break {
            target.break_label
        } else {
            match target.continue_label {
                Some(label) => label,
                None => {
                    return Err(SyntaxError::new("continue must target a loop", stmt.span));
                }
            }
        };
        self.emit_jump(Opcode::Goto, destination, &[]);
        Ok(())
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
            break_label: end,
            continue_label: None,
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
            break_label,
            // `switch` is a valid `break` target but not a `continue` target.
            continue_label: None,
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

    fn try_stmt(
        &mut self,
        _block: &[Stmt],
        _handler: &Option<CatchClause>,
        _finalizer: Option<&[Stmt]>,
    ) -> Result<(), SyntaxError> {
        Err(SyntaxError::new(
            "try is not supported by the compiler yet",
            Span::start(),
        ))
    }

    fn for_in_of(
        &mut self,
        _left: &ForHead,
        _right: &Expr,
        _body: &Stmt,
        _is_of: bool,
        _is_await: bool,
    ) -> Result<(), SyntaxError> {
        Err(SyntaxError::new(
            "for-in/of is not supported by the compiler yet",
            Span::start(),
        ))
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

    fn expr(&mut self, expr: &Expr) -> Result<(), SyntaxError> {
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
                // The reference folds `-1` into the `push_minus1` short form.
                let ExprKind::Number(value) = argument.unparen().kind else {
                    unreachable!("matched above")
                };
                self.push_number(-value);
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
            ExprKind::Call {
                callee,
                arguments,
                optional,
            } => {
                if *optional {
                    return Err(SyntaxError::new(
                        "optional calls are not supported by the compiler yet",
                        expr.span,
                    ));
                }
                self.expr(callee)?;
                for argument in arguments {
                    self.expr(argument)?;
                }
                self.emit(Opcode::Call, &[Op::Int(arguments.len() as i32)]);
            }
            ExprKind::Member {
                object,
                property,
                optional,
            } => {
                if *optional {
                    return Err(SyntaxError::new(
                        "optional member access is not supported by the compiler yet",
                        expr.span,
                    ));
                }
                self.expr(object)?;
                match property {
                    MemberProp::Ident(name) => {
                        self.emit(Opcode::GetField, &[Op::Atom(name.name.clone())]);
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
            }
            ExprKind::Array { elements } => self.array_literal(elements, expr.span)?,
            ExprKind::Arrow(func) => {
                let function = self.compile_function(func)?;
                let index = self.push_constant(Constant::Function(Box::new(function)));
                self.emit(Opcode::Fclosure, &[Op::Const(index)]);
            }
            ExprKind::Function(func) => {
                let function = self.compile_function(func)?;
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
        self.emit(Opcode::PushAtomValue, &[Op::Atom(text.to_string())]);
    }

    /// Name an anonymous function from the binding it is assigned to.
    ///
    /// `var f = function(){}` produces a function whose `name` is `f`, which the
    /// runtime cannot know without being told: the function object is created
    /// before the store. `set_name` carries the name across.
    fn set_name_if_anonymous(&mut self, value: &Expr, name: &str) {
        if value.is_anonymous_function_definition() {
            self.emit(Opcode::SetName, &[Op::Atom(name.to_string())]);
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

    fn logical(&mut self, op: LogicalOp, left: &Expr, right: &Expr) -> Result<(), SyntaxError> {
        let end = self.new_label();
        self.expr(left)?;
        self.emit(Opcode::Dup, &[]);
        match op {
            LogicalOp::And => self.emit_jump(Opcode::IfFalse, end, &[]),
            LogicalOp::Or => self.emit_jump(Opcode::IfTrue, end, &[]),
            LogicalOp::Coalesce => {
                return Err(SyntaxError::new(
                    "'??' is not supported by the compiler yet",
                    left.span,
                ));
            }
        }
        self.emit(Opcode::Drop, &[]);
        self.expr(right)?;
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
                self.emit(Opcode::PushTrue, &[]);
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

    fn assign(&mut self, op: AssignOp, target: &Pattern, value: &Expr) -> Result<(), SyntaxError> {
        match &target.kind {
            PatternKind::Ident(name) => {
                match op {
                    AssignOp::Assign => {
                        self.expr(value)?;
                        self.set_name_if_anonymous(value, &name.name);
                        self.emit(Opcode::Dup, &[]);
                        self.emit_var(VarAction::Put, &name.name);
                    }
                    _ => {
                        self.emit_var(VarAction::Get, &name.name);
                        self.expr(value)?;
                        self.emit(compound_opcode(op), &[]);
                        self.emit(Opcode::Dup, &[]);
                        self.emit_var(VarAction::Put, &name.name);
                    }
                }
                Ok(())
            }
            PatternKind::Member(member) => {
                self.expr(&member.object)?;
                match &member.property {
                    MemberProp::Ident(property) => {
                        self.expr(value)?;
                        self.emit(Opcode::Insert2, &[]);
                        self.emit(Opcode::PutField, &[Op::Atom(property.name.clone())]);
                    }
                    MemberProp::Computed(index) => {
                        self.expr(index)?;
                        self.expr(value)?;
                        self.emit(Opcode::Insert3, &[]);
                        self.emit(Opcode::PutArrayEl, &[]);
                    }
                    MemberProp::Private(_) => {
                        return Err(SyntaxError::new(
                            "private fields are not supported by the compiler yet",
                            member.span,
                        ));
                    }
                }
                Ok(())
            }
            _ => Err(SyntaxError::new(
                "this assignment target is not supported by the compiler yet",
                target.span,
            )),
        }
    }

    fn update(&mut self, op: UpdateOp, argument: &Expr, prefix: bool) -> Result<(), SyntaxError> {
        let ExprKind::Ident(name) = &argument.kind else {
            return Err(SyntaxError::new(
                "increment of a non-variable is not supported by the compiler yet",
                argument.span,
            ));
        };
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

    fn array_literal(
        &mut self,
        elements: &[Option<ArrayElement>],
        span: Span,
    ) -> Result<(), SyntaxError> {
        if elements.iter().any(|e| e.is_none()) {
            return Err(SyntaxError::new(
                "array elisions are not supported by the compiler yet",
                span,
            ));
        }
        for element in elements {
            match element {
                Some(ArrayElement::Expr(expr)) => self.expr(expr)?,
                Some(ArrayElement::Spread(_)) => {
                    return Err(SyntaxError::new(
                        "array spread is not supported by the compiler yet",
                        span,
                    ));
                }
                None => unreachable!("elisions were rejected above"),
            }
        }
        self.emit(Opcode::ArrayFrom, &[Op::Int(elements.len() as i32)]);
        Ok(())
    }

    // -- functions ---------------------------------------------------------

    fn compile_function(&mut self, func: &ast::Function) -> Result<CompiledFunction, SyntaxError> {
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
        self.stack.push(state);
        let body_key = func.params_span.end.offset as u64;
        self.push_block(body_key, 0);
        self.state().body_block = body_key;
        let body = func
            .body
            .statements()
            .ok_or_else(|| SyntaxError::new("concision bodies are not supported yet", func.span))?;
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
        let mut resolved = resolve(&mut self.stack);
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
        let (code, stack_size) = assemble(&mut resolved, &mut self.stack[current]);
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
    // Block key -> level. Level 0 is the function scope.
    let mut levels: Vec<u64> = vec![0];
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
            .unwrap_or(0);
        level_of(parent)
    };

    // Own bindings per level, in declaration order.
    let mut own: Vec<Vec<usize>> = vec![Vec::new(); levels.len()];
    let keys: Vec<u64> = (0..state.vars.len())
        .map(|index| state.var_block.get(index).copied().unwrap_or(0))
        .collect();
    for (index, key) in keys.into_iter().enumerate() {
        let level = level_of(key);
        state.vars[index].scope_level = level as u32;
        if state.vars[index].is_lexical {
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
fn resolve(stack: &mut [FnState]) -> Vec<Option<(Opcode, Vec<Op>)>> {
    let current = stack.len() - 1;
    // Dead-code elimination, on the symbolic form where labels are still
    // instruction indices. A `break` inside a loop leaves the trailing `goto`
    // unreachable, and a `return` leaves everything after it unreachable; the
    // reference drops both. Doing it here means the emitter never has to think
    // about reachability.
    eliminate_dead_code(stack, current);
    let insns = std::mem::take(&mut stack[current].insns);
    let mut out: Vec<Option<(Opcode, Vec<Op>)>> = Vec::with_capacity(insns.len());
    for insn in &insns {
        match insn {
            Insn::Fixed { op, operands } => {
                let operands = operands
                    .iter()
                    .map(|operand| match operand {
                        Op::Atom(text) => Op::AtomId(intern_atom(&mut stack[current], text)),
                        other => other.clone(),
                    })
                    .collect();
                out.push(Some((*op, operands)));
            }
            Insn::Var { action, name } => {
                let slot = resolve_slot(stack, current, name);
                out.push(Some(variable_instruction(stack, current, *action, slot)));
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
    peephole(&mut out, &label_at, &stack[current].labels);
    out
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
fn resolve_slot(stack: &mut [FnState], current: usize, name: &str) -> Slot {
    if let Some(index) = stack[current].args.iter().position(|a| a.name == name) {
        return Slot::Arg(index as u32);
    }
    if let Some(index) = stack[current].vars.iter().position(|v| v.name == name) {
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
                (ClosureType::Global | ClosureType::GlobalDecl, _) => ClosureType::GlobalRef,
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

    fold_conditional_jumps(insns, label_at, placed);

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
            Insn::Var { .. } | Insn::Dead => {
                worklist.push(index + 1);
                continue;
            }
        };
        match op {
            Opcode::Return | Opcode::ReturnUndef | Opcode::Throw => {}
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
fn intern_atom(state: &mut FnState, name: &str) -> u32 {
    if let Some(index) = state.atoms.iter().position(|a| a == name) {
        return index as u32;
    }
    state.atoms.push(name.to_string());
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
fn assemble(insns: &mut [Option<(Opcode, Vec<Op>)>], state: &mut FnState) -> (Vec<u8>, u32) {
    // Compaction: surviving instructions in order, plus the index mapping.
    let mut kept: Vec<(Opcode, Vec<Op>)> = Vec::with_capacity(insns.len());
    let mut original_to_kept: Vec<Option<usize>> = vec![None; insns.len()];
    for (original, entry) in insns.iter_mut().enumerate() {
        if let Some((op, operands)) = entry.take() {
            original_to_kept[original] = Some(kept.len());
            kept.push((op, operands));
        }
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

    // A label that lands on an unconditional jump stands for wherever that jump
    // goes, so retarget it once and every jump to the label follows for free.
    // This is what makes `continue` in a `for` head reach the loop top instead of
    // stopping at the update's trailing jump.
    let folded: Vec<Option<usize>> = label_kept
        .iter()
        .map(|slot| slot.map(|target| follow_jump_chain(&kept, &label_kept, target)))
        .collect();
    label_kept = folded;

    // A `drop` immediately before `return_undef` throws away a value the return
    // never looks at; `switch`'s discriminant drop is exactly that. Two details
    // keep this matching the reference:
    //
    //   * a label sitting between the drop and the return means they are *not*
    //     adjacent — labels are real pseudo-instructions, so `l: { … b(); }` keeps
    //     the drop that discards `b()`'s result, while `switch (x) { }` drops the
    //     discriminant because nothing separates them;
    //   * one pass, no cascading: removing a drop must not expose another one, or
    //     the value the preceding instruction left behind goes with it.
    let mut label_here = vec![false; kept.len() + 1];
    for slot in label_kept.iter().take(real_labels).flatten() {
        if *slot < label_here.len() {
            label_here[*slot] = true;
        }
    }
    let delete: Vec<bool> = (0..kept.len())
        .map(|index| {
            kept[index].0 == Opcode::Drop
                && kept.get(index + 1).map(|(op, _)| *op) == Some(Opcode::ReturnUndef)
                && !label_here[index + 1]
        })
        .collect();
    if delete.iter().any(|d| *d) {
        kept = compact(kept, &mut label_kept, &delete);
    }

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
        let mut moved: Vec<Option<usize>> = vec![None; kept.len()];
        for (old, entry) in kept.into_iter().enumerate() {
            if delete[old] {
                continue;
            }
            moved[old] = Some(next.len());
            next.push(entry);
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
    }

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
    (code, stack_size)
}

/// Drop the instructions marked in `delete` and renumber the label table so it
/// keeps pointing at the same instructions.
///
/// A label whose instruction was deleted moves to whatever follows it, since the
/// reference never leaves a jump with nowhere to land.
fn compact(
    kept: Vec<(Opcode, Vec<Op>)>,
    label_kept: &mut [Option<usize>],
    delete: &[bool],
) -> Vec<(Opcode, Vec<Op>)> {
    let mut next: Vec<(Opcode, Vec<Op>)> = Vec::with_capacity(kept.len());
    let mut moved: Vec<Option<usize>> = vec![None; kept.len()];
    for (old, entry) in kept.into_iter().enumerate() {
        if delete[old] {
            continue;
        }
        moved[old] = Some(next.len());
        next.push(entry);
    }
    for slot in label_kept.iter_mut() {
        let Some(old) = *slot else { continue };
        *slot = match moved.get(old).copied().flatten() {
            Some(new) => Some(new),
            None => (old..moved.len())
                .find_map(|i| moved[i])
                .or(Some(next.len())),
        };
    }
    next
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
    let mut worklist: Vec<(usize, u32)> = vec![(0, 0)];
    let mut max_depth = 0u32;

    while let Some((pc, depth)) = worklist.pop() {
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

        let depth = depth.saturating_sub(n_pop) + n_push;
        max_depth = max_depth.max(depth);

        // Successors.
        let target = jump_target_of(code, pc, op);
        match op {
            Opcode::Goto | Opcode::Goto8 | Opcode::Goto16 => {
                if let Some(target) = target {
                    worklist.push((target, depth));
                }
            }
            Opcode::IfFalse | Opcode::IfFalse8 | Opcode::IfTrue | Opcode::IfTrue8 => {
                if let Some(target) = target {
                    worklist.push((target, depth));
                }
                worklist.push((next, depth));
            }
            Opcode::Return | Opcode::ReturnUndef | Opcode::Throw => {}
            _ => worklist.push((next, depth)),
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
        U8 | Loc8 | Const8 => {
            if let Some(value) = operands.first().and_then(operand_uint) {
                code.push(value as u8);
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
        Atom => {
            if let Some(Op::AtomId(index)) = operands.first() {
                code.extend_from_slice(&index.to_be_bytes());
            }
        }
        Label | Label8 | Label16 => {
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
                    _ => code.extend_from_slice(&(relative as i32).to_be_bytes()),
                }
            }
        }
        _ => {}
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

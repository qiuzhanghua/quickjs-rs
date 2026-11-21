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
    ClosureType, ClosureVar, Constant, Function as CompiledFunction, FunctionKind, VarDef, VarKind,
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
}

#[derive(Debug, Clone)]
struct LexScope {
    /// Innermost variable declared in this scope, as an index into `vars`.
    first: i32,
    /// Enclosing scope.
    parent: usize,
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
    scopes: Vec<LexScope>,
    scope_level: usize,
    scope_first: i32,
    body_scope: usize,
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
            // Scope 0 is the function scope; the body gets its own child scope.
            scopes: vec![LexScope {
                first: -1,
                parent: 0,
            }],
            scope_level: 0,
            scope_first: -1,
            body_scope: 0,
            insns: Vec::new(),
            labels: Vec::new(),
            var_ref_count: 0,
            line: 1,
            column: 1,
            is_script: false,
            leaves_value: false,
            stmt_depth: 0,
        }
    }

    /// The scope a declaration lands in: a script's top-level declarations are
    /// globals, everything else gets a slot.
    fn declares_globally(&self) -> bool {
        self.is_script && self.scope_level == self.body_scope
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
        self.enter_scope();
        self.state().body_scope = self.cur().scope_level;
        self.hoist(&program.body)?;
        for stmt in &program.body {
            self.stmt(stmt)?;
        }
        // A trailing expression statement *is* the program's value, so its store
        // becomes a `set_` (which keeps the value) and the return needs no load.
        if self.cur().leaves_value
            && let Some(Insn::Fixed { op, operands }) = self.state().insns.last_mut()
            && *op == Opcode::PutLoc0
            && matches!(operands.first(), Some(Op::Slot(Slot::Loc(0))))
        {
            *op = Opcode::SetLoc0;
            self.state().insns.push(Insn::Fixed {
                op: Opcode::Return,
                operands: Vec::new(),
            });
            self.leave_scope();
            let state = self.stack.pop().expect("state");
            return Ok(self.finish(state));
        }
        self.emit(Opcode::GetLoc0, &[Op::Slot(Slot::Loc(0))]);
        self.emit(Opcode::Return, &[]);
        self.leave_scope();
        let state = self.stack.pop().expect("state");
        Ok(self.finish(state))
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
        self.emit(Opcode::PutLoc0, &[Op::Slot(Slot::Loc(0))]);
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

    fn enter_scope(&mut self) {
        let state = self.state();
        let parent = state.scope_level;
        let level = state.scopes.len();
        let first = state.scope_first;
        state.scopes.push(LexScope { first: -1, parent });
        state.scope_level = level;
        let _ = first;
    }

    fn leave_scope(&mut self) {
        let state = self.state();
        let level = state.scope_level;
        if level == 0 {
            return;
        }
        state.scope_level = state.scopes[level].parent;
        state.scope_first = first_lexical_var(state, state.scope_level);
    }

    fn declare(&mut self, name: &str, def: VarDef) -> u32 {
        if self.cur().declares_globally() {
            self.declare_global(name, &def);
            return 0;
        }
        let state = self.state();
        let index = state.vars.len() as u32;
        let mut def = def;
        if def.is_lexical {
            def.scope_next = Some(state.scope_first);
            let level = state.scope_level;
            state.scopes[level].first = index as i32;
            state.scope_first = index as i32;
        }
        state.vars.push(def);
        index
    }

    /// Declare `var` and function names before compiling any statement, so that
    /// a reference textually before the declaration still resolves.
    fn hoist(&mut self, body: &[Stmt]) -> Result<(), SyntaxError> {
        for stmt in body {
            self.hoist_stmt(stmt)?;
        }
        Ok(())
    }

    fn hoist_stmt(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        match &stmt.kind {
            StmtKind::VarDecl(decl) => {
                if decl.kind == VarDeclKind::Var {
                    for declarator in &decl.declarators {
                        let mut names = Vec::new();
                        declarator.target.bound_names(&mut names);
                        for name in names {
                            let text = name.name.clone();
                            let def = VarDef::var(text.clone());
                            self.declare_var(&text, def);
                        }
                    }
                } else {
                    for declarator in &decl.declarators {
                        let mut names = Vec::new();
                        declarator.target.bound_names(&mut names);
                        for name in names {
                            let text = name.name.clone();
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
                    let def = VarDef {
                        kind: VarKind::FunctionDecl,
                        ..VarDef::var(name.name.clone())
                    };
                    self.declare(&name.name.clone(), def);
                }
            }
            StmtKind::ClassDecl(class) => {
                if let Some(name) = &class.name {
                    self.declare(&name.name.clone(), VarDef::lexical(name.name.clone()));
                }
            }
            StmtKind::Block(inner) => self.hoist(inner)?,
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
                if let Some(ForInit::VarDecl(decl)) = init
                    && decl.kind == VarDeclKind::Var
                {
                    self.hoist_stmt(&Stmt::new(StmtKind::VarDecl(decl.clone()), stmt.span))?;
                }
                self.hoist_stmt(body)?;
            }
            StmtKind::ForIn { left, body, .. } | StmtKind::ForOf { left, body, .. } => {
                if let ForHead::VarDecl(decl) = left
                    && decl.kind == VarDeclKind::Var
                {
                    self.hoist_stmt(&Stmt::new(StmtKind::VarDecl(decl.clone()), stmt.span))?;
                }
                self.hoist_stmt(body)?;
            }
            StmtKind::Try {
                block,
                handler,
                finalizer,
            } => {
                self.hoist(block)?;
                if let Some(handler) = handler {
                    self.hoist(&handler.body)?;
                }
                if let Some(finalizer) = finalizer {
                    self.hoist(finalizer)?;
                }
            }
            StmtKind::Switch { cases, .. } => {
                for case in cases {
                    self.hoist(&case.body)?;
                }
            }
            _ => {}
        }
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
        state.vars.push(def);
    }

    /// Record a top-level script declaration as a global.
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

    // -- statements --------------------------------------------------------

    fn stmt(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        // Only a *top-level* expression statement supplies the program's value.
        let saved = self.cur().leaves_value;
        self.state().leaves_value =
            matches!(stmt.kind, StmtKind::Expr(_)) && self.cur().stmt_depth == 0;
        self.state().stmt_depth += 1;
        let result = self.stmt_inner(stmt);
        self.state().stmt_depth -= 1;
        // A nested statement never contributes the program's value, so the flag
        // the wrapper set above is what survives.
        let produced = self.cur().leaves_value && self.cur().stmt_depth == 0;
        self.state().leaves_value = produced;
        let _ = saved;
        result
    }

    fn stmt_inner(&mut self, stmt: &Stmt) -> Result<(), SyntaxError> {
        match &stmt.kind {
            StmtKind::Empty | StmtKind::Debugger => {}
            StmtKind::Expr(expr) => {
                self.expr(expr)?;
                self.emit(Opcode::PutLoc0, &[Op::Slot(Slot::Loc(0))]);
            }
            StmtKind::Block(body) => {
                self.enter_scope();
                self.hoist(body)?;
                for stmt in body {
                    self.stmt(stmt)?;
                }
                self.leave_scope();
            }
            StmtKind::VarDecl(decl) => self.var_decl(decl)?,
            StmtKind::FuncDecl(func) => self.func_decl(func)?,
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
                self.emit_jump(Opcode::IfFalse, end, &[]);
                self.loop_body(body, top, end)?;
                self.emit_jump(Opcode::Goto, top, &[]);
                self.place_label(end);
            }
            StmtKind::DoWhile { body, test } => {
                self.reset_value();
                let top = self.new_label();
                let cont = self.new_label();
                let end = self.new_label();
                self.place_label(top);
                self.loop_body(body, cont, end)?;
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
                self.enter_scope();
                if let Some(init) = init {
                    match init {
                        ForInit::VarDecl(decl) => {
                            self.hoist(&[Stmt::new(StmtKind::VarDecl(decl.clone()), stmt.span)])?;
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
                self.loop_body(body, cont, end)?;
                self.place_label(cont);
                if let Some(update) = update {
                    self.expr(update)?;
                    self.emit(Opcode::Drop, &[]);
                }
                self.emit_jump(Opcode::Goto, top, &[]);
                self.place_label(end);
                self.leave_scope();
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
                self.switch(discriminant, cases)?;
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

    /// Compile a loop body, registering the labels `break` and `continue` jump to.
    fn loop_body(
        &mut self,
        body: &Stmt,
        _continue: Label,
        _break: Label,
    ) -> Result<(), SyntaxError> {
        self.stmt(body)
    }

    fn control_transfer(&mut self, _stmt: &Stmt) -> Result<(), SyntaxError> {
        Err(SyntaxError::new(
            "break/continue are not supported by the compiler yet",
            _stmt.span,
        ))
    }

    fn labeled(&mut self, _label: &Ident, _body: &Stmt, stmt: &Stmt) -> Result<(), SyntaxError> {
        Err(SyntaxError::new(
            "labelled statements are not supported by the compiler yet",
            stmt.span,
        ))
    }

    fn switch(&mut self, _discriminant: &Expr, _cases: &[SwitchCase]) -> Result<(), SyntaxError> {
        Err(SyntaxError::new(
            "switch is not supported by the compiler yet",
            Span::start(),
        ))
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
                // `var x;` with no initialiser keeps `undefined`; a lexical
                // declaration without one was already rejected by the parser.
                continue;
            };
            match &declarator.target.kind {
                PatternKind::Ident(name) => {
                    self.expr(init)?;
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

    fn func_decl(&mut self, func: &ast::Function) -> Result<(), SyntaxError> {
        let function = self.compile_function(func)?;
        let index = self.push_constant(Constant::Function(Box::new(function)));
        self.emit(Opcode::Fclosure, &[Op::Const(index)]);
        let name = func.name.as_ref().expect("a declaration has a name");
        let action = if self.cur().is_script {
            VarAction::PutRef
        } else {
            VarAction::PutInit
        };
        self.emit_var(action, &name.name);
        Ok(())
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
        self.enter_scope();
        let body = func
            .body
            .statements()
            .ok_or_else(|| SyntaxError::new("concision bodies are not supported yet", func.span))?;
        self.hoist(body)?;
        for stmt in body {
            self.stmt(stmt)?;
        }
        // The reference never emits an implicit return after a definite one.
        let reachable = !matches!(
            self.cur().insns.last(),
            Some(Insn::Fixed {
                op: Opcode::Return | Opcode::ReturnUndef | Opcode::Throw,
                ..
            })
        );
        if reachable {
            self.emit(Opcode::ReturnUndef, &[]);
        }
        self.leave_scope();
        let state = self.stack.pop().expect("state");
        Ok(self.finish(state))
    }

    /// Resolve one function's symbolic instructions into final bytecode.
    fn finish(&mut self, mut state: FnState) -> CompiledFunction {
        let resolved = resolve(&mut state);
        let (code, stack_size) = assemble(&resolved, &state);
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

/// Resolve one function's symbolic instructions into final bytecode.
///
/// This is where names become slots. A name is looked up in the current
/// function's arguments, then its locals, then its existing closure entries; a
/// name that is nowhere to be found becomes a new `[global]` closure entry, which
/// is what lets sloppy mode assign to an undeclared variable.
fn resolve(state: &mut FnState) -> Vec<(Opcode, Vec<Op>)> {
    let insns = std::mem::take(&mut state.insns);
    let mut out = Vec::with_capacity(insns.len());
    for insn in &insns {
        match insn {
            Insn::Fixed { op, operands } => {
                let operands = operands
                    .iter()
                    .map(|operand| match operand {
                        Op::Atom(text) => Op::AtomId(intern_atom(state, text)),
                        other => other.clone(),
                    })
                    .collect();
                out.push((*op, operands));
            }
            Insn::Var { action, name } => {
                let slot = resolve_slot(state, name);
                out.push(variable_instruction(*action, slot));
            }
        }
    }
    out
}

/// Find where `name` lives, adding a closure entry if needed.
fn resolve_slot(state: &mut FnState, name: &str) -> Slot {
    if let Some(index) = state.args.iter().position(|a| a.name == name) {
        return Slot::Arg(index as u32);
    }
    if let Some(index) = state.vars.iter().position(|v| v.name == name) {
        return Slot::Loc(index as u32);
    }
    if let Some(index) = state.closure_vars.iter().position(|c| c.name == name) {
        return match state.closure_vars[index].closure_type {
            ClosureType::Global => Slot::Global(index as u32),
            ClosureType::GlobalDecl | ClosureType::ModuleDecl | ClosureType::ModuleImport => {
                Slot::GlobalDecl(index as u32)
            }
            _ => Slot::VarRef(index as u32),
        };
    }
    // Not declared anywhere: a dynamic global lookup.
    state.closure_vars.push(ClosureVar {
        name: name.to_string(),
        is_const: false,
        is_lexical: false,
        closure_type: ClosureType::Global,
        var_idx: 0,
    });
    Slot::Global(state.closure_vars.len() as u32 - 1)
}

fn first_lexical_var(state: &FnState, scope: usize) -> i32 {
    let mut scope = scope;
    loop {
        let entry = &state.scopes[scope];
        if entry.first >= 0 {
            return entry.first;
        }
        if scope == entry.parent {
            return -1;
        }
        scope = entry.parent;
    }
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

fn variable_instruction(action: VarAction, slot: Slot) -> (Opcode, Vec<Op>) {
    use Opcode::*;
    match (action, slot) {
        (VarAction::Get, Slot::Loc(i)) => (GetLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::Get, Slot::Arg(i)) => (GetArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::Get, Slot::VarRef(i)) => (GetVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::Get, Slot::Global(_)) => (GetVar, vec![Op::Slot(slot)]),
        (VarAction::GetUndef, Slot::Loc(i)) => (GetLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::GetUndef, Slot::Arg(i)) => (GetArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::GetUndef, Slot::VarRef(i)) => (GetVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::GetUndef, Slot::Global(_)) => (GetVarUndef, vec![Op::Slot(slot)]),
        (VarAction::Put, Slot::Loc(i)) => (PutLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::Put, Slot::Arg(i)) => (PutArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::Put, Slot::VarRef(i)) => (PutVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::Put, Slot::Global(_)) => (PutVar, vec![Op::Slot(slot)]),
        (VarAction::PutInit, Slot::Loc(i)) => (PutLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::PutInit, Slot::Arg(i)) => (PutArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::PutInit, Slot::VarRef(i)) => (PutVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::PutInit, Slot::Global(_)) => (PutVarInit, vec![Op::Slot(slot)]),
        (VarAction::Set, Slot::Loc(i)) => (SetLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::Set, Slot::Arg(i)) => (SetArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::Set, Slot::VarRef(i)) => (SetVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::Set, Slot::Global(_)) => (PutVar, vec![Op::Slot(slot)]),
        (VarAction::Get, Slot::GlobalDecl(i)) => (GetVar, vec![Op::Slot(Slot::Global(i))]),
        (VarAction::GetUndef, Slot::GlobalDecl(i)) => {
            (GetVarUndef, vec![Op::Slot(Slot::Global(i))])
        }
        (VarAction::Put, Slot::GlobalDecl(i)) => (PutVar, vec![Op::Slot(Slot::Global(i))]),
        (VarAction::PutInit, Slot::GlobalDecl(i)) => (PutVarInit, vec![Op::Slot(Slot::Global(i))]),
        // A function declaration's global binding is a reference cell, because
        // the function object and any closure over it must share one variable.
        (VarAction::PutRef, Slot::GlobalDecl(i)) => (PutVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::PutRef, Slot::Loc(i)) => (PutLoc, vec![Op::Slot(Slot::Loc(i))]),
        (VarAction::PutRef, Slot::Arg(i)) => (PutArg, vec![Op::Slot(Slot::Arg(i))]),
        (VarAction::PutRef, Slot::VarRef(i)) => (PutVarRef, vec![Op::Slot(Slot::VarRef(i))]),
        (VarAction::PutRef, Slot::Global(_)) => (PutVar, vec![Op::Slot(slot)]),
        (VarAction::Set, Slot::GlobalDecl(i)) => (SetVarRef, vec![Op::Slot(Slot::VarRef(i))]),
    }
}

/// Encode the resolved instruction stream, choosing short forms where possible.
fn assemble(insns: &[(Opcode, Vec<Op>)], state: &FnState) -> (Vec<u8>, u32) {
    // First pass: pick an opcode for each instruction and its size, so label
    // offsets can be computed.
    let mut sizes = Vec::with_capacity(insns.len());
    let mut chosen = Vec::with_capacity(insns.len());
    for (op, operands) in insns {
        let op = choose_short_form(*op, operands);
        sizes.push(op.size() as usize);
        chosen.push(op);
    }
    let mut offsets = Vec::with_capacity(insns.len() + 1);
    let mut offset = 0usize;
    for size in &sizes {
        offsets.push(offset);
        offset += size;
    }
    offsets.push(offset);

    let mut code = Vec::with_capacity(offset);
    for (index, (_, operands)) in insns.iter().enumerate() {
        let op = chosen[index];
        code.push(op as u8);
        let position = offsets[index];
        encode_operands(&mut code, op, operands, position, &offsets, state);
    }
    let stack_size = compute_stack_size(&code);
    (code, stack_size)
}

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
        | (GetVarRef | PutVarRef | SetVarRef, Some(Op::Slot(Slot::VarRef(index)))) => {
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
    state: &FnState,
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
                let Some(index) = state.labels[label.0 as usize] else {
                    debug_assert!(false, "unplaced label {}", label.0);
                    return;
                };
                let target = offsets[index];
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

/// Intern `name` in the function's atom table.
fn intern_atom(state: &mut FnState, name: &str) -> u32 {
    if let Some(index) = state.atoms.iter().position(|a| a == name) {
        return index as u32;
    }
    state.atoms.push(name.to_string());
    state.atoms.len() as u32 - 1
}

/// Disassemble a compiled function.
pub fn disassemble(function: &CompiledFunction) -> String {
    disasm::disassemble_program(function)
}

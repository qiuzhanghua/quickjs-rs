//! The ECMAScript 2025 abstract syntax tree.
//!
//! The tree is a straightforward Box/Vec tree: the compiler (M2) walks it once
//! per function body and immediately lowers it to bytecode, so an arena or
//! visitor infrastructure would not pay for itself yet.
//!
//! Naming follows the specification: `Script`/`Module` bodies, `StatementListItem`
//! (a statement or a declaration), `BindingIdentifier`, `AssignmentTarget`,
//! `PropertyDefinition`, `ClassStaticBlock`, … Where the spec covers two
//! productions with one node (a function declaration and a function expression,
//! a script and a module), the node carries a discriminant instead of being
//! duplicated.

use crate::token::{Position, Punct, Span};
use std::fmt;

/// A whole program: a script or a module.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Script or module.
    pub kind: ProgramKind,
    /// Top-level statements (`ScriptBody` / `ModuleItemList`).
    pub body: Vec<Stmt>,
    /// Whole-source range.
    pub span: Span,
    /// Whether the program is strict (modules always are).
    pub strict: bool,
}

/// Which grammar production a [`Program`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramKind {
    /// `Script`
    Script,
    /// `Module`
    Module,
}

/// An identifier (binding, reference, label or private name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ident {
    /// Name as written, escapes already decoded.
    pub name: String,
    /// Source range.
    pub span: Span,
}

impl Ident {
    /// Shorthand for tests.
    pub fn new(name: impl Into<String>) -> Ident {
        Ident {
            name: name.into(),
            span: Span::default(),
        }
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

/// A statement or declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    /// Statement payload.
    pub kind: StmtKind,
    /// Source range.
    pub span: Span,
}

impl Stmt {
    /// Build a statement node.
    pub fn new(kind: StmtKind, span: Span) -> Stmt {
        Stmt { kind, span }
    }

    /// Whether this is an empty statement (`;`) or a block containing only
    /// empty declarations, which ASI treats specially.
    pub fn is_empty(&self) -> bool {
        matches!(self.kind, StmtKind::Empty)
    }
}

/// Statement and declaration nodes (`StatementListItem`).
#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// `;`
    Empty,
    /// `{ ... }`
    Block(Vec<Stmt>),
    /// `expr;`
    Expr(Box<Expr>),
    /// `if (test) cons else alt`
    If {
        /// Condition.
        test: Box<Expr>,
        /// Then branch.
        cons: Box<Stmt>,
        /// Else branch.
        alt: Option<Box<Stmt>>,
    },
    /// `do body while (test);`
    DoWhile {
        /// Loop body.
        body: Box<Stmt>,
        /// Loop condition.
        test: Box<Expr>,
    },
    /// `while (test) body`
    While {
        /// Loop condition.
        test: Box<Expr>,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `for (init; test; update) body`
    For {
        /// `null` for `for (;;)`, otherwise a var/let/const declaration or an
        /// expression.
        init: Option<ForInit>,
        /// Loop condition.
        test: Option<Box<Expr>>,
        /// Per-iteration update expression.
        update: Option<Box<Expr>>,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `for (decl|lhs in expr) body`
    ForIn {
        /// Left-hand side.
        left: ForHead,
        /// Enumerated object.
        right: Box<Expr>,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `for (decl|lhs of expr) body`, plus `for await (…)`
    ForOf {
        /// Whether `await` was present.
        is_await: bool,
        /// Left-hand side.
        left: ForHead,
        /// Iterated value.
        right: Box<Expr>,
        /// Loop body.
        body: Box<Stmt>,
    },
    /// `continue;` / `continue label;`
    Continue(Option<Ident>),
    /// `break;` / `break label;`
    Break(Option<Ident>),
    /// `return;` / `return expr;`
    Return(Option<Box<Expr>>),
    /// `with (obj) body`
    With {
        /// Object to put on the scope chain.
        object: Box<Expr>,
        /// Statement executed inside the `with`.
        body: Box<Stmt>,
    },
    /// `label: stmt`
    Labeled {
        /// Label name.
        label: Ident,
        /// Labelled statement.
        body: Box<Stmt>,
    },
    /// `switch (disc) { cases }`
    Switch {
        /// Discriminant.
        discriminant: Box<Expr>,
        /// Cases in source order.
        cases: Vec<SwitchCase>,
    },
    /// `throw expr;`
    Throw(Box<Expr>),
    /// `try { } catch (e) { } finally { }`
    Try {
        /// Guarded block.
        block: Vec<Stmt>,
        /// Catch clause, if present.
        handler: Option<CatchClause>,
        /// Finalizer block, if present.
        finalizer: Option<Vec<Stmt>>,
    },
    /// `debugger;`
    Debugger,
    /// `var`/`let`/`const` declaration.
    VarDecl(VarDecl),
    /// `function` declaration.
    FuncDecl(Box<Function>),
    /// `class` declaration.
    ClassDecl(Box<Class>),
    /// `import` declaration (modules only).
    Import(Box<ImportDecl>),
    /// `export` declaration (modules only).
    Export(Box<ExportDecl>),
}

/// One `case`/`default` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// Case selector; `None` for `default`.
    pub test: Option<Box<Expr>>,
    /// Statements of the clause.
    pub body: Vec<Stmt>,
    /// Source range of the whole clause.
    pub span: Span,
}

/// The `catch` half of a `try` statement.
#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    /// Bound error, if any (`catch { }` has none).
    pub param: Option<Pattern>,
    /// Handler body.
    pub body: Vec<Stmt>,
    /// Source range.
    pub span: Span,
}

/// The `init` slot of a classic `for` statement.
#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    /// `for (var i = 0; …)`
    VarDecl(VarDecl),
    /// `for (i = 0; …)`
    Expr(Box<Expr>),
}

/// The left-hand side of a `for-in`/`for-of` head.
#[derive(Debug, Clone, PartialEq)]
pub enum ForHead {
    /// `for (let x of …)`, including single-declarator declarations.
    VarDecl(VarDecl),
    /// `for (x of …)`, `for ([a, b] of …)`, `for (obj.x of …)`.
    Assign(Box<Pattern>),
}

/// `var`/`let`/`const`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarDeclKind {
    /// `var`
    Var,
    /// `let`
    Let,
    /// `const`
    Const,
    /// `using`
    ///
    /// A lexical binding whose value is disposed at the end of the enclosing block. The
    /// await form is a separate variant so that the await-ness travels with the
    /// declaration the way the other keywords do.
    Using,
    /// `await using`
    AwaitUsing,
}

impl VarDeclKind {
    /// The keyword.
    pub fn as_str(self) -> &'static str {
        match self {
            VarDeclKind::Var => "var",
            VarDeclKind::Let => "let",
            VarDeclKind::Const => "const",
            // A message that uses this only names the binding kind, and `await using` is
            // still a `using`.
            VarDeclKind::Using | VarDeclKind::AwaitUsing => "using",
        }
    }

    /// Whether the declaration is block-scoped.
    pub fn is_lexical(self) -> bool {
        !matches!(self, VarDeclKind::Var)
    }

    /// Whether this declaration disposes its binding at the end of the block.
    pub fn is_using(self) -> bool {
        matches!(self, VarDeclKind::Using | VarDeclKind::AwaitUsing)
    }

    /// Whether the disposal is awaited.
    pub fn is_await_using(self) -> bool {
        matches!(self, VarDeclKind::AwaitUsing)
    }
}

/// A declaration statement, with all of its declarators.
#[derive(Debug, Clone, PartialEq)]
pub struct VarDecl {
    /// `var`, `let` or `const`.
    pub kind: VarDeclKind,
    /// Declarators in source order.
    pub declarators: Vec<Declarator>,
    /// Source range.
    pub span: Span,
}

/// One `pattern = init` declarator.
#[derive(Debug, Clone, PartialEq)]
pub struct Declarator {
    /// Bound names.
    pub target: Pattern,
    /// Initialiser.
    pub init: Option<Box<Expr>>,
    /// Source range.
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Functions and classes
// ---------------------------------------------------------------------------

/// A function in any of its forms (declaration, expression, arrow, method).
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// Name: required for declarations, optional for expressions and arrows.
    pub name: Option<Ident>,
    /// Parameters, in source order; a rest parameter is [`Pattern::Rest`].
    pub params: Vec<Pattern>,
    /// Body.
    pub body: FunctionBody,
    /// `async`
    pub is_async: bool,
    /// `function*` / `*method` / `async function*`
    pub is_generator: bool,
    /// Whether the body is strict: either an enclosing strict context or the
    /// function's own `"use strict"` directive.
    ///
    /// Recorded per function because strictness is not a property of the source but of
    /// the enclosing context: a `"use strict"` inside one function body leaves the code
    /// after it sloppy, so the compiler cannot infer a body's mode from the program's.
    pub strict: bool,
    /// `=>`
    pub is_arrow: bool,
    /// Source range.
    pub span: Span,
    /// Range of the parameter list, for error messages and `length`.
    pub params_span: Span,
}

/// The body of a [`Function`].
#[derive(Debug, Clone, PartialEq)]
pub enum FunctionBody {
    /// `{ … }`
    Block(Vec<Stmt>),
    /// An arrow's expression body (no braces).
    Expr(Box<Expr>),
}

impl FunctionBody {
    /// Statements of a block body, if this is one.
    pub fn statements(&self) -> Option<&[Stmt]> {
        match self {
            FunctionBody::Block(body) => Some(body),
            FunctionBody::Expr(_) => None,
        }
    }
}

/// A class body.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    /// Class name: required for declarations, optional for expressions.
    pub name: Option<Ident>,
    /// `extends` clause.
    pub super_class: Option<Box<Expr>>,
    /// Members, in source order.
    pub elements: Vec<ClassElement>,
    /// Source range.
    pub span: Span,
}

/// One member of a class body.
#[derive(Debug, Clone, PartialEq)]
pub enum ClassElement {
    /// A method (`constructor`, ordinary, `get`, `set`, generator or async).
    Method {
        /// Property key.
        key: PropName,
        /// `static`
        is_static: bool,
        /// Which kind of method this is.
        kind: MethodKind,
        /// The method itself.
        func: Box<Function>,
        /// Source range.
        span: Span,
    },
    /// A field (`x = 1`, `#x`, `static x`).
    Field {
        /// Property key.
        key: PropName,
        /// `static`
        is_static: bool,
        /// Initialiser expression.
        value: Option<Box<Expr>>,
        /// Source range.
        span: Span,
    },
    /// `static { … }`
    StaticBlock {
        /// Block body.
        body: Vec<Stmt>,
        /// Source range.
        span: Span,
    },
}

/// Which kind of method a class member or object literal property defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    /// `constructor`
    Constructor,
    /// An ordinary method.
    Method,
    /// `get name()`
    Get,
    /// `set name(v)`
    Set,
}

/// A property key.
#[derive(Debug, Clone, PartialEq)]
pub enum PropName {
    /// A bare identifier, string or numeric key. The variant records which
    /// spelling was used, because `{ 1e2: x }` needs the numeric value while
    /// `{ "1e2": x }` needs the string.
    Literal(PropKey),
    /// `[expr]`
    Computed(Box<Expr>),
    /// `#name` (classes only).
    Private(Ident),
}

impl PropName {
    /// Static name of the key, when it is not computed.
    pub fn static_name(&self) -> Option<PropKeyRef<'_>> {
        match self {
            PropName::Literal(k) => Some(k.as_ref()),
            PropName::Private(id) => Some(PropKeyRef::Private(&id.name)),
            PropName::Computed(_) => None,
        }
    }
}

/// A literal property key.
#[derive(Debug, Clone, PartialEq)]
pub enum PropKey {
    /// `foo` or `"foo"` — identical once the value is interned.
    Str(String),
    /// `1`, `1.5`, `0x10` — the numeric value matters for array indexing.
    Num(f64),
}

impl PropKey {
    /// Borrow as [`PropKeyRef`].
    pub fn as_ref(&self) -> PropKeyRef<'_> {
        match self {
            PropKey::Str(s) => PropKeyRef::Str(s),
            PropKey::Num(n) => PropKeyRef::Num(*n),
        }
    }
}

/// Borrowed view of a [`PropKey`], used by the compiler.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PropKeyRef<'a> {
    /// A string key.
    Str(&'a str),
    /// A numeric key.
    Num(f64),
    /// A private name.
    Private(&'a str),
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

/// A binding pattern or assignment target.
#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    /// Pattern payload.
    pub kind: PatternKind,
    /// Source range.
    pub span: Span,
}

impl Pattern {
    /// Build a pattern node.
    pub fn new(kind: PatternKind, span: Span) -> Pattern {
        Pattern { kind, span }
    }

    /// Collect every name bound by this pattern, in source order.
    pub fn bound_names(&self, out: &mut Vec<Ident>) {
        match &self.kind {
            PatternKind::Ident(id) => out.push(id.clone()),
            PatternKind::Array { elements, rest, .. }
            | PatternKind::Object {
                properties: elements,
                rest,
                ..
            } => {
                for element in elements.iter().flatten() {
                    match element {
                        PatternElement::Pattern(p) => p.bound_names(out),
                        PatternElement::Property { value, .. } => value.bound_names(out),
                    }
                }
                if let Some(rest) = rest {
                    match &**rest {
                        RestElement::Pattern(p) => p.bound_names(out),
                        RestElement::Member(m) => m.bound_names(out),
                    }
                }
            }
            PatternKind::Assign { target, .. } => target.bound_names(out),
            PatternKind::Rest(target) => target.bound_names(out),
            PatternKind::Invalid(_) => {}
            PatternKind::Member(m) => m.bound_names(out),
        }
    }
}

/// Pattern payloads.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
    /// A binding identifier.
    Ident(Ident),
    /// `[a, , b, ...rest]`
    Array {
        /// Element patterns; `None` for an elision (`,,`).
        elements: Vec<Option<PatternElement>>,
        /// Trailing rest element.
        rest: Option<Box<RestElement>>,
    },
    /// `{ a, b: c, [k]: d, ...rest }`
    Object {
        /// Properties, in source order.
        properties: Vec<Option<PatternElement>>,
        /// Trailing rest element.
        rest: Option<Box<RestElement>>,
    },
    /// `target = default`
    Assign {
        /// Pattern being defaulted.
        target: Box<Pattern>,
        /// Default value expression.
        default: Box<Expr>,
    },
    /// `obj.prop` / `obj[k]` — only valid as an assignment target, never as a
    /// binding pattern.
    Member(Box<MemberPattern>),
    /// `...target` — a rest element of a parameter list or pattern.
    Rest(Box<Pattern>),
    /// An assignment target that is not assignable (`a() = 1`). QuickJS rejects
    /// this at run time rather than at parse time, so the compiler has to raise
    /// the error while evaluating the expression.
    Invalid(Box<Expr>),
}

/// An element of an array or object pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum PatternElement {
    /// A nested pattern (array element, or an object property's value).
    Pattern(Pattern),
    /// An object property with an explicit key.
    Property {
        /// Property key.
        key: PropName,
        /// Bound pattern.
        value: Pattern,
        /// Whether the source used shorthand (`{a}` versus `{a: a}`).
        shorthand: bool,
        /// Source range.
        span: Span,
    },
}

/// `...` in a pattern.
#[derive(Debug, Clone, PartialEq)]
pub enum RestElement {
    /// `...rest` where `rest` is a binding/assignment pattern.
    Pattern(Pattern),
    /// `...[a, b]` style member target inside an assignment pattern.
    Member(Box<MemberPattern>),
}

/// A member expression used as an assignment target.
#[derive(Debug, Clone, PartialEq)]
pub struct MemberPattern {
    /// Object being accessed.
    pub object: Box<Expr>,
    /// Property selector.
    pub property: MemberProp,
    /// Source range.
    pub span: Span,
    /// Where the selector punctuator (`.` or `[`) sits, carried over from the
    /// expression this pattern came from.
    ///
    /// A store is blamed on that token, not on the `=` and not on the base object, so
    /// the compiler needs the position after the expression has been turned into a
    /// pattern.
    pub op_pos: Option<Position>,
}

impl MemberPattern {
    /// Member targets never bind names; provided so patterns can be walked
    /// uniformly.
    pub fn bound_names(&self, _out: &mut Vec<Ident>) {}
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

/// An expression.
#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    /// Expression payload.
    pub kind: ExprKind,
    /// Source range.
    pub span: Span,
    /// Where the operator or punctuator that produced this node sits.
    ///
    /// Stack traces report the position of the token that failed, not the start of
    /// the enclosing expression: `1 + null[0]` blames the `[`, and `o.foo = 1` blames
    /// the `.`. `span.start` cannot stand in for it — the base expression starts
    /// earlier — so the parser records the token here for the node kinds that emit an
    /// instruction at an operator, and the compiler puts it in the position table.
    pub op_pos: Option<Position>,
}

impl Expr {
    /// Build an expression node.
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr {
            kind,
            span,
            op_pos: None,
        }
    }

    /// Build an expression node whose failing instruction sits at `pos`.
    pub fn at_operator(kind: ExprKind, span: Span, pos: Position) -> Expr {
        Expr {
            kind,
            span,
            op_pos: Some(pos),
        }
    }

    /// Strip any `( … )` wrappers, which carry no runtime meaning.
    pub fn unparen(&self) -> &Expr {
        let mut expr = self;
        while let ExprKind::Paren(inner) = &expr.kind {
            expr = inner;
        }
        expr
    }

    /// Whether this expression is an anonymous function definition, which takes
    /// its name from the binding it is assigned to (`var f = function(){}`).
    pub fn is_anonymous_function_definition(&self) -> bool {
        match &self.kind {
            ExprKind::Function(f) => f.name.is_none() && !f.is_arrow,
            ExprKind::Arrow(_) => false,
            ExprKind::Class(c) => c.name.is_none(),
            _ => false,
        }
    }
}

/// Expression payloads.
#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// An identifier reference (not resolved to a binding yet).
    Ident(Ident),
    /// `null`
    Null,
    /// `true` / `false`
    Bool(bool),
    /// `1.5`
    Number(f64),
    /// `1n`
    BigInt {
        /// Digits without prefix or separators.
        digits: String,
        /// 2, 8, 10 or 16.
        radix: u32,
    },
    /// `"text"`
    String(String),
    /// `/pattern/flags`
    RegExp {
        /// Pattern source.
        pattern: String,
        /// Flag letters.
        flags: String,
    },
    /// `this`
    This,
    /// `[a, b, ...c]`
    Array {
        /// Elements; `None` for an elision.
        elements: Vec<Option<ArrayElement>>,
    },
    /// `{ a, b: c, ...d }`
    Object(Vec<ObjectProperty>),
    /// `function name() {}`
    Function(Box<Function>),
    /// `(params) => body`
    Arrow(Box<Function>),
    /// `class { }`
    Class(Box<Class>),
    /// `` `a${b}c` ``
    Template(Box<TemplateLiteral>),
    /// `` tag`a${b}c` ``
    TaggedTemplate {
        /// Tag function.
        tag: Box<Expr>,
        /// Template parts.
        quasi: Box<TemplateLiteral>,
    },
    /// `a.b`, `a[b]`, `a?.b`, `super.x`
    Member {
        /// Base object.
        object: Box<Expr>,
        /// Selector.
        property: MemberProp,
        /// Whether `?.` was used.
        optional: bool,
    },
    /// `f(args)`
    Call {
        /// Callee.
        callee: Box<Expr>,
        /// Arguments.
        arguments: Vec<Expr>,
        /// Whether `?.()` was used.
        optional: bool,
    },
    /// `new C(args)` / `new C`
    New {
        /// Constructor.
        callee: Box<Expr>,
        /// Arguments; `None` for `new C` without a list.
        arguments: Option<Vec<Expr>>,
    },
    /// `new.target`
    NewTarget,
    /// `import.meta`
    ImportMeta,
    /// `import(specifier)`
    ImportCall(Box<Expr>),
    /// `super()` — only valid in a derived constructor.
    SuperCall(Vec<Expr>),
    /// `super.x` / `super[x]` base (never standalone).
    SuperProp,
    /// `op expr`
    Unary {
        /// Operator.
        op: UnaryOp,
        /// Operand.
        argument: Box<Expr>,
    },
    /// `++x`, `x--`
    Update {
        /// Operator.
        op: UpdateOp,
        /// Operand.
        argument: Box<Expr>,
        /// Whether the operator came before the operand.
        prefix: bool,
    },
    /// `a + b`, `a instanceof b`, `a in b`, …
    Binary {
        /// Operator.
        op: BinaryOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `a && b`, `a || b`, `a ?? b` (kept separate for mixing rules and
    /// short-circuit code generation).
    Logical {
        /// Operator.
        op: LogicalOp,
        /// Left operand.
        left: Box<Expr>,
        /// Right operand.
        right: Box<Expr>,
    },
    /// `a ? b : c`
    Conditional {
        /// Condition.
        test: Box<Expr>,
        /// Consequent.
        cons: Box<Expr>,
        /// Alternate.
        alt: Box<Expr>,
    },
    /// `target op= value`
    Assign {
        /// Assignment operator.
        op: AssignOp,
        /// Assignment target (converted from the cover grammar).
        target: Box<Pattern>,
        /// Right-hand side.
        value: Box<Expr>,
    },
    /// `(expr)` — kept as a node so that early errors can tell `(a ?? b) || c`
    /// (legal) from `a ?? b || c` (SyntaxError), and `(a) = 1` from `(a, b) = 1`.
    Paren(Box<Expr>),
    /// `(a, b, c)`
    Sequence(Vec<Expr>),
    /// `yield expr` / `yield* expr`
    Yield {
        /// Operand; `None` for `yield;`.
        argument: Option<Box<Expr>>,
        /// Whether `yield*` was used.
        delegate: bool,
    },
    /// `await expr`
    Await(Box<Expr>),
    /// `...expr` — only valid in call arguments, arrays and object literals.
    Spread(Box<Expr>),
    /// A member/call chain that contains at least one `?.`. Compiling the
    /// wrapper short-circuits the whole spine when a link's base is nullish.
    OptionalChain(Box<Expr>),
    /// `#x in obj`
    PrivateIn {
        /// Private name.
        name: Ident,
        /// Tested object.
        object: Box<Expr>,
    },
}

/// One element of an array literal.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElement {
    /// An ordinary element.
    Expr(Box<Expr>),
    /// `...iterable`
    Spread(Box<Expr>),
}

/// What an object-literal property does with its value.
///
/// Distinct from [`MethodKind`] on purpose: an object literal can never contain a
/// `constructor`, and the compiler needs to tell `{a: 1}` from `{a() {}}` — the two
/// compile to different instructions (`define_field` vs `define_method`), even though
/// both hold a value on the stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// `key: value` or the `{key}` shorthand — stores a data property.
    Value,
    /// `m() {}` — defines a method.
    Method,
    /// `get m() {}` — the value is the getter.
    Get,
    /// `set m(v) {}` — the value is the setter.
    Set,
}

/// One property of an object literal.
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectProperty {
    /// `key: value`, `key`, `get key() {}`, `[key]: value`, …
    Property {
        /// Property key.
        key: PropName,
        /// Value expression; for `get`/`set` it is the function.
        value: Box<Expr>,
        /// Whether this stores a value or defines an accessor/method.
        kind: PropertyKind,
        /// Whether the source used shorthand (`{a}` / `{a = 1}`).
        shorthand: bool,
        /// Source range.
        span: Span,
    },
    /// `...spread`
    Spread(Box<Expr>),
}

/// A template literal: `quasis.len() == expressions.len() + 1`.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateLiteral {
    /// Literal chunks, in source order.
    pub quasis: Vec<TemplateChunk>,
    /// Substitution expressions, in source order.
    pub expressions: Vec<Expr>,
}

/// One chunk of a [`TemplateLiteral`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateChunk {
    /// Escape-expanded text; `None` when the chunk has an invalid escape
    /// sequence (legal in tagged templates only, where it becomes `undefined`).
    pub cooked: Option<String>,
    /// Source text, verbatim.
    pub raw: String,
    /// Whether this chunk ends with the closing backtick.
    pub tail: bool,
}

/// Property selector of a member expression.
#[derive(Debug, Clone, PartialEq)]
pub enum MemberProp {
    /// `.name`
    Ident(Ident),
    /// `[expr]`
    Computed(Box<Expr>),
    /// `.#name`
    Private(Ident),
}

/// Unary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-`
    Minus,
    /// `+`
    Plus,
    /// `!`
    Not,
    /// `~`
    BitNot,
    /// `typeof`
    Typeof,
    /// `void`
    Void,
    /// `delete`
    Delete,
}

impl UnaryOp {
    /// The operator text.
    pub fn as_str(self) -> &'static str {
        match self {
            UnaryOp::Minus => "-",
            UnaryOp::Plus => "+",
            UnaryOp::Not => "!",
            UnaryOp::BitNot => "~",
            UnaryOp::Typeof => "typeof",
            UnaryOp::Void => "void",
            UnaryOp::Delete => "delete",
        }
    }
}

/// `++`/`--`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOp {
    /// `++`
    Increment,
    /// `--`
    Decrement,
}

impl UpdateOp {
    /// The operator text.
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateOp::Increment => "++",
            UpdateOp::Decrement => "--",
        }
    }
}

/// Non-short-circuiting binary operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    /// `==`
    Eq,
    /// `!=`
    NotEq,
    /// `===`
    StrictEq,
    /// `!==`
    StrictNotEq,
    /// `<`
    Lt,
    /// `<=`
    LtEq,
    /// `>`
    Gt,
    /// `>=`
    GtEq,
    /// `<<`
    Shl,
    /// `>>`
    Shr,
    /// `>>>`
    UShr,
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `/`
    Div,
    /// `%`
    Mod,
    /// `**`
    Pow,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `&`
    BitAnd,
    /// `in`
    In,
    /// `instanceof`
    Instanceof,
}

impl BinaryOp {
    /// The operator text.
    pub fn as_str(self) -> &'static str {
        match self {
            BinaryOp::Eq => "==",
            BinaryOp::NotEq => "!=",
            BinaryOp::StrictEq => "===",
            BinaryOp::StrictNotEq => "!==",
            BinaryOp::Lt => "<",
            BinaryOp::LtEq => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::GtEq => ">=",
            BinaryOp::Shl => "<<",
            BinaryOp::Shr => ">>",
            BinaryOp::UShr => ">>>",
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Mod => "%",
            BinaryOp::Pow => "**",
            BinaryOp::BitOr => "|",
            BinaryOp::BitXor => "^",
            BinaryOp::BitAnd => "&",
            BinaryOp::In => "in",
            BinaryOp::Instanceof => "instanceof",
        }
    }

    /// Whether the operator is relational (`in`/`instanceof` included), which
    /// matters for the `no-in` restriction in `for` heads.
    pub fn is_relational(self) -> bool {
        matches!(
            self,
            BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq
                | BinaryOp::In
                | BinaryOp::Instanceof
        )
    }
}

/// Short-circuiting operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOp {
    /// `&&`
    And,
    /// `||`
    Or,
    /// `??`
    Coalesce,
}

impl LogicalOp {
    /// The operator text.
    pub fn as_str(self) -> &'static str {
        match self {
            LogicalOp::And => "&&",
            LogicalOp::Or => "||",
            LogicalOp::Coalesce => "??",
        }
    }
}

/// Assignment operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignOp {
    /// `=`
    Assign,
    /// `+=`
    Add,
    /// `-=`
    Sub,
    /// `*=`
    Mul,
    /// `/=`
    Div,
    /// `%=`
    Mod,
    /// `**=`
    Pow,
    /// `<<=`
    Shl,
    /// `>>=`
    Shr,
    /// `>>>=`
    UShr,
    /// `|=`
    BitOr,
    /// `^=`
    BitXor,
    /// `&=`
    BitAnd,
    /// `&&=`
    And,
    /// `||=`
    Or,
    /// `??=`
    Coalesce,
}

impl AssignOp {
    /// The operator text.
    pub fn as_str(self) -> &'static str {
        match self {
            AssignOp::Assign => "=",
            AssignOp::Add => "+=",
            AssignOp::Sub => "-=",
            AssignOp::Mul => "*=",
            AssignOp::Div => "/=",
            AssignOp::Mod => "%=",
            AssignOp::Pow => "**=",
            AssignOp::Shl => "<<=",
            AssignOp::Shr => ">>=",
            AssignOp::UShr => ">>>=",
            AssignOp::BitOr => "|=",
            AssignOp::BitXor => "^=",
            AssignOp::BitAnd => "&=",
            AssignOp::And => "&&=",
            AssignOp::Or => "||=",
            AssignOp::Coalesce => "??=",
        }
    }

    /// Whether this is a simple `=`.
    pub fn is_simple(self) -> bool {
        matches!(self, AssignOp::Assign)
    }

    /// The binary operator a compound assignment implies.
    pub fn binary_op(self) -> Option<BinaryOp> {
        Some(match self {
            AssignOp::Assign => return None,
            AssignOp::Add => BinaryOp::Add,
            AssignOp::Sub => BinaryOp::Sub,
            AssignOp::Mul => BinaryOp::Mul,
            AssignOp::Div => BinaryOp::Div,
            AssignOp::Mod => BinaryOp::Mod,
            AssignOp::Pow => BinaryOp::Pow,
            AssignOp::Shl => BinaryOp::Shl,
            AssignOp::Shr => BinaryOp::Shr,
            AssignOp::UShr => BinaryOp::UShr,
            AssignOp::BitOr => BinaryOp::BitOr,
            AssignOp::BitXor => BinaryOp::BitXor,
            AssignOp::BitAnd => BinaryOp::BitAnd,
            AssignOp::And | AssignOp::Or | AssignOp::Coalesce => return None,
        })
    }

    /// The logical operator a short-circuiting assignment implies.
    pub fn logical_op(self) -> Option<LogicalOp> {
        match self {
            AssignOp::And => Some(LogicalOp::And),
            AssignOp::Or => Some(LogicalOp::Or),
            AssignOp::Coalesce => Some(LogicalOp::Coalesce),
            _ => None,
        }
    }

    /// Map a punctuator to an assignment operator.
    pub fn from_punct(p: Punct) -> Option<AssignOp> {
        Some(match p {
            Punct::Eq => AssignOp::Assign,
            Punct::PlusEq => AssignOp::Add,
            Punct::MinusEq => AssignOp::Sub,
            Punct::StarEq => AssignOp::Mul,
            Punct::SlashEq => AssignOp::Div,
            Punct::PercentEq => AssignOp::Mod,
            Punct::StarStarEq => AssignOp::Pow,
            Punct::ShlEq => AssignOp::Shl,
            Punct::ShrEq => AssignOp::Shr,
            Punct::UShrEq => AssignOp::UShr,
            Punct::PipeEq => AssignOp::BitOr,
            Punct::CaretEq => AssignOp::BitXor,
            Punct::AmpEq => AssignOp::BitAnd,
            Punct::AmpAmpEq => AssignOp::And,
            Punct::PipePipeEq => AssignOp::Or,
            Punct::QuestionQuestionEq => AssignOp::Coalesce,
            _ => return None,
        })
    }
}

// ---------------------------------------------------------------------------
// Modules
// ---------------------------------------------------------------------------

/// The name on either side of a module binding, which may be a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleName {
    /// `foo`
    Ident(Ident),
    /// `"foo bar"`
    Str(String),
}

impl ModuleName {
    /// The textual name.
    pub fn as_str(&self) -> &str {
        match self {
            ModuleName::Ident(id) => &id.name,
            ModuleName::Str(s) => s,
        }
    }
}

/// `import …`
#[derive(Debug, Clone, PartialEq)]
pub struct ImportDecl {
    /// What is imported.
    pub specifiers: Vec<ImportSpecifier>,
    /// Module specifier string.
    pub source: String,
    /// `with { type: "json" }` attributes.
    pub attributes: Vec<ImportAttribute>,
    /// Source range.
    pub span: Span,
}

/// One entry of an import clause.
#[derive(Debug, Clone, PartialEq)]
pub enum ImportSpecifier {
    /// `import d from "m"`
    Default(Ident),
    /// `import * as ns from "m"`
    Namespace(Ident),
    /// `import { a, b as c } from "m"`
    Named {
        /// Name in the exporting module.
        imported: ModuleName,
        /// Local binding.
        local: Ident,
    },
}

/// `with { key: "value" }`
#[derive(Debug, Clone, PartialEq)]
pub struct ImportAttribute {
    /// Attribute key.
    pub key: String,
    /// Attribute value.
    pub value: String,
    /// Source range.
    pub span: Span,
}

/// `export …`
#[derive(Debug, Clone, PartialEq)]
pub struct ExportDecl {
    /// What is exported.
    pub kind: ExportKind,
    /// Source range.
    pub span: Span,
}

/// The variants of an export declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum ExportKind {
    /// `export var/let/const/function/class …`
    Declaration(Box<Stmt>),
    /// `export default expr` / `export default function …` / `export default class …`
    Default(Box<Expr>),
    /// `export { a, b as c }` / `export { a } from "m"`
    Named {
        /// Exported bindings.
        specifiers: Vec<ExportSpecifier>,
        /// Re-export source.
        source: Option<String>,
        /// Import attributes for the `from` clause.
        attributes: Vec<ImportAttribute>,
    },
    /// `export * from "m"` / `export * as ns from "m"`
    All {
        /// Re-export source.
        source: String,
        /// `as ns`, when present.
        exported: Option<ModuleName>,
        /// Import attributes for the `from` clause.
        attributes: Vec<ImportAttribute>,
    },
}

/// One entry of an `export { … }` clause.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportSpecifier {
    /// Local name (or the name in the source module).
    pub local: ModuleName,
    /// Exported name.
    pub exported: ModuleName,
    /// Source range.
    pub span: Span,
}

//! The bytecode representation of a function.
//!
//! Compilation happens in two phases, exactly as in the reference implementation:
//!
//! * **Unresolved** ([`FunctionBuilder`]): jump targets are label ids and every
//!   variable is still described by its declaration, not its storage class.
//! * **Resolved** ([`Function`]): jump targets are byte offsets and every
//!   variable reference names its storage class (`arg`, `loc`, `var_ref` or
//!   `var`), so the bytecode can be executed and serialised.
//!
//! The split matters for testing: each phase can be compared against the
//! corresponding pass of the reference compiler.

use crate::opcode::Opcode;
use std::fmt;

/// One entry of a function's atom table.
///
/// The reference keeps two kinds of atom in one namespace, and the distinction is
/// visible in the bytecode: an *array-index* atom is not a string at all but a
/// tagged `uint32`, which is why `{ 2: x }` disassembles as `define_field 2` while
/// `{ "1.5": x }` prints `define_field "1.5"`. Collapsing the two would make the
/// disassembly wrong and lose information on the way back in, so the kind is
/// carried here.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Atom {
    /// An interned string.
    Str(String),
    /// A canonical array index, held as a number rather than a string.
    Index(u32),
}

impl Atom {
    /// The atom the reference's `JS_NewAtomStr` would make of `text`.
    ///
    /// A canonical array index no greater than `2^31 - 1` is never interned as a
    /// string: the reference returns a tagged integer instead. Anything else —
    /// `"007"`, `"1e2"`, `"-0"`, or an index past the tag range — stays a string.
    pub fn from_text(text: &str) -> Atom {
        match canonical_array_index(text) {
            Some(index) => Atom::Index(index),
            None => Atom::Str(text.to_string()),
        }
    }

    /// The text form, for callers that only care about the name.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Atom::Str(text) => Some(text),
            Atom::Index(_) => None,
        }
    }
}

impl fmt::Display for Atom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Atom::Str(text) => write!(f, "{text}"),
            Atom::Index(index) => write!(f, "{index}"),
        }
    }
}

/// `is_num_string` from the reference: the canonical decimal spelling of an index
/// small enough to be tagged (at most `2^31 - 1`).
///
/// Canonical means no sign, no leading zeros and no other characters, so `"10"`
/// qualifies while `"007"`, `"-0"` and `"1e2"` do not.
fn canonical_array_index(text: &str) -> Option<u32> {
    // `2^31 - 1` has ten digits, which bounds how long a canonical index can be.
    if text.is_empty() || text.len() > 10 {
        return None;
    }
    let bytes = text.as_bytes();
    if !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    if bytes[0] == b'0' && bytes.len() != 1 {
        return None;
    }
    let value: u32 = text.parse().ok()?;
    (value <= 0x7fff_ffff).then_some(value)
}

/// How a function was declared. Determines the calling convention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FunctionKind {
    /// A plain function.
    Normal,
    /// `function*`
    Generator,
    /// `async function`
    Async,
    /// `async function*`
    AsyncGenerator,
}

impl FunctionKind {
    /// Whether the function is a generator (plain or async).
    pub fn is_generator(self) -> bool {
        matches!(self, FunctionKind::Generator | FunctionKind::AsyncGenerator)
    }

    /// Whether the function is async (plain or generator).
    pub fn is_async(self) -> bool {
        matches!(self, FunctionKind::Async | FunctionKind::AsyncGenerator)
    }
}

/// What a declared name is used for, which decides how it is displayed and how
/// the function prologue initialises it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// An ordinary binding.
    Normal,
    /// A lexical binding holding a hoisted function declaration.
    FunctionDecl,
    /// A lexical binding holding a hoisted async/generator function declaration.
    NewFunctionDecl,
    /// A `catch` parameter.
    Catch,
    /// The name bound to a named function expression, visible inside itself.
    FunctionName,
    /// A private class field.
    PrivateField,
    /// A private class method.
    PrivateMethod,
    /// A private class getter.
    PrivateGetter,
    /// A private class setter.
    PrivateSetter,
    /// A private class accessor pair (`get` and `set` sharing one name).
    PrivateGetterSetter,
    /// A global function declaration.
    GlobalFunctionDecl,
    /// A `using` declaration's binding.
    Using,
    /// The hidden local holding a `using` declaration's cached dispose method.
    UsingMethod,
    /// The hidden local holding a `with` statement's boxed object.
    ///
    /// Marked rather than merely named so the runtime can find the active ones without
    /// guessing from a name prefix: an `eval` inside a `with` has to resolve names
    /// against these objects before it reaches the realm.
    With,
}

impl VarKind {
    /// The word the disassembler prints for this kind.
    pub fn label(self) -> &'static str {
        match self {
            VarKind::Catch => "catch",
            VarKind::FunctionDecl | VarKind::NewFunctionDecl | VarKind::GlobalFunctionDecl => {
                "function"
            }
            _ => "",
        }
    }

    /// Whether the binding is initialised when the function is entered.
    pub fn is_hoisted_declaration(self) -> bool {
        matches!(self, VarKind::FunctionDecl | VarKind::NewFunctionDecl)
    }
}

/// A declared variable: an argument or a local.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VarDef {
    /// The name, as written.
    pub name: String,
    /// Whether the binding is immutable.
    pub is_const: bool,
    /// Whether the binding is block-scoped (`let`/`const`/`class`).
    pub is_lexical: bool,
    /// What the binding is used for.
    pub kind: VarKind,
    /// Whether a closure captures this variable, which forces it into a
    /// heap-allocated reference cell.
    pub is_captured: bool,
    /// The lexical scope the binding belongs to.
    ///
    /// Scope 0 is the function scope, where `var` and hoisted function
    /// declarations live; the body and every block introduce a child scope. A
    /// binding only exists between its scope's entry and exit, which is what the
    /// runtime uses to swap `let`/`const` bindings in and out.
    pub scope_level: u32,
    /// Index of the next block-scoped variable in the enclosing scope chain, or
    /// `None` for a `var` (which is visible in the whole function).
    ///
    /// The reference stores this in `JSVarDef::scope_next` and only fills it in
    /// when `scope_level != 0`; the interpreter walks the chain to instantiate
    /// and close the `let`/`const` bindings that are live at a given point.
    pub scope_next: Option<i32>,
}

impl VarDef {
    /// A plain `var` binding.
    pub fn var(name: impl Into<String>) -> VarDef {
        VarDef {
            name: name.into(),
            is_const: false,
            is_lexical: false,
            kind: VarKind::Normal,
            is_captured: false,
            scope_level: 0,
            scope_next: None,
        }
    }

    /// Whether reading this binding can hit the temporal dead zone, i.e. it is
    /// `let`/`const`/`class` and therefore starts out uninitialised.
    pub fn needs_check(&self) -> bool {
        self.is_lexical
    }

    /// A `let` binding.
    pub fn lexical(name: impl Into<String>) -> VarDef {
        VarDef {
            is_lexical: true,
            ..VarDef::var(name)
        }
    }

    /// A `const` binding.
    pub fn constant(name: impl Into<String>) -> VarDef {
        VarDef {
            is_lexical: true,
            is_const: true,
            ..VarDef::var(name)
        }
    }

    /// The word the disassembler prints for this definition.
    pub fn label(&self) -> &'static str {
        let kind = self.kind.label();
        if !kind.is_empty() {
            return kind;
        }
        if self.is_const {
            "const"
        } else if self.is_lexical {
            "let"
        } else {
            "var"
        }
    }
}

/// Where a captured variable actually lives, and therefore how a closure
/// reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClosureType {
    /// A local of the parent function (`[loc<n>]`).
    Local,
    /// An argument of the parent function (`[arg<n>]`).
    Arg,
    /// A reference cell in the parent function (`[ref<n>]`).
    Ref,
    /// A global, reached through the reference table by index.
    GlobalRef,
    /// A global declared in this script.
    GlobalDecl,
    /// A global reached by name.
    Global,
    /// A module-level declaration.
    ModuleDecl,
    /// A module import binding.
    ModuleImport,
}

impl ClosureType {
    /// The suffix the disassembler prints.
    pub fn describe(self) -> &'static str {
        match self {
            ClosureType::Local => "loc",
            ClosureType::Arg => "arg",
            ClosureType::Ref => "ref",
            ClosureType::GlobalDecl => "global_decl",
            ClosureType::Global => "global",
            ClosureType::ModuleDecl => "module_decl",
            ClosureType::ModuleImport => "module_import",
            ClosureType::GlobalRef => "global_ref",
        }
    }
}

/// One captured variable of a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureVar {
    /// The name, as written in the enclosing function.
    pub name: String,
    /// Whether the captured binding is immutable.
    pub is_const: bool,
    /// Whether the captured binding is block-scoped.
    pub is_lexical: bool,
    /// How the closure reaches it.
    pub closure_type: ClosureType,
    /// Index into the parent's locals, arguments or reference cells.
    pub var_idx: u16,
}

/// A constant in the pool.
#[derive(Debug, Clone, PartialEq)]
pub enum Constant {
    /// A number.
    Number(f64),
    /// A string.
    String(String),
    /// A BigInt literal: digits and radix.
    BigInt {
        /// Digits, without prefix or separators.
        digits: String,
        /// 2, 8, 10 or 16.
        radix: u32,
    },
    /// A nested function, referenced by `fclosure`.
    Function(Box<Function>),
    /// A template object: one call site's chunks.
    ///
    /// The *raw* text is carried as well as the cooked text because `String.raw` reads
    /// it back verbatim, and a tagged template may legally contain an invalid escape
    /// sequence whose cooked form is `undefined`.
    Template {
        /// Cooked text; `None` for a chunk with an invalid escape sequence.
        cooked: Vec<Option<String>>,
        /// Source text of each chunk, verbatim.
        raw: Vec<String>,
    },
}

impl Constant {
    /// A short description for disassembly, matching the reference's
    /// `JS_PrintValue` for the cases the compiler can produce.
    pub fn describe(&self) -> String {
        match self {
            Constant::Number(n) => format_number(*n),
            Constant::String(s) => format!("\"{}\"", escape_js_string(s)),
            // The reference prints BigInts in decimal, whatever the source radix.
            Constant::BigInt { digits, .. } => format!("{digits}n"),
            Constant::Function(f) => {
                // The reference prints the function's name atom, which is `<null>`
                // for an anonymous function expression.
                if f.name.is_empty() {
                    "[bytecode <null>]".to_string()
                } else {
                    format!("[bytecode {}]", f.name)
                }
            }
            Constant::Template { cooked, .. } => {
                let parts: Vec<String> = cooked
                    .iter()
                    .map(|q| q.clone().unwrap_or_else(|| "undefined".to_string()))
                    .collect();
                format!("template object [{}]", parts.join(", "))
            }
        }
    }
}

/// Format a number the way the reference implementation prints it.
fn format_number(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_string();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "inf".to_string()
        } else {
            "-inf".to_string()
        };
    }
    if value == value.trunc() && value.abs() < 1e21 {
        return format!("{}", value as i64);
    }
    format!("{value}")
}

/// Escape a string for the double-quoted form the disassembler prints.
fn escape_js_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// An operand of an instruction during compilation, before addresses are known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// No operand.
    None,
    /// A label, resolved to a byte offset.
    Label(u32),
    /// A local slot index.
    Loc(u32),
    /// An argument slot index.
    Arg(u32),
    /// A closure variable index.
    VarRef(u32),
    /// A constant pool index.
    Const(u32),
    /// An atom (an interned name), stored as its UTF-8 text before interning.
    Atom(String),
    /// A small unsigned immediate.
    U8(u8),
    /// A small signed immediate.
    I8(i8),
    /// A medium unsigned immediate.
    U16(u16),
    /// A medium signed immediate.
    I16(i16),
    /// A signed 32-bit immediate.
    I32(i32),
    /// An unsigned 32-bit immediate.
    U32(u32),
}

/// A resolved instruction: opcode plus the operands encoded in its payload.
#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    /// The instruction.
    pub op: Opcode,
    /// Operands in the order the encoding stores them.
    pub operands: Vec<Operand>,
}

impl Instruction {
    /// An instruction with no operands.
    pub fn new(op: Opcode) -> Instruction {
        Instruction {
            op,
            operands: Vec::new(),
        }
    }

    /// An instruction with one operand.
    pub fn with(op: Opcode, operand: Operand) -> Instruction {
        Instruction {
            op,
            operands: vec![operand],
        }
    }
}

/// Where a variable lives in the resolved function.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    /// Argument slot.
    Arg(u32),
    /// Local slot.
    Loc(u32),
    /// Closure reference.
    VarRef(u32),
    /// Global, by atom.
    Global,
}

/// A stretch of bytecode and the source position it came from.
///
/// One entry per run of instructions that share a position, ordered by `pc`, which is
/// what [`Function::position_at`] binary searches. This is the port of the reference's
/// `pc2line` table: it is how a stack trace says `file:line:col` for a frame that is
/// deep inside a function rather than at its first statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineCol {
    /// Byte offset in [`Function::code`] where this position takes over.
    pub pc: u32,
    /// 1-based line.
    pub line: u32,
    /// 1-based column, in UTF-16 code units like every other position here.
    pub column: u32,
}

/// A compiled function.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// The function's name; `<eval>` for a script body.
    pub name: String,
    /// The declaration form.
    pub kind: FunctionKind,
    /// Whether the body is strict.
    pub strict: bool,
    /// Whether this came from an arrow function.
    ///
    /// An arrow has no `this` and no `new.target` of its own, so the runtime has to know
    /// to look outward for both — an `eval` inside an arrow sees the enclosing function's
    /// receiver, not the arrow's.
    pub is_arrow: bool,
    /// Parameters.
    pub args: Vec<VarDef>,
    /// How many parameters count towards `Function.prototype.length`.
    ///
    /// A parameter with a default or a rest parameter ends the count, because `.length`
    /// is the arity the function can be called with before anything is substituted.
    pub defined_args: u32,
    /// Locals, including the implicit return-value slot named `<ret>`.
    pub vars: Vec<VarDef>,
    /// Captured variables of enclosing functions.
    pub closure_vars: Vec<ClosureVar>,
    /// Constant pool.
    pub cpool: Vec<Constant>,
    /// Atom table: the names this function mentions. The runtime shares one
    /// interned table across the whole program; indexing it per function keeps
    /// the bytecode self-contained and makes disassembly readable.
    pub atoms: Vec<Atom>,
    /// Resolved bytecode.
    pub code: Vec<u8>,
    /// Maximum value-stack depth.
    pub stack_size: u32,
    /// Source positions, ordered by bytecode offset.
    pub positions: Vec<LineCol>,
    /// Number of reference cells allocated on entry.
    pub var_ref_count: u32,
    /// Source text, kept for `Function.prototype.toString` and stack traces.
    pub source: Option<String>,
    /// Line of the function's declaration.
    pub line: u32,
    /// Column of the function's declaration.
    pub column: u32,
    /// File name, if known.
    pub filename: Option<String>,
}

impl Function {
    /// Name the script this function belongs to.
    ///
    /// Nested functions inherit it, because a stack trace names every frame with the
    /// file the code came from and the parser does not know file names — the host
    /// does, after the source has already been compiled.
    pub fn set_filename(&mut self, name: &str) {
        self.filename = Some(name.to_string());
        for constant in &mut self.cpool {
            if let Constant::Function(inner) = constant {
                inner.set_filename(name);
            }
        }
    }

    /// The source position of the instruction at `pc`.
    ///
    /// The last entry at or before `pc` wins, so a position applies until the next one
    /// starts. `None` when the function carries no positions at all — a script compiled
    /// without source information.
    pub fn position_at(&self, pc: u32) -> Option<(u32, u32)> {
        let index = match self.positions.binary_search_by_key(&pc, |entry| entry.pc) {
            Ok(index) => index,
            Err(0) => return None,
            Err(index) => index - 1,
        };
        self.positions
            .get(index)
            .map(|entry| (entry.line, entry.column))
    }

    /// Decode the instruction stream back into instructions.
    ///
    /// Labels are already resolved, so a `label` operand is an absolute byte
    /// offset.
    pub fn instructions(&self) -> Vec<(usize, Instruction)> {
        let mut out = Vec::new();
        let mut pos = 0usize;
        while pos < self.code.len() {
            let Some(op) = Opcode::from_u8(self.code[pos]) else {
                break;
            };
            let info = op.info();
            let mut operands = Vec::new();
            let payload = &self.code[pos + 1..(pos + info.size as usize).min(self.code.len())];
            crate::disasm::decode_operands(info.format, payload, &mut operands);
            out.push((pos, Instruction { op, operands }));
            pos += info.size as usize;
        }
        out
    }

    /// Byte offsets that are the target of a jump, which the disassembler marks.
    pub fn jump_targets(&self) -> Vec<usize> {
        let mut targets = Vec::new();
        for (pc, instruction) in self.instructions() {
            if instruction.op.info().format.is_label()
                && let Some(target) = instruction.jump_target(pc)
            {
                targets.push(target as usize);
            }
        }
        targets
    }

    /// Total number of variable slots, as the serialiser stores them.
    pub fn slot_count(&self) -> u32 {
        self.args.len() as u32 + self.vars.len() as u32
    }
}

impl fmt::Display for FunctionKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            FunctionKind::Normal => "function",
            FunctionKind::Generator => "generator",
            FunctionKind::Async => "async function",
            FunctionKind::AsyncGenerator => "async generator",
        };
        f.write_str(text)
    }
}

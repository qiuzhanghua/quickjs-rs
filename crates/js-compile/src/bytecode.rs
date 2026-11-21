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
            scope_next: None,
        }
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
    /// A template object: the cooked strings of one call site.
    Template(Vec<Option<String>>),
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
                // The reference prints `[bytecode <name>]` from the object pool,
                // where the name is `""` for an anonymous function.
                format!("[bytecode {}]", f.name)
            }
            Constant::Template(quasis) => {
                let parts: Vec<String> = quasis
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

/// A compiled function.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// The function's name; `<eval>` for a script body.
    pub name: String,
    /// The declaration form.
    pub kind: FunctionKind,
    /// Whether the body is strict.
    pub strict: bool,
    /// Parameters.
    pub args: Vec<VarDef>,
    /// Locals, including the implicit return-value slot named `<ret>`.
    pub vars: Vec<VarDef>,
    /// Captured variables of enclosing functions.
    pub closure_vars: Vec<ClosureVar>,
    /// Constant pool.
    pub cpool: Vec<Constant>,
    /// Atom table: the names this function mentions. The runtime shares one
    /// interned table across the whole program; indexing it per function keeps
    /// the bytecode self-contained and makes disassembly readable.
    pub atoms: Vec<String>,
    /// Resolved bytecode.
    pub code: Vec<u8>,
    /// Maximum value-stack depth.
    pub stack_size: u32,
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

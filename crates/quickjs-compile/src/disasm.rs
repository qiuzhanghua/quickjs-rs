//! Text disassembly, in exactly the format the reference implementation prints.
//!
//! This exists to be *diffed*. The reference compiler, built with
//! `-DDUMP_BYTECODE=1`, prints its bytecode through `js_dump_function_bytecode`
//! and `dump_byte_code`, and the differential test compares that text with ours
//! character by character. Keeping the format identical means a mismatch points
//! straight at the instruction that differs rather than at a formatting detail.
//!
//! Format reference: `js_dump_function_bytecode` and `dump_byte_code` in
//! `quickjs.c`.

use crate::bytecode::{Atom, ClosureType, Constant, Function, Instruction, Operand};
use crate::opcode::{OpFormat, Opcode};
use std::fmt::Write;

/// Print a function the way `js_dump_function_bytecode` does.
pub fn disassemble_function(out: &mut String, function: &Function) {
    if let Some(filename) = &function.filename {
        let _ = write!(out, "{filename}:{}:{}: ", function.line, function.column);
    }
    let star = if function.kind.is_generator() {
        "*"
    } else {
        ""
    };
    // An anonymous function has no name atom, which the reference prints as
    // `<null>`.
    let name = if function.name.is_empty() {
        "<null>"
    } else {
        function.name.as_str()
    };
    let _ = writeln!(out, "function: {star}{name}");
    if function.strict {
        let _ = writeln!(out, "  mode: strict");
    }
    if !function.args.is_empty() {
        let names: Vec<&str> = function.args.iter().map(|a| a.name.as_str()).collect();
        let _ = writeln!(out, "  args: {}", names.join(" "));
    }
    if !function.vars.is_empty() {
        let _ = writeln!(out, "  locals:");
        for (index, var) in function.vars.iter().enumerate() {
            let _ = write!(out, "{index:5}: {} {}", var.label(), var.name);
            // The scope chain is printed for every binding outside the function
            // scope, i.e. everything the runtime has to instantiate per block.
            // `-1` means "no further binding visible from this scope".
            if var.scope_level != 0 {
                let _ = write!(out, " [next:{}]", var.scope_next.unwrap_or(-1));
            }
            let _ = writeln!(out);
        }
    }
    if !function.closure_vars.is_empty() {
        let _ = writeln!(out, "  closure vars:");
        for (index, closure) in function.closure_vars.iter().enumerate() {
            let kind = if closure.is_const {
                "const"
            } else if closure.is_lexical {
                "let"
            } else {
                "var"
            };
            let suffix = match closure.closure_type {
                ClosureType::GlobalDecl
                | ClosureType::Global
                | ClosureType::ModuleDecl
                | ClosureType::ModuleImport => {
                    format!(" [{}]", closure.closure_type.describe())
                }
                other => format!(" [{}{}]", other.describe(), closure.var_idx),
            };
            let _ = writeln!(out, "{index:5}: {kind} {}{suffix}", closure.name);
        }
    }
    let _ = writeln!(out, "  stack_size: {}", function.stack_size);
    let _ = writeln!(out, "  var_ref_count: {}", function.var_ref_count);
    let _ = writeln!(out, "  opcodes:");
    disassemble_code(out, function);
    let _ = writeln!(out);
}

/// Print a whole program in the order the reference emits it.
///
/// The reference dumps each function as its bytecode object is created, which
/// happens while the parent is still being compiled, so nested functions come
/// out innermost-first and the top-level function comes last.
pub fn disassemble_program(function: &Function) -> String {
    let mut out = String::new();
    disassemble_children(&mut out, function);
    disassemble_function(&mut out, function);
    out
}

fn disassemble_children(out: &mut String, function: &Function) {
    for constant in &function.cpool {
        if let Constant::Function(child) = constant {
            disassemble_children(out, child);
            disassemble_function(out, child);
        }
    }
}

/// Print the instruction stream.
pub fn disassemble_code(out: &mut String, function: &Function) {
    let targets = function.jump_targets();
    for (pc, instruction) in function.instructions() {
        if targets.contains(&pc) {
            let _ = write!(out, "{pc:5}:  ");
        } else {
            out.push_str("        ");
        }
        let _ = write!(out, "{}", instruction.op.name());
        format_operands(out, function, pc, &instruction);
        let _ = writeln!(out);
    }
}

/// Print an instruction's operands, following `dump_byte_code`'s switch.
fn format_operands(out: &mut String, function: &Function, pc: usize, instruction: &Instruction) {
    let info = instruction.op.info();

    // The short forms carry their operand in the opcode number itself.
    if let Some(index) = instruction.op.specialised_index() {
        match info.format {
            OpFormat::NoneInt | OpFormat::Npopx => {
                let _ = write!(out, " {index}");
                return;
            }
            OpFormat::NoneLoc => {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::Loc(index as u32));
                return;
            }
            OpFormat::NoneArg => {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::Arg(index as u32));
                return;
            }
            OpFormat::NoneVarRef => {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::VarRef(index as u32));
                return;
            }
            _ => {}
        }
    }

    let first = instruction.operands.first();
    match info.format {
        OpFormat::None
        | OpFormat::NoneInt
        | OpFormat::NoneLoc
        | OpFormat::NoneArg
        | OpFormat::NoneVarRef
        | OpFormat::Npopx => {}
        OpFormat::U8 => {
            if let Some(Operand::U32(value)) = first {
                let _ = write!(out, " {value}");
            }
        }
        OpFormat::Const8 | OpFormat::Const => {
            if let Some(Operand::U32(index)) = first {
                let _ = write!(out, " {index}: ");
                if let Some(constant) = function.cpool.get(*index as usize) {
                    let _ = write!(out, "{}", constant.describe());
                }
            }
        }
        OpFormat::I8 | OpFormat::I16 | OpFormat::I32 => match first {
            Some(Operand::I32(value)) => {
                let _ = write!(out, " {value}");
            }
            Some(Operand::U32(value)) => {
                let _ = write!(out, " {value}");
            }
            _ => {}
        },
        OpFormat::U16 | OpFormat::U32 | OpFormat::Npop => {
            if let Some(Operand::U32(value)) = first {
                let _ = write!(out, " {value}");
            }
        }
        OpFormat::NpopU16 => {
            if let (Some(Operand::U32(a)), Some(Operand::U32(b))) =
                (first, instruction.operands.get(1))
            {
                let _ = write!(out, " {a},{b}");
            }
        }
        // `loc8` is the narrow form of `loc`: same operand, same name, one byte.
        OpFormat::Loc | OpFormat::Loc8 => {
            if let Some(Operand::U32(index)) = first {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::Loc(*index));
            }
        }
        OpFormat::Arg => {
            if let Some(Operand::U32(index)) = first {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::Arg(*index));
            }
        }
        OpFormat::VarRef => {
            if let Some(Operand::U32(index)) = first {
                let _ = write!(out, " {index}: ");
                print_var_name(out, function, Slot::VarRef(*index));
            }
        }
        OpFormat::Atom => {
            if let Some(atom) = instruction.atom(function, 0) {
                let _ = write!(out, " {}", print_atom(&atom));
            }
        }
        OpFormat::AtomU8 | OpFormat::AtomU16 => {
            if let (Some(atom), Some(Operand::U32(value))) =
                (instruction.atom(function, 0), instruction.operands.get(1))
            {
                let _ = write!(out, " {},{}", print_atom(&atom), value);
            }
        }
        OpFormat::AtomLabelU8 | OpFormat::AtomLabelU16 => {
            if let (Some(atom), Some(Operand::I32(target))) =
                (instruction.atom(function, 0), instruction.operands.get(1))
            {
                let _ = write!(out, " {},{}", print_atom(&atom), target);
                if let Some(Operand::U32(value)) = instruction.operands.get(2) {
                    let _ = write!(out, ",{value}");
                }
            }
        }
        OpFormat::LabelU16 => {
            if let (Some(Operand::I32(target)), Some(Operand::U32(value))) =
                (first, instruction.operands.get(1))
            {
                let _ = write!(out, " {target},{value}");
            }
        }
        OpFormat::Label | OpFormat::Label8 | OpFormat::Label16 => {
            // The reference prints an absolute address, computed as
            // "operand position + relative offset".
            if let Some(target) = instruction.jump_target(pc) {
                let _ = write!(out, " {target}");
            }
        }
    }
}

/// Which slot table a variable index refers to.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Loc(u32),
    Arg(u32),
    VarRef(u32),
}

/// Print the name a variable slot refers to, if it is in range.
fn print_var_name(out: &mut String, function: &Function, slot: Slot) {
    let name = match slot {
        Slot::Loc(index) => function.vars.get(index as usize).map(|v| v.name.as_str()),
        Slot::Arg(index) => function.args.get(index as usize).map(|v| v.name.as_str()),
        Slot::VarRef(index) => function
            .closure_vars
            .get(index as usize)
            .map(|c| c.name.as_str()),
    };
    if let Some(name) = name {
        // The reference prints variable names as atoms, so `<ret>` comes out
        // quoted and `a` does not. A variable can never be named with a bare
        // index, so the string form is always the right one here.
        let _ = write!(out, "{}", print_atom(&Atom::Str(name.to_string())));
    }
}

/// Print an atom the way `js_print_atom2` does.
///
/// A tagged array index prints as a bare number — that is the whole point of
/// tagging it — while a string atom prints bare only if it is an ASCII
/// identifier, and double-quoted otherwise.
pub fn print_atom(atom: &Atom) -> String {
    if let Atom::Index(index) = atom {
        return index.to_string();
    }
    let text = atom.as_str().unwrap_or_default();
    if is_ascii_ident(text) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    out.push_str(&escape_string(text));
    out.push('"');
    out
}

/// Whether an atom needs no quoting.
fn is_ascii_ident(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The escapes `js_print_string1` produces.
fn escape_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\u{b}' => out.push_str("\\v"),
            '\0' => out.push_str("\\0"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Decode one instruction's operands from its payload bytes.
///
/// Jump targets stay relative here; [`Instruction::jump_target`] resolves them
/// against the instruction's position, which is what the reference prints.
pub fn decode_operands(format: OpFormat, payload: &[u8], out: &mut Vec<Operand>) {
    let be = |offset: usize, count: usize| -> u32 {
        let mut value = 0u32;
        for i in 0..count {
            value = (value << 8) | payload.get(offset + i).copied().unwrap_or(0) as u32;
        }
        value
    };
    match format {
        OpFormat::None
        | OpFormat::NoneInt
        | OpFormat::NoneLoc
        | OpFormat::NoneArg
        | OpFormat::NoneVarRef
        | OpFormat::Npopx => {}
        OpFormat::U8 | OpFormat::Loc8 | OpFormat::Const8 => {
            out.push(Operand::U32(be(0, 1)));
        }
        OpFormat::I8 | OpFormat::Label8 => {
            out.push(Operand::I32(be(0, 1) as u8 as i8 as i32));
        }
        OpFormat::U16 | OpFormat::Npop => out.push(Operand::U32(be(0, 2))),
        OpFormat::I16 | OpFormat::Label16 => {
            out.push(Operand::I32(be(0, 2) as u16 as i16 as i32));
        }
        OpFormat::NpopU16 => {
            out.push(Operand::U32(be(0, 2)));
            out.push(Operand::U32(be(2, 2)));
        }
        OpFormat::Loc | OpFormat::Arg | OpFormat::VarRef => out.push(Operand::U32(be(0, 2))),
        OpFormat::U32 | OpFormat::Const | OpFormat::Atom => out.push(Operand::U32(be(0, 4))),
        OpFormat::I32 | OpFormat::Label => out.push(Operand::I32(be(0, 4) as i32)),
        OpFormat::AtomU8 | OpFormat::AtomLabelU8 => {
            out.push(Operand::U32(be(0, 4)));
            out.push(Operand::U32(be(4, 1)));
        }
        OpFormat::AtomU16 | OpFormat::AtomLabelU16 | OpFormat::LabelU16 => {
            out.push(Operand::U32(be(0, 4)));
            out.push(Operand::U32(be(4, 2)));
        }
    }
}

impl Instruction {
    /// The atom named by operand `index`, looked up in the function's table.
    pub fn atom(&self, function: &Function, index: usize) -> Option<Atom> {
        let Some(Operand::U32(atom)) = self.operands.get(index) else {
            return None;
        };
        function.atoms.get(*atom as usize).cloned()
    }
}

/// Resolve a relative jump operand into the absolute address the reference
/// prints.
impl Instruction {
    /// The absolute target of a jump, if this instruction has one.
    pub fn jump_target(&self, pc: usize) -> Option<i64> {
        // The operand follows the opcode byte, and the reference computes the
        // target as `operand_position + offset`.
        let operand_position = pc as i64 + 1;
        match self.op.info().format {
            OpFormat::Label8 | OpFormat::Label16 | OpFormat::Label => match self.operands.first() {
                Some(Operand::I32(offset)) => Some(operand_position + *offset as i64),
                Some(Operand::U32(offset)) => Some(operand_position + *offset as i64),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The short form a long instruction folds into, for the assembler.
pub fn long_to_short(op: Opcode) -> Option<Opcode> {
    use Opcode::*;
    Some(match op {
        GetLoc => GetLoc0,
        PutLoc => PutLoc0,
        SetLoc => SetLoc0,
        GetArg => GetArg0,
        PutArg => PutArg0,
        SetArg => SetArg0,
        GetVarRef => GetVarRef0,
        PutVarRef => PutVarRef0,
        SetVarRef => SetVarRef0,
        Call => Call0,
        PushI32 => Push0,
        IfFalse => IfFalse8,
        IfTrue => IfTrue8,
        Goto => Goto8,
        _ => return None,
    })
}

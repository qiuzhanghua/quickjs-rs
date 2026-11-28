//! Bytecode compiler: takes the AST and produces the bytecode of a function.
//!
//! The pipeline matches the reference implementation:
//!
//! 1. [`compile`] walks the AST once and emits *unresolved* bytecode, where
//!    every variable reference is a slot in a flat table of arguments, locals
//!    and closure variables, and every jump is a label rather than an offset.
//! 2. [`Function::resolve`] turns those into the final bytecode: variables become
//!    `get_loc`/`get_arg`/`get_var_ref`/`get_var`, jumps become byte offsets, and
//!    the stack size is computed.
//! 3. [`serialize`] writes the result in a self-describing format with a magic
//!    number and a version.
//!
//! Keeping steps 1 and 2 apart is what makes the compiler testable: step 1 can be
//! compared against the reference's pass-1 dump, and step 2 against its pass-3
//! dump.

pub mod bytecode;
pub mod compiler;
pub mod disasm;
pub mod opcode;
pub mod serialize;

pub use bytecode::{Function, FunctionKind, LineCol, VarDef, VarKind};
pub use compiler::{
    Compiler, compile_module, compile_script, compile_script_suspensible, disassemble,
};
pub use opcode::{OpFormat, OpInfo, Opcode};

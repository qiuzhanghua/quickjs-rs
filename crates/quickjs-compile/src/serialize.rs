//! Serialisation of compiled bytecode.
//!
//! The format is our own — it deliberately does not interoperate with the C
//! implementation's binary blobs — but it keeps the same *semantics*: a compiled
//! function can be written out and read back without the source, which is what
//! `qjsc` relies on for ahead-of-time compilation.
//!
//! Layout, all integers big-endian:
//!
//! ```text
//! magic    4 bytes  "QJSB"
//! version  u16      format version
//! function          one function record, recursively
//!
//! function record:
//!   name        string (u32 length + UTF-8 bytes)
//!   kind        u8
//!   flags       u8    bit 0 = strict
//!   line, column u32 each
//!   args        u32 count, then name/flags records
//!   vars        u32 count, then name/flags/scope records
//!   closures    u32 count, then name/flags/type/index records
//!   cpool       u32 count, then constants
//!   code        u32 length + bytes
//!   stack_size  u32
//!   var_refs    u32
//! ```
//!
//! A version bump is required for any change to this layout; [`MAGIC`] plus
//! [`VERSION`] make a stale blob fail loudly instead of being misinterpreted.

use crate::bytecode::{ClosureType, ClosureVar, Constant, Function, FunctionKind, VarDef, VarKind};
use std::fmt;

/// File magic.
pub const MAGIC: [u8; 4] = *b"QJSB";

/// Format version. Bumped whenever the layout below changes.
pub const VERSION: u16 = 2;

/// Why a blob could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The magic number is wrong; this is not our bytecode.
    BadMagic,
    /// The format version is not one this build understands.
    BadVersion(u16),
    /// The blob ends in the middle of a value.
    Truncated,
    /// A discriminant is out of range.
    BadTag(&'static str, u8),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::BadMagic => f.write_str("not QuickJS-RS bytecode (bad magic)"),
            DecodeError::BadVersion(v) => write!(f, "unsupported bytecode version {v}"),
            DecodeError::Truncated => f.write_str("truncated bytecode"),
            DecodeError::BadTag(what, tag) => write!(f, "invalid {what} tag {tag}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Write a compiled function, including its nested functions.
pub fn serialize(function: &Function) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_be_bytes());
    write_function(&mut out, function);
    out
}

/// Read a compiled function back.
pub fn deserialize(bytes: &[u8]) -> Result<Function, DecodeError> {
    let mut reader = Reader { bytes, pos: 0 };
    if reader.take(4)? != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let version = u16::from_be_bytes(reader.array::<2>()?);
    if version != VERSION {
        return Err(DecodeError::BadVersion(version));
    }
    read_function(&mut reader)
}

fn write_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn write_var(out: &mut Vec<u8>, var: &VarDef) {
    write_string(out, &var.name);
    let mut flags = 0u8;
    if var.is_const {
        flags |= 1;
    }
    if var.is_lexical {
        flags |= 2;
    }
    if var.is_captured {
        flags |= 4;
    }
    out.push(flags);
    out.push(var_kind_tag(var.kind));
    out.extend_from_slice(&var.scope_level.to_be_bytes());
    match var.scope_next {
        Some(next) => {
            out.push(1);
            out.extend_from_slice(&next.to_be_bytes());
        }
        None => out.push(0),
    }
}

fn write_function(out: &mut Vec<u8>, function: &Function) {
    write_string(out, &function.name);
    out.push(kind_tag(function.kind));
    out.push(u8::from(function.strict));
    out.extend_from_slice(&function.line.to_be_bytes());
    out.extend_from_slice(&function.column.to_be_bytes());

    out.extend_from_slice(&(function.args.len() as u32).to_be_bytes());
    for arg in &function.args {
        write_var(out, arg);
    }
    out.extend_from_slice(&(function.vars.len() as u32).to_be_bytes());
    for var in &function.vars {
        write_var(out, var);
    }

    out.extend_from_slice(&(function.closure_vars.len() as u32).to_be_bytes());
    for closure in &function.closure_vars {
        write_string(out, &closure.name);
        let mut flags = 0u8;
        if closure.is_const {
            flags |= 1;
        }
        if closure.is_lexical {
            flags |= 2;
        }
        out.push(flags);
        out.push(closure_type_tag(closure.closure_type));
        out.extend_from_slice(&closure.var_idx.to_be_bytes());
    }

    out.extend_from_slice(&(function.atoms.len() as u32).to_be_bytes());
    for atom in &function.atoms {
        write_string(out, atom);
    }

    out.extend_from_slice(&(function.cpool.len() as u32).to_be_bytes());
    for constant in &function.cpool {
        write_constant(out, constant);
    }

    out.extend_from_slice(&(function.code.len() as u32).to_be_bytes());
    out.extend_from_slice(&function.code);
    out.extend_from_slice(&function.stack_size.to_be_bytes());
    out.extend_from_slice(&function.var_ref_count.to_be_bytes());
}

fn write_constant(out: &mut Vec<u8>, constant: &Constant) {
    match constant {
        Constant::Number(value) => {
            out.push(0);
            out.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        Constant::String(text) => {
            out.push(1);
            write_string(out, text);
        }
        Constant::BigInt { digits, radix } => {
            out.push(2);
            write_string(out, digits);
            out.push(*radix as u8);
        }
        Constant::Function(function) => {
            out.push(3);
            write_function(out, function);
        }
        Constant::Template(quasis) => {
            out.push(4);
            out.extend_from_slice(&(quasis.len() as u32).to_be_bytes());
            for quasi in quasis {
                match quasi {
                    Some(text) => {
                        out.push(1);
                        write_string(out, text);
                    }
                    None => out.push(0),
                }
            }
        }
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(count).ok_or(DecodeError::Truncated)?;
        if end > self.bytes.len() {
            return Err(DecodeError::Truncated);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        Ok(u16::from_be_bytes(self.array::<2>()?))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        Ok(u32::from_be_bytes(self.array::<4>()?))
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        Ok(i32::from_be_bytes(self.array::<4>()?))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        let length = self.u32()? as usize;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| DecodeError::Truncated)
    }
}

fn read_var(reader: &mut Reader<'_>) -> Result<VarDef, DecodeError> {
    let name = reader.string()?;
    let flags = reader.u8()?;
    let kind = var_kind_from_tag(reader.u8()?)?;
    let scope_level = reader.u32()?;
    let scope_next = match reader.u8()? {
        0 => None,
        1 => Some(reader.i32()?),
        other => return Err(DecodeError::BadTag("scope", other)),
    };
    Ok(VarDef {
        name,
        is_const: flags & 1 != 0,
        is_lexical: flags & 2 != 0,
        is_captured: flags & 4 != 0,
        kind,
        scope_level,
        scope_next,
    })
}

fn read_function(reader: &mut Reader<'_>) -> Result<Function, DecodeError> {
    let name = reader.string()?;
    let kind = kind_from_tag(reader.u8()?)?;
    let strict = reader.u8()? != 0;
    let line = reader.u32()?;
    let column = reader.u32()?;

    let arg_count = reader.u32()? as usize;
    let mut args = Vec::with_capacity(arg_count);
    for _ in 0..arg_count {
        args.push(read_var(reader)?);
    }
    let var_count = reader.u32()? as usize;
    let mut vars = Vec::with_capacity(var_count);
    for _ in 0..var_count {
        vars.push(read_var(reader)?);
    }

    let closure_count = reader.u32()? as usize;
    let mut closure_vars = Vec::with_capacity(closure_count);
    for _ in 0..closure_count {
        let name = reader.string()?;
        let flags = reader.u8()?;
        let closure_type = closure_type_from_tag(reader.u8()?)?;
        let var_idx = reader.u16()?;
        closure_vars.push(ClosureVar {
            name,
            is_const: flags & 1 != 0,
            is_lexical: flags & 2 != 0,
            closure_type,
            var_idx,
        });
    }

    let atom_count = reader.u32()? as usize;
    let mut atoms = Vec::with_capacity(atom_count);
    for _ in 0..atom_count {
        atoms.push(reader.string()?);
    }

    let cpool_count = reader.u32()? as usize;
    let mut cpool = Vec::with_capacity(cpool_count);
    for _ in 0..cpool_count {
        cpool.push(read_constant(reader)?);
    }

    let code_length = reader.u32()? as usize;
    let code = reader.take(code_length)?.to_vec();
    let stack_size = reader.u32()?;
    let var_ref_count = reader.u32()?;
    Ok(Function {
        name,
        kind,
        strict,
        args,
        vars,
        closure_vars,
        cpool,
        atoms,
        code,
        stack_size,
        var_ref_count,
        source: None,
        line,
        column,
        filename: None,
    })
}

fn read_constant(reader: &mut Reader<'_>) -> Result<Constant, DecodeError> {
    match reader.u8()? {
        0 => Ok(Constant::Number(f64::from_bits(u64::from_be_bytes(
            reader.array::<8>()?,
        )))),
        1 => Ok(Constant::String(reader.string()?)),
        2 => {
            let digits = reader.string()?;
            let radix = reader.u8()? as u32;
            Ok(Constant::BigInt { digits, radix })
        }
        3 => Ok(Constant::Function(Box::new(read_function(reader)?))),
        4 => {
            let count = reader.u32()? as usize;
            let mut quasis = Vec::with_capacity(count);
            for _ in 0..count {
                quasis.push(match reader.u8()? {
                    0 => None,
                    1 => Some(reader.string()?),
                    other => return Err(DecodeError::BadTag("template", other)),
                });
            }
            Ok(Constant::Template(quasis))
        }
        other => Err(DecodeError::BadTag("constant", other)),
    }
}

fn kind_tag(kind: FunctionKind) -> u8 {
    match kind {
        FunctionKind::Normal => 0,
        FunctionKind::Generator => 1,
        FunctionKind::Async => 2,
        FunctionKind::AsyncGenerator => 3,
    }
}

fn kind_from_tag(tag: u8) -> Result<FunctionKind, DecodeError> {
    Ok(match tag {
        0 => FunctionKind::Normal,
        1 => FunctionKind::Generator,
        2 => FunctionKind::Async,
        3 => FunctionKind::AsyncGenerator,
        other => return Err(DecodeError::BadTag("function kind", other)),
    })
}

fn var_kind_tag(kind: VarKind) -> u8 {
    match kind {
        VarKind::Normal => 0,
        VarKind::FunctionDecl => 1,
        VarKind::NewFunctionDecl => 2,
        VarKind::Catch => 3,
        VarKind::FunctionName => 4,
        VarKind::PrivateField => 5,
        VarKind::PrivateMethod => 6,
        VarKind::PrivateGetter => 7,
        VarKind::PrivateSetter => 8,
        VarKind::PrivateGetterSetter => 9,
        VarKind::GlobalFunctionDecl => 10,
        VarKind::Using => 11,
        VarKind::UsingMethod => 12,
    }
}

fn var_kind_from_tag(tag: u8) -> Result<VarKind, DecodeError> {
    Ok(match tag {
        0 => VarKind::Normal,
        1 => VarKind::FunctionDecl,
        2 => VarKind::NewFunctionDecl,
        3 => VarKind::Catch,
        4 => VarKind::FunctionName,
        5 => VarKind::PrivateField,
        6 => VarKind::PrivateMethod,
        7 => VarKind::PrivateGetter,
        8 => VarKind::PrivateSetter,
        9 => VarKind::PrivateGetterSetter,
        10 => VarKind::GlobalFunctionDecl,
        11 => VarKind::Using,
        12 => VarKind::UsingMethod,
        other => return Err(DecodeError::BadTag("variable kind", other)),
    })
}

fn closure_type_tag(kind: ClosureType) -> u8 {
    match kind {
        ClosureType::Local => 0,
        ClosureType::Arg => 1,
        ClosureType::Ref => 2,
        ClosureType::GlobalRef => 3,
        ClosureType::GlobalDecl => 4,
        ClosureType::Global => 5,
        ClosureType::ModuleDecl => 6,
        ClosureType::ModuleImport => 7,
    }
}

fn closure_type_from_tag(tag: u8) -> Result<ClosureType, DecodeError> {
    Ok(match tag {
        0 => ClosureType::Local,
        1 => ClosureType::Arg,
        2 => ClosureType::Ref,
        3 => ClosureType::GlobalRef,
        4 => ClosureType::GlobalDecl,
        5 => ClosureType::Global,
        6 => ClosureType::ModuleDecl,
        7 => ClosureType::ModuleImport,
        other => return Err(DecodeError::BadTag("closure type", other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Function {
        Function {
            name: "<eval>".to_string(),
            kind: FunctionKind::Normal,
            strict: false,
            args: vec![],
            vars: vec![VarDef::var("<ret>"), VarDef::constant("k")],
            closure_vars: vec![ClosureVar {
                name: "g".to_string(),
                is_const: false,
                is_lexical: false,
                closure_type: ClosureType::GlobalDecl,
                var_idx: 0,
            }],
            cpool: vec![Constant::Number(1.5), Constant::String("s".to_string())],
            atoms: vec!["a".to_string(), "b".to_string()],
            code: vec![0x2c, 0x00, 0x00],
            stack_size: 3,
            var_ref_count: 0,
            source: None,
            line: 1,
            column: 1,
            filename: None,
        }
    }

    #[test]
    fn round_trips() {
        let function = sample();
        let bytes = serialize(&function);
        let back = deserialize(&bytes).expect("decode");
        assert_eq!(back, function);
    }

    #[test]
    fn rejects_foreign_input() {
        assert_eq!(deserialize(b"JUNK\x00\x01"), Err(DecodeError::BadMagic));
    }

    #[test]
    fn rejects_a_future_version() {
        let mut bytes = serialize(&sample());
        bytes[4..6].copy_from_slice(&(VERSION + 1).to_be_bytes());
        assert_eq!(
            deserialize(&bytes),
            Err(DecodeError::BadVersion(VERSION + 1))
        );
    }

    #[test]
    fn rejects_truncated_input() {
        let bytes = serialize(&sample());
        assert_eq!(
            deserialize(&bytes[..bytes.len() - 1]),
            Err(DecodeError::Truncated)
        );
    }
}

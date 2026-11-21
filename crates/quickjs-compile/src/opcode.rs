//! The instruction set, transcribed from the reference `quickjs-opcode.h`.
//!
//! That header is the single source of truth for three things: the opcode
//! numbering, the instruction length, and how many values each instruction pops
//! and pushes. Those are exactly the facts the compiler, the verifier and the
//! disassembler need, so the table below is a transcription rather than a
//! re-derivation: a typo there would be a typo in three places at once.
//!
//! Two numbering spaces exist in the reference implementation:
//!
//! * [`Opcode`] — instructions that appear in finished bytecode, numbered
//!   `0 .. OP_COUNT`.
//! * [`TempOpcode`] — instructions the compiler emits while building a function
//!   and removes before the bytecode is finished (scope bookkeeping, labels,
//!   line numbers). Their numbers overlap the short-form opcodes, so they can
//!   never be confused with a finished instruction.
//!
//! The numbering matches the C implementation even though the serialised format
//! is our own (see [`crate::serialize`]): it makes disassembly diffs against
//! `qjsc` readable, and costs nothing.

/// Operand encoding of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpFormat {
    /// `none`
    None,
    /// `none_int`
    NoneInt,
    /// `none_loc`
    NoneLoc,
    /// `none_arg`
    NoneArg,
    /// `none_var_ref`
    NoneVarRef,
    /// `u8`
    U8,
    /// `i8`
    I8,
    /// `loc8`
    Loc8,
    /// `const8`
    Const8,
    /// `label8`
    Label8,
    /// `u16`
    U16,
    /// `i16`
    I16,
    /// `label16`
    Label16,
    /// `npop`
    Npop,
    /// `npopx`
    Npopx,
    /// `npop_u16`
    NpopU16,
    /// `loc`
    Loc,
    /// `arg`
    Arg,
    /// `var_ref`
    VarRef,
    /// `u32`
    U32,
    /// `i32`
    I32,
    /// `const`
    Const,
    /// `label`
    Label,
    /// `atom`
    Atom,
    /// `atom_u8`
    AtomU8,
    /// `atom_u16`
    AtomU16,
    /// `atom_label_u8`
    AtomLabelU8,
    /// `atom_label_u16`
    AtomLabelU16,
    /// `label_u16`
    LabelU16,
}

impl OpFormat {
    /// The encoding name as written in the reference implementation.
    pub fn as_str(self) -> &'static str {
        match self {
            OpFormat::None => "none",
            OpFormat::NoneInt => "none_int",
            OpFormat::NoneLoc => "none_loc",
            OpFormat::NoneArg => "none_arg",
            OpFormat::NoneVarRef => "none_var_ref",
            OpFormat::U8 => "u8",
            OpFormat::I8 => "i8",
            OpFormat::Loc8 => "loc8",
            OpFormat::Const8 => "const8",
            OpFormat::Label8 => "label8",
            OpFormat::U16 => "u16",
            OpFormat::I16 => "i16",
            OpFormat::Label16 => "label16",
            OpFormat::Npop => "npop",
            OpFormat::Npopx => "npopx",
            OpFormat::NpopU16 => "npop_u16",
            OpFormat::Loc => "loc",
            OpFormat::Arg => "arg",
            OpFormat::VarRef => "var_ref",
            OpFormat::U32 => "u32",
            OpFormat::I32 => "i32",
            OpFormat::Const => "const",
            OpFormat::Label => "label",
            OpFormat::Atom => "atom",
            OpFormat::AtomU8 => "atom_u8",
            OpFormat::AtomU16 => "atom_u16",
            OpFormat::AtomLabelU8 => "atom_label_u8",
            OpFormat::AtomLabelU16 => "atom_label_u16",
            OpFormat::LabelU16 => "label_u16",
        }
    }

    /// Whether the encoding carries a jump target.
    pub fn is_label(self) -> bool {
        matches!(
            self,
            OpFormat::Label
                | OpFormat::Label8
                | OpFormat::Label16
                | OpFormat::AtomLabelU8
                | OpFormat::AtomLabelU16
                | OpFormat::LabelU16
        )
    }
}

/// Static description of one instruction.
#[derive(Debug, Clone, Copy)]
pub struct OpInfo {
    /// Mnemonic, as printed by the reference disassembler.
    pub name: &'static str,
    /// Instruction length in bytes, including the opcode byte.
    pub size: u8,
    /// Number of operands popped from the value stack.
    pub n_pop: u8,
    /// Number of results pushed onto the value stack.
    pub n_push: u8,
    /// Operand encoding.
    pub format: OpFormat,
}

/// An instruction of finished bytecode.
///
/// Discriminants are the opcode numbers: the byte stored in the instruction
/// stream is exactly `self as u8`, which keeps [`Opcode::from_u8`] a table
/// lookup and lets a disassembly diff against `qjsc` point at a byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Opcode {
    /// `invalid` — 1 byte(s), pops 0, pushes 0
    Invalid = 0,
    /// `push_i32` — 5 byte(s), pops 0, pushes 1
    PushI32 = 1,
    /// `push_const` — 5 byte(s), pops 0, pushes 1
    PushConst = 2,
    /// `fclosure` — 5 byte(s), pops 0, pushes 1
    Fclosure = 3,
    /// `push_atom_value` — 5 byte(s), pops 0, pushes 1
    PushAtomValue = 4,
    /// `private_symbol` — 5 byte(s), pops 0, pushes 1
    PrivateSymbol = 5,
    /// `undefined` — 1 byte(s), pops 0, pushes 1
    Undefined = 6,
    /// `null` — 1 byte(s), pops 0, pushes 1
    Null = 7,
    /// `push_this` — 1 byte(s), pops 0, pushes 1
    PushThis = 8,
    /// `push_false` — 1 byte(s), pops 0, pushes 1
    PushFalse = 9,
    /// `push_true` — 1 byte(s), pops 0, pushes 1
    PushTrue = 10,
    /// `object` — 1 byte(s), pops 0, pushes 1
    Object = 11,
    /// `special_object` — 2 byte(s), pops 0, pushes 1
    SpecialObject = 12,
    /// `rest` — 3 byte(s), pops 0, pushes 1
    Rest = 13,
    /// `drop` — 1 byte(s), pops 1, pushes 0
    Drop = 14,
    /// `nip` — 1 byte(s), pops 2, pushes 1
    Nip = 15,
    /// `nip1` — 1 byte(s), pops 3, pushes 2
    Nip1 = 16,
    /// `dup` — 1 byte(s), pops 1, pushes 2
    Dup = 17,
    /// `dup1` — 1 byte(s), pops 2, pushes 3
    Dup1 = 18,
    /// `dup2` — 1 byte(s), pops 2, pushes 4
    Dup2 = 19,
    /// `dup3` — 1 byte(s), pops 3, pushes 6
    Dup3 = 20,
    /// `insert2` — 1 byte(s), pops 2, pushes 3
    Insert2 = 21,
    /// `insert3` — 1 byte(s), pops 3, pushes 4
    Insert3 = 22,
    /// `insert4` — 1 byte(s), pops 4, pushes 5
    Insert4 = 23,
    /// `perm3` — 1 byte(s), pops 3, pushes 3
    Perm3 = 24,
    /// `perm4` — 1 byte(s), pops 4, pushes 4
    Perm4 = 25,
    /// `perm5` — 1 byte(s), pops 5, pushes 5
    Perm5 = 26,
    /// `swap` — 1 byte(s), pops 2, pushes 2
    Swap = 27,
    /// `swap2` — 1 byte(s), pops 4, pushes 4
    Swap2 = 28,
    /// `rot3l` — 1 byte(s), pops 3, pushes 3
    Rot3l = 29,
    /// `rot3r` — 1 byte(s), pops 3, pushes 3
    Rot3r = 30,
    /// `rot4l` — 1 byte(s), pops 4, pushes 4
    Rot4l = 31,
    /// `rot5l` — 1 byte(s), pops 5, pushes 5
    Rot5l = 32,
    /// `call_constructor` — 3 byte(s), pops 2, pushes 1
    CallConstructor = 33,
    /// `call` — 3 byte(s), pops 1, pushes 1
    Call = 34,
    /// `tail_call` — 3 byte(s), pops 1, pushes 0
    TailCall = 35,
    /// `call_method` — 3 byte(s), pops 2, pushes 1
    CallMethod = 36,
    /// `tail_call_method` — 3 byte(s), pops 2, pushes 0
    TailCallMethod = 37,
    /// `array_from` — 3 byte(s), pops 0, pushes 1
    ArrayFrom = 38,
    /// `apply` — 3 byte(s), pops 3, pushes 1
    Apply = 39,
    /// `return` — 1 byte(s), pops 1, pushes 0
    Return = 40,
    /// `return_undef` — 1 byte(s), pops 0, pushes 0
    ReturnUndef = 41,
    /// `check_ctor_return` — 1 byte(s), pops 1, pushes 2
    CheckCtorReturn = 42,
    /// `check_ctor` — 1 byte(s), pops 0, pushes 0
    CheckCtor = 43,
    /// `init_ctor` — 1 byte(s), pops 0, pushes 1
    InitCtor = 44,
    /// `check_brand` — 1 byte(s), pops 2, pushes 2
    CheckBrand = 45,
    /// `add_brand` — 1 byte(s), pops 2, pushes 0
    AddBrand = 46,
    /// `return_async` — 1 byte(s), pops 1, pushes 0
    ReturnAsync = 47,
    /// `throw` — 1 byte(s), pops 1, pushes 0
    Throw = 48,
    /// `throw_error` — 6 byte(s), pops 0, pushes 0
    ThrowError = 49,
    /// `eval` — 5 byte(s), pops 1, pushes 1
    Eval = 50,
    /// `apply_eval` — 3 byte(s), pops 2, pushes 1
    ApplyEval = 51,
    /// `regexp` — 1 byte(s), pops 2, pushes 1
    Regexp = 52,
    /// `get_super` — 1 byte(s), pops 1, pushes 1
    GetSuper = 53,
    /// `import` — 1 byte(s), pops 2, pushes 1
    Import = 54,
    /// `get_var_undef` — 3 byte(s), pops 0, pushes 1
    GetVarUndef = 55,
    /// `get_var` — 3 byte(s), pops 0, pushes 1
    GetVar = 56,
    /// `put_var` — 3 byte(s), pops 1, pushes 0
    PutVar = 57,
    /// `put_var_init` — 3 byte(s), pops 1, pushes 0
    PutVarInit = 58,
    /// `get_ref_value` — 1 byte(s), pops 2, pushes 3
    GetRefValue = 59,
    /// `put_ref_value` — 1 byte(s), pops 3, pushes 0
    PutRefValue = 60,
    /// `get_field` — 5 byte(s), pops 1, pushes 1
    GetField = 61,
    /// `get_field2` — 5 byte(s), pops 1, pushes 2
    GetField2 = 62,
    /// `put_field` — 5 byte(s), pops 2, pushes 0
    PutField = 63,
    /// `get_private_field` — 1 byte(s), pops 2, pushes 1
    GetPrivateField = 64,
    /// `put_private_field` — 1 byte(s), pops 3, pushes 0
    PutPrivateField = 65,
    /// `define_private_field` — 1 byte(s), pops 3, pushes 1
    DefinePrivateField = 66,
    /// `get_array_el` — 1 byte(s), pops 2, pushes 1
    GetArrayEl = 67,
    /// `get_array_el2` — 1 byte(s), pops 2, pushes 2
    GetArrayEl2 = 68,
    /// `get_array_el3` — 1 byte(s), pops 2, pushes 3
    GetArrayEl3 = 69,
    /// `put_array_el` — 1 byte(s), pops 3, pushes 0
    PutArrayEl = 70,
    /// `get_super_value` — 1 byte(s), pops 3, pushes 1
    GetSuperValue = 71,
    /// `put_super_value` — 1 byte(s), pops 4, pushes 0
    PutSuperValue = 72,
    /// `define_field` — 5 byte(s), pops 2, pushes 1
    DefineField = 73,
    /// `set_name` — 5 byte(s), pops 1, pushes 1
    SetName = 74,
    /// `set_name_computed` — 1 byte(s), pops 2, pushes 2
    SetNameComputed = 75,
    /// `set_proto` — 1 byte(s), pops 2, pushes 1
    SetProto = 76,
    /// `set_home_object` — 1 byte(s), pops 2, pushes 2
    SetHomeObject = 77,
    /// `define_array_el` — 1 byte(s), pops 3, pushes 2
    DefineArrayEl = 78,
    /// `append` — 1 byte(s), pops 3, pushes 2
    Append = 79,
    /// `copy_data_properties` — 2 byte(s), pops 3, pushes 3
    CopyDataProperties = 80,
    /// `define_method` — 6 byte(s), pops 2, pushes 1
    DefineMethod = 81,
    /// `define_method_computed` — 2 byte(s), pops 3, pushes 1
    DefineMethodComputed = 82,
    /// `define_class` — 6 byte(s), pops 2, pushes 2
    DefineClass = 83,
    /// `define_class_computed` — 6 byte(s), pops 3, pushes 3
    DefineClassComputed = 84,
    /// `get_loc` — 3 byte(s), pops 0, pushes 1
    GetLoc = 85,
    /// `put_loc` — 3 byte(s), pops 1, pushes 0
    PutLoc = 86,
    /// `set_loc` — 3 byte(s), pops 1, pushes 1
    SetLoc = 87,
    /// `get_arg` — 3 byte(s), pops 0, pushes 1
    GetArg = 88,
    /// `put_arg` — 3 byte(s), pops 1, pushes 0
    PutArg = 89,
    /// `set_arg` — 3 byte(s), pops 1, pushes 1
    SetArg = 90,
    /// `get_var_ref` — 3 byte(s), pops 0, pushes 1
    GetVarRef = 91,
    /// `put_var_ref` — 3 byte(s), pops 1, pushes 0
    PutVarRef = 92,
    /// `set_var_ref` — 3 byte(s), pops 1, pushes 1
    SetVarRef = 93,
    /// `set_loc_uninitialized` — 3 byte(s), pops 0, pushes 0
    SetLocUninitialized = 94,
    /// `get_loc_check` — 3 byte(s), pops 0, pushes 1
    GetLocCheck = 95,
    /// `put_loc_check` — 3 byte(s), pops 1, pushes 0
    PutLocCheck = 96,
    /// `set_loc_check` — 3 byte(s), pops 1, pushes 1
    SetLocCheck = 97,
    /// `put_loc_check_init` — 3 byte(s), pops 1, pushes 0
    PutLocCheckInit = 98,
    /// `get_loc_checkthis` — 3 byte(s), pops 0, pushes 1
    GetLocCheckthis = 99,
    /// `get_var_ref_check` — 3 byte(s), pops 0, pushes 1
    GetVarRefCheck = 100,
    /// `put_var_ref_check` — 3 byte(s), pops 1, pushes 0
    PutVarRefCheck = 101,
    /// `put_var_ref_check_init` — 3 byte(s), pops 1, pushes 0
    PutVarRefCheckInit = 102,
    /// `close_loc` — 3 byte(s), pops 0, pushes 0
    CloseLoc = 103,
    /// `if_false` — 5 byte(s), pops 1, pushes 0
    IfFalse = 104,
    /// `if_true` — 5 byte(s), pops 1, pushes 0
    IfTrue = 105,
    /// `goto` — 5 byte(s), pops 0, pushes 0
    Goto = 106,
    /// `catch` — 5 byte(s), pops 0, pushes 1
    Catch = 107,
    /// `gosub` — 5 byte(s), pops 0, pushes 0
    Gosub = 108,
    /// `ret` — 1 byte(s), pops 1, pushes 0
    Ret = 109,
    /// `nip_catch` — 1 byte(s), pops 2, pushes 1
    NipCatch = 110,
    /// `to_object` — 1 byte(s), pops 1, pushes 1
    ToObject = 111,
    /// `to_propkey` — 1 byte(s), pops 1, pushes 1
    ToPropkey = 112,
    /// `with_get_var` — 10 byte(s), pops 1, pushes 0
    WithGetVar = 113,
    /// `with_put_var` — 10 byte(s), pops 2, pushes 1
    WithPutVar = 114,
    /// `with_delete_var` — 10 byte(s), pops 1, pushes 0
    WithDeleteVar = 115,
    /// `with_make_ref` — 10 byte(s), pops 1, pushes 0
    WithMakeRef = 116,
    /// `with_get_ref` — 10 byte(s), pops 1, pushes 0
    WithGetRef = 117,
    /// `make_loc_ref` — 7 byte(s), pops 0, pushes 2
    MakeLocRef = 118,
    /// `make_arg_ref` — 7 byte(s), pops 0, pushes 2
    MakeArgRef = 119,
    /// `make_var_ref_ref` — 7 byte(s), pops 0, pushes 2
    MakeVarRefRef = 120,
    /// `make_var_ref` — 5 byte(s), pops 0, pushes 2
    MakeVarRef = 121,
    /// `for_in_start` — 1 byte(s), pops 1, pushes 1
    ForInStart = 122,
    /// `for_of_start` — 1 byte(s), pops 1, pushes 3
    ForOfStart = 123,
    /// `for_await_of_start` — 1 byte(s), pops 1, pushes 3
    ForAwaitOfStart = 124,
    /// `for_in_next` — 1 byte(s), pops 1, pushes 3
    ForInNext = 125,
    /// `for_of_next` — 2 byte(s), pops 3, pushes 5
    ForOfNext = 126,
    /// `for_await_of_next` — 1 byte(s), pops 3, pushes 4
    ForAwaitOfNext = 127,
    /// `iterator_check_object` — 1 byte(s), pops 1, pushes 1
    IteratorCheckObject = 128,
    /// `iterator_get_value_done` — 1 byte(s), pops 2, pushes 3
    IteratorGetValueDone = 129,
    /// `iterator_close` — 1 byte(s), pops 3, pushes 0
    IteratorClose = 130,
    /// `iterator_next` — 1 byte(s), pops 4, pushes 4
    IteratorNext = 131,
    /// `iterator_call` — 2 byte(s), pops 4, pushes 5
    IteratorCall = 132,
    /// `initial_yield` — 1 byte(s), pops 0, pushes 0
    InitialYield = 133,
    /// `yield` — 1 byte(s), pops 1, pushes 2
    Yield = 134,
    /// `yield_star` — 1 byte(s), pops 1, pushes 2
    YieldStar = 135,
    /// `async_yield_star` — 1 byte(s), pops 1, pushes 2
    AsyncYieldStar = 136,
    /// `await` — 1 byte(s), pops 1, pushes 1
    Await = 137,
    /// `neg` — 1 byte(s), pops 1, pushes 1
    Neg = 138,
    /// `plus` — 1 byte(s), pops 1, pushes 1
    Plus = 139,
    /// `dec` — 1 byte(s), pops 1, pushes 1
    Dec = 140,
    /// `inc` — 1 byte(s), pops 1, pushes 1
    Inc = 141,
    /// `post_dec` — 1 byte(s), pops 1, pushes 2
    PostDec = 142,
    /// `post_inc` — 1 byte(s), pops 1, pushes 2
    PostInc = 143,
    /// `dec_loc` — 2 byte(s), pops 0, pushes 0
    DecLoc = 144,
    /// `inc_loc` — 2 byte(s), pops 0, pushes 0
    IncLoc = 145,
    /// `add_loc` — 2 byte(s), pops 1, pushes 0
    AddLoc = 146,
    /// `not` — 1 byte(s), pops 1, pushes 1
    Not = 147,
    /// `lnot` — 1 byte(s), pops 1, pushes 1
    Lnot = 148,
    /// `typeof` — 1 byte(s), pops 1, pushes 1
    Typeof = 149,
    /// `delete` — 1 byte(s), pops 2, pushes 1
    Delete = 150,
    /// `delete_var` — 5 byte(s), pops 0, pushes 1
    DeleteVar = 151,
    /// `mul` — 1 byte(s), pops 2, pushes 1
    Mul = 152,
    /// `div` — 1 byte(s), pops 2, pushes 1
    Div = 153,
    /// `mod` — 1 byte(s), pops 2, pushes 1
    Mod = 154,
    /// `add` — 1 byte(s), pops 2, pushes 1
    Add = 155,
    /// `sub` — 1 byte(s), pops 2, pushes 1
    Sub = 156,
    /// `pow` — 1 byte(s), pops 2, pushes 1
    Pow = 157,
    /// `shl` — 1 byte(s), pops 2, pushes 1
    Shl = 158,
    /// `sar` — 1 byte(s), pops 2, pushes 1
    Sar = 159,
    /// `shr` — 1 byte(s), pops 2, pushes 1
    Shr = 160,
    /// `lt` — 1 byte(s), pops 2, pushes 1
    Lt = 161,
    /// `lte` — 1 byte(s), pops 2, pushes 1
    Lte = 162,
    /// `gt` — 1 byte(s), pops 2, pushes 1
    Gt = 163,
    /// `gte` — 1 byte(s), pops 2, pushes 1
    Gte = 164,
    /// `instanceof` — 1 byte(s), pops 2, pushes 1
    Instanceof = 165,
    /// `in` — 1 byte(s), pops 2, pushes 1
    In = 166,
    /// `eq` — 1 byte(s), pops 2, pushes 1
    Eq = 167,
    /// `neq` — 1 byte(s), pops 2, pushes 1
    Neq = 168,
    /// `strict_eq` — 1 byte(s), pops 2, pushes 1
    StrictEq = 169,
    /// `strict_neq` — 1 byte(s), pops 2, pushes 1
    StrictNeq = 170,
    /// `and` — 1 byte(s), pops 2, pushes 1
    And = 171,
    /// `xor` — 1 byte(s), pops 2, pushes 1
    Xor = 172,
    /// `or` — 1 byte(s), pops 2, pushes 1
    Or = 173,
    /// `is_undefined_or_null` — 1 byte(s), pops 1, pushes 1
    IsUndefinedOrNull = 174,
    /// `private_in` — 1 byte(s), pops 2, pushes 1
    PrivateIn = 175,
    /// `push_bigint_i32` — 5 byte(s), pops 0, pushes 1
    PushBigintI32 = 176,
    /// `using_dispose_init` — 1 byte(s), pops 0, pushes 1
    UsingDisposeInit = 177,
    /// `using_dispose` — 3 byte(s), pops 1, pushes 1
    UsingDispose = 178,
    /// `using_dispose_async` — 3 byte(s), pops 0, pushes 1
    UsingDisposeAsync = 179,
    /// `using_dispose_merge` — 1 byte(s), pops 2, pushes 1
    UsingDisposeMerge = 180,
    /// `using_dispose_end` — 1 byte(s), pops 1, pushes 0
    UsingDisposeEnd = 181,
    /// `using_check` — 2 byte(s), pops 1, pushes 2
    UsingCheck = 182,
    /// `nop` — 1 byte(s), pops 0, pushes 0
    Nop = 183,
    /// `push_minus1` — 1 byte(s), pops 0, pushes 1
    PushMinus1 = 184,
    /// `push_0` — 1 byte(s), pops 0, pushes 1
    Push0 = 185,
    /// `push_1` — 1 byte(s), pops 0, pushes 1
    Push1 = 186,
    /// `push_2` — 1 byte(s), pops 0, pushes 1
    Push2 = 187,
    /// `push_3` — 1 byte(s), pops 0, pushes 1
    Push3 = 188,
    /// `push_4` — 1 byte(s), pops 0, pushes 1
    Push4 = 189,
    /// `push_5` — 1 byte(s), pops 0, pushes 1
    Push5 = 190,
    /// `push_6` — 1 byte(s), pops 0, pushes 1
    Push6 = 191,
    /// `push_7` — 1 byte(s), pops 0, pushes 1
    Push7 = 192,
    /// `push_i8` — 2 byte(s), pops 0, pushes 1
    PushI8 = 193,
    /// `push_i16` — 3 byte(s), pops 0, pushes 1
    PushI16 = 194,
    /// `push_const8` — 2 byte(s), pops 0, pushes 1
    PushConst8 = 195,
    /// `fclosure8` — 2 byte(s), pops 0, pushes 1
    Fclosure8 = 196,
    /// `push_empty_string` — 1 byte(s), pops 0, pushes 1
    PushEmptyString = 197,
    /// `get_loc8` — 2 byte(s), pops 0, pushes 1
    GetLoc8 = 198,
    /// `put_loc8` — 2 byte(s), pops 1, pushes 0
    PutLoc8 = 199,
    /// `set_loc8` — 2 byte(s), pops 1, pushes 1
    SetLoc8 = 200,
    /// `get_loc0` — 1 byte(s), pops 0, pushes 1
    GetLoc0 = 201,
    /// `get_loc1` — 1 byte(s), pops 0, pushes 1
    GetLoc1 = 202,
    /// `get_loc2` — 1 byte(s), pops 0, pushes 1
    GetLoc2 = 203,
    /// `get_loc3` — 1 byte(s), pops 0, pushes 1
    GetLoc3 = 204,
    /// `put_loc0` — 1 byte(s), pops 1, pushes 0
    PutLoc0 = 205,
    /// `put_loc1` — 1 byte(s), pops 1, pushes 0
    PutLoc1 = 206,
    /// `put_loc2` — 1 byte(s), pops 1, pushes 0
    PutLoc2 = 207,
    /// `put_loc3` — 1 byte(s), pops 1, pushes 0
    PutLoc3 = 208,
    /// `set_loc0` — 1 byte(s), pops 1, pushes 1
    SetLoc0 = 209,
    /// `set_loc1` — 1 byte(s), pops 1, pushes 1
    SetLoc1 = 210,
    /// `set_loc2` — 1 byte(s), pops 1, pushes 1
    SetLoc2 = 211,
    /// `set_loc3` — 1 byte(s), pops 1, pushes 1
    SetLoc3 = 212,
    /// `get_arg0` — 1 byte(s), pops 0, pushes 1
    GetArg0 = 213,
    /// `get_arg1` — 1 byte(s), pops 0, pushes 1
    GetArg1 = 214,
    /// `get_arg2` — 1 byte(s), pops 0, pushes 1
    GetArg2 = 215,
    /// `get_arg3` — 1 byte(s), pops 0, pushes 1
    GetArg3 = 216,
    /// `put_arg0` — 1 byte(s), pops 1, pushes 0
    PutArg0 = 217,
    /// `put_arg1` — 1 byte(s), pops 1, pushes 0
    PutArg1 = 218,
    /// `put_arg2` — 1 byte(s), pops 1, pushes 0
    PutArg2 = 219,
    /// `put_arg3` — 1 byte(s), pops 1, pushes 0
    PutArg3 = 220,
    /// `set_arg0` — 1 byte(s), pops 1, pushes 1
    SetArg0 = 221,
    /// `set_arg1` — 1 byte(s), pops 1, pushes 1
    SetArg1 = 222,
    /// `set_arg2` — 1 byte(s), pops 1, pushes 1
    SetArg2 = 223,
    /// `set_arg3` — 1 byte(s), pops 1, pushes 1
    SetArg3 = 224,
    /// `get_var_ref0` — 1 byte(s), pops 0, pushes 1
    GetVarRef0 = 225,
    /// `get_var_ref1` — 1 byte(s), pops 0, pushes 1
    GetVarRef1 = 226,
    /// `get_var_ref2` — 1 byte(s), pops 0, pushes 1
    GetVarRef2 = 227,
    /// `get_var_ref3` — 1 byte(s), pops 0, pushes 1
    GetVarRef3 = 228,
    /// `put_var_ref0` — 1 byte(s), pops 1, pushes 0
    PutVarRef0 = 229,
    /// `put_var_ref1` — 1 byte(s), pops 1, pushes 0
    PutVarRef1 = 230,
    /// `put_var_ref2` — 1 byte(s), pops 1, pushes 0
    PutVarRef2 = 231,
    /// `put_var_ref3` — 1 byte(s), pops 1, pushes 0
    PutVarRef3 = 232,
    /// `set_var_ref0` — 1 byte(s), pops 1, pushes 1
    SetVarRef0 = 233,
    /// `set_var_ref1` — 1 byte(s), pops 1, pushes 1
    SetVarRef1 = 234,
    /// `set_var_ref2` — 1 byte(s), pops 1, pushes 1
    SetVarRef2 = 235,
    /// `set_var_ref3` — 1 byte(s), pops 1, pushes 1
    SetVarRef3 = 236,
    /// `get_length` — 1 byte(s), pops 1, pushes 1
    GetLength = 237,
    /// `if_false8` — 2 byte(s), pops 1, pushes 0
    IfFalse8 = 238,
    /// `if_true8` — 2 byte(s), pops 1, pushes 0
    IfTrue8 = 239,
    /// `goto8` — 2 byte(s), pops 0, pushes 0
    Goto8 = 240,
    /// `goto16` — 3 byte(s), pops 0, pushes 0
    Goto16 = 241,
    /// `call0` — 1 byte(s), pops 1, pushes 1
    Call0 = 242,
    /// `call1` — 1 byte(s), pops 1, pushes 1
    Call1 = 243,
    /// `call2` — 1 byte(s), pops 1, pushes 1
    Call2 = 244,
    /// `call3` — 1 byte(s), pops 1, pushes 1
    Call3 = 245,
    /// `is_undefined` — 1 byte(s), pops 1, pushes 1
    IsUndefined = 246,
    /// `is_null` — 1 byte(s), pops 1, pushes 1
    IsNull = 247,
    /// `typeof_is_undefined` — 1 byte(s), pops 1, pushes 1
    TypeofIsUndefined = 248,
    /// `typeof_is_function` — 1 byte(s), pops 1, pushes 1
    TypeofIsFunction = 249,
}

/// Number of finished instructions (the reference `OP_COUNT`).
pub const OP_COUNT: u8 = 250;

/// An instruction that only exists while a function is being compiled.
///
/// The numbers overlap the short-form opcodes, exactly as in the reference
/// implementation, because these can never appear in finished bytecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum TempOpcode {
    /// `enter_scope` — 3 byte(s), pops 0, pushes 0
    EnterScope = 1,
    /// `leave_scope` — 3 byte(s), pops 0, pushes 0
    LeaveScope = 2,
    /// `label` — 5 byte(s), pops 0, pushes 0
    Label = 3,
    /// `scope_get_var_undef` — 7 byte(s), pops 0, pushes 1
    ScopeGetVarUndef = 4,
    /// `scope_get_var` — 7 byte(s), pops 0, pushes 1
    ScopeGetVar = 5,
    /// `scope_put_var` — 7 byte(s), pops 1, pushes 0
    ScopePutVar = 6,
    /// `scope_delete_var` — 7 byte(s), pops 0, pushes 1
    ScopeDeleteVar = 7,
    /// `scope_make_ref` — 11 byte(s), pops 0, pushes 2
    ScopeMakeRef = 8,
    /// `scope_get_ref` — 7 byte(s), pops 0, pushes 2
    ScopeGetRef = 9,
    /// `scope_put_var_init` — 7 byte(s), pops 0, pushes 2
    ScopePutVarInit = 10,
    /// `scope_get_var_checkthis` — 7 byte(s), pops 0, pushes 1
    ScopeGetVarCheckthis = 11,
    /// `scope_get_private_field` — 7 byte(s), pops 1, pushes 1
    ScopeGetPrivateField = 12,
    /// `scope_get_private_field2` — 7 byte(s), pops 1, pushes 2
    ScopeGetPrivateField2 = 13,
    /// `scope_put_private_field` — 7 byte(s), pops 2, pushes 0
    ScopePutPrivateField = 14,
    /// `scope_in_private_field` — 7 byte(s), pops 1, pushes 1
    ScopeInPrivateField = 15,
    /// `get_field_opt_chain` — 5 byte(s), pops 1, pushes 1
    GetFieldOptChain = 16,
    /// `get_array_el_opt_chain` — 1 byte(s), pops 2, pushes 1
    GetArrayElOptChain = 17,
    /// `set_class_name` — 5 byte(s), pops 1, pushes 1
    SetClassName = 18,
    /// `line_num` — 5 byte(s), pops 0, pushes 0
    LineNum = 19,
}

impl Opcode {
    /// The static description of this instruction.
    pub fn info(self) -> OpInfo {
        OP_INFO[self as usize]
    }

    /// The mnemonic.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// Instruction length in bytes.
    pub fn size(self) -> u8 {
        self.info().size
    }

    /// Decode an opcode byte. `None` for a value that is not an instruction.
    pub fn from_u8(byte: u8) -> Option<Opcode> {
        OP_BY_BYTE.get(byte as usize).copied().flatten()
    }

    /// Look up by mnemonic.
    pub fn by_name(name: &str) -> Option<Opcode> {
        OP_INFO
            .iter()
            .position(|info| info.name == name)
            .map(|index| OP_ALL[index])
    }
}

impl TempOpcode {
    /// The static description of this instruction.
    pub fn info(self) -> OpInfo {
        TEMP_INFO[self as usize - TEMP_START]
    }

    /// The mnemonic.
    pub fn name(self) -> &'static str {
        self.info().name
    }
}

const TEMP_START: usize = 1;

impl Opcode {
    /// The operand index that a short-form opcode encodes in its own number.
    ///
    /// `get_loc0` … `get_loc3` carry their slot index in the opcode byte, so the
    /// disassembler and the interpreter recover it by subtraction. Returns
    /// `None` for instructions with an explicit operand.
    pub fn specialised_index(self) -> Option<i32> {
        use Opcode::*;
        let byte = self as u32;
        let base = |op: Opcode| op as u32;
        // `push_minus1` sits immediately before `push_0`, so the family is a
        // contiguous range that starts one opcode earlier.
        if (base(PushMinus1)..=base(Push7)).contains(&byte) {
            // `push_minus1` yields -1, exactly as the reference computes it.
            return Some(byte as i32 - base(Push0) as i32);
        }
        if (base(GetLoc0)..=base(SetLoc3)).contains(&byte) {
            return Some(((byte - base(GetLoc0)) % 4) as i32);
        }
        if (base(GetArg0)..=base(SetArg3)).contains(&byte) {
            return Some(((byte - base(GetArg0)) % 4) as i32);
        }
        if (base(GetVarRef0)..=base(SetVarRef3)).contains(&byte) {
            return Some(((byte - base(GetVarRef0)) % 4) as i32);
        }
        if (base(Call0)..=base(Call3)).contains(&byte) {
            return Some((byte - base(Call0)) as i32);
        }
        None
    }

    /// The short form of `self` for a given operand index, when one exists.
    ///
    /// The compiler uses this once slot indices are known: `get_loc 7` becomes
    /// `get_loc3`, which is one byte instead of three.
    pub fn short_form(self, index: u32) -> Option<Opcode> {
        use Opcode::*;
        let base = match self {
            PushI32 => Push0,
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
            _ => return None,
        };
        if self == PushI32 {
            // `push_i32` folds into `push_minus1` (index -1, encoded as u32::MAX)
            // or `push_N` for 0..=7.
            if index == u32::MAX {
                return Some(PushMinus1);
            }
            return if index <= 7 {
                Opcode::from_u8(u8::try_from(Push0 as u32 + index).ok()?)
            } else {
                None
            };
        }
        if index > 3 {
            return None;
        }
        u8::try_from(base as u32 + index)
            .ok()
            .and_then(Opcode::from_u8)
    }
}

/// Every finished opcode, indexed by discriminant.
static OP_ALL: [Opcode; 250] = [
    Opcode::Invalid,
    Opcode::PushI32,
    Opcode::PushConst,
    Opcode::Fclosure,
    Opcode::PushAtomValue,
    Opcode::PrivateSymbol,
    Opcode::Undefined,
    Opcode::Null,
    Opcode::PushThis,
    Opcode::PushFalse,
    Opcode::PushTrue,
    Opcode::Object,
    Opcode::SpecialObject,
    Opcode::Rest,
    Opcode::Drop,
    Opcode::Nip,
    Opcode::Nip1,
    Opcode::Dup,
    Opcode::Dup1,
    Opcode::Dup2,
    Opcode::Dup3,
    Opcode::Insert2,
    Opcode::Insert3,
    Opcode::Insert4,
    Opcode::Perm3,
    Opcode::Perm4,
    Opcode::Perm5,
    Opcode::Swap,
    Opcode::Swap2,
    Opcode::Rot3l,
    Opcode::Rot3r,
    Opcode::Rot4l,
    Opcode::Rot5l,
    Opcode::CallConstructor,
    Opcode::Call,
    Opcode::TailCall,
    Opcode::CallMethod,
    Opcode::TailCallMethod,
    Opcode::ArrayFrom,
    Opcode::Apply,
    Opcode::Return,
    Opcode::ReturnUndef,
    Opcode::CheckCtorReturn,
    Opcode::CheckCtor,
    Opcode::InitCtor,
    Opcode::CheckBrand,
    Opcode::AddBrand,
    Opcode::ReturnAsync,
    Opcode::Throw,
    Opcode::ThrowError,
    Opcode::Eval,
    Opcode::ApplyEval,
    Opcode::Regexp,
    Opcode::GetSuper,
    Opcode::Import,
    Opcode::GetVarUndef,
    Opcode::GetVar,
    Opcode::PutVar,
    Opcode::PutVarInit,
    Opcode::GetRefValue,
    Opcode::PutRefValue,
    Opcode::GetField,
    Opcode::GetField2,
    Opcode::PutField,
    Opcode::GetPrivateField,
    Opcode::PutPrivateField,
    Opcode::DefinePrivateField,
    Opcode::GetArrayEl,
    Opcode::GetArrayEl2,
    Opcode::GetArrayEl3,
    Opcode::PutArrayEl,
    Opcode::GetSuperValue,
    Opcode::PutSuperValue,
    Opcode::DefineField,
    Opcode::SetName,
    Opcode::SetNameComputed,
    Opcode::SetProto,
    Opcode::SetHomeObject,
    Opcode::DefineArrayEl,
    Opcode::Append,
    Opcode::CopyDataProperties,
    Opcode::DefineMethod,
    Opcode::DefineMethodComputed,
    Opcode::DefineClass,
    Opcode::DefineClassComputed,
    Opcode::GetLoc,
    Opcode::PutLoc,
    Opcode::SetLoc,
    Opcode::GetArg,
    Opcode::PutArg,
    Opcode::SetArg,
    Opcode::GetVarRef,
    Opcode::PutVarRef,
    Opcode::SetVarRef,
    Opcode::SetLocUninitialized,
    Opcode::GetLocCheck,
    Opcode::PutLocCheck,
    Opcode::SetLocCheck,
    Opcode::PutLocCheckInit,
    Opcode::GetLocCheckthis,
    Opcode::GetVarRefCheck,
    Opcode::PutVarRefCheck,
    Opcode::PutVarRefCheckInit,
    Opcode::CloseLoc,
    Opcode::IfFalse,
    Opcode::IfTrue,
    Opcode::Goto,
    Opcode::Catch,
    Opcode::Gosub,
    Opcode::Ret,
    Opcode::NipCatch,
    Opcode::ToObject,
    Opcode::ToPropkey,
    Opcode::WithGetVar,
    Opcode::WithPutVar,
    Opcode::WithDeleteVar,
    Opcode::WithMakeRef,
    Opcode::WithGetRef,
    Opcode::MakeLocRef,
    Opcode::MakeArgRef,
    Opcode::MakeVarRefRef,
    Opcode::MakeVarRef,
    Opcode::ForInStart,
    Opcode::ForOfStart,
    Opcode::ForAwaitOfStart,
    Opcode::ForInNext,
    Opcode::ForOfNext,
    Opcode::ForAwaitOfNext,
    Opcode::IteratorCheckObject,
    Opcode::IteratorGetValueDone,
    Opcode::IteratorClose,
    Opcode::IteratorNext,
    Opcode::IteratorCall,
    Opcode::InitialYield,
    Opcode::Yield,
    Opcode::YieldStar,
    Opcode::AsyncYieldStar,
    Opcode::Await,
    Opcode::Neg,
    Opcode::Plus,
    Opcode::Dec,
    Opcode::Inc,
    Opcode::PostDec,
    Opcode::PostInc,
    Opcode::DecLoc,
    Opcode::IncLoc,
    Opcode::AddLoc,
    Opcode::Not,
    Opcode::Lnot,
    Opcode::Typeof,
    Opcode::Delete,
    Opcode::DeleteVar,
    Opcode::Mul,
    Opcode::Div,
    Opcode::Mod,
    Opcode::Add,
    Opcode::Sub,
    Opcode::Pow,
    Opcode::Shl,
    Opcode::Sar,
    Opcode::Shr,
    Opcode::Lt,
    Opcode::Lte,
    Opcode::Gt,
    Opcode::Gte,
    Opcode::Instanceof,
    Opcode::In,
    Opcode::Eq,
    Opcode::Neq,
    Opcode::StrictEq,
    Opcode::StrictNeq,
    Opcode::And,
    Opcode::Xor,
    Opcode::Or,
    Opcode::IsUndefinedOrNull,
    Opcode::PrivateIn,
    Opcode::PushBigintI32,
    Opcode::UsingDisposeInit,
    Opcode::UsingDispose,
    Opcode::UsingDisposeAsync,
    Opcode::UsingDisposeMerge,
    Opcode::UsingDisposeEnd,
    Opcode::UsingCheck,
    Opcode::Nop,
    Opcode::PushMinus1,
    Opcode::Push0,
    Opcode::Push1,
    Opcode::Push2,
    Opcode::Push3,
    Opcode::Push4,
    Opcode::Push5,
    Opcode::Push6,
    Opcode::Push7,
    Opcode::PushI8,
    Opcode::PushI16,
    Opcode::PushConst8,
    Opcode::Fclosure8,
    Opcode::PushEmptyString,
    Opcode::GetLoc8,
    Opcode::PutLoc8,
    Opcode::SetLoc8,
    Opcode::GetLoc0,
    Opcode::GetLoc1,
    Opcode::GetLoc2,
    Opcode::GetLoc3,
    Opcode::PutLoc0,
    Opcode::PutLoc1,
    Opcode::PutLoc2,
    Opcode::PutLoc3,
    Opcode::SetLoc0,
    Opcode::SetLoc1,
    Opcode::SetLoc2,
    Opcode::SetLoc3,
    Opcode::GetArg0,
    Opcode::GetArg1,
    Opcode::GetArg2,
    Opcode::GetArg3,
    Opcode::PutArg0,
    Opcode::PutArg1,
    Opcode::PutArg2,
    Opcode::PutArg3,
    Opcode::SetArg0,
    Opcode::SetArg1,
    Opcode::SetArg2,
    Opcode::SetArg3,
    Opcode::GetVarRef0,
    Opcode::GetVarRef1,
    Opcode::GetVarRef2,
    Opcode::GetVarRef3,
    Opcode::PutVarRef0,
    Opcode::PutVarRef1,
    Opcode::PutVarRef2,
    Opcode::PutVarRef3,
    Opcode::SetVarRef0,
    Opcode::SetVarRef1,
    Opcode::SetVarRef2,
    Opcode::SetVarRef3,
    Opcode::GetLength,
    Opcode::IfFalse8,
    Opcode::IfTrue8,
    Opcode::Goto8,
    Opcode::Goto16,
    Opcode::Call0,
    Opcode::Call1,
    Opcode::Call2,
    Opcode::Call3,
    Opcode::IsUndefined,
    Opcode::IsNull,
    Opcode::TypeofIsUndefined,
    Opcode::TypeofIsFunction,
];

/// Static descriptions, indexed by discriminant.
static OP_INFO: [OpInfo; 250] = [
    OpInfo {
        name: "invalid",
        size: 1,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_i32",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::I32,
    },
    OpInfo {
        name: "push_const",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Const,
    },
    OpInfo {
        name: "fclosure",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Const,
    },
    OpInfo {
        name: "push_atom_value",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "private_symbol",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "undefined",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "null",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_this",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_false",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_true",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "object",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "special_object",
        size: 2,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "rest",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::U16,
    },
    OpInfo {
        name: "drop",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "nip",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "nip1",
        size: 1,
        n_pop: 3,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dup",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dup1",
        size: 1,
        n_pop: 2,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dup2",
        size: 1,
        n_pop: 2,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dup3",
        size: 1,
        n_pop: 3,
        n_push: 6,
        format: OpFormat::None,
    },
    OpInfo {
        name: "insert2",
        size: 1,
        n_pop: 2,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "insert3",
        size: 1,
        n_pop: 3,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "insert4",
        size: 1,
        n_pop: 4,
        n_push: 5,
        format: OpFormat::None,
    },
    OpInfo {
        name: "perm3",
        size: 1,
        n_pop: 3,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "perm4",
        size: 1,
        n_pop: 4,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "perm5",
        size: 1,
        n_pop: 5,
        n_push: 5,
        format: OpFormat::None,
    },
    OpInfo {
        name: "swap",
        size: 1,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "swap2",
        size: 1,
        n_pop: 4,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "rot3l",
        size: 1,
        n_pop: 3,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "rot3r",
        size: 1,
        n_pop: 3,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "rot4l",
        size: 1,
        n_pop: 4,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "rot5l",
        size: 1,
        n_pop: 5,
        n_push: 5,
        format: OpFormat::None,
    },
    OpInfo {
        name: "call_constructor",
        size: 3,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "call",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "tail_call",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "call_method",
        size: 3,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "tail_call_method",
        size: 3,
        n_pop: 2,
        n_push: 0,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "array_from",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Npop,
    },
    OpInfo {
        name: "apply",
        size: 3,
        n_pop: 3,
        n_push: 1,
        format: OpFormat::U16,
    },
    OpInfo {
        name: "return",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "return_undef",
        size: 1,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "check_ctor_return",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "check_ctor",
        size: 1,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "init_ctor",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "check_brand",
        size: 1,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "add_brand",
        size: 1,
        n_pop: 2,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "return_async",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "throw",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "throw_error",
        size: 6,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::AtomU8,
    },
    OpInfo {
        name: "eval",
        size: 5,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NpopU16,
    },
    OpInfo {
        name: "apply_eval",
        size: 3,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::U16,
    },
    OpInfo {
        name: "regexp",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_super",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "import",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_var_undef",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "get_var",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "put_var",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "put_var_init",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "get_ref_value",
        size: 1,
        n_pop: 2,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "put_ref_value",
        size: 1,
        n_pop: 3,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_field",
        size: 5,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "get_field2",
        size: 5,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "put_field",
        size: 5,
        n_pop: 2,
        n_push: 0,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "get_private_field",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "put_private_field",
        size: 1,
        n_pop: 3,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "define_private_field",
        size: 1,
        n_pop: 3,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_array_el",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_array_el2",
        size: 1,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_array_el3",
        size: 1,
        n_pop: 2,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "put_array_el",
        size: 1,
        n_pop: 3,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_super_value",
        size: 1,
        n_pop: 3,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "put_super_value",
        size: 1,
        n_pop: 4,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "define_field",
        size: 5,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "set_name",
        size: 5,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "set_name_computed",
        size: 1,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "set_proto",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "set_home_object",
        size: 1,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "define_array_el",
        size: 1,
        n_pop: 3,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "append",
        size: 1,
        n_pop: 3,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "copy_data_properties",
        size: 2,
        n_pop: 3,
        n_push: 3,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "define_method",
        size: 6,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::AtomU8,
    },
    OpInfo {
        name: "define_method_computed",
        size: 2,
        n_pop: 3,
        n_push: 1,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "define_class",
        size: 6,
        n_pop: 2,
        n_push: 2,
        format: OpFormat::AtomU8,
    },
    OpInfo {
        name: "define_class_computed",
        size: 6,
        n_pop: 3,
        n_push: 3,
        format: OpFormat::AtomU8,
    },
    OpInfo {
        name: "get_loc",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "put_loc",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "set_loc",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "get_arg",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Arg,
    },
    OpInfo {
        name: "put_arg",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Arg,
    },
    OpInfo {
        name: "set_arg",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Arg,
    },
    OpInfo {
        name: "get_var_ref",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "put_var_ref",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "set_var_ref",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "set_loc_uninitialized",
        size: 3,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "get_loc_check",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "put_loc_check",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "set_loc_check",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "put_loc_check_init",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "get_loc_checkthis",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "get_var_ref_check",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "put_var_ref_check",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "put_var_ref_check_init",
        size: 3,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::VarRef,
    },
    OpInfo {
        name: "close_loc",
        size: 3,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "if_false",
        size: 5,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "if_true",
        size: 5,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "goto",
        size: 5,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "catch",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "gosub",
        size: 5,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "ret",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "nip_catch",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "to_object",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "to_propkey",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "with_get_var",
        size: 10,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::AtomLabelU8,
    },
    OpInfo {
        name: "with_put_var",
        size: 10,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::AtomLabelU8,
    },
    OpInfo {
        name: "with_delete_var",
        size: 10,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::AtomLabelU8,
    },
    OpInfo {
        name: "with_make_ref",
        size: 10,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::AtomLabelU8,
    },
    OpInfo {
        name: "with_get_ref",
        size: 10,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::AtomLabelU8,
    },
    OpInfo {
        name: "make_loc_ref",
        size: 7,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "make_arg_ref",
        size: 7,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "make_var_ref_ref",
        size: 7,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "make_var_ref",
        size: 5,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "for_in_start",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "for_of_start",
        size: 1,
        n_pop: 1,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "for_await_of_start",
        size: 1,
        n_pop: 1,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "for_in_next",
        size: 1,
        n_pop: 1,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "for_of_next",
        size: 2,
        n_pop: 3,
        n_push: 5,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "for_await_of_next",
        size: 1,
        n_pop: 3,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "iterator_check_object",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "iterator_get_value_done",
        size: 1,
        n_pop: 2,
        n_push: 3,
        format: OpFormat::None,
    },
    OpInfo {
        name: "iterator_close",
        size: 1,
        n_pop: 3,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "iterator_next",
        size: 1,
        n_pop: 4,
        n_push: 4,
        format: OpFormat::None,
    },
    OpInfo {
        name: "iterator_call",
        size: 2,
        n_pop: 4,
        n_push: 5,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "initial_yield",
        size: 1,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "yield",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "yield_star",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "async_yield_star",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "await",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "neg",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "plus",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dec",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "inc",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "post_dec",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "post_inc",
        size: 1,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::None,
    },
    OpInfo {
        name: "dec_loc",
        size: 2,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "inc_loc",
        size: 2,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "add_loc",
        size: 2,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "not",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "lnot",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "typeof",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "delete",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "delete_var",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "mul",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "div",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "mod",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "add",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "sub",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "pow",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "shl",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "sar",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "shr",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "lt",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "lte",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "gt",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "gte",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "instanceof",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "in",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "eq",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "neq",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "strict_eq",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "strict_neq",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "and",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "xor",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "or",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "is_undefined_or_null",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "private_in",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_bigint_i32",
        size: 5,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::I32,
    },
    OpInfo {
        name: "using_dispose_init",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "using_dispose",
        size: 3,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "using_dispose_async",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Loc,
    },
    OpInfo {
        name: "using_dispose_merge",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "using_dispose_end",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "using_check",
        size: 2,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::U8,
    },
    OpInfo {
        name: "nop",
        size: 1,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::None,
    },
    OpInfo {
        name: "push_minus1",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_0",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_1",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_2",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_3",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_4",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_5",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_6",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_7",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneInt,
    },
    OpInfo {
        name: "push_i8",
        size: 2,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::I8,
    },
    OpInfo {
        name: "push_i16",
        size: 3,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::I16,
    },
    OpInfo {
        name: "push_const8",
        size: 2,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Const8,
    },
    OpInfo {
        name: "fclosure8",
        size: 2,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Const8,
    },
    OpInfo {
        name: "push_empty_string",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "get_loc8",
        size: 2,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "put_loc8",
        size: 2,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "set_loc8",
        size: 2,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Loc8,
    },
    OpInfo {
        name: "get_loc0",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "get_loc1",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "get_loc2",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "get_loc3",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "put_loc0",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "put_loc1",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "put_loc2",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "put_loc3",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "set_loc0",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "set_loc1",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "set_loc2",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "set_loc3",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneLoc,
    },
    OpInfo {
        name: "get_arg0",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "get_arg1",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "get_arg2",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "get_arg3",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "put_arg0",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "put_arg1",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "put_arg2",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "put_arg3",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "set_arg0",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "set_arg1",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "set_arg2",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "set_arg3",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneArg,
    },
    OpInfo {
        name: "get_var_ref0",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "get_var_ref1",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "get_var_ref2",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "get_var_ref3",
        size: 1,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "put_var_ref0",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "put_var_ref1",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "put_var_ref2",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "put_var_ref3",
        size: 1,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "set_var_ref0",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "set_var_ref1",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "set_var_ref2",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "set_var_ref3",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::NoneVarRef,
    },
    OpInfo {
        name: "get_length",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "if_false8",
        size: 2,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Label8,
    },
    OpInfo {
        name: "if_true8",
        size: 2,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::Label8,
    },
    OpInfo {
        name: "goto8",
        size: 2,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Label8,
    },
    OpInfo {
        name: "goto16",
        size: 3,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Label16,
    },
    OpInfo {
        name: "call0",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Npopx,
    },
    OpInfo {
        name: "call1",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Npopx,
    },
    OpInfo {
        name: "call2",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Npopx,
    },
    OpInfo {
        name: "call3",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Npopx,
    },
    OpInfo {
        name: "is_undefined",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "is_null",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "typeof_is_undefined",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "typeof_is_function",
        size: 1,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::None,
    },
];

/// Descriptions of the temporary opcodes.
static TEMP_INFO: [OpInfo; 19] = [
    OpInfo {
        name: "enter_scope",
        size: 3,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::U16,
    },
    OpInfo {
        name: "leave_scope",
        size: 3,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::U16,
    },
    OpInfo {
        name: "label",
        size: 5,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::Label,
    },
    OpInfo {
        name: "scope_get_var_undef",
        size: 7,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_get_var",
        size: 7,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_put_var",
        size: 7,
        n_pop: 1,
        n_push: 0,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_delete_var",
        size: 7,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_make_ref",
        size: 11,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomLabelU16,
    },
    OpInfo {
        name: "scope_get_ref",
        size: 7,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_put_var_init",
        size: 7,
        n_pop: 0,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_get_var_checkthis",
        size: 7,
        n_pop: 0,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_get_private_field",
        size: 7,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_get_private_field2",
        size: 7,
        n_pop: 1,
        n_push: 2,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_put_private_field",
        size: 7,
        n_pop: 2,
        n_push: 0,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "scope_in_private_field",
        size: 7,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::AtomU16,
    },
    OpInfo {
        name: "get_field_opt_chain",
        size: 5,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::Atom,
    },
    OpInfo {
        name: "get_array_el_opt_chain",
        size: 1,
        n_pop: 2,
        n_push: 1,
        format: OpFormat::None,
    },
    OpInfo {
        name: "set_class_name",
        size: 5,
        n_pop: 1,
        n_push: 1,
        format: OpFormat::U32,
    },
    OpInfo {
        name: "line_num",
        size: 5,
        n_pop: 0,
        n_push: 0,
        format: OpFormat::U32,
    },
];

/// Decode table indexed by opcode byte.
const OP_BY_BYTE: [Option<Opcode>; 256] = [
    Some(Opcode::Invalid),
    Some(Opcode::PushI32),
    Some(Opcode::PushConst),
    Some(Opcode::Fclosure),
    Some(Opcode::PushAtomValue),
    Some(Opcode::PrivateSymbol),
    Some(Opcode::Undefined),
    Some(Opcode::Null),
    Some(Opcode::PushThis),
    Some(Opcode::PushFalse),
    Some(Opcode::PushTrue),
    Some(Opcode::Object),
    Some(Opcode::SpecialObject),
    Some(Opcode::Rest),
    Some(Opcode::Drop),
    Some(Opcode::Nip),
    Some(Opcode::Nip1),
    Some(Opcode::Dup),
    Some(Opcode::Dup1),
    Some(Opcode::Dup2),
    Some(Opcode::Dup3),
    Some(Opcode::Insert2),
    Some(Opcode::Insert3),
    Some(Opcode::Insert4),
    Some(Opcode::Perm3),
    Some(Opcode::Perm4),
    Some(Opcode::Perm5),
    Some(Opcode::Swap),
    Some(Opcode::Swap2),
    Some(Opcode::Rot3l),
    Some(Opcode::Rot3r),
    Some(Opcode::Rot4l),
    Some(Opcode::Rot5l),
    Some(Opcode::CallConstructor),
    Some(Opcode::Call),
    Some(Opcode::TailCall),
    Some(Opcode::CallMethod),
    Some(Opcode::TailCallMethod),
    Some(Opcode::ArrayFrom),
    Some(Opcode::Apply),
    Some(Opcode::Return),
    Some(Opcode::ReturnUndef),
    Some(Opcode::CheckCtorReturn),
    Some(Opcode::CheckCtor),
    Some(Opcode::InitCtor),
    Some(Opcode::CheckBrand),
    Some(Opcode::AddBrand),
    Some(Opcode::ReturnAsync),
    Some(Opcode::Throw),
    Some(Opcode::ThrowError),
    Some(Opcode::Eval),
    Some(Opcode::ApplyEval),
    Some(Opcode::Regexp),
    Some(Opcode::GetSuper),
    Some(Opcode::Import),
    Some(Opcode::GetVarUndef),
    Some(Opcode::GetVar),
    Some(Opcode::PutVar),
    Some(Opcode::PutVarInit),
    Some(Opcode::GetRefValue),
    Some(Opcode::PutRefValue),
    Some(Opcode::GetField),
    Some(Opcode::GetField2),
    Some(Opcode::PutField),
    Some(Opcode::GetPrivateField),
    Some(Opcode::PutPrivateField),
    Some(Opcode::DefinePrivateField),
    Some(Opcode::GetArrayEl),
    Some(Opcode::GetArrayEl2),
    Some(Opcode::GetArrayEl3),
    Some(Opcode::PutArrayEl),
    Some(Opcode::GetSuperValue),
    Some(Opcode::PutSuperValue),
    Some(Opcode::DefineField),
    Some(Opcode::SetName),
    Some(Opcode::SetNameComputed),
    Some(Opcode::SetProto),
    Some(Opcode::SetHomeObject),
    Some(Opcode::DefineArrayEl),
    Some(Opcode::Append),
    Some(Opcode::CopyDataProperties),
    Some(Opcode::DefineMethod),
    Some(Opcode::DefineMethodComputed),
    Some(Opcode::DefineClass),
    Some(Opcode::DefineClassComputed),
    Some(Opcode::GetLoc),
    Some(Opcode::PutLoc),
    Some(Opcode::SetLoc),
    Some(Opcode::GetArg),
    Some(Opcode::PutArg),
    Some(Opcode::SetArg),
    Some(Opcode::GetVarRef),
    Some(Opcode::PutVarRef),
    Some(Opcode::SetVarRef),
    Some(Opcode::SetLocUninitialized),
    Some(Opcode::GetLocCheck),
    Some(Opcode::PutLocCheck),
    Some(Opcode::SetLocCheck),
    Some(Opcode::PutLocCheckInit),
    Some(Opcode::GetLocCheckthis),
    Some(Opcode::GetVarRefCheck),
    Some(Opcode::PutVarRefCheck),
    Some(Opcode::PutVarRefCheckInit),
    Some(Opcode::CloseLoc),
    Some(Opcode::IfFalse),
    Some(Opcode::IfTrue),
    Some(Opcode::Goto),
    Some(Opcode::Catch),
    Some(Opcode::Gosub),
    Some(Opcode::Ret),
    Some(Opcode::NipCatch),
    Some(Opcode::ToObject),
    Some(Opcode::ToPropkey),
    Some(Opcode::WithGetVar),
    Some(Opcode::WithPutVar),
    Some(Opcode::WithDeleteVar),
    Some(Opcode::WithMakeRef),
    Some(Opcode::WithGetRef),
    Some(Opcode::MakeLocRef),
    Some(Opcode::MakeArgRef),
    Some(Opcode::MakeVarRefRef),
    Some(Opcode::MakeVarRef),
    Some(Opcode::ForInStart),
    Some(Opcode::ForOfStart),
    Some(Opcode::ForAwaitOfStart),
    Some(Opcode::ForInNext),
    Some(Opcode::ForOfNext),
    Some(Opcode::ForAwaitOfNext),
    Some(Opcode::IteratorCheckObject),
    Some(Opcode::IteratorGetValueDone),
    Some(Opcode::IteratorClose),
    Some(Opcode::IteratorNext),
    Some(Opcode::IteratorCall),
    Some(Opcode::InitialYield),
    Some(Opcode::Yield),
    Some(Opcode::YieldStar),
    Some(Opcode::AsyncYieldStar),
    Some(Opcode::Await),
    Some(Opcode::Neg),
    Some(Opcode::Plus),
    Some(Opcode::Dec),
    Some(Opcode::Inc),
    Some(Opcode::PostDec),
    Some(Opcode::PostInc),
    Some(Opcode::DecLoc),
    Some(Opcode::IncLoc),
    Some(Opcode::AddLoc),
    Some(Opcode::Not),
    Some(Opcode::Lnot),
    Some(Opcode::Typeof),
    Some(Opcode::Delete),
    Some(Opcode::DeleteVar),
    Some(Opcode::Mul),
    Some(Opcode::Div),
    Some(Opcode::Mod),
    Some(Opcode::Add),
    Some(Opcode::Sub),
    Some(Opcode::Pow),
    Some(Opcode::Shl),
    Some(Opcode::Sar),
    Some(Opcode::Shr),
    Some(Opcode::Lt),
    Some(Opcode::Lte),
    Some(Opcode::Gt),
    Some(Opcode::Gte),
    Some(Opcode::Instanceof),
    Some(Opcode::In),
    Some(Opcode::Eq),
    Some(Opcode::Neq),
    Some(Opcode::StrictEq),
    Some(Opcode::StrictNeq),
    Some(Opcode::And),
    Some(Opcode::Xor),
    Some(Opcode::Or),
    Some(Opcode::IsUndefinedOrNull),
    Some(Opcode::PrivateIn),
    Some(Opcode::PushBigintI32),
    Some(Opcode::UsingDisposeInit),
    Some(Opcode::UsingDispose),
    Some(Opcode::UsingDisposeAsync),
    Some(Opcode::UsingDisposeMerge),
    Some(Opcode::UsingDisposeEnd),
    Some(Opcode::UsingCheck),
    Some(Opcode::Nop),
    Some(Opcode::PushMinus1),
    Some(Opcode::Push0),
    Some(Opcode::Push1),
    Some(Opcode::Push2),
    Some(Opcode::Push3),
    Some(Opcode::Push4),
    Some(Opcode::Push5),
    Some(Opcode::Push6),
    Some(Opcode::Push7),
    Some(Opcode::PushI8),
    Some(Opcode::PushI16),
    Some(Opcode::PushConst8),
    Some(Opcode::Fclosure8),
    Some(Opcode::PushEmptyString),
    Some(Opcode::GetLoc8),
    Some(Opcode::PutLoc8),
    Some(Opcode::SetLoc8),
    Some(Opcode::GetLoc0),
    Some(Opcode::GetLoc1),
    Some(Opcode::GetLoc2),
    Some(Opcode::GetLoc3),
    Some(Opcode::PutLoc0),
    Some(Opcode::PutLoc1),
    Some(Opcode::PutLoc2),
    Some(Opcode::PutLoc3),
    Some(Opcode::SetLoc0),
    Some(Opcode::SetLoc1),
    Some(Opcode::SetLoc2),
    Some(Opcode::SetLoc3),
    Some(Opcode::GetArg0),
    Some(Opcode::GetArg1),
    Some(Opcode::GetArg2),
    Some(Opcode::GetArg3),
    Some(Opcode::PutArg0),
    Some(Opcode::PutArg1),
    Some(Opcode::PutArg2),
    Some(Opcode::PutArg3),
    Some(Opcode::SetArg0),
    Some(Opcode::SetArg1),
    Some(Opcode::SetArg2),
    Some(Opcode::SetArg3),
    Some(Opcode::GetVarRef0),
    Some(Opcode::GetVarRef1),
    Some(Opcode::GetVarRef2),
    Some(Opcode::GetVarRef3),
    Some(Opcode::PutVarRef0),
    Some(Opcode::PutVarRef1),
    Some(Opcode::PutVarRef2),
    Some(Opcode::PutVarRef3),
    Some(Opcode::SetVarRef0),
    Some(Opcode::SetVarRef1),
    Some(Opcode::SetVarRef2),
    Some(Opcode::SetVarRef3),
    Some(Opcode::GetLength),
    Some(Opcode::IfFalse8),
    Some(Opcode::IfTrue8),
    Some(Opcode::Goto8),
    Some(Opcode::Goto16),
    Some(Opcode::Call0),
    Some(Opcode::Call1),
    Some(Opcode::Call2),
    Some(Opcode::Call3),
    Some(Opcode::IsUndefined),
    Some(Opcode::IsNull),
    Some(Opcode::TypeofIsUndefined),
    Some(Opcode::TypeofIsFunction),
    None,
    None,
    None,
    None,
    None,
    None,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_discriminant_matches_its_index() {
        for (index, opcode) in OP_ALL.iter().enumerate() {
            assert_eq!(
                *opcode as usize, index,
                "OP_ALL must be ordered by discriminant"
            );
            assert!(!opcode.name().is_empty());
            assert!(opcode.size() >= 1, "{} has a zero size", opcode.name());
        }
        assert_eq!(OP_ALL.len(), OP_COUNT as usize);
        assert_eq!(OP_INFO.len(), OP_ALL.len());
        assert_eq!(TEMP_INFO.len(), 20 - TEMP_START);
    }

    #[test]
    fn opcode_bytes_round_trip() {
        for opcode in OP_ALL {
            assert_eq!(
                Opcode::from_u8(opcode as u8),
                Some(opcode),
                "{}",
                opcode.name()
            );
        }
        assert_eq!(Opcode::from_u8(OP_COUNT), None);
        assert_eq!(Opcode::from_u8(255), None);
    }

    #[test]
    fn names_round_trip() {
        for opcode in OP_ALL {
            assert_eq!(
                Opcode::by_name(opcode.name()),
                Some(opcode),
                "{}",
                opcode.name()
            );
        }
    }

    #[test]
    fn temporary_opcodes_overlap_real_opcodes() {
        // The reference implementation reuses these numbers for the short-form
        // opcodes, so a temporary opcode can never survive into finished
        // bytecode: its byte decodes as some other, real instruction.
        assert_eq!(TempOpcode::EnterScope as u8, Opcode::PushI32 as u8);
        assert_eq!(TempOpcode::LineNum as u8, 19);
        assert!(Opcode::from_u8(TempOpcode::EnterScope as u8).is_some());
        assert!(Opcode::from_u8(TempOpcode::LineNum as u8).is_some());
    }
}

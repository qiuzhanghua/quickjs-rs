//! Structural invariants of object and array literals.
//!
//! The differential test in `disasm_diff.rs` proves the compiler produces what the
//! reference produces for the cases it lists. This file states the rules that have
//! to hold for any literal, so a case the reference suite does not cover still
//! cannot quietly collapse two source forms onto one instruction sequence.

use js_compile::bytecode::{Atom, Constant};
use js_compile::{Function, Opcode, compile_script, disassemble};
use js_parse::parse_script;

/// The function named `f`, which every case keeps its body in so that literals are
/// compiled by the same code path regardless of the test.
fn inner(source: &str) -> Function {
    let program = parse_script(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
    let outer = compile_script(&program).unwrap_or_else(|e| panic!("{source:?}: {e}"));
    find(&outer, "f").unwrap_or_else(|| panic!("no function f in {source:?}"))
}

fn find(function: &Function, name: &str) -> Option<Function> {
    for constant in &function.cpool {
        if let Constant::Function(inner) = constant {
            if inner.name == name {
                return Some((**inner).clone());
            }
            if let Some(found) = find(inner, name) {
                return Some(found);
            }
        }
    }
    None
}

fn opcodes(function: &Function) -> Vec<Opcode> {
    function
        .instructions()
        .into_iter()
        .map(|(_, instruction)| general_form(instruction.op))
        .collect()
}

fn general_form(op: Opcode) -> Opcode {
    use Opcode::*;
    // Only real variant names here: an identifier that is not a variant silently
    // becomes a binding, which turns the arm into a catch-all that type-checks and
    // is wrong. `crates/quickjs-compile/tests/tdz.rs` keeps the same list.
    match op {
        GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => GetLoc,
        GetLoc8 => GetLoc,
        PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => PutLoc,
        PutLoc8 => PutLoc,
        SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 => SetLoc,
        SetLoc8 => SetLoc,
        GetArg0 | GetArg1 | GetArg2 | GetArg3 => GetArg,
        PutArg0 | PutArg1 | PutArg2 | PutArg3 => PutArg,
        SetArg0 | SetArg1 | SetArg2 | SetArg3 => SetArg,
        GetVarRef0 | GetVarRef1 | GetVarRef2 | GetVarRef3 => GetVarRef,
        PutVarRef0 | PutVarRef1 | PutVarRef2 | PutVarRef3 => PutVarRef,
        SetVarRef0 | SetVarRef1 | SetVarRef2 | SetVarRef3 => SetVarRef,
        Call0 | Call1 | Call2 | Call3 => Call,
        PushMinus1 | Push0 | Push1 | Push2 | Push3 | Push4 | Push5 | Push6 | Push7 => PushI32,
        PushI8 => PushI32,
        PushI16 => PushI32,
        IfFalse8 => IfFalse,
        IfTrue8 => IfTrue,
        Goto8 | Goto16 => Goto,
        other => other,
    }
}

/// Atom tests, because the whole `define_field 2` vs `define_field "1.5"`
/// distinction rests on this one rule.
mod atoms {
    use super::Atom;

    #[test]
    fn a_canonical_index_becomes_a_number() {
        for text in ["0", "1", "10", "2147483647"] {
            assert!(
                matches!(Atom::from_text(text), Atom::Index(_)),
                "{text:?} is a canonical index"
            );
        }
    }

    #[test]
    fn everything_else_stays_a_string() {
        // A leading zero, a sign, an exponent, a decimal point, or an index past
        // the tag range: each of these is a string in the reference too, and the
        // disassembly shows it.
        for text in [
            "007",
            "-0",
            "1e2",
            "1.5",
            "2147483648",
            "4294967295",
            "a",
            "",
            " length",
            "0x10",
        ] {
            assert!(
                matches!(Atom::from_text(text), Atom::Str(_)),
                "{text:?} must stay a string"
            );
        }
    }

    #[test]
    fn the_index_is_the_numeric_value() {
        assert_eq!(Atom::from_text("2147483647"), Atom::Index(2147483647));
        assert_eq!(Atom::from_text("0"), Atom::Index(0));
    }
}

#[test]
fn a_field_key_and_a_string_key_are_different_atoms() {
    // `{2: x}` stores a tagged index; `{"1.5": x}` stores an interned string. The
    // two carry the same-looking operand slot but mean different things, so the
    // atom table has to remember which is which.
    let function = inner("function f(x){ return {2:x}; }");
    assert!(
        function.atoms.contains(&Atom::Index(2)),
        "numeric key must be a tagged atom: {:?}\n{}",
        function.atoms,
        disassemble(&function)
    );

    let function = inner("function f(x){ return {1.5:x}; }");
    assert!(
        function.atoms.contains(&Atom::Str("1.5".to_string())),
        "decimal key must be a string atom: {:?}\n{}",
        function.atoms,
        disassemble(&function)
    );
}

#[test]
fn a_method_is_not_a_field_holding_a_function() {
    // These two differ only in syntax, and only the AST can tell them apart: a
    // method defines a property with `define_method`, while a function expression
    // as a value is a plain field that also carries the property name.
    let method = opcodes(&inner("function f(){ return {m(){}}; }"));
    assert!(
        method.contains(&Opcode::DefineMethod),
        "a method uses define_method: {method:?}"
    );
    assert!(
        !method.contains(&Opcode::DefineField),
        "a method does not use define_field: {method:?}"
    );

    let field = opcodes(&inner("function f(){ return {a:function(){}}; }"));
    assert!(
        field.contains(&Opcode::DefineField),
        "a function value uses define_field: {field:?}"
    );
    assert!(
        !field.contains(&Opcode::DefineMethod),
        "a function value is not a method: {field:?}"
    );
    assert!(
        field.contains(&Opcode::SetName),
        "an anonymous function value takes the property name: {field:?}"
    );
}

#[test]
fn proto_syntax_sets_the_prototype_only_for_a_literal_key() {
    let ops = opcodes(&inner("function f(p){ return {__proto__:p}; }"));
    assert!(
        ops.contains(&Opcode::SetProto),
        "`__proto__: v` sets the prototype: {ops:?}"
    );
    assert!(!ops.contains(&Opcode::DefineField));

    // The shorthand is an ordinary property, and a computed key is one too: only
    // the plain `__proto__: v` form is special.
    let ops = opcodes(&inner("function f(){ return {__proto__}; }"));
    assert!(
        !ops.contains(&Opcode::SetProto),
        "the shorthand is a plain field: {ops:?}"
    );

    let ops = opcodes(&inner("function f(k){ return {[k]:p}; }"));
    assert!(
        !ops.contains(&Opcode::SetProto),
        "a computed key is not special: {ops:?}"
    );
}

#[test]
fn a_computed_method_converts_its_own_key() {
    // `{[k]: v}` needs `to_propkey` before the value, but `{[k](){}}` does not:
    // `define_method_computed` converts the key itself. Emitting the conversion in
    // both places would double it in one and change the stack in the other.
    let value = opcodes(&inner("function f(k){ return {[k]:1}; }"));
    assert!(
        value.contains(&Opcode::ToPropkey),
        "a computed value converts the key: {value:?}"
    );

    let method = opcodes(&inner("function f(k){ return {[k](){}}; }"));
    assert!(
        !method.contains(&Opcode::ToPropkey),
        "a computed method leaves the conversion to define_method_computed: {method:?}"
    );
    assert!(method.contains(&Opcode::DefineMethodComputed));
}

#[test]
fn array_literals_take_the_phase_their_shape_requires() {
    // Phase 1: a leading run of plain elements builds the array in one go.
    let ops = opcodes(&inner("function f(){ return [1,2,3]; }"));
    assert_eq!(
        ops.iter().filter(|op| **op == Opcode::ArrayFrom).count(),
        1,
        "{ops:?}"
    );
    assert!(!ops.contains(&Opcode::DefineField), "{ops:?}");
    assert!(!ops.contains(&Opcode::Append), "{ops:?}");

    // Phase 2: a hole cannot be passed to `array_from`, so it switches to explicit
    // indices. A hole at the end is the one shape that has to fix up `length`,
    // because an explicit index does not extend the array.
    let ops = opcodes(&inner("function f(){ return [1,,3]; }"));
    assert!(ops.contains(&Opcode::DefineField), "{ops:?}");
    assert!(
        !ops.contains(&Opcode::PutField),
        "an interior hole needs no length fixup: {ops:?}"
    );

    let ops = opcodes(&inner("function f(){ return [1,,]; }"));
    assert!(
        ops.contains(&Opcode::PutField),
        "a trailing hole sets the length: {ops:?}"
    );

    // Phase 3: a spread puts the index on the stack.
    let ops = opcodes(&inner("function f(b){ return [1,...b]; }"));
    assert!(ops.contains(&Opcode::Append), "{ops:?}");
}

#[test]
fn a_hole_before_a_spread_still_fixes_the_length() {
    // `need_length` has to survive from phase 2 into phase 3: the array is not
    // finished until the tail is appended, but the hole was already counted.
    let ops = opcodes(&inner("function f(){ return [1,,...a]; }"));
    assert!(ops.contains(&Opcode::Append), "{ops:?}");
    assert!(
        ops.contains(&Opcode::PutField),
        "the hole still has to reach `length`: {ops:?}"
    );
    assert!(
        !ops.contains(&Opcode::Drop),
        "the index on the stack goes into `length` rather than being dropped: {ops:?}"
    );
}

#[test]
fn more_than_thirty_two_leading_elements_leave_phase_one() {
    // The reference stops phase 1 at 32 elements, which is observable: the rest is
    // stored with explicit indices.
    let one_to_32: Vec<String> = (1..=32).map(|n| n.to_string()).collect();
    let ops = opcodes(&inner(&format!(
        "function f(){{ return [{}]; }}",
        one_to_32.join(",")
    )));
    assert!(
        !ops.contains(&Opcode::DefineField),
        "32 fit in one array_from: {ops:?}"
    );

    let one_to_33: Vec<String> = (1..=33).map(|n| n.to_string()).collect();
    let ops = opcodes(&inner(&format!(
        "function f(){{ return [{}]; }}",
        one_to_33.join(",")
    )));
    assert!(
        ops.contains(&Opcode::DefineField),
        "the 33rd element leaves phase one: {ops:?}"
    );
}

#[test]
fn atom_kind_survives_serialisation() {
    // The atom table is part of the bytecode, so a tagged index has to come back as
    // one; turning it into its decimal spelling would change what a later
    // disassembly claims.
    let function = inner(r#"function f(x){ return {2:x, "1.5":x}; }"#);
    let bytes = js_compile::serialize::serialize(&function);
    let restored = js_compile::serialize::deserialize(&bytes).expect("round trip");
    assert_eq!(restored.atoms, function.atoms);
    assert!(restored.atoms.contains(&Atom::Index(2)));
    assert!(restored.atoms.contains(&Atom::Str("1.5".to_string())));
    assert_eq!(disassemble(&restored), disassemble(&function));
}

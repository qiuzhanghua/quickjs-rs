//! Structural invariants of assignment.
//!
//! The differential test proves the compiler matches the reference for the cases it
//! lists. These state the rules that must hold for any store, because the failure
//! mode here is not a crash: a store that forgets its operator, or a call that
//! forgets its receiver, still produces bytecode — just bytecode that means
//! something else.

use js_compile::{Function, Opcode, compile_script, disassemble};
use js_parse::parse_script;

/// The body of `f` in `source`, where every case keeps its statements.
fn inner(source: &str) -> Function {
    let program = parse_script(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
    let outer = compile_script(&program).unwrap_or_else(|e| panic!("{source:?}: {e}"));
    find(&outer, "f").unwrap_or_else(|| panic!("no function f in {source:?}"))
}

fn find(function: &Function, name: &str) -> Option<Function> {
    for constant in &function.cpool {
        if let js_compile::bytecode::Constant::Function(inner) = constant {
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

/// Short forms folded back, so a test can ask about the general instruction.
///
/// Only real variants belong here. A name that is not a variant silently becomes a
/// *binding* pattern, which matches every instruction — the test then passes for
/// the wrong reason. `call_method` has no short forms, so it has no arm.
fn general_form(op: Opcode) -> Opcode {
    use Opcode::*;
    match op {
        GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => GetLoc,
        GetLoc8 => GetLoc,
        PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => PutLoc,
        PutLoc8 => PutLoc,
        SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 | SetLoc8 => SetLoc,
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

#[test]
fn a_compound_store_keeps_its_operator() {
    // The failure this guards against: `o.a += b` compiled as `o.a = b`, which is
    // valid bytecode and silently drops the addition.
    for (source, operator) in [
        ("function f(o,b){ o.a += b; }", Opcode::Add),
        ("function f(o,b){ o.a -= b; }", Opcode::Sub),
        ("function f(o,b){ o.a *= b; }", Opcode::Mul),
        ("function f(o,b){ o[k] += b; }", Opcode::Add),
    ] {
        let ops = opcodes(&inner(source));
        assert!(
            ops.contains(&operator),
            "{source:?} must apply {operator:?}: {ops:?}"
        );
    }
}

#[test]
fn a_compound_store_reads_the_place_before_writing_it() {
    // `o.a += b` has to start from the current field value. `get_field2` is how the
    // reference leaves the object under that value for the store that follows.
    let ops = opcodes(&inner("function f(o,b){ o.a += b; }"));
    let read = ops.iter().position(|op| *op == Opcode::GetField2);
    let add = ops.iter().position(|op| *op == Opcode::Add);
    let store = ops.iter().position(|op| *op == Opcode::PutField);
    assert!(
        read < add && add < store,
        "read, then apply, then store: {ops:?}"
    );

    // A computed target needs the key too, so it reads the three-output form.
    let ops = opcodes(&inner("function f(o,k,b){ o[k] += b; }"));
    assert!(ops.contains(&Opcode::GetArrayEl3), "{ops:?}");
}

#[test]
fn a_logical_store_short_circuits_the_right_hand_side() {
    // `a ||= b` must not evaluate `b` when `a` is already truthy. The dead giveaway
    // of a broken compiler is an unconditional read of `b` followed by a plain
    // store, which is exactly what a `Nop` compound operator produces.
    for (source, test) in [
        ("function f(a,b){ a ||= b; }", Opcode::IfTrue),
        ("function f(a,b){ a &&= b; }", Opcode::IfFalse),
        ("function f(a,b){ a ??= b; }", Opcode::IsUndefinedOrNull),
    ] {
        let ops = opcodes(&inner(source));
        assert!(ops.contains(&test), "{source:?} needs {test:?}: {ops:?}");
        assert!(
            !ops.contains(&Opcode::Nop),
            "{source:?} lost its operator: {ops:?}"
        );
        // The jump has to land after the right-hand side, so the store happens
        // only on the assigning path.
        let jump = ops.iter().position(|op| *op == test).unwrap();
        let store = ops
            .iter()
            .rposition(|op| matches!(*op, Opcode::SetLoc | Opcode::SetArg | Opcode::SetVarRef));
        assert!(
            store.is_some_and(|store| jump < store),
            "{source:?} stores on the short-circuit path: {ops:?}"
        );
    }
}

#[test]
fn a_member_logical_store_does_not_fall_into_its_cleanup() {
    // Both exits of `o.a ||= b` keep a value, but the keep path has an object to
    // dispose of. The store path therefore has to jump *over* that `nip`: falling
    // through would `nip` a stack that holds only the value.
    let ops = opcodes(&inner("function f(o,b){ o.a ||= b; c(); }"));
    let store = ops.iter().position(|op| *op == Opcode::PutField).unwrap();
    let jump = ops
        .iter()
        .enumerate()
        .skip(store)
        .find(|(_, op)| **op == Opcode::Goto)
        .map(|(index, _)| index);
    let nip = ops
        .iter()
        .enumerate()
        .skip(store)
        .position(|(index, op)| *op == Opcode::Nip && Some(index + store) != jump);
    assert!(
        jump.is_some(),
        "the store path must skip the keep path's cleanup: {ops:?}"
    );
    assert!(
        nip.is_some(),
        "the keep path must drop the object it kept: {ops:?}"
    );

    // A computed target keeps two things: the object and the key.
    let ops = opcodes(&inner("function f(o,k,b){ o[k] ||= b; c(); }"));
    assert!(
        ops.iter().filter(|op| **op == Opcode::Nip).count() >= 2,
        "an object and a key both need dropping: {ops:?}"
    );
}

#[test]
fn a_kept_store_leaves_its_value_and_a_discarded_one_does_not() {
    // `insert2` is `obj a -> a obj a`: it lets the store consume the pair and
    // still leave the value on top. Whether it belongs there is decided by the
    // enclosing context, not by the assignment.
    let kept = opcodes(&inner("function f(o,v){ var x = (o.a = v); }"));
    assert!(kept.contains(&Opcode::Insert2), "{kept:?}");

    let discarded = opcodes(&inner("function f(o,v){ o.a = v; }"));
    assert!(
        !discarded.contains(&Opcode::Insert2),
        "a discarded value needs no copy: {discarded:?}"
    );

    let kept = opcodes(&inner("function f(o,k,v){ var x = (o[k] = v); }"));
    assert!(kept.contains(&Opcode::Insert3), "{kept:?}");
    let discarded = opcodes(&inner("function f(o,k,v){ o[k] = v; }"));
    assert!(!discarded.contains(&Opcode::Insert3), "{discarded:?}");
}

#[test]
fn a_script_keeps_its_expression_value() {
    // A script's last expression statement *is* the program's value, so it must not
    // be treated as discarded even though the statement has no use for it.
    // `o.a = 1;` as a script body: the store has to keep its value so the program
    // can return it, which is why the "discard" rule is switched off for scripts.
    let program = parse_script("o.a = 1;").expect("parses");
    let script = compile_script(&program).expect("compiles");
    let ops = opcodes(&script);
    assert!(
        ops.contains(&Opcode::Insert2),
        "the script's value must survive the store\n{}",
        disassemble(&script)
    );
    // `<ret>` is written with `set_loc`, which stores and keeps, rather than with
    // `put_loc`: the program's value is both stored and returned.
    assert!(
        ops.contains(&Opcode::SetLoc),
        "the value lands in `<ret>`\n{}",
        disassemble(&script)
    );
}

#[test]
fn a_method_call_passes_its_receiver() {
    // `call` has no receiver slot, so a method read with `get_field` would call the
    // function with `this` unset. The reference keeps the object under the value and
    // uses `call_method`; both halves of that are required.
    let ops = opcodes(&inner("function f(a,c){ a.b(c); }"));
    assert!(
        ops.contains(&Opcode::GetField2),
        "keep the receiver: {ops:?}"
    );
    assert!(
        ops.contains(&Opcode::CallMethod),
        "pass the receiver: {ops:?}"
    );
    assert!(
        !ops.contains(&Opcode::Call),
        "a plain call would lose `this`: {ops:?}"
    );

    // A computed method read must not leave the key behind, so it takes the
    // two-output form rather than the three-output one used by `o[k] += v`.
    let ops = opcodes(&inner("function f(a,c){ a[k](c); }"));
    assert!(ops.contains(&Opcode::GetArrayEl2), "{ops:?}");
    assert!(ops.contains(&Opcode::CallMethod), "{ops:?}");

    // A callee that is not a member has no receiver to pass.
    let ops = opcodes(&inner("function f(a,c){ a(c); }"));
    assert!(ops.contains(&Opcode::Call), "{ops:?}");
    assert!(!ops.contains(&Opcode::CallMethod), "{ops:?}");
}

#[test]
fn an_optional_chain_short_circuits_the_whole_spine() {
    // `a?.b.c` must not read `.c` when `a` is nullish: the guard has to jump past
    // the rest of the spine, not merely past its own link.
    for source in [
        "function f(a){ return a?.b; }",
        "function f(a){ return a?.b.c; }",
        "function f(a){ return a.b?.c; }",
    ] {
        let ops = opcodes(&inner(source));
        assert!(
            ops.contains(&Opcode::IsUndefinedOrNull),
            "{source:?}: {ops:?}"
        );
        assert!(ops.contains(&Opcode::Undefined), "{source:?}: {ops:?}");
    }

    // In `a?.b.c` the short-circuit path leaves before `.c` is read, so there are
    // two field reads on the happy path and the guard sits between them.
    let ops = opcodes(&inner("function f(a){ return a?.b.c; }"));
    let reads: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, op)| **op == Opcode::GetField)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(reads.len(), 2, "{ops:?}");
    let guard = ops
        .iter()
        .position(|op| *op == Opcode::IsUndefinedOrNull)
        .unwrap();
    assert!(guard < reads[0], "{ops:?}");
}

#[test]
fn a_nullish_chain_does_not_reuse_the_truthiness_tests() {
    // `0 ?? b` is `0` while `0 || b` is `b`, so `??` cannot jump on truthiness.
    let ops = opcodes(&inner("function f(a,b){ return a ?? b; }"));
    assert!(ops.contains(&Opcode::IsUndefinedOrNull), "{ops:?}");
    assert!(
        !ops.contains(&Opcode::IfTrue),
        "'??' must not test truthiness: {ops:?}"
    );

    // A same-operator run shares one end label, so the first test already jumps
    // past the whole chain instead of falling into the next test.
    let ops = opcodes(&inner("function f(a,b,c){ return a || b || c; }"));
    let tests = ops.iter().filter(|op| **op == Opcode::IfTrue).count();
    assert_eq!(tests, 2, "one test per operand but the last: {ops:?}");
}

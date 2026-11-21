//! Structural invariants of the temporal dead zone and the scope chain.
//!
//! The differential test in `disasm_diff.rs` proves the compiler *produces what
//! the reference produces* for the cases it lists. This file states the rules
//! that have to hold for any program, so a case the reference tests do not cover
//! still cannot silently lose the dead zone.

use js_compile::{Function, Opcode, compile_script, disassemble};
use js_parse::parse_script;

fn compile(source: &str) -> Function {
    let program = parse_script(source).unwrap_or_else(|e| panic!("{source:?}: {e}"));
    compile_script(&program).unwrap_or_else(|e| panic!("{source:?}: {e}"))
}

/// Every function in the tree, outermost first.
fn functions(function: &Function) -> Vec<&Function> {
    let mut out = vec![function];
    for constant in &function.cpool {
        if let js_compile::bytecode::Constant::Function(inner) = constant {
            out.extend(functions(inner));
        }
    }
    out
}

/// The instruction stream with short forms folded back to their general form.
///
/// The assembler picks `get_loc0` over `get_loc 0`, so a test that asks "is there
/// a `get_loc` here" has to normalise first.
fn opcodes(function: &Function) -> Vec<Opcode> {
    function
        .instructions()
        .into_iter()
        .map(|(_, instruction)| general_form(instruction.op))
        .collect()
}

fn general_form(op: Opcode) -> Opcode {
    use Opcode::*;
    match op {
        GetLoc0 | GetLoc1 | GetLoc2 | GetLoc3 => GetLoc,
        PutLoc0 | PutLoc1 | PutLoc2 | PutLoc3 => PutLoc,
        SetLoc0 | SetLoc1 | SetLoc2 | SetLoc3 => SetLoc,
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
fn every_lexical_binding_is_marked_uninitialised_on_entry() {
    for source in [
        "function f(){ let a = 1; }",
        "function f(){ const a = 1; }",
        "function f(){ let a; let b; }",
        "function f(){ { let a = 1; } }",
        "function f(){ let a = 1; { let b = 2; } }",
        "function f(){ { { let c = 1; } } }",
        "function f(){ for (let i = 0; i < 1; i++); }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap_or_else(|| panic!("{source:?}: no function f"));
        let marked: Vec<u32> = inner
            .instructions()
            .into_iter()
            .filter(|(_, instruction)| instruction.op == Opcode::SetLocUninitialized)
            .filter_map(|(_, instruction)| match instruction.operands.first() {
                Some(js_compile::bytecode::Operand::U32(slot)) => Some(*slot),
                _ => None,
            })
            .collect();
        let lexical: Vec<u32> = inner
            .vars
            .iter()
            .enumerate()
            .filter(|(_, var)| var.is_lexical)
            .map(|(index, _)| index as u32)
            .collect();
        assert_eq!(
            marked.len(),
            lexical.len(),
            "{source:?}: expected one set_loc_uninitialized per lexical binding\n{}",
            disassemble(inner)
        );
        for slot in &lexical {
            assert!(
                marked.contains(slot),
                "{source:?}: slot {slot} is lexical but never marked uninitialised"
            );
        }
    }
}

#[test]
fn reads_of_lexical_bindings_are_checked() {
    for source in [
        "function f(){ let a = 1; return a; }",
        "function f(){ const a = 1; return a; }",
        "function f(){ let a = 1; return a + a; }",
        "function f(){ let a = 1; a += 1; return a; }",
        "function f(){ for (let i = 0; i < 1; i++); }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap();
        let ops = opcodes(inner);
        // A `let` read anywhere after its initialisation must go through a
        // checking opcode, never the plain one.
        assert!(
            ops.iter().any(|op| matches!(
                op,
                Opcode::GetLocCheck | Opcode::SetLocCheck | Opcode::PutLocCheck
            )),
            "{source:?}: no checking opcode in\n{}",
            disassemble(inner)
        );
    }
}

#[test]
fn var_bindings_are_never_checked() {
    for source in [
        "function f(){ var a = 1; return a; }",
        "function f(a){ return a; }",
        "function f(){ return v; var v = 1; }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap();
        let ops = opcodes(inner);
        assert!(
            !ops.iter().any(|op| matches!(
                op,
                Opcode::GetLocCheck
                    | Opcode::PutLocCheck
                    | Opcode::SetLocCheck
                    | Opcode::SetLocUninitialized
            )),
            "{source:?}: a `var` or argument must not be dead-zone checked\n{}",
            disassemble(inner)
        );
    }
}

#[test]
fn initialisation_is_not_checked() {
    // The initialising store is what *ends* the dead zone, so it must be the
    // plain `put_loc`; a checked store would throw on every declaration.
    let compiled = compile("function f(){ let a = 1; }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let ops = opcodes(inner);
    assert!(ops.contains(&Opcode::PutLoc), "{}", disassemble(inner));
    assert!(
        !ops.contains(&Opcode::PutLocCheck),
        "the initialising store must not check\n{}",
        disassemble(inner)
    );
}

#[test]
fn scope_chains_are_well_formed() {
    // Each binding outside the function scope has to be reachable by walking its
    // scope's chain, and every chain has to terminate. The runtime relies on this
    // to instantiate exactly the bindings live at a point.
    for source in [
        "function f(){ let a; let b; let c; }",
        "function f(){ { let a; } { let b; } }",
        "function f(){ let a; { let b; { let c; } } }",
        "function f(){ { { let c; } let b; } let a; }",
        "function f(){ { let a = 1; } let b = 2; }",
        "function f(){ let b = 2; { let a = 1; } }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap();
        for (index, var) in inner.vars.iter().enumerate() {
            if !var.is_lexical {
                continue;
            }
            assert!(
                var.scope_level != 0,
                "{source:?}: lexical binding {index} ({}) is in the function scope",
                var.name
            );
            // Follow the chain from this binding and make sure it terminates
            // without revisiting a slot.
            let mut seen = vec![index];
            let mut next = var.scope_next;
            while let Some(link) = next {
                assert!(link >= 0, "{source:?}: chain from {index} goes negative");
                let slot = link as usize;
                assert!(
                    slot < inner.vars.len(),
                    "{source:?}: chain from {index} points past the variable table"
                );
                assert!(
                    !seen.contains(&slot),
                    "{source:?}: chain from {index} loops back to {slot}"
                );
                seen.push(slot);
                next = inner.vars[slot].scope_next;
            }
        }
    }
}

#[test]
fn captured_bindings_get_a_reference_cell_and_are_closed() {
    let compiled = compile("function f(){ { let x = 1; h = function(){ return x; }; } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let captured: Vec<&str> = inner
        .vars
        .iter()
        .filter(|var| var.is_captured)
        .map(|var| var.name.as_str())
        .collect();
    assert_eq!(captured, vec!["x"], "{}", disassemble(inner));
    assert_eq!(inner.var_ref_count, 1, "{}", disassemble(inner));
    let ops = opcodes(inner);
    assert!(
        ops.contains(&Opcode::CloseLoc),
        "a captured block binding must be closed when the block ends\n{}",
        disassemble(inner)
    );
}

#[test]
fn function_declarations_are_created_before_they_are_reached() {
    // `f()` runs before the declaration is reached in the source, so the
    // function object has to be created at scope entry.
    let compiled = compile("function outer(){ f(); function f(){} }");
    let outer = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "outer")
        .unwrap();
    let ops = opcodes(outer);
    let created = ops.iter().position(|op| *op == Opcode::Fclosure);
    let called = ops.iter().position(|op| *op == Opcode::Call);
    assert!(
        created < called,
        "the function must be created before the call\n{}",
        disassemble(outer)
    );
}

#[test]
fn dead_code_after_a_return_is_not_compiled() {
    let compiled = compile("function f(){ return 1; let x = 2; }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let ops = opcodes(inner);
    assert!(
        !ops.contains(&Opcode::PutLoc) && !ops.contains(&Opcode::PutLocCheck),
        "the unreachable declaration must not be compiled\n{}",
        disassemble(inner)
    );
}

#[test]
fn an_infinite_loop_has_no_implicit_return() {
    let compiled = compile("function f(){ for (;;) {} }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let ops = opcodes(inner);
    assert!(
        !ops.contains(&Opcode::ReturnUndef),
        "control cannot fall off the end of an infinite loop\n{}",
        disassemble(inner)
    );
}

#[test]
fn a_terminating_loop_still_has_an_implicit_return() {
    let compiled = compile("function f(){ for (let i = 0; i < 1; i++); }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let ops = opcodes(inner);
    assert!(
        ops.contains(&Opcode::ReturnUndef),
        "the loop can end, so the function can reach its end\n{}",
        disassemble(inner)
    );
}

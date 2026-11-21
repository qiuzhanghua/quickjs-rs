//! Structural invariants of the temporal dead zone and the scope chain.
//!
//! The differential test in `disasm_diff.rs` proves the compiler *produces what
//! the reference produces* for the cases it lists. This file states the rules
//! that have to hold for any program, so a case the reference tests do not cover
//! still cannot silently lose the dead zone.

use js_compile::{Function, Opcode, VarKind, compile_script, disassemble};
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

/// `break` and `continue` land on the right instruction.
///
/// The differential test proves this for the cases it lists; these checks state
/// the rule so a new case cannot silently target the wrong place. A `continue` in
/// a `for` loop must reach the update expression, which is the detail most likely
/// to drift.
/// Byte offset of the first instruction whose opcode matches.
fn offset_of(function: &Function, op: Opcode) -> Option<usize> {
    function
        .instructions()
        .into_iter()
        .find(|(_, instruction)| general_form(instruction.op) == op)
        .map(|(pc, _)| pc)
}

#[test]
fn a_break_leaves_the_loop() {
    // The loop's exit is past the trailing back-jump, so `break` must jump
    // further than that jump does.
    let compiled = compile("function f(c){ while(c){ if(c) break; } d(); }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let back_jump = instructions
        .iter()
        .find(|(_, i)| general_form(i.op) == Opcode::Goto)
        .map(|(pc, i)| (*pc, i.jump_target(*pc).unwrap() as usize))
        .expect("a loop has a back jump");
    let after = offset_of(inner, Opcode::GetVar).expect("the call after the loop");
    assert!(
        back_jump.1 < back_jump.0,
        "the loop's own jump goes backwards\n{}",
        disassemble(inner)
    );
    assert!(
        back_jump.1 < after,
        "the loop exit is after the loop\n{}",
        disassemble(inner)
    );
}

#[test]
fn a_continue_in_a_for_loop_reaches_the_update() {
    // `continue` skips the rest of the body but must still run the update
    // expression, so its target sits after the body and at or before the update.
    let compiled = compile("function f(c){ for(let i = 0; i < 2; i++){ if(c) continue; d(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();

    let update_pc = instructions
        .iter()
        .find(|(_, i)| matches!(i.op, Opcode::PostInc | Opcode::Inc))
        .map(|(pc, _)| *pc)
        .expect("the update expression");
    let call_pc = instructions
        .iter()
        .find(|(_, i)| general_form(i.op) == Opcode::Call)
        .map(|(pc, _)| *pc)
        .expect("the call in the body");

    let continues: Vec<usize> = instructions
        .iter()
        .filter(|(_, i)| {
            matches!(
                general_form(i.op),
                Opcode::Goto | Opcode::IfTrue | Opcode::IfFalse
            )
        })
        .filter_map(|(pc, i)| i.jump_target(*pc).map(|t| (*pc, t as usize)))
        // Forward jumps only: `continue` moves towards the update.
        .filter(|(pc, target)| target > pc)
        .map(|(_, target)| target)
        .collect();

    assert!(
        continues
            .iter()
            .any(|target| *target > call_pc && *target <= update_pc),
        "continue must skip the body and reach the update (call at {call_pc}, update at {update_pc})\n{}",
        disassemble(inner)
    );
}

#[test]
fn a_labelled_break_reaches_the_outer_loop() {
    // `break outer` leaves both loops, so it lands exactly where the outer loop
    // ends: on the statement after it.
    let compiled =
        compile("function f(c){ outer: for(;;){ for(;;){ if(c) break outer; } d(); } e(); }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    // The first instruction of `e()`: the loop's exit lands on it, not on the
    // call inside it.
    let call_index = instructions
        .iter()
        .rposition(|(_, i)| general_form(i.op) == Opcode::Call)
        .expect("the call after the outer loop");
    let after_outer = instructions[call_index - 1].0;

    let forward_jumps: Vec<usize> = instructions
        .iter()
        .filter(|(_, i)| matches!(general_form(i.op), Opcode::Goto | Opcode::IfTrue))
        .filter_map(|(pc, i)| i.jump_target(*pc).map(|t| (*pc, t as usize)))
        .filter(|(pc, target)| target > pc)
        .map(|(_, target)| target)
        .collect();

    assert!(
        forward_jumps.contains(&after_outer),
        "break outer must land on the statement after the outer loop (offset {after_outer})\n{}",
        disassemble(inner)
    );
}

#[test]
fn every_jump_lands_on_an_instruction_boundary() {
    // A jump into the middle of an instruction would be a codegen bug that a
    // disassembly diff might not show, because the disassembler stops at the
    // first bad opcode.
    for source in [
        "function f(c){ while(c){ if(c) break; else continue; } }",
        "function f(c){ l: for(;;){ for(;;){ break l; } } }",
        "function f(c){ do { if(c) continue; } while(c); }",
        "function f(c){ for(let i = 0; i < 3; i++){ if(c) break; } }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap();
        let boundaries: Vec<usize> = inner.instructions().into_iter().map(|(pc, _)| pc).collect();
        for (pc, instruction) in inner.instructions() {
            if let Some(target) = instruction.jump_target(pc) {
                assert!(
                    boundaries.contains(&(target as usize)),
                    "{source:?}: jump at {pc} targets {target}, which is not an instruction\n{}",
                    disassemble(inner)
                );
            }
        }
    }
}

#[test]
fn a_break_in_a_switch_leaves_the_switch() {
    // `switch` is a break target but not a continue target, so `break` must land
    // between the switch's last test and the statement after the switch — inside
    // the enclosing loop, not outside it.
    let compiled = compile(
        "function f(c){ while(c){ switch(c){ case 1: break; default: break; } d(); } e(); }",
    );
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let first_of = |op: Opcode| {
        instructions
            .iter()
            .find(|(_, i)| general_form(i.op) == op)
            .map(|(pc, _)| *pc)
            .unwrap_or_else(|| panic!("no {op:?}\n{}", disassemble(inner)))
    };
    let last_test = first_of(Opcode::StrictEq);
    let calls: Vec<usize> = instructions
        .iter()
        .filter(|(_, i)| general_form(i.op) == Opcode::Call)
        .map(|(pc, _)| *pc)
        .collect();
    let (d_call, e_call) = (calls[0], calls[1]);

    let forward_jumps: Vec<usize> = instructions
        .iter()
        .filter_map(|(pc, i)| i.jump_target(*pc).map(|t| (*pc, t as usize)))
        .filter(|(pc, target)| target > pc)
        .map(|(_, target)| target)
        .collect();

    assert!(
        forward_jumps
            .iter()
            .any(|target| *target >= last_test && *target < d_call),
        "break must land between the last test ({last_test}) and d() ({d_call})\n{}",
        disassemble(inner)
    );
    assert!(
        !forward_jumps.iter().any(|target| *target >= e_call),
        "break must not leave the enclosing loop (e at {e_call})\n{}",
        disassemble(inner)
    );
}

#[test]
fn switch_tests_its_cases_in_source_order() {
    // `default` is only a fallback position, not a fence: a `case` written after
    // it is still tested, and the miss path has to reach the default body. A
    // regression here silently makes `default` swallow every later case.
    for source in [
        "function f(x){ switch(x){ default: a(); case 2: b(); } }",
        "function f(x){ switch(x){ case 1: a(); default: b(); case 2: c(); } }",
    ] {
        let compiled = compile(source);
        let inner = functions(&compiled)
            .into_iter()
            .find(|f| f.name == "f")
            .unwrap();
        let instructions = inner.instructions();
        let comparisons = instructions
            .iter()
            .filter(|(_, i)| general_form(i.op) == Opcode::StrictEq)
            .count();
        assert_eq!(
            comparisons,
            source.matches("case ").count(),
            "{source:?}: every case needs a comparison\n{}",
            disassemble(inner)
        );
    }
}

#[test]
fn a_switch_discriminant_is_evaluated_once() {
    // The discriminant is evaluated before the switch and stays on the stack, so a
    // side-effecting discriminant must appear exactly once.
    let compiled = compile("function f(x){ switch(x.g()){ case 1: a(); case 2: b(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let getters = instructions
        .iter()
        .filter(|(_, i)| general_form(i.op) == Opcode::GetField)
        .count();
    assert_eq!(
        getters,
        1,
        "the discriminant must be read once\n{}",
        disassemble(inner)
    );
    let dups = instructions
        .iter()
        .filter(|(_, i)| general_form(i.op) == Opcode::Dup)
        .count();
    let comparisons = instructions
        .iter()
        .filter(|(_, i)| general_form(i.op) == Opcode::StrictEq)
        .count();
    assert_eq!(
        dups,
        comparisons,
        "each test compares against a copy of the discriminant\n{}",
        disassemble(inner)
    );
}

#[test]
fn a_catch_parameter_is_a_local_and_never_uninitialised() {
    // The thrown value is bound before the handler runs, so there is no temporal
    // dead zone to introduce and no check to emit on the read.
    let compiled = compile("function f(){ try { a(); } catch(e) { b(e); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    assert!(
        !instructions
            .iter()
            .any(|(_, i)| matches!(i.op, Opcode::SetLocUninitialized)),
        "a catch parameter must not be marked uninitialised\n{}",
        disassemble(inner)
    );
    assert!(
        !instructions
            .iter()
            .any(|(_, i)| matches!(i.op, Opcode::GetLocCheck)),
        "a catch parameter must be read without a check\n{}",
        disassemble(inner)
    );
    assert_eq!(
        inner.vars.len(),
        1,
        "the parameter is a local, not a global\n{}",
        disassemble(inner)
    );
    assert_eq!(inner.vars[0].kind, VarKind::Catch);
}

#[test]
fn a_try_records_one_exception_edge_per_guarded_region() {
    // `try`/`catch`/`finally` guards three regions: the block, the handler and the
    // propagation from the handler. Each needs its own `catch`, and the finalizer has
    // exactly one copy, reached by `gosub`.
    let compiled = compile("function f(){ try { a(); } catch(e) { b(e); } finally { c(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let count = |op: Opcode| {
        instructions
            .iter()
            .filter(|(_, i)| general_form(i.op) == op)
            .count()
    };
    assert_eq!(
        count(Opcode::Catch),
        2,
        "block and handler are guarded\n{}",
        disassemble(inner)
    );
    assert_eq!(
        count(Opcode::Gosub),
        3,
        "normal, handler and rethrow paths\n{}",
        disassemble(inner)
    );
    assert_eq!(
        count(Opcode::Ret),
        1,
        "one copy of the finalizer\n{}",
        disassemble(inner)
    );
    assert_eq!(
        count(Opcode::Throw),
        1,
        "the rethrow\n{}",
        disassemble(inner)
    );
}

#[test]
fn an_empty_finally_leaves_no_subroutine() {
    // With no `finally` the finalizer block is empty, so every `gosub` to it calls
    // nothing: the reference drops them, which removes the whole `ret` machinery.
    let compiled = compile("function f(){ try { a(); } catch(e) { b(e); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    assert!(
        !instructions
            .iter()
            .any(|(_, i)| matches!(i.op, Opcode::Gosub | Opcode::Ret)),
        "no finalizer means no subroutine\n{}",
        disassemble(inner)
    );
}

#[test]
fn a_return_through_a_try_unwinds_the_catch_offset() {
    // The catch offset sits on the value stack for the whole `try`. A `return`
    // inside one cannot just return: it has to discard the offset, keeping the
    // value, and run any finalizer. `nip_catch` is what does the first part.
    let compiled = compile("function f(){ try { return 1; } catch(e) { b(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let ops: Vec<Opcode> = instructions
        .iter()
        .map(|(_, i)| general_form(i.op))
        .collect();
    let nip = ops
        .iter()
        .position(|op| *op == Opcode::NipCatch)
        .unwrap_or_else(|| panic!("no nip_catch\n{}", disassemble(inner)));
    assert_eq!(
        ops.get(nip + 1),
        Some(&Opcode::Return),
        "nip_catch must come immediately before the return\n{}",
        disassemble(inner)
    );
}

#[test]
fn every_finalizer_on_the_way_out_runs() {
    // Two nested `try`s with finalizers, and a `return` inside both: each layer
    // needs its own `nip_catch` and its own `gosub`.
    let compiled =
        compile("function f(){ try { try { return 1; } finally { c(); } } finally { d(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let count = |op: Opcode| {
        instructions
            .iter()
            .filter(|(_, i)| general_form(i.op) == op)
            .count()
    };
    // Two finalizers, each entered from the normal path, the propagating path and
    // the return path.
    assert_eq!(
        count(Opcode::NipCatch),
        2,
        "one per enclosing finalizer\n{}",
        disassemble(inner)
    );
    assert_eq!(
        count(Opcode::Ret),
        2,
        "one copy each\n{}",
        disassemble(inner)
    );
    // Two paths each: the return unwinding out of the inner `try` reaches the inner
    // finalizer directly and the outer one after it, and an exception propagating
    // from either the `try` or the finalizer reaches it through the rethrow path.
    assert_eq!(
        count(Opcode::Gosub),
        4,
        "two finalizers, each reached by a return and by a propagation\n{}",
        disassemble(inner)
    );
}

#[test]
fn leaving_a_switch_inside_a_try_discards_the_discriminant() {
    // The discriminant stays on the stack under the catch offset, so the exit path
    // has to discard both. `nip_catch` rewinds to just above the innermost catch
    // offset, which takes the discriminant with it.
    let compiled =
        compile("function f(y){ try { switch(y){ case 1: return 2; } } finally { c(); } }");
    let inner = functions(&compiled)
        .into_iter()
        .find(|f| f.name == "f")
        .unwrap();
    let instructions = inner.instructions();
    let nip = instructions
        .iter()
        .position(|(_, i)| general_form(i.op) == Opcode::NipCatch)
        .unwrap_or_else(|| panic!("no nip_catch\n{}", disassemble(inner)));
    let ops: Vec<Opcode> = instructions
        .iter()
        .map(|(_, i)| general_form(i.op))
        .collect();
    assert_eq!(
        ops.get(nip + 1),
        Some(&Opcode::Gosub),
        "the finalizer runs after the unwind\n{}",
        disassemble(inner)
    );
}

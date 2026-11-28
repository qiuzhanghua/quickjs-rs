//! The compiled shape of a *suspensible* script — the `JS_EVAL_FLAG_ASYNC` body.
//!
//! `std.evalScript(source, {async: true})` compiles the source as a script whose top level
//! is the body of an `async` function. Two things follow, and both are pinned here because
//! the behaviour tests cannot see them:
//!
//! * the function's kind is `Async`, so the host runs it as a coroutine instead of a call —
//!   without that, `await` suspends in a body nothing knows how to resume;
//! * the completion value is wrapped in `{value}` before returning. Resolving the body's
//!   promise *with* the completion value would adopt it, so `evalScript("Promise.resolve(7)")`
//!   would answer `7` rather than a promise.

use js_compile::disasm::disassemble_code;
use js_compile::{compile_script, compile_script_suspensible, disassemble};
use js_parse::{parse_script, parse_suspensible_script};

#[test]
fn a_suspensible_script_is_compiled_as_an_async_function() {
    let program = parse_suspensible_script("1 + 1").expect("parses");
    let compiled = compile_script_suspensible(&program).expect("compiles");
    assert!(
        compiled.kind.is_async(),
        "the body must be an async function, or `await` suspends in a normal call"
    );
}

/// The epilogue, in the reference's order: `object`, `dup`, `get_loc <ret>`, `put_field`.
///
/// The shape comes straight from `quickjs.c` — "wrap the return value in an object so that
/// promises can be safely returned".
#[test]
fn a_suspensible_script_wraps_its_completion_value() {
    let program = parse_suspensible_script("1 + 1").expect("parses");
    let compiled = compile_script_suspensible(&program).expect("compiles");
    let mut code = String::new();
    disassemble_code(&mut code, &compiled);
    let instructions: Vec<&str> = code
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("loc"))
        .collect();
    let tail = &instructions[instructions.len().saturating_sub(5)..];
    let shape: Vec<&str> = tail
        .iter()
        .map(|line| line.split_whitespace().next().unwrap_or(""))
        .collect();
    assert_eq!(
        shape,
        ["object", "dup", "get_loc0", "put_field", "return"],
        "the wrapper epilogue differs from the reference's"
    );
    assert!(
        tail[3].contains("value"),
        "the wrapped field is named `value`, got: {:?}",
        tail[3]
    );
}

/// A plain script keeps the unwrapped epilogue, so the flag cannot be leaking.
#[test]
fn a_plain_script_does_not_wrap_its_completion_value() {
    let program = parse_script("1 + 1").expect("parses");
    let compiled = compile_script(&program).expect("compiles");
    assert!(!compiled.kind.is_async());
    assert!(
        !disassemble(&compiled).contains("put_field"),
        "a plain script must not build a {{value}} wrapper"
    );
}

/// `await` inside the body compiles to an `await` instruction, not to a call.
#[test]
fn await_compiles_to_an_await_instruction() {
    let program = parse_suspensible_script("await 1").expect("parses");
    let compiled = compile_script_suspensible(&program).expect("compiles");
    let mut code = String::new();
    disassemble_code(&mut code, &compiled);
    assert!(
        code.lines().any(|line| line.trim().starts_with("await")),
        "expected an `await` instruction, got:\n{code}"
    );
}

# quickjs-compile

The AST-to-bytecode compiler for [`quickjs-pure`].

It turns the tree [`quickjs-parse`] produces into the stack bytecode the VM runs, using
the same instruction set and the same opcode selection as
[QuickJS](https://github.com/bellard/quickjs). "Same" is meant literally: the
disassembly is compared instruction for instruction against the reference's `qjsc -d`,
so a construct that produces different code counts as a bug even when its observable
behaviour matches.

```toml
[dependencies]
quickjs-compile = "0.5"
```

The package is `quickjs-compile` but the library is `js_compile`:

```rust
use js_compile::{compile_script, disassemble};
use js_parse::parse_script;

let program = parse_script("var x = 1; x + 2;")?;
let function = compile_script(&program)?;
print!("{}", disassemble(&function));
# Ok::<(), Box<dyn std::error::Error>>(())
```

This example lives in `examples/compile.rs` and is run by the test suite.

## What is here

| Item | Role |
|---|---|
| `compile_script` / `compile_module` | `Program` to a `CompiledFunction` |
| `disassemble` | the readable listing the differential tests compare |
| `bytecode::Function` | atoms, constant pool, variables, resolved code, stack size |
| `opcode::Opcode` | the instruction set, with per-opcode size and stack effect |
| `serialize` | a stable on-disk form, for the AOT milestone |

## How it gets the code right

Compiling in two passes is what makes the output match the reference:

1. **Symbolic pass.** Instructions are emitted with labels and *names*, not slots.
   That is the only way to compile a reference to a binding declared later, or to
   decide that a `let` in a nested block shadows an outer one — the binding that wins
   is the one whose scope encloses the access.
2. **Resolve pass.** Names become slots, block scope chains are built, and then a
   peephole runs: `dup`+`put` becomes `set`, a `get` right after its `put` folds away,
   and unreachable code disappears. Each pattern is annotated with the bytecode it
   removes, because the whole point is to be one instruction shorter than a literal
   translation.

Scope handling is the part that has to be exactly right, and its failure mode is
silent: a `let` that leaks out of its block produces a program that keeps running with
the wrong answer. Sentinel block keys are therefore values no source offset can
produce, and a lookup for a function-scoped `var` is kept strictly separate from the
scope-chain walk that finds lexical bindings.

## Status

The compiler covers the constructs the VM can execute — expressions, control flow,
functions and closures, classes' absence aside — and reports
`… is not supported by the compiler yet` for the rest rather than emitting wrong code.
Milestone M5 extends the set.

[`quickjs-pure`]: https://crates.io/crates/quickjs-pure
[`quickjs-parse`]: https://crates.io/crates/quickjs-parse

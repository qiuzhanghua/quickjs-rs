# quickjs-parse

An ECMAScript lexer, AST and parser — the front end of [`quickjs-pure`].

It accepts the ES2025 grammar with the same accept/reject decisions as
[QuickJS](https://github.com/bellard/quickjs), including the early errors that are
easy to skip: duplicate lexical declarations, `let` used as a name in strict mode,
a `const` without an initialiser, an invalid `with` in strict mode, and the
parameter/body restrictions of a strict-mode function.

```toml
[dependencies]
quickjs-parse = "0.2"
```

The package is `quickjs-parse` but the library is `js_parse`:

```rust
use js_parse::{ast::StmtKind, parse_script};

let program = parse_script("var x = 1 + 2;")?;
for stmt in &program.body {
    if let StmtKind::VarDecl(decl) = &stmt.kind {
        assert_eq!(decl.declarators.len(), 1);
    }
}
# Ok::<(), js_parse::ParseError>(())
```

This example lives in `examples/parse.rs` and is run by the test suite.

## What is here

| Item | Role |
|---|---|
| `parse_script` | source to a `Program`; sloppy unless it starts with `"use strict"` |
| `parse_module` | the same, with module semantics (strict, top-level `await`, imports) |
| `parse_expression` | a single expression, for tests and `eval`-like entry points |
| `Lexer` / `Token` / `TokenKind` | the token stream, with spans |
| `ast` | `Program`, `Stmt`, `Expr`, `Function`, and every node kind |
| `Span` / `Position` | byte offset plus line and column, for diagnostics |

Every node carries a `Span`, which is what lets the compiler report a source position
and what a stack trace will use.

## Numbers and strings are decoded exactly

Both are places where a parser can look right and be wrong:

- **Numeric literals** produce the same `f64` bits as the C implementation, checked
  bit-for-bit through a `DataView` against the reference — including `0.1`, `1e21`,
  the largest denormal, and the boundary where `parseInt` and the grammar disagree.
- **String literals** decode every escape the grammar allows, and a surrogate pair
  written as two escapes combines into the code point it spells, so
  `"\u{dbff}\u{dfff}"` and `"\u{10ffff}"` are the same string.

## Recursion is bounded

`parser_limits::DEFAULT_MAX_DEPTH` caps nesting, so a pathological input reports a
syntax error instead of overflowing the stack.

## Status

Part of the [quickjs-rs] project. The parser accepts the upstream test suite and
examples with the same verdicts as the reference (`qjsc -c`) — see `tests/` for the
differential harnesses against a locally built oracle.

[`quickjs-pure`]: https://crates.io/crates/quickjs-pure
[quickjs-rs]: https://github.com/qiuzhanghua/quickjs-rs

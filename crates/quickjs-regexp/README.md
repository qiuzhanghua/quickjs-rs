# quickjs-regexp

A regular expression engine written from scratch, as used by [`quickjs-compile`] and
[`quickjs-pure`].

It has **no dependencies**, and that is the reason it is its own crate rather than a
module inside the interpreter. A `/…/u` literal is an *early error* when the pattern is
invalid (the spec says so for `RegularExpressionLiteral`), so the compiler has to be able
to check one while it is still parsing. The compiler sits below the interpreter, so it
cannot reach the interpreter's code — both reach this crate instead.

```toml
[dependencies]
quickjs-regexp = "0.5"
```

## The parts that are not obvious

**Input is `&[u32]`, not `&str`.** A JavaScript string may contain an unpaired surrogate:
`String.fromCharCode(0xD800)` is one code unit long and perfectly legal. `\p{Cs}` has to
match it, and no Rust `char` can hold it. Matching therefore works on code points, and the
caller supplies them; the engine never converts the text itself.

**Every property table is generated from the Unicode Character Database**, by
`scripts/gen-unicode-tables.py` in the repository — not from any test's expected output.
Generating tables from the expectations they are meant to satisfy would make the tests
agree with the data by construction and prove nothing. The tables cover the
`General_Category` values, `Script` and `Script_Extensions`, and the binary properties the
grammar permits.

**The set of property names is closed.** `\p{Expands_On_NFC}` and `\p{Hyphen}` exist in
the UCD but are deprecated, `\p{Block=Adlam}` is not a property the grammar exposes, and
`\p{Greek}` (a bare script name) is not one either. All of them are `SyntaxError`s, which
is why the generator drops everything outside the grammar's list instead of harvesting
whatever it finds.

## Supported

Literals, `.`, character classes with ranges, `\d \D \w \W \s \S`, captures and
backreferences, alternation, greedy and lazy quantifiers, anchors, lookahead, `\b`/`\B`,
`\p{…}` with the `u` and `v` flags, `v`-mode class set algebra (`&&`, `--`, `\q{…}`),
named groups, and `d`/`g`/`i`/`m`/`s`/`u`/`v`/`y`.

## Not supported

- Lookbehind (`(?<=…)`, `(?<!…)`) — rejected at compile time.
- The `v`-mode emoji *string* properties (`Basic_Emoji`, `RGI_Emoji` and friends). They
  denote sequences of code points rather than a set of single ones, so they need a
  different representation and a different matcher path.
- Full Unicode case folding. `i` uses the simple mapping, which covers the ranges scripts
  actually use.

[`quickjs-compile`]: https://crates.io/crates/quickjs-compile
[`quickjs-pure`]: https://crates.io/crates/quickjs-pure

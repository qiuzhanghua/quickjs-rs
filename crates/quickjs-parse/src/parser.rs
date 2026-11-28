//! The parser: tokens to [`crate::ast`].
//!
//! A recursive-descent parser with precedence climbing for expressions. It
//! follows the specification's grammar productions closely, which keeps the
//! early errors next to the code that can detect them.
//!
//! Two productions need backtracking / reinterpretation, exactly as in an
//! engine:
//!
//! * **arrow functions** use the cover grammar. `(` starts either a
//!   parenthesized expression or a parameter list; the parser speculatively
//!   parses a parameter list and commits only if `=>` follows, otherwise it
//!   restores the lexer and parses an expression.
//! * **`let`** at statement start is a declaration keyword when followed by a
//!   binding, otherwise an identifier.
//!
//! Automatic semicolon insertion is implemented where the grammar allows it
//! (`Stmt` terminators, `do…while`, `return`/`throw` restricted productions,
//! postfix `++`/`--`, `async` before arrow parameters, `yield`/`await`).
//!
//! Early errors implemented here (not at run time):
//!
//! * `with`, legacy octal literals/escapes, `delete identifier`,
//!   `eval`/`arguments` as binding or assignment target — strict mode only;
//! * `super` outside a method, `super()` outside a derived constructor,
//!   `new.target` outside a function;
//! * `await`/`yield` used as identifiers where they are reserved;
//! * duplicate `__proto__` in an object literal, duplicate private names;
//! * `break`/`continue` target validity, `return` outside a function;
//! * `??` mixed with `&&`/`||` without parentheses, `-x ** y`, assignment to a
//!   non-assignable target, `new` on an optional chain;
//! * duplicate parameter names (always for arrows/methods/non-simple lists).
//!
//! Not implemented yet (tracked for M1b.2): scope-analysis early errors such as
//! `let x; let x;` in the same block, and parameter/`var` conflicts.

use crate::ast::*;
use crate::error::{Result, SyntaxError};
use crate::lexer::Lexer;
use crate::token::{Keyword, NumberLiteral, Position, Punct, Span, TemplateKind, Token, TokenKind};

/// Everything that has to be saved to backtrack.
struct Snapshot<'a> {
    lexer: Lexer<'a>,
    current: Token,
    prev_end: Position,
    strict: bool,
    allow_in: bool,
    ctx: FnCtx,
    loop_depth: u32,
    switch_depth: u32,
    labels: Vec<Label>,
    private: Vec<PrivateEnv>,
    top_level: bool,
    cover_inits: Vec<Span>,
    pending_exports: Vec<(String, Span)>,
    current_legacy_octal: bool,
    scopes: Vec<Scope>,
}

/// Context of the function currently being parsed.
#[derive(Debug, Clone, Copy, Default)]
struct FnCtx {
    in_function: bool,
    in_generator: bool,
    in_async: bool,
    /// Inside a method (or a nested arrow in one): `super.x` is legal.
    in_method: bool,
    /// Inside a derived class constructor: `super()` is legal.
    in_derived_constructor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LabelKind {
    /// Labels a loop.
    Iteration,
    /// Labels any other statement.
    Other,
}

#[derive(Debug, Clone)]
struct Label {
    name: String,
    kind: LabelKind,
}

/// Private names declared by the class currently being parsed.
#[derive(Debug, Clone, Default)]
struct PrivateEnv {
    names: Vec<PrivateName>,
}

#[derive(Debug, Clone)]
struct PrivateName {
    name: String,
    has_get: bool,
    has_set: bool,
}

impl PrivateEnv {
    /// Register a private name. `get #x` + `set #x` is the only legal pair;
    /// every other duplicate (including `get #x` twice) is an early error.
    fn declare(&mut self, name: &str, accessor: Option<MethodKind>) -> bool {
        if let Some(existing) = self.names.iter_mut().find(|n| n.name == name) {
            return match accessor {
                Some(MethodKind::Get) if !existing.has_get => {
                    existing.has_get = true;
                    true
                }
                Some(MethodKind::Set) if !existing.has_set => {
                    existing.has_set = true;
                    true
                }
                _ => false,
            };
        }
        let mut entry = PrivateName {
            name: name.to_string(),
            has_get: false,
            has_set: false,
        };
        match accessor {
            Some(MethodKind::Get) => entry.has_get = true,
            Some(MethodKind::Set) => entry.has_set = true,
            _ => {}
        }
        self.names.push(entry);
        true
    }

    fn contains(&self, name: &str) -> bool {
        self.names.iter().any(|n| n.name == name)
    }
}

/// One lexical scope, used for the declaration-conflict early errors.
/// Which of the reference's four "redefinition" messages a clash produces.
///
/// The reference picks between them in `js_parse_error_redecl` (`quickjs.c:25055` onward)
/// from *where* the colliding binding lives, and the distinction is visible to a script
/// author, so it is reproduced rather than collapsed into one message:
///
/// | colliding binding | message |
/// |---|---|
/// | lexical, same scope | `invalid redefinition of lexical identifier` |
/// | `var` in the top-level scope | `invalid redefinition of global identifier` |
/// | `var`/function inside a function | `invalid redefinition of a variable` |
/// | a simple parameter of the same body | `invalid redefinition of parameter name` |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Redefinition {
    Lexical,
    Global,
    Variable,
    Parameter,
}

impl Redefinition {
    fn message(self) -> &'static str {
        match self {
            Redefinition::Lexical => "invalid redefinition of lexical identifier",
            Redefinition::Global => "invalid redefinition of global identifier",
            Redefinition::Variable => "invalid redefinition of a variable",
            Redefinition::Parameter => "invalid redefinition of parameter name",
        }
    }
}

#[derive(Debug, Clone)]
struct Scope {
    /// Whether this is a function/module scope (where `var` lands).
    is_var_scope: bool,
    /// `var`-declared names, including ones hoisted through this scope from a block.
    var_names: Vec<String>,
    /// `var`-declared names written *at this scope level*.
    ///
    /// `var_names` is the transitive set (`{ var a; }` puts `a` in the block *and* in the
    /// enclosing function), which is what the lexical-conflict checks want. This one is
    /// not transitive, because a module's function-declaration check compares scope *levels*:
    /// a `var` written inside a block only clashes with a function in that same block, not
    /// with one at the module's top level.
    var_names_here: Vec<String>,
    /// `let`/`const`/`class`/`import` names declared directly in this scope.
    lex_names: Vec<String>,
    /// Function-declaration names declared directly in this scope.
    ///
    /// Tracked apart from `var_names` because engines treat them differently:
    /// two function declarations may share a name, a function at the top level
    /// does not clash with an earlier `let`, and a function inside a block is
    /// scoped to that block instead of hoisting.
    fn_names: Vec<String>,
    /// Names of *simple* (identifier) parameters, if this is a function body.
    params: Vec<String>,
    /// Names bound by a `catch` parameter, as a simple identifier.
    ///
    /// Kept apart from `params` because the two clash differently: a `let` colliding with a
    /// function parameter is `invalid redefinition of parameter name`, while one colliding
    /// with a catch binding is `invalid redefinition of lexical identifier`. Both are exempt
    /// from the `var` check (`catch (e) { var e; }` is legal, and so is `function f(a){ var
    /// a; }`), which is why `declare_var` looks at neither.
    catch_names: Vec<String>,
    /// Function-declaration names whose function is a *plain* one — not `async`, not a
    /// generator.
    ///
    /// The sloppy-mode allowance for two function declarations in one block
    /// (Annex B.3.3.4) covers only the plain kind: the reference requires
    /// `var_def_type == JS_VAR_DEF_FUNCTION_DECL`, which an `async function` or a generator
    /// does not use. So `{ function f(){} function f(){} }` is accepted in a sloppy script
    /// while `{ function f(){} function* f(){} }` is not.
    plain_fn_names: Vec<String>,
    /// `import`-declared names.
    ///
    /// Recorded in `lex_names` as well, because an import *is* a lexical binding of the
    /// module. Kept apart because a module-level function declaration conflicting with an
    /// import is not an error in the reference — its `find_global_var` lookup does not see
    /// imports — so the module-code check has to be able to tell the two apart.
    import_names: Vec<String>,
}

impl Scope {
    fn block() -> Scope {
        Scope {
            is_var_scope: false,
            var_names: Vec::new(),
            var_names_here: Vec::new(),
            lex_names: Vec::new(),
            fn_names: Vec::new(),
            params: Vec::new(),
            catch_names: Vec::new(),
            plain_fn_names: Vec::new(),
            import_names: Vec::new(),
        }
    }

    fn function(params: Vec<String>) -> Scope {
        Scope {
            is_var_scope: true,
            var_names: Vec::new(),
            var_names_here: Vec::new(),
            lex_names: Vec::new(),
            fn_names: Vec::new(),
            params,
            catch_names: Vec::new(),
            plain_fn_names: Vec::new(),
            import_names: Vec::new(),
        }
    }
}

/// Which kind of target a pattern is being built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    /// `let x = 1`, `function f(x)`, `catch (x)` — binds new names.
    Binding,
    /// `x = 1`, `[a, b] = c` — assigns to existing bindings.
    Assignment,
}

/// The arrow head produced by a successful speculative parse.
struct ArrowHead {
    params: Vec<Pattern>,
    is_async: bool,
    span: Span,
    params_span: Span,
}

/// Default nesting limit.
///
/// QuickJS stops at roughly 700 nested parentheses, because it compares the real
/// stack pointer against `--stack-size` (1 MiB by default). This port cannot do
/// that portably, so it counts recursion depth instead; the depth a given stack
/// can hold depends on the frame size, which differs a lot between build
/// profiles (measured on an 8 MiB stack: ~220 levels in debug, ~1500 in release).
/// The limit is therefore profile-dependent and deliberately leaves headroom, so
/// that over-deep input produces a `SyntaxError: stack overflow` — exactly what
/// QuickJS reports — instead of aborting the process.
///
/// [`Parser::set_max_depth`] raises it for callers that know they have a bigger
/// stack (or a release build).
pub const DEFAULT_MAX_DEPTH: u32 = if cfg!(debug_assertions) { 120 } else { 600 };

/// ECMAScript 2025 parser.
pub struct Parser<'a> {
    lexer: Lexer<'a>,
    current: Token,
    prev_end: Position,
    strict: bool,
    module: bool,
    allow_in: bool,
    ctx: FnCtx,
    loop_depth: u32,
    switch_depth: u32,
    labels: Vec<Label>,
    private: Vec<PrivateEnv>,
    top_level: bool,
    /// Whether the token currently held by the parser contains an Annex B octal
    /// literal or escape (checked again when strictness is switched on).
    current_legacy_octal: bool,
    /// Whether the statement list being parsed is a `case`/`default` clause body.
    ///
    /// `using` is allowed in a block, a `for` head, a function body or a class static
    /// block, and nowhere else — in particular not *directly* in a clause's
    /// `StatementList`, which is what this flag distinguishes from an ordinary block.
    in_case_clause: bool,
    /// Current recursion depth, and the limit that turns it into a
    /// `SyntaxError` instead of a stack overflow.
    depth: u32,
    max_depth: u32,

    /// Spans of `{a = 1}` object-literal shorthands. They are only legal when
    /// the object is converted to a destructuring pattern, so any entry left
    /// over at the end of a statement is a syntax error.
    cover_inits: Vec<Span>,
    /// `export {name}` clauses whose local binding still has to be checked
    /// against the module's declared names (forward references are legal).
    pending_exports: Vec<(String, Span)>,
    /// Open lexical scopes, innermost last.
    scopes: Vec<Scope>,
}

/// Parse `src` as a `Script`.
pub fn parse_script(src: &str) -> Result<Program> {
    Parser::new(src, false)?.parse_program()
}

/// Parse `src` as a `Module`.
pub fn parse_module(src: &str) -> Result<Program> {
    Parser::new(src, true)?.parse_program()
}

/// Parse `src` as a `Script` whose top level is the body of an `async` function.
///
/// This is the reference's `JS_EVAL_FLAG_ASYNC`, which `std.evalScript` sets from
/// `{async: true}`. The flag buys exactly one thing: top-level `await` becomes an
/// operator, because the program's own function is an `async` one. It does *not* make the
/// top level a function — `return` stays an error and `new.target` stays unavailable — so
/// the compiled body still reports its completion value the way a script does, wrapped in
/// `{value}` so that a promise is not adopted on the way out.
pub fn parse_suspensible_script(src: &str) -> Result<Program> {
    Parser::new_suspensible(src)?.parse_program()
}

/// Parse `src` as a `Script` whose code is strict from the first token.
///
/// This is what a *direct* `eval` needs: the eval'd code inherits strictness from the
/// code that called it, so `eval("x = 1")` inside a strict function is a
/// `ReferenceError` rather than an implicit global. Prepending a `"use strict"`
/// directive instead would shift every column on the first line, and the positions are
/// exactly what a stack trace has to report.
pub fn parse_script_strict(src: &str) -> Result<Program> {
    let mut parser = Parser::new(src, false)?;
    parser.enter_strict()?;
    parser.parse_program()
}

/// Parse `src` as a direct `eval` body, inheriting `new.target` from the caller.
///
/// `eval("new.target")` is legal inside a function because the eval'd code runs in the
/// caller's function context, even though the source being parsed is a bare script with
/// no function of its own. `strict` is the caller's mode, which a sloppy eval does not
/// derive from its own text.
pub fn parse_eval_script(src: &str, strict: bool) -> Result<Program> {
    parse_eval_script_in(src, strict, EvalContext::default())
}

/// What the code that called a direct `eval` allows its body to use.
#[derive(Debug, Clone, Copy, Default)]
pub struct EvalContext {
    /// The caller is a method (or a nested arrow in one), so `super.x` is legal — the
    /// eval'd code runs with the caller's `[[HomeObject]]`.
    pub in_method: bool,
    /// The caller is a derived class constructor, so `super()` is legal too.
    pub in_derived_constructor: bool,
}

/// Parse `src` as a direct `eval` body with the caller's privileges.
///
/// An eval'd body is not a function of its own, but it runs *inside* one: `eval("super.f()")`
/// is legal in a method, and `eval("new.target")` is legal in any function. The parser has
/// to be told, because the source text alone has no way to know.
pub fn parse_eval_script_in(src: &str, strict: bool, context: EvalContext) -> Result<Program> {
    let mut parser = Parser::new(src, false)?;
    parser.ctx.in_function = true;
    parser.ctx.in_method = context.in_method;
    parser.ctx.in_derived_constructor = context.in_derived_constructor;
    if strict {
        parser.enter_strict()?;
    }
    parser.parse_program()
}

/// Parse `src` as the suspensible (async) form of a *global* eval body.
///
/// Two differences from [`parse_eval_script_in`], both from the same place in the
/// reference: a global eval is not a function body (`new_target_allowed`,
/// `super_allowed` and `super_call_allowed` are all false), and `JS_EVAL_FLAG_ASYNC`
/// makes the body `async`. So `new.target` and `super` stay out of reach while `await`
/// does not.
pub fn parse_suspensible_global_eval(src: &str, strict: bool) -> Result<Program> {
    let mut parser = Parser::new_suspensible(src)?;
    if strict {
        parser.enter_strict()?;
    }
    parser.parse_program()
}

/// Parse `src` as a single `Expression` (used by tests and by `eval`-like entry
/// points that need the expression grammar only).
pub fn parse_expression(src: &str) -> Result<Expr> {
    let mut parser = Parser::new(src, false)?;
    let expr = parser.parse_expression(true)?;
    if !parser.current.is_eof() {
        return Err(parser.unexpected());
    }
    Ok(expr)
}

impl<'a> Parser<'a> {
    /// Create a parser over `src`. A module is always strict.
    ///
    /// The program's own function is an `async` one for a module, so its top level may
    /// `await`. That is set here rather than at each `await` site: [`Parser::ctx`]
    /// describes the function currently being parsed, and the reference keys every
    /// `await` rule off the same thing (`fd->func_kind & JS_FUNC_ASYNC`).
    pub fn new(src: &'a str, module: bool) -> Result<Parser<'a>> {
        Parser::with_modes(src, module, false)
    }

    /// Create a parser whose program *is* the body of an `async` function.
    ///
    /// This is the reference's `JS_EVAL_FLAG_ASYNC`: a script compiled with it may use
    /// top-level `await`, exactly as a module body may. It is not the same as being inside
    /// a function — `return` is still an error, `new.target` is still unavailable — so
    /// `in_function` stays false.
    pub fn new_suspensible(src: &'a str) -> Result<Parser<'a>> {
        Parser::with_modes(src, false, true)
    }

    fn with_modes(src: &'a str, module: bool, async_body: bool) -> Result<Parser<'a>> {
        let mut lexer = Lexer::new(src);
        lexer.set_strict(module);
        let current = lexer.next_token()?;
        let current_legacy_octal = lexer.had_legacy_octal();
        if module && current_legacy_octal {
            return Err(SyntaxError::new(
                "octal literals and octal escape sequences are not allowed in strict mode",
                current.span,
            ));
        }
        Ok(Parser {
            lexer,
            current,
            prev_end: Position::START,
            strict: module,
            module,
            allow_in: true,
            ctx: FnCtx {
                in_async: module || async_body,
                ..FnCtx::default()
            },
            loop_depth: 0,
            switch_depth: 0,
            labels: Vec::new(),
            private: Vec::new(),
            top_level: true,
            current_legacy_octal,
            in_case_clause: false,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,

            cover_inits: Vec::new(),
            pending_exports: Vec::new(),
            scopes: vec![Scope::function(Vec::new())],
        })
    }

    /// Raise or lower the nesting limit.
    pub fn set_max_depth(&mut self, depth: u32) {
        self.max_depth = depth;
    }

    /// Enter one level of recursion.
    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        if self.depth > self.max_depth {
            return Err(SyntaxError::new("stack overflow", self.current.span));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    /// Parse the whole source.
    pub fn parse_program(&mut self) -> Result<Program> {
        let start = self.current.span.start;
        let mut body = Vec::new();
        let mut prologue = true;
        while !self.current.is_eof() {
            let stmt = self.parse_statement_list_item()?;
            if prologue {
                match self.directive_value(&stmt) {
                    Some(directive) if directive == "use strict" => self.enter_strict()?,
                    Some(_) => {}
                    None => prologue = false,
                }
            }
            body.push(stmt);
        }
        if self.module {
            self.check_module_exports(&body)?;
        }
        Ok(Program {
            kind: if self.module {
                ProgramKind::Module
            } else {
                ProgramKind::Script
            },
            body,
            span: Span::new(start, self.prev_end),
            strict: self.strict,
        })
    }

    /// `export {a}` requires `a` to be declared somewhere in the module (forward
    /// references are legal, so this can only run once the whole body is known).
    fn check_module_exports(&mut self, body: &[Stmt]) -> Result<()> {
        if self.pending_exports.is_empty() {
            return Ok(());
        }
        let mut declared: Vec<String> = Vec::new();
        for stmt in body {
            collect_module_binding(stmt, &mut declared);
        }
        let pending = std::mem::take(&mut self.pending_exports);
        for (name, span) in pending {
            if !declared.contains(&name) {
                // The reference makes this check in `js_resolve_module`, after parsing, so
                // it prints no position. See `SyntaxError::positionless`.
                return Err(self
                    .error_at(format!("exported variable '{name}' does not exist"), span)
                    .positionless());
            }
        }
        Ok(())
    }

    // -- declaration tracking (early errors) -------------------------------

    fn push_scope(&mut self, scope: Scope) {
        self.scopes.push(scope);
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declare a `let`/`const`/`class`/`import` name in the current scope.
    ///
    /// Conflicts are per scope: a lexical declaration clashes with another
    /// lexical declaration, a `var`/function declaration or a simple parameter
    /// *of the same scope*. `var a; { let a; }` is legal, `{ let a; { var a; } }`
    /// is not (the `var` hoists past the `let`, which `declare_var` checks).
    fn declare_lexical(&mut self, name: &str) -> Result<()> {
        let Some(scope) = self.scopes.last() else {
            return Ok(());
        };
        // Ordered so the most specific reason wins, matching the reference's check order:
        // the parameter test comes before the var walk, and a lexical name before both.
        let reason = if scope.lex_names.iter().any(|n| n == name)
            || scope.catch_names.iter().any(|n| n == name)
        {
            Some(Redefinition::Lexical)
        } else if scope.params.iter().any(|n| n == name) {
            Some(Redefinition::Parameter)
        } else if scope.fn_names.iter().any(|n| n == name)
            || scope.var_names.iter().any(|n| n == name)
        {
            // A body-scope function declaration is recorded in both lists, and the
            // message for either depends on whether the clash is at the global level:
            // `function a(){} let a;` at the top of a script says "global identifier",
            // the same two inside a function say "a variable".
            Some(if self.at_top_level_scope() {
                Redefinition::Global
            } else {
                Redefinition::Variable
            })
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(self.duplicate_declaration(reason));
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.lex_names.push(name.to_string());
        }
        Ok(())
    }

    /// Declare a `var` or function-declaration name: it hoists to the nearest
    /// function scope, but may not collide with a lexical name on the way up.
    fn declare_var(&mut self, name: &str) -> Result<()> {
        for scope in self.scopes.iter().rev() {
            if scope.lex_names.iter().any(|n| n == name) {
                return Err(self.duplicate_declaration(Redefinition::Lexical));
            }
            if scope.is_var_scope {
                break;
            }
        }
        let Some(index) = self.scopes.iter().rposition(|s| s.is_var_scope) else {
            return Ok(());
        };
        // Record it in every scope it passes through: `{ var a; let a; }` is an
        // early error because the block's VarDeclaredNames contain `a`.
        for scope in &mut self.scopes[index..] {
            if !scope.var_names.iter().any(|n| n == name) {
                scope.var_names.push(name.to_string());
            }
        }
        if let Some(scope) = self.scopes.last_mut()
            && !scope.var_names_here.iter().any(|n| n == name)
        {
            scope.var_names_here.push(name.to_string());
        }
        Ok(())
    }

    /// Reject a parameter list that binds the same name twice.
    ///
    /// Two questions, answered separately and in the reference's order:
    ///
    /// * *Which* duplicate is reported. A name bound inside a destructuring pattern is
    ///   checked as soon as it is read (`js_parse_destructuring_var`), and so is a plain
    ///   name once the list is known to contain an `=`; a plain name in a list without one
    ///   waits for the end. So `function f([a], a, [a])` reports the *third* parameter, not
    ///   the second — the second is not checked yet when the third is.
    /// * *Which message*. The early check says `duplicate parameter names`; the final one
    ///   says `duplicate argument names`, and carries no name.
    /// * *Where it points*. Both checks use `s->token.ptr`, so the position follows from
    ///   when they run: the early one fires on the offending name itself, the final one
    ///   only once the body has been entered, so it points at the body's **first token**
    ///   (`}` for `function f([a], a, a){}`, `return` for `{ return 1 }`, the expression
    ///   for `(a, a) => x`). That is what `deferred_at` is.
    ///
    /// `plain_is_illegal` is the caller's answer to "are duplicates illegal here at all":
    /// a sloppy `function f(a, a){}` accepts them (last wins), while a strict or
    /// non-simple list, an arrow, or a method does not.
    fn check_duplicate_parameters(
        &self,
        params: &[Pattern],
        plain_is_illegal: bool,
        deferred_at: Span,
    ) -> Result<()> {
        let has_default = params.iter().any(pattern_has_default);
        let mut seen: Vec<String> = Vec::new();
        let mut deferred = false;
        for param in params {
            for (name, span, in_pattern) in parameter_bindings(param) {
                if seen.iter().any(|seen| seen == &name) {
                    if has_default || in_pattern {
                        return Err(self.error_at(
                            "duplicate parameter names not allowed in this context".to_string(),
                            span,
                        ));
                    }
                    deferred = true;
                }
                seen.push(name);
            }
        }
        if deferred && plain_is_illegal {
            return Err(self.error_at(
                "duplicate argument names not allowed in this context".to_string(),
                deferred_at,
            ));
        }
        Ok(())
    }

    /// Declare a function declaration name in the scope that holds it.
    ///
    /// Split in two because the reference checks the two cases in different places, and
    /// the reported position follows from where the check is: a block-level declaration is
    /// checked at its *name* (`js_parse_function_decl2` calls `define_var` with the name
    /// just consumed, before the parameters), a body-level one only once the whole function
    /// has been parsed. So the block half runs from [`Parser::parse_function`] and this one
    /// from the call site afterwards, and only one of the two does anything — which one
    /// depends on the scope the declaration lands in.
    ///
    /// A function declaration in a function/script/module *body* behaves like a `var`:
    /// `function a(){} var a;` and `var a; function a(){}` are both accepted, in either
    /// mode. It is not quite a `var` though — a later `let a` reports it as "a variable"
    /// (or "global identifier" at the top level), which is what `fn_names` records:
    /// `find_var_in_child_scope` in the reference, consulted after the lexical check.
    fn declare_function(&mut self, name: &str) -> Result<()> {
        if !self.scopes.last().is_none_or(|scope| scope.is_var_scope) {
            return Ok(());
        }
        // The lexical check is skipped at the top level of a script or module, which is
        // the reference's "avoid creating a lexical variable in the global scope" branch —
        // so `let a; function a(){}` there is *accepted*, while the same two inside a
        // function body are a lexical redefinition. The other order still errors, because
        // then it is the `let` that does the checking.
        if !self.at_top_level_scope()
            && self
                .scopes
                .last()
                .is_some_and(|scope| scope.lex_names.iter().any(|n| n == name))
        {
            return Err(self.duplicate_declaration(Redefinition::Lexical));
        }
        if let Some(scope) = self.scopes.last_mut() {
            if !scope.var_names.iter().any(|n| n == name) {
                scope.var_names.push(name.to_string());
            }
            if !scope.fn_names.iter().any(|n| n == name) {
                scope.fn_names.push(name.to_string());
            }
        }
        Ok(())
    }

    /// The at-the-name half of a function declaration: the block-scope check and the
    /// module-code one.
    ///
    /// In a *block* a function declaration is a lexical binding of that block, which is why
    /// `{ function f(){} var f }` is an early error (test262's
    /// `block-scope/syntax/redeclaration`) while the same two declarations in a body are
    /// not. The reference gets there by way of `define_var(JS_VAR_DEF_FUNCTION_DECL)`,
    /// which sets `is_lexical`.
    fn declare_block_function(&mut self, name: &str, is_plain: bool) -> Result<()> {
        // A module is stricter about a function declaration sharing a name with anything
        // already declared at the same level, and says so with its own message. The
        // reference has this as a separate check (`js_parse_function_decl2`, "invalid
        // redefinition of global identifier in module code") that runs at the name, before
        // everything else, and matches on the *scope level* — so a `var` inside one block
        // clashes with a function in that same block, and one at the module's top level
        // does not.
        if self.module
            && self.at_top_level_scope()
            && let Some(scope) = self.scopes.last()
        {
            // What can clash is only what the reference keeps in its *global* var table: a
            // `var` written at this level (wherever that is), and — at the module's own
            // body — the lexical declarations, which are module variables too. A `let`
            // inside a block is not, so `{ let a; function a(){} }` in a module reports the
            // ordinary lexical error instead.
            let mut taken = scope.var_names_here.iter().any(|existing| existing == name);
            if scope.is_var_scope {
                let module_bindings = scope
                    .lex_names
                    .iter()
                    .filter(|existing| !scope.import_names.contains(existing))
                    .chain(&scope.fn_names);
                taken |= module_bindings.into_iter().any(|existing| existing == name);
            }
            if taken {
                return Err(self.error_at(
                    "invalid redefinition of global identifier in module code",
                    self.current.span,
                ));
            }
        }
        if self.scopes.last().is_none_or(|scope| scope.is_var_scope) {
            return Ok(());
        }
        let Some(scope) = self.scopes.last() else {
            return Ok(());
        };
        if scope.lex_names.iter().any(|n| n == name) || scope.catch_names.iter().any(|n| n == name)
        {
            // Annex B.3.3.4 lets a block re-declare a *function* in sloppy mode, and says
            // nothing about a lexical binding of any other kind. A second function
            // declaration is therefore accepted only where the first came from one too.
            let from_plain_function = scope.plain_fn_names.iter().any(|n| n == name);
            if !(from_plain_function && is_plain && !self.strict) {
                return Err(self.duplicate_declaration(Redefinition::Lexical));
            }
        } else if scope.var_names.iter().any(|n| n == name) {
            // A `var` in the same block clashes, and which message is right depends on
            // where that `var` landed: at the top level it became a global var, and the
            // reference checks globals separately.
            let reason = if self.at_top_level_scope() {
                Redefinition::Global
            } else {
                Redefinition::Variable
            };
            return Err(self.duplicate_declaration(reason));
        }
        if let Some(scope) = self.scopes.last_mut() {
            if !scope.lex_names.iter().any(|n| n == name) {
                scope.lex_names.push(name.to_string());
            }
            if !scope.fn_names.iter().any(|n| n == name) {
                scope.fn_names.push(name.to_string());
            }
            if is_plain && !scope.plain_fn_names.iter().any(|n| n == name) {
                scope.plain_fn_names.push(name.to_string());
            }
        }
        Ok(())
    }

    /// A redefinition error, positioned the way the reference positions one.
    ///
    /// `js_parse_error` always uses `s->token.ptr` — the *current* token — so a
    /// redeclaration points at whatever follows the offending name, not at the name
    /// itself: `{ let a; let a = 1; }` reports the `=`, and `{ let a; let a; }` reports
    /// the `;`. Every call site is placed at the point the reference makes the check, so
    /// the current token here is the same one it would be there.
    ///
    /// Known gap: for a destructuring target the reference reports the token after the
    /// *inner* binding identifier (`let [a] = []` points at the `]`), while our check
    /// runs once the whole pattern is parsed.
    fn duplicate_declaration(&self, reason: Redefinition) -> SyntaxError {
        self.error_at(reason.message(), self.current.span)
    }

    /// Whether declarations here belong to the script/module's own top-level scope.
    ///
    /// This is what splits the reference's `global identifier` message from
    /// `a variable`: a `var` recorded in the outermost var scope is a global var, and one
    /// inside a function is an ordinary variable.
    fn at_top_level_scope(&self) -> bool {
        self.scopes.iter().rposition(|scope| scope.is_var_scope) == Some(0)
    }

    /// Declare every name bound by a pattern.
    fn declare_pattern(&mut self, pattern: &Pattern, is_var: bool) -> Result<()> {
        let mut names = Vec::new();
        pattern.bound_names(&mut names);
        for name in names {
            if is_var {
                self.declare_var(&name.name)?;
            } else {
                self.declare_lexical(&name.name)?;
            }
        }
        Ok(())
    }

    // -- token helpers -----------------------------------------------------

    fn snapshot(&self) -> Snapshot<'a> {
        Snapshot {
            lexer: self.lexer.clone(),
            current: self.current.clone(),
            prev_end: self.prev_end,
            strict: self.strict,
            allow_in: self.allow_in,
            ctx: self.ctx,
            loop_depth: self.loop_depth,
            switch_depth: self.switch_depth,
            labels: self.labels.clone(),
            private: self.private.clone(),
            top_level: self.top_level,
            cover_inits: self.cover_inits.clone(),
            pending_exports: self.pending_exports.clone(),
            current_legacy_octal: self.current_legacy_octal,
            scopes: self.scopes.clone(),
        }
    }

    fn restore(&mut self, snap: Snapshot<'a>) {
        self.lexer = snap.lexer;
        self.current = snap.current;
        self.prev_end = snap.prev_end;
        self.strict = snap.strict;
        self.allow_in = snap.allow_in;
        self.ctx = snap.ctx;
        self.loop_depth = snap.loop_depth;
        self.switch_depth = snap.switch_depth;
        self.labels = snap.labels;
        self.private = snap.private;
        self.top_level = snap.top_level;
        self.cover_inits = snap.cover_inits;
        self.pending_exports = snap.pending_exports;
        self.current_legacy_octal = snap.current_legacy_octal;
        self.scopes = snap.scopes;
    }

    fn bump(&mut self) -> Result<()> {
        self.prev_end = self.current.span.end;
        self.current = self.lexer.next_token()?;
        self.current_legacy_octal = self.lexer.had_legacy_octal();
        if self.strict && self.current_legacy_octal {
            return Err(SyntaxError::new(
                "octal literals and octal escape sequences are not allowed in strict mode",
                self.current.span,
            ));
        }
        Ok(())
    }

    fn peek(&self) -> Result<Token> {
        let mut lexer = self.lexer.clone();
        lexer.next_token()
    }

    fn enter_strict(&mut self) -> Result<()> {
        self.strict = true;
        self.lexer.set_strict(true);
        // The token after the `"use strict"` directive has already been scanned
        // with the old (sloppy) rules.
        if self.current_legacy_octal {
            return Err(SyntaxError::new(
                "octal literals and octal escape sequences are not allowed in strict mode",
                self.current.span,
            ));
        }
        Ok(())
    }

    fn at_punct(&self, p: Punct) -> bool {
        matches!(&self.current.kind, TokenKind::Punct(q) if *q == p)
    }

    fn at_keyword(&self, k: Keyword) -> bool {
        matches!(&self.current.kind, TokenKind::Keyword(q) if *q == k)
    }

    fn at_contextual(&self, name: &str) -> bool {
        self.at_ident(name)
    }

    fn at_ident(&self, name: &str) -> bool {
        matches!(&self.current.kind, TokenKind::Ident(n) if n == name)
    }

    fn eat_punct(&mut self, p: Punct) -> Result<bool> {
        if self.at_punct(p) {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn expect_punct(&mut self, p: Punct) -> Result<()> {
        if self.eat_punct(p)? {
            Ok(())
        } else {
            // `expecting ';'`, the reference's `js_parse_expect`. It formats the expected
            // token with `%c`, so only single-character punctuators can be named this way —
            // and every call site here passes one, which is why this is the reference's set
            // exactly: `( ) { } [ ] ; :`.
            Err(self.error(format!("expecting '{}'", p.as_str())))
        }
    }

    fn eat_keyword(&mut self, k: Keyword) -> Result<bool> {
        if self.at_keyword(k) {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    fn eat_contextual(&mut self, name: &str) -> Result<bool> {
        if self.at_ident(name) {
            self.bump()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// `unexpected token in expression: '<token text>'` — the reference's message when a
    /// primary expression was expected and the token cannot start one.
    ///
    /// The token's text is its **raw source slice**, not a description: `js_parse_error(s,
    /// "unexpected token in expression: '%.*s'", s->buf_ptr - s->token.ptr, s->token.ptr)`.
    /// That is why end of input prints as `''` and a string literal prints with its quotes.
    fn unexpected(&self) -> SyntaxError {
        let source = self.lexer.source();
        let start = self.current.span.start.offset as usize;
        let end = self.current.span.end.offset as usize;
        let text = source.get(start..end).unwrap_or("");
        SyntaxError::new(
            format!("unexpected token in expression: '{text}'"),
            self.current.span,
        )
    }

    fn error(&self, message: impl Into<String>) -> SyntaxError {
        SyntaxError::new(message, self.current.span)
    }

    fn error_at(&self, message: impl Into<String>, span: Span) -> SyntaxError {
        SyntaxError::new(message, span)
    }

    fn span_from(&self, start: Position) -> Span {
        Span::new(start, self.prev_end)
    }

    /// Read an identifier that *binds* a name, applying the binding rules
    /// (`eval`/`arguments`/`let` are restricted in strict mode).
    fn expect_binding_ident(&mut self) -> Result<Ident> {
        let ident = self.expect_ident()?;
        self.check_binding_name(&ident.name, ident.span)?;
        Ok(ident)
    }

    /// Read an identifier, rejecting reserved words.
    fn expect_ident(&mut self) -> Result<Ident> {
        match self.current.kind.clone() {
            TokenKind::Ident(name) => {
                // A *name* position: `await` is a token there, not an identifier, so the
                // reference never gets as far as complaining about it by name.
                if name == "await" && self.await_is_reserved() {
                    return Err(self.error("variable name expected"));
                }
                let span = self.current.span;
                self.check_identifier(&name, span)?;
                self.bump()?;
                Ok(Ident { name, span })
            }
            // The reference has no "got X" suffix: `js_parse_error(s, "variable name
            // expected")`. The token is already the error position.
            _ => Err(self.error("variable name expected")),
        }
    }

    /// Whether the current token can begin a formal parameter.
    ///
    /// A token that cannot is a *missing* parameter rather than a malformed one, which is
    /// the reference's `missing formal parameter`. `await` counts as missing where it is
    /// reserved: the reference never sees it as an identifier, so it never gets to the
    /// name check that would say `variable name expected`.
    fn at_formal_parameter(&self) -> bool {
        if self.at_punct(Punct::LBracket) || self.at_punct(Punct::LBrace) {
            return true;
        }
        match &self.current.kind {
            TokenKind::Ident(name) => !(name == "await" && self.await_is_reserved()),
            _ => false,
        }
    }

    /// Whether `await` cannot be written as a name here.
    ///
    /// Two separate reasons, and the reference reaches them from two different places:
    ///
    /// * the enclosing function is `async`, where `await` is a keyword rather than an
    ///   identifier (`fd->func_kind & JS_FUNC_ASYNC`);
    /// * the code is module code, where `await` is reserved at *every* depth — not just
    ///   the module's own top level — so `function f(){ var await = 1 }` in a module is
    ///   an error while the same function in a script is not.
    fn await_is_reserved(&self) -> bool {
        self.ctx.in_async || self.module
    }

    /// Reject words that may not appear as an `IdentifierReference` here.
    fn check_identifier(&self, name: &str, span: Span) -> Result<()> {
        if Keyword::lookup(name).is_some() {
            return Err(self.error_at(format!("unexpected keyword '{name}'"), span));
        }
        if self.strict && Keyword::is_strict_reserved(name) {
            return Err(self.error_at(
                format!("invalid use of strict mode reserved word '{name}'"),
                span,
            ));
        }
        if self.ctx.in_generator && name == "yield" {
            return Err(self.error_at(
                "invalid use of 'yield' identifier in a generator function",
                span,
            ));
        }
        if name == "await" && self.await_is_reserved() {
            // `await` never reaches the identifier path where it is a keyword: in an async
            // function it is an operator, and in module code at *any* depth it is reserved
            // even inside a plain function, which is where this message comes from —
            // `function f(){ await 1 }` in a module is a keyword used as a name.
            return Err(self.error_at("unexpected 'await' keyword", span));
        }
        Ok(())
    }

    /// A declarative name (`let`, `class`, `function`, parameter): stricter than
    /// an identifier reference because `let` is reserved in strict mode.
    fn check_binding_name(&self, name: &str, span: Span) -> Result<()> {
        self.check_identifier(name, span)?;
        if self.strict && (name == "eval" || name == "arguments") {
            // One message for both, and it names neither: `js_parse_error(s, "invalid
            // variable name in strict mode")`.
            return Err(self.error_at("invalid variable name in strict mode", span));
        }
        if self.strict && name == "let" {
            return Err(self.error_at(
                "invalid use of 'let' as a variable name in strict mode",
                span,
            ));
        }
        Ok(())
    }

    /// The value of an expression statement that is a string literal with no
    /// escape sequences (a directive prologue candidate).
    fn directive_value(&self, stmt: &Stmt) -> Option<String> {
        let StmtKind::Expr(expr) = &stmt.kind else {
            return None;
        };
        let ExprKind::String(value) = &expr.unparen().kind else {
            return None;
        };
        let raw = self.slice(expr.span);
        if !raw.starts_with('"') && !raw.starts_with('\'') {
            return None;
        }
        if raw.trim_matches(|c| c == '"' || c == '\'').contains('\\') {
            return None;
        }
        Some(value.clone())
    }

    fn slice(&self, span: Span) -> &'a str {
        let src = self.lexer.source();
        let start = span.start.offset as usize;
        let end = (span.end.offset as usize).min(src.len());
        src.get(start..end).unwrap_or("")
    }

    // -- automatic semicolon insertion ------------------------------------

    fn can_insert_semicolon(&self) -> bool {
        self.current.is_eof() || self.at_punct(Punct::RBrace) || self.current.newline_before
    }

    fn semicolon(&mut self) -> Result<()> {
        if self.eat_punct(Punct::Semicolon)? {
            return Ok(());
        }
        if self.can_insert_semicolon() {
            return Ok(());
        }
        // Not `unexpected()`: the reference's `js_parse_expect_semi` names what it wanted
        // (`expecting ';'`), which is why `1 2` says so rather than describing the `2`.
        Err(self.error("expecting ';'"))
    }

    // -- statements --------------------------------------------------------

    fn parse_statement_list_item(&mut self) -> Result<Stmt> {
        self.enter()?;
        let result = self.parse_statement_list_item_guarded();
        self.leave();
        result
    }

    fn parse_statement_list_item_guarded(&mut self) -> Result<Stmt> {
        let cover_mark = self.cover_inits.len();
        let stmt = self.parse_statement_list_item_inner()?;
        if self.cover_inits.len() > cover_mark {
            let span = self.cover_inits[cover_mark];
            self.cover_inits.truncate(cover_mark);
            return Err(self.error_at("expecting '}'", span));
        }
        Ok(stmt)
    }

    fn parse_statement_list_item_inner(&mut self) -> Result<Stmt> {
        if self.at_keyword(Keyword::Function) {
            let start = self.current.span.start;
            let func = self.parse_function(FunctionForm::Declaration, false)?;
            if let Some(name) = &func.name {
                self.declare_function(&name.name)?;
            }
            return Ok(Stmt::new(
                StmtKind::FuncDecl(Box::new(func)),
                self.span_from(start),
            ));
        }
        if self.at_ident("async") {
            let peek = self.peek()?;
            if matches!(peek.kind, TokenKind::Keyword(Keyword::Function)) && !peek.newline_before {
                let start = self.current.span.start;
                self.bump()?; // async
                let func = self.parse_function(FunctionForm::Declaration, true)?;
                if let Some(name) = &func.name {
                    self.declare_function(&name.name)?;
                }
                return Ok(Stmt::new(
                    StmtKind::FuncDecl(Box::new(func)),
                    self.span_from(start),
                ));
            }
        }
        if self.at_keyword(Keyword::Class) {
            let start = self.current.span.start;
            let class = self.parse_class(false)?;
            if let Some(name) = &class.name {
                self.declare_lexical(&name.name)?;
            }
            return Ok(Stmt::new(
                StmtKind::ClassDecl(Box::new(class)),
                self.span_from(start),
            ));
        }
        if self.at_keyword(Keyword::Var) {
            let start = self.current.span.start;
            let decl = self.parse_var_decl(VarDeclKind::Var, false, true)?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::VarDecl(decl), self.span_from(start)));
        }
        if self.at_keyword(Keyword::Const) {
            let start = self.current.span.start;
            let decl = self.parse_var_decl(VarDeclKind::Const, false, true)?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::VarDecl(decl), self.span_from(start)));
        }
        if self.at_ident("let") && self.let_starts_declaration()? {
            let start = self.current.span.start;
            let decl = self.parse_var_decl(VarDeclKind::Let, false, true)?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::VarDecl(decl), self.span_from(start)));
        }
        // `await using x = …` is one declaration; `await` is only a keyword here when the
        // enclosing code may await at all, which is the same rule `await` expressions use.
        if self.at_ident("await") && self.ctx.in_async && self.await_using_follows()? {
            let start = self.current.span.start;
            // No top-level rejection here, unlike a plain `using`: the reference's check
            // lives on the `using` path only (`fd->is_eval && scope_level == body_scope`),
            // while `await using` reaches a branch that does not ask.
            self.bump()?; // await
            let decl = self.parse_var_decl_with_keyword(VarDeclKind::AwaitUsing, "using")?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::VarDecl(decl), self.span_from(start)));
        }
        if self.at_ident("using") && self.using_starts_declaration()? {
            let start = self.current.span.start;
            // A script's *own* scope has no block to end, so there would be nowhere to
            // dispose. The reference asks `scope_level == body_scope`: the program's scope
            // itself, not a block nested inside it — `{ using y = 1 }` is legal there and
            // disposes at the closing brace. A module's top level *is* a scope, so it does
            // not ask at all.
            if self.scopes.len() == 1 && !self.module {
                return Err(self.error_at(
                    "using declaration is not allowed at the top level of a script",
                    self.span_from(start),
                ));
            }
            // A `case`/`default` clause is a statement list with nothing to end it, so a
            // `using` there has no disposal point either. This is a separate early error
            // from the one above: it applies inside a function too, where the scope depth
            // is not 1.
            if self.in_case_clause {
                return Err(self.error_at(
                    "using declaration is not allowed in this context",
                    self.span_from(start),
                ));
            }
            let decl = self.parse_var_decl(VarDeclKind::Using, false, true)?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::VarDecl(decl), self.span_from(start)));
        }
        if self.module && self.at_keyword(Keyword::Import) {
            let peek = self.peek()?;
            // `import(` and `import.meta` are expressions, not declarations.
            if !matches!(
                peek.kind,
                TokenKind::Punct(Punct::LParen) | TokenKind::Punct(Punct::Dot)
            ) {
                return self.parse_import_decl();
            }
        }
        if self.module && self.at_keyword(Keyword::Export) {
            return self.parse_export_decl();
        }
        self.parse_statement()
    }

    /// `let` is a declaration keyword when followed by a binding or a pattern.
    fn let_starts_declaration(&self) -> Result<bool> {
        let peek = self.peek()?;
        Ok(matches!(
            peek.kind,
            TokenKind::Ident(_)
                | TokenKind::Punct(Punct::LBracket)
                | TokenKind::Punct(Punct::LBrace)
        ))
    }

    /// The token after a `using` declaration head's binding name, if a head starts here.
    ///
    /// `skip` is how many words come before `using` — `1` for `await using`. Returns `None`
    /// when the shape is not a declaration head at all: the binding must be a plain
    /// identifier with no line terminator before it.
    fn using_head_tail(&self, skip: usize) -> Result<Option<Token>> {
        let mut lexer = self.lexer.clone();
        // The lexer is already positioned after the current token, so the first
        // `next_token` yields the token that follows it. `using` itself is `skip` words in,
        // and the binding is the token after it.
        let mut binding = None;
        for _ in 0..=skip {
            binding = Some(lexer.next_token()?);
        }
        let Some(binding) = binding else {
            return Ok(None);
        };
        if binding.newline_before || !matches!(binding.kind, TokenKind::Ident(_)) {
            return Ok(None);
        }
        Ok(Some(lexer.next_token()?))
    }

    /// Whether `await using` starts a declaration here.
    ///
    /// `await` is followed by `using`, which is in turn followed by a binding name and `=`.
    /// Anything else — `await using(x)` where `using` is a function — is an `await`
    /// expression over an ordinary call.
    fn await_using_follows(&self) -> Result<bool> {
        Ok(matches!(
            self.using_head_tail(1)?.map(|token| token.kind),
            Some(TokenKind::Punct(Punct::Eq))
        ))
    }

    /// Whether the `using` at the current token begins a declaration.
    ///
    /// `using` is a contextual keyword, not a reserved word: `using = 1`, `using(x)` and
    /// `using.f` are all ordinary expressions over a variable named `using`. The spec's
    /// rule is `using [no LineTerminator] BindingIdentifier =`, so the binding name has to
    /// be followed by `=`.
    ///
    /// Patterns are excluded on purpose: `using {a} = …` is not a using declaration, and
    /// the grammar forbids it, so it stays an expression statement that fails to parse as
    /// one.
    fn using_starts_declaration(&self) -> Result<bool> {
        Ok(matches!(
            self.using_head_tail(0)?.map(|token| token.kind),
            Some(TokenKind::Punct(Punct::Eq))
        ))
    }

    /// Whether `using x of …` starts a `for` head here.
    ///
    /// The head has no initialiser, so the token after the binding is `of` (or `in`, which
    /// the caller then rejects: a `using` may only iterate a `for`-`of`).
    fn using_starts_for_head(&self) -> Result<bool> {
        Ok(matches!(
            self.using_head_tail(0)?.map(|token| token.kind),
            Some(TokenKind::Ident(name)) if name == "of" || name == "in"
        ))
    }

    /// Whether `await using x of …` starts a `for` head here.
    fn await_using_starts_for_head(&self) -> Result<bool> {
        Ok(matches!(
            self.using_head_tail(1)?.map(|token| token.kind),
            Some(TokenKind::Ident(name)) if name == "of" || name == "in"
        ))
    }

    fn parse_statement(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        if self.at_punct(Punct::Semicolon) {
            self.bump()?;
            return Ok(Stmt::new(StmtKind::Empty, self.span_from(start)));
        }
        if self.at_punct(Punct::LBrace) {
            let body = self.parse_block()?;
            return Ok(Stmt::new(StmtKind::Block(body), self.span_from(start)));
        }
        if self.at_keyword(Keyword::If) {
            return self.parse_if();
        }
        if self.at_keyword(Keyword::Do) {
            return self.parse_do_while();
        }
        if self.at_keyword(Keyword::While) {
            return self.parse_while();
        }
        if self.at_keyword(Keyword::For) {
            return self.parse_for();
        }
        if self.at_keyword(Keyword::Switch) {
            return self.parse_switch();
        }
        if self.at_keyword(Keyword::Try) {
            return self.parse_try();
        }
        if self.at_keyword(Keyword::Return) {
            return self.parse_return();
        }
        if self.at_keyword(Keyword::Throw) {
            return self.parse_throw();
        }
        if self.at_keyword(Keyword::Break) {
            return self.parse_break_continue(true);
        }
        if self.at_keyword(Keyword::Continue) {
            return self.parse_break_continue(false);
        }
        if self.at_keyword(Keyword::With) {
            return self.parse_with();
        }
        if self.at_keyword(Keyword::Debugger) {
            self.bump()?;
            self.semicolon()?;
            return Ok(Stmt::new(StmtKind::Debugger, self.span_from(start)));
        }
        if self.at_keyword(Keyword::Function) {
            // Annex B: function declarations in statement position are legal in
            // sloppy mode (`if (x) function f() {}`).
            if self.strict {
                return Err(
                    self.error("invalid 'function' declaration in a single statement context")
                );
            }
            let func = self.parse_function(FunctionForm::Declaration, false)?;
            if let Some(name) = &func.name {
                self.declare_function(&name.name)?;
            }
            return Ok(Stmt::new(
                StmtKind::FuncDecl(Box::new(func)),
                self.span_from(start),
            ));
        }
        if self.at_keyword(Keyword::Class) {
            return Err(self.error("invalid 'class' declaration in a single statement context"));
        }
        if self.at_keyword(Keyword::Import) {
            // `import(...)` and `import.meta` are *expressions*, so they are an ordinary
            // expression statement here. Only a declaration is restricted to the top
            // level of a module.
            let peek = self.peek()?;
            let expression = matches!(
                peek.kind,
                TokenKind::Punct(Punct::LParen) | TokenKind::Punct(Punct::Dot)
            );
            if !expression {
                return Err(self.error("'import' can only appear at the top level of a module"));
            }
        }
        if self.at_keyword(Keyword::Export) {
            return Err(self.error("'export' can only appear at the top level of a module"));
        }
        // Labelled statement.
        if let TokenKind::Ident(name) = &self.current.kind {
            // `await` is not a `LabelIdentifier` in module code, and where it is an
            // operator it is not an identifier at all — `await: 1` in a suspensible body
            // is an `await` expression whose operand is missing, not a label.
            if name == "await" && self.await_is_reserved() {
                if !self.ctx.in_async {
                    // Reserved because this is module code, but not an operator: the
                    // reference has a message for exactly this case.
                    let peek = self.peek()?;
                    if matches!(peek.kind, TokenKind::Punct(Punct::Colon)) {
                        return Err(self.error("unexpected 'await' keyword"));
                    }
                }
                // Where `await` *is* an operator it is not an identifier at all, so
                // `await: 1` is an `await` expression with a missing operand. Falling
                // through leaves that to the expression parser, which says so.
            } else {
                let peek = self.peek()?;
                if matches!(peek.kind, TokenKind::Punct(Punct::Colon)) {
                    return self.parse_labeled();
                }
            }
        }
        self.parse_expression_statement()
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>> {
        self.expect_punct(Punct::LBrace)?;
        self.push_scope(Scope::block());
        // A block *inside* a `case` clause is a block, so `using` is legal there: the
        // early error is about a `using` sitting directly in the clause's statement list.
        let saved_in_case = self.in_case_clause;
        self.in_case_clause = false;
        let result = self.parse_block_body();
        self.in_case_clause = saved_in_case;
        self.pop_scope();
        let body = result?;
        self.bump()?; // }
        Ok(body)
    }

    /// `{ … }` of a `catch` clause: parsed in the scope holding its parameter.
    fn parse_catch_body(&mut self) -> Result<Vec<Stmt>> {
        self.expect_punct(Punct::LBrace)?;
        let body = self.parse_block_body()?;
        self.bump()?; // }
        Ok(body)
    }

    fn parse_block_body(&mut self) -> Result<Vec<Stmt>> {
        let mut body = Vec::new();
        while !self.at_punct(Punct::RBrace) {
            if self.current.is_eof() {
                return Err(self.unexpected());
            }
            body.push(self.parse_statement_list_item()?);
        }
        Ok(body)
    }

    fn parse_expression_statement(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        let expr = self.parse_expression(true)?;
        self.semicolon()?;
        Ok(Stmt::new(
            StmtKind::Expr(Box::new(expr)),
            self.span_from(start),
        ))
    }

    fn parse_if(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // if
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression(true)?;
        self.expect_punct(Punct::RParen)?;
        let cons = self.parse_statement()?;
        let alt = if self.eat_keyword(Keyword::Else)? {
            Some(Box::new(self.parse_statement()?))
        } else {
            None
        };
        Ok(Stmt::new(
            StmtKind::If {
                test: Box::new(test),
                cons: Box::new(cons),
                alt,
            },
            self.span_from(start),
        ))
    }

    fn parse_do_while(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // do
        self.loop_depth += 1;
        let body = self.parse_statement();
        self.loop_depth -= 1;
        let body = body?;
        if !self.eat_keyword(Keyword::While)? {
            return Err(self.unexpected());
        }
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression(true)?;
        self.expect_punct(Punct::RParen)?;
        // `do … while (x)` allows a missing semicolon even without a newline.
        let _ = self.eat_punct(Punct::Semicolon)?;
        Ok(Stmt::new(
            StmtKind::DoWhile {
                body: Box::new(body),
                test: Box::new(test),
            },
            self.span_from(start),
        ))
    }

    fn parse_while(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // while
        self.expect_punct(Punct::LParen)?;
        let test = self.parse_expression(true)?;
        self.expect_punct(Punct::RParen)?;
        self.loop_depth += 1;
        let body = self.parse_statement();
        self.loop_depth -= 1;
        Ok(Stmt::new(
            StmtKind::While {
                test: Box::new(test),
                body: Box::new(body?),
            },
            self.span_from(start),
        ))
    }

    fn parse_for(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // for
        self.push_scope(Scope::block());
        let result = self.parse_for_inner(start);
        self.pop_scope();
        result
    }

    fn parse_for_inner(&mut self, start: Position) -> Result<Stmt> {
        let mut is_await = false;
        if self.at_ident("await") && self.ctx.in_async {
            let peek = self.peek()?;
            if matches!(peek.kind, TokenKind::Punct(Punct::LParen)) && !peek.newline_before {
                is_await = true;
                self.bump()?;
            }
        }
        self.expect_punct(Punct::LParen)?;

        // `for (;;)`
        if self.at_punct(Punct::Semicolon) {
            self.bump()?;
            return self.parse_for_classic(start, None);
        }

        // Declaration head.
        let decl_kind = if self.at_keyword(Keyword::Var) {
            Some(VarDeclKind::Var)
        } else if self.at_keyword(Keyword::Const) {
            Some(VarDeclKind::Const)
        } else if self.at_ident("let") && self.let_starts_declaration()? {
            Some(VarDeclKind::Let)
        } else if self.at_ident("using") && self.using_starts_for_head()? {
            Some(VarDeclKind::Using)
        } else if self.at_ident("await")
            && self.ctx.in_async
            && self.await_using_starts_for_head()?
        {
            self.bump()?; // await, leaving `using` as the keyword
            Some(VarDeclKind::AwaitUsing)
        } else {
            None
        };
        if let Some(kind) = decl_kind {
            let decl_start = self.current.span.start;
            let decl = self.parse_var_decl(kind, true, false)?;
            if self.at_for_head_keyword() {
                let is_of = self.at_contextual("of");
                if !is_of && decl.declarators.len() != 1 {
                    return Err(self.error_at(
                        "only one binding allowed in a for-in loop",
                        self.span_from(decl_start),
                    ));
                }
                if is_of && decl.declarators.len() != 1 {
                    return Err(self.error_at(
                        "only one binding allowed in a for-of loop",
                        self.span_from(decl_start),
                    ));
                }
                if let Some(declarator) = decl.declarators.first() {
                    if declarator.init.is_some() {
                        // `for (var x = 1 in y)` is legacy in sloppy mode only.
                        if is_of || kind.is_lexical() || self.strict {
                            return Err(self.error_at(
                                if is_of {
                                    // The reference formats `%s` with the loop kind.
                                    "a declaration in the head of a for-of loop can't have an initializer"
                                } else {
                                    "a declaration in the head of a for-in loop can't have an initializer"
                                },
                                declarator.span,
                            ));
                        }
                    }
                }
                if is_await && !is_of {
                    return Err(self.error("for await is only valid with 'of'"));
                }
                self.bump()?; // in / of
                let right = self.parse_expression(true)?;
                self.expect_punct(Punct::RParen)?;
                self.loop_depth += 1;
                let body = self.parse_statement();
                self.loop_depth -= 1;
                let head = ForHead::VarDecl(decl);
                let span = self.span_from(start);
                return Ok(Stmt::new(
                    if is_of {
                        StmtKind::ForOf {
                            is_await,
                            left: head,
                            right: Box::new(right),
                            body: Box::new(body?),
                        }
                    } else {
                        StmtKind::ForIn {
                            left: head,
                            right: Box::new(right),
                            body: Box::new(body?),
                        }
                    },
                    span,
                ));
            }
            self.expect_punct(Punct::Semicolon)?;
            return self.parse_for_classic(start, Some(ForInit::VarDecl(decl)));
        }

        // Expression head.
        let expr_start = self.current.span.start;
        let expr = self.parse_expression(false)?;
        if self.at_for_head_keyword() {
            let is_of = self.at_contextual("of");
            if is_await && !is_of {
                return Err(self.error("for await is only valid with 'of'"));
            }
            let target = self.expr_to_pattern(expr, TargetKind::Assignment)?;
            self.bump()?; // in / of
            let right = self.parse_expression(true)?;
            self.expect_punct(Punct::RParen)?;
            self.loop_depth += 1;
            let body = self.parse_statement();
            self.loop_depth -= 1;
            let head = ForHead::Assign(Box::new(target));
            let span = self.span_from(start);
            return Ok(Stmt::new(
                if is_of {
                    StmtKind::ForOf {
                        is_await,
                        left: head,
                        right: Box::new(right),
                        body: Box::new(body?),
                    }
                } else {
                    StmtKind::ForIn {
                        left: head,
                        right: Box::new(right),
                        body: Box::new(body?),
                    }
                },
                span,
            ));
        }
        let _ = expr_start;
        self.expect_punct(Punct::Semicolon)?;
        self.parse_for_classic(start, Some(ForInit::Expr(Box::new(expr))))
    }

    /// Whether the `for` head continues with `in` (a keyword) or `of` (a
    /// contextual keyword).
    fn at_for_head_keyword(&self) -> bool {
        self.at_keyword(Keyword::In) || self.at_contextual("of")
    }

    fn parse_for_classic(&mut self, start: Position, init: Option<ForInit>) -> Result<Stmt> {
        let test = if self.at_punct(Punct::Semicolon) {
            None
        } else {
            Some(Box::new(self.parse_expression(true)?))
        };
        self.expect_punct(Punct::Semicolon)?;
        let update = if self.at_punct(Punct::RParen) {
            None
        } else {
            Some(Box::new(self.parse_expression(true)?))
        };
        self.expect_punct(Punct::RParen)?;
        self.loop_depth += 1;
        let body = self.parse_statement();
        self.loop_depth -= 1;
        Ok(Stmt::new(
            StmtKind::For {
                init,
                test,
                update,
                body: Box::new(body?),
            },
            self.span_from(start),
        ))
    }

    fn parse_switch(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // switch
        self.expect_punct(Punct::LParen)?;
        let discriminant = self.parse_expression(true)?;
        self.expect_punct(Punct::RParen)?;
        self.expect_punct(Punct::LBrace)?;
        self.switch_depth += 1;
        self.push_scope(Scope::block());
        let mut cases = Vec::new();
        let mut seen_default = false;
        while !self.at_punct(Punct::RBrace) {
            if self.current.is_eof() {
                self.switch_depth -= 1;
                return Err(self.unexpected());
            }
            let case_start = self.current.span.start;
            let test = if self.eat_keyword(Keyword::Case)? {
                Some(Box::new(self.parse_expression(true)?))
            } else if self.eat_keyword(Keyword::Default)? {
                if seen_default {
                    self.switch_depth -= 1;
                    return Err(self.error("duplicate 'default' clause in switch statement"));
                }
                seen_default = true;
                None
            } else {
                self.switch_depth -= 1;
                return Err(self.unexpected());
            };
            self.expect_punct(Punct::Colon)?;
            let mut body = Vec::new();
            let saved_in_case = self.in_case_clause;
            self.in_case_clause = true;
            while !self.at_punct(Punct::RBrace)
                && !self.at_keyword(Keyword::Case)
                && !self.at_keyword(Keyword::Default)
            {
                if self.current.is_eof() {
                    self.in_case_clause = saved_in_case;
                    self.switch_depth -= 1;
                    return Err(self.unexpected());
                }
                match self.parse_statement_list_item() {
                    Ok(stmt) => body.push(stmt),
                    Err(error) => {
                        self.in_case_clause = saved_in_case;
                        self.switch_depth -= 1;
                        return Err(error);
                    }
                }
            }
            self.in_case_clause = saved_in_case;
            cases.push(SwitchCase {
                test,
                body,
                span: self.span_from(case_start),
            });
        }
        self.switch_depth -= 1;
        self.bump()?; // }
        // The switch body's block scope has to be popped here. Leaving it pushed leaked one
        // scope per `switch` for the rest of the file, so everything after a switch was
        // declared into a *stale* scope: `function f(value){ switch(value){} } let value=9`
        // was rejected as "identifier 'value' has already been declared", and the `value`
        // it clashed with was the function's own parameter. That made the parser refuse
        // valid programs — and it never showed up on its own, only when a top-level `let`
        // happened to reuse a name used inside an earlier switch.
        self.pop_scope();
        Ok(Stmt::new(
            StmtKind::Switch {
                discriminant: Box::new(discriminant),
                cases,
            },
            self.span_from(start),
        ))
    }

    fn parse_try(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // try
        let block = self.parse_block()?;
        let mut handler = None;
        let mut finalizer = None;
        if self.at_keyword(Keyword::Catch) {
            let catch_start = self.current.span.start;
            self.bump()?;
            // The catch parameter and the body share one scope, so `catch (e) {
            // let e; }` is an early error. A *simple* parameter is exempt from
            // the `var` check (`catch (e) { var e; }` is legal), a destructured
            // one is not (`catch ({e}) { var e; }` is rejected) — this matches
            // the reference engine.
            let param = if self.eat_punct(Punct::LParen)? {
                let pattern = self.parse_binding_target()?;
                self.expect_punct(Punct::RParen)?;
                Some(pattern)
            } else {
                None
            };
            self.push_scope(Scope::block());
            let body = match &param {
                // A simple parameter shares the body's scope: `catch (e) { let e;
                // }` is an error, `catch (e) { var e; }` is not.
                Some(Pattern {
                    kind: PatternKind::Ident(id),
                    ..
                }) => {
                    if let Some(scope) = self.scopes.last_mut() {
                        scope.catch_names.push(id.name.clone());
                    }
                    self.parse_catch_body()
                }
                // A destructuring parameter is declared lexically in the clause's
                // own scope, with the body nested inside it: `catch ({e}) { var
                // e; }` is an error (the `var` hoists past the binding) while
                // `catch ({e}) { let e; }` is not.
                Some(pattern) => {
                    self.declare_pattern(pattern, false)?;
                    self.parse_block()
                }
                None => self.parse_catch_body(),
            };
            self.pop_scope();
            let body = body?;
            handler = Some(CatchClause {
                param,
                body,
                span: self.span_from(catch_start),
            });
        }
        if self.eat_keyword(Keyword::Finally)? {
            finalizer = Some(self.parse_block()?);
        }
        if handler.is_none() && finalizer.is_none() {
            return Err(self.error("expecting catch or finally"));
        }
        Ok(Stmt::new(
            StmtKind::Try {
                block,
                handler,
                finalizer,
            },
            self.span_from(start),
        ))
    }

    fn parse_return(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        let span = self.current.span;
        if !self.ctx.in_function {
            return Err(self.error_at("return not in a function", span));
        }
        self.bump()?;
        let argument = if self.at_punct(Punct::Semicolon)
            || self.at_punct(Punct::RBrace)
            || self.current.is_eof()
            || self.current.newline_before
        {
            None
        } else {
            Some(Box::new(self.parse_expression(true)?))
        };
        self.semicolon()?;
        Ok(Stmt::new(StmtKind::Return(argument), self.span_from(start)))
    }

    fn parse_throw(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // throw
        if self.current.newline_before {
            return Err(self.error("line terminator not allowed after 'throw'"));
        }
        let argument = self.parse_expression(true)?;
        self.semicolon()?;
        Ok(Stmt::new(
            StmtKind::Throw(Box::new(argument)),
            self.span_from(start),
        ))
    }

    fn parse_break_continue(&mut self, is_break: bool) -> Result<Stmt> {
        let start = self.current.span.start;
        let keyword_span = self.current.span;
        self.bump()?;
        let label = if matches!(self.current.kind, TokenKind::Ident(_))
            && !self.current.newline_before
        {
            let ident = self.expect_ident()?;
            let found = self.labels.iter().rev().find(|l| l.name == ident.name);
            match found {
                None => {
                    return Err(
                        self.error_at(format!("label '{}' not found", ident.name), ident.span)
                    );
                }
                Some(label) => {
                    if !is_break && label.kind != LabelKind::Iteration {
                        return Err(self.error_at(
                            format!("continue must be inside loop (label '{}')", ident.name),
                            ident.span,
                        ));
                    }
                }
            }
            Some(ident)
        } else {
            if !is_break && self.loop_depth == 0 {
                return Err(self.error_at("continue must be inside loop", keyword_span));
            }
            if is_break && self.loop_depth == 0 && self.switch_depth == 0 && self.labels.is_empty()
            {
                return Err(self.error_at("break must be inside loop or switch", keyword_span));
            }
            None
        };
        self.semicolon()?;
        Ok(Stmt::new(
            if is_break {
                StmtKind::Break(label)
            } else {
                StmtKind::Continue(label)
            },
            self.span_from(start),
        ))
    }

    fn parse_with(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        let span = self.current.span;
        if self.strict {
            return Err(self.error_at("invalid keyword: with", span));
        }
        self.bump()?; // with
        self.expect_punct(Punct::LParen)?;
        let object = self.parse_expression(true)?;
        self.expect_punct(Punct::RParen)?;
        let body = self.parse_statement()?;
        Ok(Stmt::new(
            StmtKind::With {
                object: Box::new(object),
                body: Box::new(body),
            },
            self.span_from(start),
        ))
    }

    fn parse_labeled(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        let label = self.expect_ident()?;
        self.expect_punct(Punct::Colon)?;
        if self.labels.iter().any(|l| l.name == label.name) {
            return Err(self.error_at(
                format!("label '{}' already declared", label.name),
                label.span,
            ));
        }
        let kind = if self.at_keyword(Keyword::For)
            || self.at_keyword(Keyword::While)
            || self.at_keyword(Keyword::Do)
        {
            LabelKind::Iteration
        } else {
            LabelKind::Other
        };
        self.labels.push(Label {
            name: label.name.clone(),
            kind,
        });
        let body = self.parse_statement();
        self.labels.pop();
        Ok(Stmt::new(
            StmtKind::Labeled {
                label,
                body: Box::new(body?),
            },
            self.span_from(start),
        ))
    }

    // -- declarations ------------------------------------------------------

    fn parse_var_decl(
        &mut self,
        kind: VarDeclKind,
        in_for_head: bool,
        require_init: bool,
    ) -> Result<VarDecl> {
        let start = self.current.span.start;
        self.bump()?; // var / let / const / using
        self.parse_var_declarators(kind, in_for_head, require_init, start)
    }

    /// A declaration whose leading keyword has already been consumed by the caller.
    ///
    /// `await using x = …` is written with two words, so the `await` is read first and only
    /// the `using` is left here.
    fn parse_var_decl_with_keyword(&mut self, kind: VarDeclKind, keyword: &str) -> Result<VarDecl> {
        let start = self.current.span.start;
        if !self.at_ident(keyword) {
            return Err(self.error(format!("expected '{keyword}'")));
        }
        self.bump()?;
        self.parse_var_declarators(kind, false, true, start)
    }

    fn parse_var_declarators(
        &mut self,
        kind: VarDeclKind,
        in_for_head: bool,
        require_init: bool,
        start: Position,
    ) -> Result<VarDecl> {
        let mut declarators = Vec::new();
        loop {
            let declarator_start = self.current.span.start;
            let target = self.parse_binding_target()?;
            // Declared here, before the initialiser: the reference calls `js_define_var`
            // from `js_parse_var`'s `set_val`, which is reached with the name just
            // consumed, so a redeclaration points at the `=` (or whatever follows)
            // rather than at the end of the statement.
            self.declare_pattern(&target, !kind.is_lexical())?;
            // `using` disposes a value, so it binds a name and nothing else: a pattern has
            // no single resource to dispose.
            if kind.is_using() && !matches!(target.unparen_kind(), PatternKind::Ident(_)) {
                return Err(self.error_at(
                    "binding patterns are not allowed in using declarations",
                    target.span,
                ));
            }
            let init = if self.eat_punct(Punct::Eq)? {
                // `for (var x = a in b;;)` must not parse `a in b` as the
                // initialiser: the head uses AssignmentExpression[~In].
                Some(Box::new(self.parse_assignment(!in_for_head)?))
            } else {
                None
            };
            if init.is_none() && require_init {
                if kind == VarDeclKind::Const {
                    return Err(self.error("missing initializer for const variable"));
                }
                if kind.is_using() {
                    return Err(self.error("missing initializer in using declaration"));
                }
                if !matches!(target.unparen_kind(), PatternKind::Ident(_)) && !in_for_head {
                    return Err(self.error_at(
                        "missing initializer in destructuring declaration",
                        target.span,
                    ));
                }
            }
            if kind.is_lexical() {
                let mut names = Vec::new();
                target.bound_names(&mut names);
                if let Some(name) = names.iter().find(|n| n.name == "let") {
                    return Err(self.error_at("'let' is not a valid lexical identifier", name.span));
                }
            }
            declarators.push(Declarator {
                target,
                init,
                span: self.span_from(declarator_start),
            });
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        Ok(VarDecl {
            kind,
            declarators,
            span: self.span_from(start),
        })
    }

    /// A binding element: `target` or `target = default`. Used for parameters and
    /// for elements of array/object patterns.
    fn parse_binding_element(&mut self) -> Result<Pattern> {
        let pattern = self.parse_binding_target()?;
        if self.eat_punct(Punct::Eq)? {
            let start = pattern.span.start;
            let default = self.parse_assignment(self.allow_in)?;
            return Ok(Pattern::new(
                PatternKind::Assign {
                    target: Box::new(pattern),
                    default: Box::new(default),
                },
                self.span_from(start),
            ));
        }
        Ok(pattern)
    }

    /// A binding target: `Ident`, `[ … ]` or `{ … }`, with no default value.
    /// Used for declarators, `catch` parameters, rest elements and `for` heads.
    fn parse_binding_target(&mut self) -> Result<Pattern> {
        self.enter()?;
        let result = self.parse_binding_target_unguarded();
        self.leave();
        result
    }

    fn parse_binding_target_unguarded(&mut self) -> Result<Pattern> {
        let start = self.current.span.start;
        if self.at_punct(Punct::LBracket) {
            return self.parse_array_pattern(start);
        }
        if self.at_punct(Punct::LBrace) {
            return self.parse_object_pattern(start);
        }
        let ident = self.expect_binding_ident()?;
        Ok(Pattern::new(
            PatternKind::Ident(ident),
            self.span_from(start),
        ))
    }

    fn parse_array_pattern(&mut self, start: Position) -> Result<Pattern> {
        self.expect_punct(Punct::LBracket)?;
        let mut elements = Vec::new();
        let mut rest = None;
        loop {
            if self.at_punct(Punct::RBracket) {
                break;
            }
            if self.at_punct(Punct::Comma) {
                self.bump()?;
                elements.push(None);
                continue;
            }
            if self.at_punct(Punct::Ellipsis) {
                self.bump()?;
                let target = self.parse_binding_target()?;
                if self.at_punct(Punct::Comma) {
                    return Err(self.error("rest element must be the last one"));
                }
                rest = Some(Box::new(RestElement::Pattern(target)));
                break;
            }
            let element_start = self.current.span.start;
            let element = self.parse_binding_element()?;
            let span = self.span_from(element_start);
            elements.push(Some(PatternElement::Pattern(element)));
            let _ = span;
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(Pattern::new(
            PatternKind::Array { elements, rest },
            self.span_from(start),
        ))
    }

    fn parse_object_pattern(&mut self, start: Position) -> Result<Pattern> {
        self.expect_punct(Punct::LBrace)?;
        let mut properties = Vec::new();
        let mut rest = None;
        loop {
            if self.at_punct(Punct::RBrace) {
                break;
            }
            if self.at_punct(Punct::Ellipsis) {
                self.bump()?;
                let target = self.parse_binding_target()?;
                rest = Some(Box::new(RestElement::Pattern(target)));
                if self.at_punct(Punct::Comma) {
                    return Err(self.error("rest element must be the last one"));
                }
                break;
            }
            let property_start = self.current.span.start;
            let key = self.parse_property_key()?;
            if self.eat_punct(Punct::Colon)? {
                let value = self.parse_binding_element()?;
                properties.push(Some(PatternElement::Property {
                    key,
                    value,
                    shorthand: false,
                    span: self.span_from(property_start),
                }));
            } else {
                let PropName::Literal(PropKey::Str(name)) = &key else {
                    return Err(
                        self.error("expecting ':' after a computed or numeric property key")
                    );
                };
                let span = self.span_from(property_start);
                let ident = Ident {
                    name: name.clone(),
                    span,
                };
                self.check_binding_name(&ident.name, ident.span)?;
                let mut value = Pattern::new(PatternKind::Ident(ident), span);
                if self.eat_punct(Punct::Eq)? {
                    let default = self.parse_assignment(self.allow_in)?;
                    value = Pattern::new(
                        PatternKind::Assign {
                            target: Box::new(value),
                            default: Box::new(default),
                        },
                        span,
                    );
                }
                properties.push(Some(PatternElement::Property {
                    key,
                    value,
                    shorthand: true,
                    span,
                }));
            }
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Pattern::new(
            PatternKind::Object { properties, rest },
            self.span_from(start),
        ))
    }

    /// A property key: identifier, string, number, computed or `#private`.
    fn parse_property_key(&mut self) -> Result<PropName> {
        match self.current.kind.clone() {
            TokenKind::Ident(name) => {
                self.bump()?;
                Ok(PropName::Literal(PropKey::Str(name)))
            }
            TokenKind::Keyword(kw) => {
                // Reserved words are valid property names (`{ if: 1 }`).
                self.bump()?;
                Ok(PropName::Literal(PropKey::Str(kw.as_str().to_string())))
            }
            TokenKind::String(value) => {
                self.bump()?;
                Ok(PropName::Literal(PropKey::Str(value)))
            }
            TokenKind::Number(NumberLiteral::Int(v)) => {
                self.bump()?;
                Ok(PropName::Literal(PropKey::Num(v as f64)))
            }
            TokenKind::Number(NumberLiteral::Float(v)) => {
                self.bump()?;
                Ok(PropName::Literal(PropKey::Num(v)))
            }
            TokenKind::BigInt(big) => Err(self.error(format!(
                "BigInt property keys are not supported ('{}n')",
                big.digits
            ))),
            TokenKind::Punct(Punct::LBracket) => {
                self.bump()?;
                let expr = self.parse_assignment(true)?;
                self.expect_punct(Punct::RBracket)?;
                Ok(PropName::Computed(Box::new(expr)))
            }
            TokenKind::Punct(Punct::Hash) => {
                self.bump()?;
                let ident = self.expect_ident()?;
                Ok(PropName::Private(ident))
            }
            // `js_parse_property_name` in the reference: an object literal or a class
            // member whose name is not an identifier, string, number or computed key.
            _ => Err(self.error("invalid property name")),
        }
    }

    // -- functions ---------------------------------------------------------

    fn parse_function(&mut self, form: FunctionForm, is_async: bool) -> Result<Function> {
        let start = self.current.span.start;
        self.bump()?; // function
        let is_generator = self.eat_punct(Punct::Star)?;
        let name = if matches!(self.current.kind, TokenKind::Ident(_)) {
            let ident = if form == FunctionForm::Declaration {
                self.expect_binding_ident()?
            } else {
                self.expect_ident()?
            };
            if is_generator && ident.name == "yield" {
                return Err(self.error_at("invalid use of 'yield' as a function name", ident.span));
            }
            if is_async && ident.name == "await" {
                return Err(self.error_at("invalid use of 'await' as a function name", ident.span));
            }
            Some(ident)
        } else {
            if form == FunctionForm::Declaration {
                return Err(self.error("function name expected"));
            }
            None
        };
        // A function declaration in a *block* is a lexical binding of that block, and the
        // reference checks it right here: `js_parse_function_decl2` calls
        // `define_var(JS_VAR_DEF_FUNCTION_DECL)` with the name just consumed, so the
        // reported position is the token that follows it — `(` for `function a(`. The
        // body-scope case is a different check and happens after the whole function, which
        // is why it is made at the call site instead.
        //
        // Doing it here also means a block-level conflict is found before the body is
        // parsed, so the body's own errors cannot mask it.
        if form == FunctionForm::Declaration
            && let Some(ident) = &name
        {
            self.declare_block_function(&ident.name, !is_async && !is_generator)?;
        }
        let saved = self.ctx;
        self.ctx = FnCtx {
            in_function: true,
            in_generator: is_generator,
            in_async: is_async,
            in_method: false,
            in_derived_constructor: false,
        };
        let saved_top = self.top_level;
        self.top_level = false;
        // Strictness is per function: `function f(){ "use strict"; }` must leave the
        // code *after* it sloppy. Without this save the directive leaked outwards, the
        // enclosing script was compiled as strict, and a later `function g(){}` lost its
        // declaration status — `typeof g` then raised instead of answering, because an
        // implicit global is not a declaration.
        let params_result = self.parse_formal_parameters();
        let result = match params_result {
            Ok(params) => self.parse_function_body(params, false),
            Err(err) => Err(err),
        };
        self.ctx = saved;
        self.top_level = saved_top;
        let (params, params_span, body, strict) = result?;
        Ok(Function {
            name,
            params,
            body,
            is_async,
            is_generator,
            strict,
            is_arrow: false,
            span: self.span_from(start),
            params_span,
        })
    }

    fn parse_formal_parameters(&mut self) -> Result<Vec<Pattern>> {
        self.expect_punct(Punct::LParen)?;
        let mut params = Vec::new();
        loop {
            if self.at_punct(Punct::RParen) {
                break;
            }
            if self.at_punct(Punct::Ellipsis) {
                self.bump()?;
                // The rest parameter's own binding needs the same check: `function f(...)`
                // is a missing parameter, not a malformed one.
                if !self.at_formal_parameter() {
                    return Err(self.error("missing formal parameter"));
                }
                let target = self.parse_binding_target()?;
                if matches!(target.unparen_kind(), PatternKind::Assign { .. }) {
                    return Err(
                        self.error_at("rest element cannot have a default value", target.span)
                    );
                }
                params.push(Pattern::new(
                    PatternKind::Rest(Box::new(target.clone())),
                    target.span,
                ));
                // A rest parameter ends the list, so the only thing that may follow is
                // `)`. The reference simply asks for it: `js_parse_expect(s, ')')`.
                if self.at_punct(Punct::Comma) {
                    return Err(self.error("expecting ')'"));
                }
                break;
            }
            if !self.at_formal_parameter() {
                return Err(self.error("missing formal parameter"));
            }
            let param = self.parse_binding_element()?;
            self.check_parameter(&param)?;
            params.push(param);
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(params)
    }

    fn check_parameter(&self, param: &Pattern) -> Result<()> {
        match &param.kind {
            PatternKind::Ident(ident) => self.check_binding_name(&ident.name, ident.span),
            PatternKind::Assign { target, .. } => self.check_parameter(target),
            _ => Ok(()),
        }
    }

    fn parse_function_body(
        &mut self,
        params: Vec<Pattern>,
        is_method: bool,
    ) -> Result<(Vec<Pattern>, Span, FunctionBody, bool)> {
        let params_span = self.prev_end;
        // Strictness is per function, so the body saves and restores it itself rather
        // than trusting every caller to do so: `function f(){ "use strict" }` must
        // leave the code after it sloppy, and the enclosing scope may well have been
        // strict too. The returned flag is the body's own mode.
        let saved_strict = self.strict;
        if !self.at_punct(Punct::LBrace) {
            return Err(self.unexpected());
        }
        // A function body is one of the places a `using` may live, so a function defined
        // inside a `case` clause starts with a clean slate for that check.
        let saved_in_case = self.in_case_clause;
        self.in_case_clause = false;
        let param_names = simple_parameter_names(&params);
        let simple_params = params
            .iter()
            .all(|p| matches!(p.unparen_kind(), PatternKind::Ident(_)));
        let mut body = Vec::new();
        let mut prologue = true;
        let mut strict_seen = false;
        self.bump()?; // {
        // Where the final duplicate-parameter check points: the reference runs it with the
        // body's first token as `s->token`, so an empty body reports the `}` and
        // `{ return 1 }` reports the `return`.
        let body_start = self.current.span;
        self.push_scope(Scope::function(param_names));
        while !self.at_punct(Punct::RBrace) {
            if self.current.is_eof() {
                return Err(self.unexpected());
            }
            let stmt = self.parse_statement_list_item()?;
            if prologue {
                match self.directive_value(&stmt) {
                    Some(directive) if directive == "use strict" => {
                        strict_seen = true;
                        self.enter_strict()?;
                    }
                    Some(_) => {}
                    None => prologue = false,
                }
            }
            body.push(stmt);
        }
        self.pop_scope();
        self.bump()?; // }
        // The body's own mode, still in force while the early errors below are
        // checked; the enclosing mode is restored once they are done.
        let body_strict = self.strict;
        if strict_seen && !simple_params {
            return Err(self.error_at(
                // The reference spells the directive with double quotes inside the message.
                "\"use strict\" not allowed in function with default or destructuring parameter",
                Span::new(params_span, params_span),
            ));
        }
        // Duplicate parameter names. Where they are illegal at all is the reference's
        // condition in `js_parse_function_args`: strict mode, a non-simple list, an arrow,
        // or a method. A plain sloppy `function f(a, a){}` is legal (last wins), and so is
        // a sloppy generator or `async function` — only an async *method* is covered.
        //
        // Which of the two messages is used depends on how the duplicate was reached, and
        // that is a different question: at *parameter-parse* time the reference checks each
        // name with `duplicate parameter names`, and it only gets there when the list holds
        // an `=` anywhere or the name sits inside a destructuring pattern. Everything else
        // waits for the check above, which says `duplicate argument names`.
        self.check_duplicate_parameters(
            &params,
            body_strict || !simple_params || is_method,
            body_start,
        )?;
        let params_span_value = Span::new(params_span, params_span);
        self.strict = saved_strict;
        self.lexer.set_strict(saved_strict);
        self.in_case_clause = saved_in_case;
        Ok((
            params,
            params_span_value,
            FunctionBody::Block(body),
            body_strict,
        ))
    }

    // -- classes -----------------------------------------------------------

    fn parse_class(&mut self, is_expression: bool) -> Result<Class> {
        let start = self.current.span.start;
        self.bump()?; // class
        let name = if matches!(self.current.kind, TokenKind::Ident(_)) {
            if is_expression {
                Some(self.expect_ident()?)
            } else {
                Some(self.expect_binding_ident()?)
            }
        } else {
            if !is_expression {
                return Err(self.error("class statement requires a name"));
            }
            None
        };
        let super_class = if self.eat_keyword(Keyword::Extends)? {
            Some(Box::new(self.parse_lhs_expression()?))
        } else {
            None
        };
        let has_super = super_class.is_some();
        self.private.push(PrivateEnv::default());
        self.expect_punct(Punct::LBrace)?;
        let mut elements = Vec::new();
        let mut seen_constructor = false;
        let result = (|| -> Result<()> {
            while !self.at_punct(Punct::RBrace) {
                // No end-of-input guard here: `class A {` must report `invalid property
                // name`, which is what falls out of asking for the element's name. An
                // early `unexpected()` would pre-empt that with a vaguer message.
                if self.eat_punct(Punct::Semicolon)? {
                    continue;
                }
                elements.push(self.parse_class_element(has_super, &mut seen_constructor)?);
            }
            Ok(())
        })();
        self.private.pop();
        result?;
        // The `}` itself, before it is consumed: a class with no written constructor gets
        // a synthetic one created *at* this token (see [`Class::body_end`]).
        let body_end = self.current.span.start;
        self.bump()?; // }
        Ok(Class {
            name,
            super_class,
            elements,
            span: self.span_from(start),
            body_end,
        })
    }

    fn parse_class_element(
        &mut self,
        has_super: bool,
        seen_constructor: &mut bool,
    ) -> Result<ClassElement> {
        let start = self.current.span.start;
        let mut is_static = false;
        if self.at_ident("static") {
            let peek = self.peek()?;
            let is_modifier = !matches!(
                peek.kind,
                TokenKind::Punct(Punct::LParen)
                    | TokenKind::Punct(Punct::Eq)
                    | TokenKind::Punct(Punct::Semicolon)
                    | TokenKind::Punct(Punct::RBrace)
            );
            if is_modifier {
                if matches!(peek.kind, TokenKind::Punct(Punct::LBrace)) {
                    self.bump()?; // static
                    let body = self.parse_block()?;
                    return Ok(ClassElement::StaticBlock {
                        body,
                        span: self.span_from(start),
                    });
                }
                self.bump()?; // static
                is_static = true;
                if self.at_punct(Punct::LBrace) {
                    let body = self.parse_block()?;
                    return Ok(ClassElement::StaticBlock {
                        body,
                        span: self.span_from(start),
                    });
                }
            }
        }

        // Modifiers: async, get, set, *
        let mut is_async = false;
        let mut is_generator = false;
        let mut accessor: Option<MethodKind> = None;
        if self.at_ident("async") {
            let peek = self.peek()?;
            let is_modifier = !matches!(
                peek.kind,
                TokenKind::Punct(Punct::LParen)
                    | TokenKind::Punct(Punct::Eq)
                    | TokenKind::Punct(Punct::Semicolon)
                    | TokenKind::Punct(Punct::RBrace)
            ) && !peek.newline_before;
            if is_modifier {
                self.bump()?;
                is_async = true;
            }
        }
        if self.at_punct(Punct::Star) {
            self.bump()?;
            is_generator = true;
        }
        if accessor.is_none() && (self.at_ident("get") || self.at_ident("set")) {
            let is_get = self.at_ident("get");
            let peek = self.peek()?;
            if is_property_key_start(&peek.kind) {
                self.bump()?;
                accessor = Some(if is_get {
                    MethodKind::Get
                } else {
                    MethodKind::Set
                });
            }
        }
        let key = self.parse_property_key()?;

        // Private names must be registered (duplicates are early errors).
        if let PropName::Private(ident) = &key {
            if let Some(env) = self.private.last_mut() {
                if !env.declare(&ident.name, accessor) {
                    return Err(self.error_at(
                        // The reference words this without the name:
                        // `js_parse_error(s, "private class field is already defined")`.
                        "private class field is already defined".to_string(),
                        ident.span,
                    ));
                }
            }
        }

        if self.at_punct(Punct::LParen) {
            let kind = match accessor {
                Some(kind) => kind,
                None => {
                    let is_constructor = !is_static
                        && !is_generator
                        && !is_async
                        && matches!(&key, PropName::Literal(PropKey::Str(s)) if s == "constructor");
                    if is_constructor {
                        if *seen_constructor {
                            return Err(self.error_at(
                                "property constructor appears more than once",
                                self.span_from(start),
                            ));
                        }
                        *seen_constructor = true;
                        MethodKind::Constructor
                    } else {
                        MethodKind::Method
                    }
                }
            };
            if kind == MethodKind::Constructor && (is_generator || is_async) {
                return Err(self.error("a constructor may not be a generator or async"));
            }
            let saved = self.ctx;
            self.ctx = FnCtx {
                in_function: true,
                in_generator: is_generator,
                in_async: is_async,
                in_method: true,
                in_derived_constructor: kind == MethodKind::Constructor && has_super,
            };
            let saved_top = self.top_level;
            self.top_level = false;
            let result = (|| -> Result<Function> {
                let params = self.parse_formal_parameters()?;
                let (params, params_span, body, strict) = self.parse_function_body(params, true)?;
                Ok(Function {
                    name: None,
                    params,
                    body,
                    is_async,
                    is_generator,
                    strict,
                    is_arrow: false,
                    span: self.span_from(start),
                    params_span,
                })
            })();
            self.ctx = saved;
            self.top_level = saved_top;
            let func = result?;
            if is_static {
                if let PropName::Literal(PropKey::Str(name)) = &key {
                    if name == "prototype" {
                        return Err(self.error_at("invalid method name", func.span));
                    }
                }
            }
            if kind == MethodKind::Get && !func.params.is_empty() {
                return Err(self.error_at(
                    "invalid number of arguments for getter or setter",
                    func.span,
                ));
            }
            if kind == MethodKind::Set && func.params.len() != 1 {
                return Err(self.error_at(
                    "invalid number of arguments for getter or setter",
                    func.span,
                ));
            }
            return Ok(ClassElement::Method {
                key,
                is_static,
                kind,
                func: Box::new(func),
                span: self.span_from(start),
            });
        }

        // Field definition.
        if accessor.is_some() {
            return Err(self.error("expecting '(' after an accessor name"));
        }
        if let PropName::Literal(PropKey::Str(name)) = &key {
            if name == "constructor" && !is_static {
                return Err(self.error("classes may not have a field named 'constructor'"));
            }
            if name == "prototype" && is_static {
                return Err(self.error("classes may not have a static property named 'prototype'"));
            }
        }
        let value = if self.eat_punct(Punct::Eq)? {
            Some(Box::new(self.parse_assignment(true)?))
        } else {
            None
        };
        // Fields are terminated by `;`, `}` or a line break.
        self.semicolon()?;
        Ok(ClassElement::Field {
            key,
            is_static,
            value,
            span: self.span_from(start),
        })
    }

    // -- modules -----------------------------------------------------------

    fn parse_import_decl(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // import
        let mut specifiers = Vec::new();
        if let TokenKind::String(source) = self.current.kind.clone() {
            self.bump()?;
            let attributes = self.parse_import_attributes()?;
            self.semicolon()?;
            return Ok(Stmt::new(
                StmtKind::Import(Box::new(ImportDecl {
                    specifiers,
                    source,
                    attributes,
                    span: self.span_from(start),
                })),
                self.span_from(start),
            ));
        }
        // Default import.
        if matches!(self.current.kind, TokenKind::Ident(_)) {
            let local = self.expect_ident()?;
            specifiers.push(ImportSpecifier::Default(local));
            if !self.eat_punct(Punct::Comma)? {
                return self.finish_import(start, specifiers);
            }
        }
        if self.at_punct(Punct::Star) {
            self.bump()?;
            if !self.eat_contextual("as")? {
                return Err(self.unexpected());
            }
            let local = self.expect_ident()?;
            specifiers.push(ImportSpecifier::Namespace(local));
            return self.finish_import(start, specifiers);
        }
        if self.at_punct(Punct::LBrace) {
            self.bump()?;
            while !self.at_punct(Punct::RBrace) {
                let imported = self.parse_module_name()?;
                let local = if self.eat_contextual("as")? {
                    self.expect_ident()?
                } else {
                    match &imported {
                        ModuleName::Ident(id) => id.clone(),
                        ModuleName::Str(_) => {
                            return Err(self.error_at(
                                "a string module name must be followed by 'as'",
                                self.current.span,
                            ));
                        }
                    }
                };
                specifiers.push(ImportSpecifier::Named { imported, local });
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace)?;
        } else if !specifiers.is_empty() {
            return Err(self.unexpected());
        } else if self.at_punct(Punct::LBrace) {
            unreachable!("handled above");
        } else {
            return Err(self.unexpected());
        }
        self.finish_import(start, specifiers)
    }

    fn finish_import(&mut self, start: Position, specifiers: Vec<ImportSpecifier>) -> Result<Stmt> {
        if !self.eat_contextual("from")? {
            return Err(self.unexpected());
        }
        let TokenKind::String(source) = self.current.kind.clone() else {
            return Err(self.error("string expected"));
        };
        self.bump()?;
        let attributes = self.parse_import_attributes()?;
        self.semicolon()?;
        let span = self.span_from(start);
        for specifier in &specifiers {
            let local = match specifier {
                ImportSpecifier::Default(id) => id,
                ImportSpecifier::Namespace(id) => id,
                ImportSpecifier::Named { local, .. } => local,
            };
            self.declare_lexical(&local.name)?;
            if let Some(scope) = self.scopes.last_mut()
                && !scope.import_names.iter().any(|n| n == &local.name)
            {
                scope.import_names.push(local.name.clone());
            }
        }
        Ok(Stmt::new(
            StmtKind::Import(Box::new(ImportDecl {
                specifiers,
                source,
                attributes,
                span,
            })),
            span,
        ))
    }

    fn parse_import_attributes(&mut self) -> Result<Vec<ImportAttribute>> {
        let mut attributes = Vec::new();
        let has_with = self.at_keyword(Keyword::With) || self.at_ident("assert");
        if !has_with {
            return Ok(attributes);
        }
        let peek = self.peek()?;
        if !matches!(peek.kind, TokenKind::Punct(Punct::LBrace)) {
            return Ok(attributes);
        }
        self.bump()?; // with / assert
        self.expect_punct(Punct::LBrace)?;
        while !self.at_punct(Punct::RBrace) {
            let attr_start = self.current.span.start;
            let key = match self.current.kind.clone() {
                TokenKind::Ident(name) => name,
                TokenKind::String(value) => value,
                _ => return Err(self.error("expecting an import attribute name")),
            };
            self.bump()?;
            self.expect_punct(Punct::Colon)?;
            let TokenKind::String(value) = self.current.kind.clone() else {
                return Err(self.error("expecting an import attribute value string"));
            };
            self.bump()?;
            attributes.push(ImportAttribute {
                key,
                value,
                span: self.span_from(attr_start),
            });
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(attributes)
    }

    fn parse_module_name(&mut self) -> Result<ModuleName> {
        match self.current.kind.clone() {
            TokenKind::String(value) => {
                self.bump()?;
                Ok(ModuleName::Str(value))
            }
            TokenKind::Ident(_) => Ok(ModuleName::Ident(self.expect_ident()?)),
            TokenKind::Keyword(kw) => {
                let span = self.current.span;
                self.bump()?;
                Ok(ModuleName::Ident(Ident {
                    name: kw.as_str().to_string(),
                    span,
                }))
            }
            // `export {a as}`: the alias may be an identifier or a string, and the
            // reference names both (`js_parse_error(s, "identifier or string expected")`).
            _ => Err(self.error("identifier or string expected")),
        }
    }

    fn parse_export_decl(&mut self) -> Result<Stmt> {
        let start = self.current.span.start;
        self.bump()?; // export
        if self.at_punct(Punct::Star) {
            self.bump()?;
            let mut exported = None;
            if self.eat_contextual("as")? {
                exported = Some(self.parse_module_name()?);
            }
            if !self.eat_contextual("from")? {
                return Err(self.error("expecting 'from' after 'export *'"));
            }
            let TokenKind::String(source) = self.current.kind.clone() else {
                return Err(self.error("string expected"));
            };
            self.bump()?;
            let attributes = self.parse_import_attributes()?;
            self.semicolon()?;
            let span = self.span_from(start);
            return Ok(Stmt::new(
                StmtKind::Export(Box::new(ExportDecl {
                    kind: ExportKind::All {
                        source,
                        exported,
                        attributes,
                    },
                    span,
                })),
                span,
            ));
        }
        if self.eat_keyword(Keyword::Default)? {
            let value = if self.at_keyword(Keyword::Function) {
                let func = self.parse_function(FunctionForm::Expression, false)?;
                Expr::new(ExprKind::Function(Box::new(func)), self.span_from(start))
            } else if self.at_ident("async") {
                let peek = self.peek()?;
                if matches!(peek.kind, TokenKind::Keyword(Keyword::Function)) {
                    self.bump()?;
                    let func = self.parse_function(FunctionForm::Expression, true)?;
                    Expr::new(ExprKind::Function(Box::new(func)), self.span_from(start))
                } else {
                    self.parse_assignment(true)?
                }
            } else if self.at_keyword(Keyword::Class) {
                let class = self.parse_class(true)?;
                Expr::new(ExprKind::Class(Box::new(class)), self.span_from(start))
            } else {
                let expr = self.parse_assignment(true)?;
                self.semicolon()?;
                expr
            };
            let span = self.span_from(start);
            return Ok(Stmt::new(
                StmtKind::Export(Box::new(ExportDecl {
                    kind: ExportKind::Default(Box::new(value)),
                    span,
                })),
                span,
            ));
        }
        if self.at_keyword(Keyword::Var)
            || self.at_keyword(Keyword::Const)
            || self.at_keyword(Keyword::Function)
            || self.at_keyword(Keyword::Class)
            || (self.at_ident("let") && self.let_starts_declaration()?)
        {
            let decl = self.parse_statement_list_item()?;
            let span = self.span_from(start);
            return Ok(Stmt::new(
                StmtKind::Export(Box::new(ExportDecl {
                    kind: ExportKind::Declaration(Box::new(decl)),
                    span,
                })),
                span,
            ));
        }
        if self.at_ident("async") {
            let peek = self.peek()?;
            if matches!(peek.kind, TokenKind::Keyword(Keyword::Function)) && !peek.newline_before {
                let decl = self.parse_statement_list_item()?;
                let span = self.span_from(start);
                return Ok(Stmt::new(
                    StmtKind::Export(Box::new(ExportDecl {
                        kind: ExportKind::Declaration(Box::new(decl)),
                        span,
                    })),
                    span,
                ));
            }
        }
        if self.at_punct(Punct::LBrace) {
            self.bump()?;
            let mut specifiers = Vec::new();
            let mut candidates: Vec<(String, Span)> = Vec::new();
            let mut source = None;
            while !self.at_punct(Punct::RBrace) {
                let spec_start = self.current.span.start;
                let local = self.parse_module_name()?;
                let exported = if self.eat_contextual("as")? {
                    self.parse_module_name()?
                } else {
                    local.clone()
                };
                // Deferred: whether these name *local* bindings (and so have to be
                // checked against the module's declarations) depends on whether a
                // `from` follows, which is not known until the closing brace.
                if let ModuleName::Ident(id) = &local {
                    candidates.push((id.name.clone(), id.span));
                }
                specifiers.push(ExportSpecifier {
                    local,
                    exported,
                    span: self.span_from(spec_start),
                });
                if !self.eat_punct(Punct::Comma)? {
                    break;
                }
            }
            self.expect_punct(Punct::RBrace)?;
            let mut attributes = Vec::new();
            let has_from = self.eat_contextual("from")?;
            if !has_from {
                // `export { a }` re-exports a binding of this module and must name one;
                // `export { a } from "m"` names an export of `m` and is checked when the
                // module is linked, not here.
                self.pending_exports.extend(candidates);
            }
            if has_from {
                let TokenKind::String(value) = self.current.kind.clone() else {
                    return Err(self.error("string expected"));
                };
                self.bump()?;
                source = Some(value);
                attributes = self.parse_import_attributes()?;
            }
            self.semicolon()?;
            let span = self.span_from(start);
            return Ok(Stmt::new(
                StmtKind::Export(Box::new(ExportDecl {
                    kind: ExportKind::Named {
                        specifiers,
                        source,
                        attributes,
                    },
                    span,
                })),
                span,
            ));
        }
        Err(self.unexpected())
    }

    // -- patterns from expressions ----------------------------------------

    fn expr_to_pattern(&mut self, expr: Expr, kind: TargetKind) -> Result<Pattern> {
        self.expr_to_pattern_inner(expr, kind, false)
    }

    fn expr_to_pattern_inner(
        &mut self,
        expr: Expr,
        kind: TargetKind,
        nested: bool,
    ) -> Result<Pattern> {
        let span = expr.span;
        match expr.kind {
            ExprKind::Ident(ident) => {
                if kind == TargetKind::Binding {
                    self.check_binding_name(&ident.name, ident.span)?;
                } else {
                    self.check_identifier(&ident.name, ident.span)?;
                    if self.strict && (ident.name == "eval" || ident.name == "arguments") {
                        return Err(self.error_at(
                            format!("invalid assignment to '{}' in strict mode", ident.name),
                            ident.span,
                        ));
                    }
                }
                Ok(Pattern::new(PatternKind::Ident(ident), span))
            }
            ExprKind::Paren(inner) => self.expr_to_pattern_inner(*inner, kind, nested),
            ExprKind::Array { elements } => {
                let mut out = Vec::new();
                let mut rest = None;
                let count = elements.len();
                for (index, element) in elements.into_iter().enumerate() {
                    match element {
                        None => out.push(None),
                        Some(ArrayElement::Expr(expr)) => {
                            let pattern = self.expr_to_pattern_inner(*expr, kind, true)?;
                            out.push(Some(PatternElement::Pattern(pattern)));
                        }
                        Some(ArrayElement::Spread(expr)) => {
                            if index + 1 != count {
                                return Err(
                                    self.error_at("rest element must be the last one", expr.span)
                                );
                            }
                            rest = Some(Box::new(self.expr_to_rest(*expr, kind)?));
                        }
                    }
                }
                Ok(Pattern::new(
                    PatternKind::Array {
                        elements: out,
                        rest,
                    },
                    span,
                ))
            }
            ExprKind::Object(properties) => {
                self.cover_inits
                    .retain(|cover| !span_contains(span, *cover));
                let mut out = Vec::new();
                let mut rest = None;
                let count = properties.len();
                for (index, property) in properties.into_iter().enumerate() {
                    match property {
                        ObjectProperty::Spread(expr) => {
                            if index + 1 != count {
                                return Err(
                                    self.error_at("rest element must be the last one", expr.span)
                                );
                            }
                            rest = Some(Box::new(self.expr_to_rest(*expr, kind)?));
                        }
                        ObjectProperty::Property {
                            key,
                            value,
                            kind: _,
                            shorthand,
                            span,
                        } => {
                            let pattern = self.expr_to_pattern_inner(*value, kind, true)?;
                            out.push(Some(PatternElement::Property {
                                key,
                                value: pattern,
                                shorthand,
                                span,
                            }));
                        }
                    }
                }
                Ok(Pattern::new(
                    PatternKind::Object {
                        properties: out,
                        rest,
                    },
                    span,
                ))
            }
            ExprKind::Assign { op, target, value } if op.is_simple() => {
                // `{a = 1}` inside a pattern: the target must itself be a pattern.
                let target = match target.kind {
                    PatternKind::Ident(ident) => {
                        Pattern::new(PatternKind::Ident(ident), target.span)
                    }
                    other => {
                        let expr = pattern_to_expr(Pattern::new(other, target.span), target.span);
                        self.expr_to_pattern_inner(expr, kind, nested)?
                    }
                };
                Ok(Pattern::new(
                    PatternKind::Assign {
                        target: Box::new(target),
                        default: value,
                    },
                    span,
                ))
            }
            ExprKind::Member {
                object,
                property,
                optional,
            } => {
                if kind == TargetKind::Binding {
                    return Err(self.error_at("invalid binding pattern", span));
                }
                if optional {
                    return Err(self.error_at("invalid assignment target", span));
                }
                let op_pos = expr.op_pos;
                Ok(Pattern::new(
                    PatternKind::Member(Box::new(MemberPattern {
                        object,
                        property,
                        span,
                        op_pos,
                    })),
                    span,
                ))
            }
            ExprKind::OptionalChain(_) => {
                Err(self.error_at("invalid assignment left-hand side", span))
            }
            // QuickJS reports `a() = 1` at run time (it evaluates the callee
            // first, then rejects the reference) but rejects the same thing
            // inside a destructuring pattern at parse time. Every other
            // non-assignable expression is rejected here.
            other @ ExprKind::Call { .. } if !nested && kind == TargetKind::Assignment => Ok(
                Pattern::new(PatternKind::Invalid(Box::new(Expr::new(other, span))), span),
            ),
            _ if kind == TargetKind::Binding => Err(self.error_at("invalid binding pattern", span)),
            // Inside an object or array literal being reinterpreted as a pattern the
            // reference says `invalid destructuring target`; a non-reference at the top
            // level of an assignment says `invalid assignment left-hand side`. `nested` is
            // exactly that distinction — `[1] = 2` is the first, `1 = 2` the second.
            _ if nested => Err(self.error_at("invalid destructuring target", span)),
            _ => Err(self.error_at("invalid assignment left-hand side", span)),
        }
    }

    fn expr_to_rest(&mut self, expr: Expr, kind: TargetKind) -> Result<RestElement> {
        // Array and object literals wrap their spread elements, so the target is
        // one level in.
        let expr = match expr.kind {
            ExprKind::Spread(inner) => *inner,
            other => Expr::new(other, expr.span),
        };
        if kind == TargetKind::Binding {
            return Ok(RestElement::Pattern(
                self.expr_to_pattern_inner(expr, kind, true)?,
            ));
        }
        match &expr.kind {
            ExprKind::Member { optional, .. } => {
                if *optional {
                    return Err(self.error_at("invalid assignment target", expr.span));
                }
                match self.expr_to_pattern_inner(expr, kind, true)?.kind {
                    PatternKind::Member(member) => Ok(RestElement::Member(member)),
                    _ => unreachable!("member expressions convert to member patterns"),
                }
            }
            _ => Ok(RestElement::Pattern(
                self.expr_to_pattern_inner(expr, kind, true)?,
            )),
        }
    }

    // -- expressions -------------------------------------------------------

    /// `Expression` (comma separated).
    fn parse_expression(&mut self, allow_in: bool) -> Result<Expr> {
        self.enter()?;
        let result = self.parse_expression_unguarded(allow_in);
        self.leave();
        result
    }

    fn parse_expression_unguarded(&mut self, allow_in: bool) -> Result<Expr> {
        let start = self.current.span.start;
        let first = self.parse_assignment(allow_in)?;
        if !self.at_punct(Punct::Comma) {
            return Ok(first);
        }
        let mut items = vec![first];
        while self.eat_punct(Punct::Comma)? {
            items.push(self.parse_assignment(allow_in)?);
        }
        Ok(Expr::new(ExprKind::Sequence(items), self.span_from(start)))
    }

    fn parse_assignment(&mut self, allow_in: bool) -> Result<Expr> {
        let start = self.current.span.start;

        // `yield` in a generator.
        if self.ctx.in_generator && self.at_ident("yield") {
            return self.parse_yield(allow_in);
        }

        let lhs = match self.try_parse_arrow(allow_in)? {
            Some(arrow) => return Ok(arrow),
            None => self.parse_conditional(allow_in)?,
        };

        if let Some(op) = self.current.kind.as_punct().and_then(AssignOp::from_punct) {
            let op_pos = self.current.span.start;
            let target = self.expr_to_pattern(lhs, TargetKind::Assignment)?;
            self.bump()?;
            let value = self.parse_assignment(allow_in)?;
            return Ok(Expr::at_operator(
                ExprKind::Assign {
                    op,
                    target: Box::new(target),
                    value: Box::new(value),
                },
                self.span_from(start),
                op_pos,
            ));
        }
        let _ = start;
        Ok(lhs)
    }

    fn parse_yield(&mut self, allow_in: bool) -> Result<Expr> {
        let start = self.current.span.start;
        self.bump()?; // yield
        let delegate = if self.at_punct(Punct::Star) && !self.current.newline_before {
            self.bump()?;
            true
        } else {
            false
        };
        let argument = if delegate {
            Some(Box::new(self.parse_assignment(allow_in)?))
        } else if self.current.newline_before
            || self.at_punct(Punct::RParen)
            || self.at_punct(Punct::RBracket)
            || self.at_punct(Punct::RBrace)
            || self.at_punct(Punct::Comma)
            || self.at_punct(Punct::Semicolon)
            || self.at_punct(Punct::Colon)
            || self.current.is_eof()
        {
            None
        } else {
            Some(Box::new(self.parse_assignment(allow_in)?))
        };
        Ok(Expr::new(
            ExprKind::Yield { argument, delegate },
            self.span_from(start),
        ))
    }

    fn try_parse_arrow(&mut self, allow_in: bool) -> Result<Option<Expr>> {
        let starts_paren = self.at_punct(Punct::LParen);
        let starts_ident = matches!(self.current.kind, TokenKind::Ident(_));
        if !starts_paren && !starts_ident {
            return Ok(None);
        }
        // `(` could be a parenthesised expression instead of a parameter list.
        // Scanning ahead for the matching `)` and looking for `=>` avoids the
        // speculative parse (and its stack cost) in the common case.
        if starts_paren && !self.arrow_follows_paren()? {
            return Ok(None);
        }
        let snapshot = self.snapshot();
        let head = match self.parse_arrow_head() {
            Ok(head) => head,
            Err(_) => {
                self.restore(snapshot);
                return Ok(None);
            }
        };
        let Some(head) = head else {
            self.restore(snapshot);
            return Ok(None);
        };
        // Committed: `=>` has been consumed, so errors from here on are real.
        // Arrow parameter lists may never repeat a name, in any mode. The message is the
        // same one `js_parse_function_args` uses, with no name appended — an earlier
        // version added ` ('a')`, which the reference does not print. The position is the
        // body's first token: `(a, a) => x` points at the `x`, and `(a, a) => {}` at the
        // `}`, because the reference has already consumed the `{` by then.
        let body_start = if self.at_punct(Punct::LBrace) {
            self.peek()?.span
        } else {
            self.current.span
        };
        self.check_duplicate_parameters(&head.params, true, body_start)?;
        let saved_ctx = self.ctx;
        let saved_loop = self.loop_depth;
        let saved_switch = self.switch_depth;
        let saved_labels = std::mem::take(&mut self.labels);
        let saved_top = self.top_level;
        self.ctx = FnCtx {
            in_function: true,
            in_generator: false,
            in_async: head.is_async,
            in_method: saved_ctx.in_method,
            in_derived_constructor: saved_ctx.in_derived_constructor,
        };
        self.loop_depth = 0;
        self.switch_depth = 0;
        self.top_level = false;
        let body_start = self.current.span.start;
        let (body, strict) = if self.at_punct(Punct::LBrace) {
            let (block, strict) = self.parse_arrow_block(&head.params)?;
            (block, (self.prev_end, strict))
        } else {
            let expr = self.parse_assignment(allow_in)?;
            let end = expr.span.end;
            // An expression body has no directive prologue, so it inherits.
            let strict = self.strict;
            (FunctionBody::Expr(Box::new(expr)), (end, strict))
        };
        self.ctx = saved_ctx;
        self.loop_depth = saved_loop;
        self.switch_depth = saved_switch;
        self.labels = saved_labels;
        self.top_level = saved_top;
        let _ = body_start;
        let (body, (end, strict)) = (body, strict);
        let span = Span::new(head.span.start, end);
        Ok(Some(Expr::new(
            ExprKind::Arrow(Box::new(Function {
                name: None,
                params: head.params,
                body,
                is_async: head.is_async,
                is_generator: false,
                strict,
                is_arrow: true,
                span,
                params_span: head.params_span,
            })),
            span,
        )))
    }

    /// Parse the `{ … }` body of an arrow, honouring a `"use strict"` prologue.
    fn parse_arrow_block(&mut self, params: &[Pattern]) -> Result<(FunctionBody, bool)> {
        // Like a function body: the arrow's own directive governs the arrow, and the
        // enclosing scope's mode is restored afterwards.
        let saved_strict = self.strict;
        self.expect_punct(Punct::LBrace)?;
        self.push_scope(Scope::function(simple_parameter_names(params)));
        let result = (|| -> Result<Vec<Stmt>> {
            let mut body = Vec::new();
            let mut prologue = true;
            while !self.at_punct(Punct::RBrace) {
                if self.current.is_eof() {
                    return Err(self.unexpected());
                }
                let stmt = self.parse_statement_list_item()?;
                if prologue {
                    match self.directive_value(&stmt) {
                        Some(directive) if directive == "use strict" => self.enter_strict()?,
                        Some(_) => {}
                        None => prologue = false,
                    }
                }
                body.push(stmt);
            }
            Ok(body)
        })();
        self.pop_scope();
        let body_strict = self.strict;
        self.strict = saved_strict;
        self.lexer.set_strict(saved_strict);
        let body = result?;
        self.bump()?;
        Ok((FunctionBody::Block(body), body_strict))
    }

    /// Whether the parenthesis at the current token is the parameter list of an
    /// arrow function, i.e. the matching `)` is followed by `=>`.
    ///
    /// This is a plain token walk with a bracket counter: no recursion, so it
    /// costs a clone of the lexer and nothing else.
    fn arrow_follows_paren(&self) -> Result<bool> {
        let mut lexer = self.lexer.clone();
        // The opening `(` was already consumed by the caller.
        let mut depth = 1usize;
        loop {
            let token = lexer.next_token()?;
            match token.kind {
                TokenKind::Eof => return Ok(false),
                TokenKind::Punct(Punct::LParen)
                | TokenKind::Punct(Punct::LBracket)
                | TokenKind::Punct(Punct::LBrace) => depth += 1,
                TokenKind::Punct(Punct::RParen)
                | TokenKind::Punct(Punct::RBracket)
                | TokenKind::Punct(Punct::RBrace) => {
                    if depth == 0 {
                        // Unbalanced input: the caller reports it.
                        return Ok(false);
                    }
                    depth -= 1;
                    if depth == 0 {
                        // The current token is the opening `(`, so the matching
                        // `)` is the token that brought us back to zero.
                        let next = lexer.next_token()?;
                        return Ok(matches!(next.kind, TokenKind::Punct(Punct::Arrow))
                            && !next.newline_before);
                    }
                }
                _ => {}
            }
        }
    }

    /// Speculatively parse an arrow head, consuming `=>` on success.
    fn parse_arrow_head(&mut self) -> Result<Option<ArrowHead>> {
        let start = self.current.span.start;
        let mut is_async = false;
        if self.at_ident("async") {
            let peek = self.peek()?;
            if peek.newline_before {
                return Ok(None);
            }
            let plausible =
                matches!(peek.kind, TokenKind::Punct(Punct::LParen)) || peek.kind.is_identifier();
            if !plausible {
                return Ok(None);
            }
            self.bump()?;
            is_async = true;
        }
        let params_start = self.current.span.start;
        let params = if self.at_punct(Punct::LParen) {
            self.parse_formal_parameters()?
        } else if matches!(self.current.kind, TokenKind::Ident(_)) {
            let ident = self.expect_ident()?;
            vec![Pattern::new(
                PatternKind::Ident(ident),
                self.span_from(params_start),
            )]
        } else {
            return Ok(None);
        };
        let params_span = self.span_from(params_start);
        if !self.at_punct(Punct::Arrow) || self.current.newline_before {
            return Ok(None);
        }
        self.bump()?; // =>
        Ok(Some(ArrowHead {
            params,
            is_async,
            span: self.span_from(start),
            params_span,
        }))
    }

    fn parse_conditional(&mut self, allow_in: bool) -> Result<Expr> {
        let start = self.current.span.start;
        let test = self.parse_binary(allow_in, 0)?;
        if !self.at_punct(Punct::Question) {
            return Ok(test);
        }
        self.bump()?;
        let consequent = self.parse_assignment(true)?;
        self.expect_punct(Punct::Colon)?;
        let alternate = self.parse_assignment(allow_in)?;
        Ok(Expr::new(
            ExprKind::Conditional {
                test: Box::new(test),
                cons: Box::new(consequent),
                alt: Box::new(alternate),
            },
            self.span_from(start),
        ))
    }

    fn parse_binary(&mut self, allow_in: bool, min_prec: u8) -> Result<Expr> {
        let start = self.current.span.start;
        let mut lhs = self.parse_unary()?;
        while let Some((prec, op)) = binary_operator(&self.current.kind, allow_in) {
            if prec < min_prec {
                break;
            }
            let op_pos = self.current.span.start;
            self.bump()?;
            let next_min = if op.is_right_associative() {
                prec
            } else {
                prec + 1
            };
            let rhs = self.parse_binary(allow_in, next_min)?;
            lhs = build_binary(op, lhs, rhs, self.span_from(start), op_pos)?;
        }
        Ok(lhs)
    }

    /// Unary and postfix operators. `in` is handled by the binary layer, so no
    /// `allow_in` flag is needed here.
    fn parse_unary(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        // Unary operators.
        let unary = match &self.current.kind {
            TokenKind::Keyword(Keyword::Delete) => Some(UnaryOp::Delete),
            TokenKind::Keyword(Keyword::Void) => Some(UnaryOp::Void),
            TokenKind::Keyword(Keyword::Typeof) => Some(UnaryOp::Typeof),
            TokenKind::Punct(Punct::Plus) => Some(UnaryOp::Plus),
            TokenKind::Punct(Punct::Minus) => Some(UnaryOp::Minus),
            TokenKind::Punct(Punct::Bang) => Some(UnaryOp::Not),
            TokenKind::Punct(Punct::Tilde) => Some(UnaryOp::BitNot),
            _ => None,
        };
        if let Some(op) = unary {
            let op_pos = self.current.span.start;
            self.bump()?;
            let argument = self.parse_unary()?;
            if op == UnaryOp::Delete && self.strict {
                if let ExprKind::Ident(ident) = &argument.unparen().kind {
                    return Err(self.error_at(
                        "cannot delete a direct reference in strict mode",
                        ident.span,
                    ));
                }
            }
            let expr = Expr::at_operator(
                ExprKind::Unary {
                    op,
                    argument: Box::new(argument),
                },
                self.span_from(start),
                op_pos,
            );
            if self.at_punct(Punct::StarStar) {
                return Err(self.error(
                    // The reference's wording, from `js_parse_error` in the `**` check.
                    "unparenthesized unary expression can't appear on the left-hand side of '**'",
                ));
            }
            return Ok(expr);
        }
        // Prefix increment/decrement.
        if self.at_punct(Punct::PlusPlus) || self.at_punct(Punct::MinusMinus) {
            let op_pos = self.current.span.start;
            let op = if self.at_punct(Punct::PlusPlus) {
                UpdateOp::Increment
            } else {
                UpdateOp::Decrement
            };
            self.bump()?;
            let argument = self.parse_unary()?;
            self.check_simple_target(&argument)?;
            return Ok(Expr::at_operator(
                ExprKind::Update {
                    op,
                    argument: Box::new(argument),
                    prefix: true,
                },
                self.span_from(start),
                op_pos,
            ));
        }
        // `await` wherever the enclosing function is `async`, operator or not.
        //
        // There is no "unless the next token rules it out" case: the reference reaches
        // `TOK_AWAIT` and consumes it unconditionally, so `await;` and `f(await)` report
        // the *operand* as unexpected rather than treating `await` as an identifier. That
        // is why this asks nothing about what follows.
        if self.at_ident("await") && self.ctx.in_async {
            self.bump()?;
            let argument = self.parse_unary()?;
            return Ok(Expr::new(
                ExprKind::Await(Box::new(argument)),
                self.span_from(start),
            ));
        }
        let expr = self.parse_lhs_expression()?;
        // Postfix increment/decrement (no line break allowed before the operator).
        let result = if (self.at_punct(Punct::PlusPlus) || self.at_punct(Punct::MinusMinus))
            && !self.current.newline_before
        {
            let op_pos = self.current.span.start;
            let op = if self.at_punct(Punct::PlusPlus) {
                UpdateOp::Increment
            } else {
                UpdateOp::Decrement
            };
            self.check_simple_target(&expr)?;
            self.bump()?;
            Expr::at_operator(
                ExprKind::Update {
                    op,
                    argument: Box::new(expr),
                    prefix: false,
                },
                self.span_from(start),
                op_pos,
            )
        } else {
            expr
        };
        Ok(result)
    }

    /// The operand of `++`/`--`, which must be a plain reference.
    ///
    /// Its own message, not the assignment one: the reference rejects `++1` and `1++` with
    /// `js_parse_error(s, "invalid increment/decrement operand")`.
    fn check_simple_target(&self, expr: &Expr) -> Result<()> {
        match &expr.unparen().kind {
            ExprKind::Ident(_) => Ok(()),
            ExprKind::Member {
                optional: false, ..
            } => Ok(()),
            _ => Err(self.error_at("invalid increment/decrement operand", expr.span)),
        }
    }

    fn parse_lhs_expression(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        let mut expr = if self.at_keyword(Keyword::New) {
            self.parse_new_expression()?
        } else {
            self.parse_primary()?
        };
        let mut saw_optional = false;
        loop {
            // The position of the punctuator that follows the base expression: a
            // failed `.`, `[` or `(` is blamed on that token, not on the base.
            let op_pos = self.current.span.start;
            match self.current.kind.clone() {
                TokenKind::Punct(Punct::Dot) => {
                    self.bump()?;
                    let property = self.parse_member_property()?;
                    expr = Expr::at_operator(
                        ExprKind::Member {
                            object: Box::new(expr),
                            property,
                            optional: false,
                        },
                        self.span_from(start),
                        op_pos,
                    );
                }
                TokenKind::Punct(Punct::QuestionDot) => {
                    saw_optional = true;
                    self.bump()?;
                    if self.at_punct(Punct::LParen) {
                        let arguments = self.parse_arguments()?;
                        expr = Expr::at_operator(
                            ExprKind::Call {
                                callee: Box::new(expr),
                                arguments,
                                optional: true,
                            },
                            self.span_from(start),
                            op_pos,
                        );
                    } else if self.at_punct(Punct::LBracket) {
                        self.bump()?;
                        let index = self.parse_expression(true)?;
                        self.expect_punct(Punct::RBracket)?;
                        expr = Expr::at_operator(
                            ExprKind::Member {
                                object: Box::new(expr),
                                property: MemberProp::Computed(Box::new(index)),
                                optional: true,
                            },
                            self.span_from(start),
                            op_pos,
                        );
                    } else {
                        let property = self.parse_member_property()?;
                        expr = Expr::at_operator(
                            ExprKind::Member {
                                object: Box::new(expr),
                                property,
                                optional: true,
                            },
                            self.span_from(start),
                            op_pos,
                        );
                    }
                }
                TokenKind::Punct(Punct::LBracket) => {
                    self.bump()?;
                    let index = self.parse_expression(true)?;
                    self.expect_punct(Punct::RBracket)?;
                    expr = Expr::at_operator(
                        ExprKind::Member {
                            object: Box::new(expr),
                            property: MemberProp::Computed(Box::new(index)),
                            optional: false,
                        },
                        self.span_from(start),
                        op_pos,
                    );
                }
                TokenKind::Punct(Punct::LParen) => {
                    let arguments = self.parse_arguments()?;
                    expr = Expr::at_operator(
                        ExprKind::Call {
                            callee: Box::new(expr),
                            arguments,
                            optional: false,
                        },
                        self.span_from(start),
                        op_pos,
                    );
                }
                TokenKind::Template(_) => {
                    if saw_optional || matches!(expr.unparen().kind, ExprKind::OptionalChain(_)) {
                        return Err(
                            self.error("tagged template cannot be used in an optional chain")
                        );
                    }
                    let quasi = self.parse_template()?;
                    expr = Expr::new(
                        ExprKind::TaggedTemplate {
                            tag: Box::new(expr),
                            quasi: Box::new(quasi),
                        },
                        self.span_from(start),
                    );
                }
                _ => break,
            }
        }
        if saw_optional {
            expr = Expr::new(
                ExprKind::OptionalChain(Box::new(expr)),
                self.span_from(start),
            );
        }
        Ok(expr)
    }

    fn parse_member_property(&mut self) -> Result<MemberProp> {
        if self.at_punct(Punct::Hash) {
            self.bump()?;
            let ident = self.expect_ident()?;
            if !self.private.iter().any(|env| env.contains(&ident.name)) {
                // The reference names the field and nothing else:
                // `js_parse_error(s, "undefined private field '#%s'")`.
                return Err(self.error_at(
                    format!("undefined private field '#{}'", ident.name),
                    ident.span,
                ));
            }
            return Ok(MemberProp::Private(ident));
        }
        match self.current.kind.clone() {
            TokenKind::Ident(name) => {
                let span = self.current.span;
                self.bump()?;
                Ok(MemberProp::Ident(Ident { name, span }))
            }
            TokenKind::Keyword(kw) => {
                let span = self.current.span;
                self.bump()?;
                Ok(MemberProp::Ident(Ident {
                    name: kw.as_str().to_string(),
                    span,
                }))
            }
            // After `.` or `?.` the reference says "expecting field name", not
            // "invalid property name" — it is `js_parse_error(s, "expecting field name")`
            // at the member-access site.
            _ => Err(self.error("expecting field name")),
        }
    }

    fn parse_new_expression(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        self.bump()?; // new
        if self.at_punct(Punct::Dot) {
            self.bump()?;
            if !self.at_ident("target") {
                return Err(self.error("expecting target"));
            }
            if !self.ctx.in_function {
                return Err(self.error("new.target only allowed within functions"));
            }
            self.bump()?;
            return Ok(Expr::new(ExprKind::NewTarget, self.span_from(start)));
        }
        let mut callee = if self.at_keyword(Keyword::New) {
            self.parse_new_expression()?
        } else {
            self.parse_primary()?
        };
        // Member accesses bind tighter than `new`, calls do not.
        loop {
            let op_pos = self.current.span.start;
            match self.current.kind.clone() {
                TokenKind::Punct(Punct::Dot) => {
                    self.bump()?;
                    let property = self.parse_member_property()?;
                    callee = Expr::at_operator(
                        ExprKind::Member {
                            object: Box::new(callee),
                            property,
                            optional: false,
                        },
                        self.span_from(start),
                        op_pos,
                    );
                }
                TokenKind::Punct(Punct::LBracket) => {
                    self.bump()?;
                    let index = self.parse_expression(true)?;
                    self.expect_punct(Punct::RBracket)?;
                    callee = Expr::at_operator(
                        ExprKind::Member {
                            object: Box::new(callee),
                            property: MemberProp::Computed(Box::new(index)),
                            optional: false,
                        },
                        self.span_from(start),
                        op_pos,
                    );
                }
                TokenKind::Punct(Punct::QuestionDot) => {
                    return Err(self.error("new keyword cannot be used with an optional chain"));
                }
                _ => break,
            }
        }
        if matches!(callee.unparen().kind, ExprKind::OptionalChain(_)) {
            return Err(self.error_at("cannot use 'new' with an optional chain", callee.span));
        }
        let arguments = if self.at_punct(Punct::LParen) {
            // `new C(...)`: a failure in the constructor call is blamed on the `(`.
            let op_pos = self.current.span.start;
            let arguments = Some(self.parse_arguments()?);
            return Ok(Expr::at_operator(
                ExprKind::New {
                    callee: Box::new(callee),
                    arguments,
                },
                self.span_from(start),
                op_pos,
            ));
        } else {
            None
        };
        // `new C` with no argument list: the reference emits its position at the token
        // *after* the constructor (`emit_source_pos(s, s->token.ptr)` for "new operator on
        // an object"), not at the `new`.
        Ok(Expr::at_operator(
            ExprKind::New {
                callee: Box::new(callee),
                arguments,
            },
            self.span_from(start),
            self.current.span.start,
        ))
    }

    fn parse_arguments(&mut self) -> Result<Vec<Expr>> {
        self.expect_punct(Punct::LParen)?;
        let mut arguments = Vec::new();
        while !self.at_punct(Punct::RParen) {
            if self.at_punct(Punct::Ellipsis) {
                let start = self.current.span.start;
                self.bump()?;
                let argument = self.parse_assignment(true)?;
                arguments.push(Expr::new(
                    ExprKind::Spread(Box::new(argument)),
                    self.span_from(start),
                ));
            } else {
                arguments.push(self.parse_assignment(true)?);
            }
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RParen)?;
        Ok(arguments)
    }

    /// Primary expressions.
    ///
    /// The rarely used forms are dispatched to [`Parser::parse_primary_complex`]
    /// so that this function — which sits on every nesting level — keeps a small
    /// stack frame. That directly raises how deeply a program may nest before
    /// the stack limit is hit (debug builds get ~4x more depth out of it).
    fn parse_primary(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        match self.current.kind.clone() {
            TokenKind::Number(NumberLiteral::Int(value)) => {
                self.bump()?;
                Ok(Expr::new(
                    ExprKind::Number(value as f64),
                    self.span_from(start),
                ))
            }
            TokenKind::Number(NumberLiteral::Float(value)) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::Number(value), self.span_from(start)))
            }
            TokenKind::String(value) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::String(value), self.span_from(start)))
            }
            TokenKind::Ident(name) if name == "async" => {
                let peek = self.peek()?;
                if matches!(peek.kind, TokenKind::Keyword(Keyword::Function))
                    && !peek.newline_before
                {
                    self.bump()?; // async
                    let func = self.parse_function(FunctionForm::Expression, true)?;
                    return Ok(Expr::new(
                        ExprKind::Function(Box::new(func)),
                        self.span_from(start),
                    ));
                }
                let span = self.current.span;
                self.bump()?;
                Ok(Expr::new(ExprKind::Ident(Ident { name, span }), span))
            }
            TokenKind::Ident(name) => {
                let span = self.current.span;
                self.check_identifier(&name, span)?;
                self.bump()?;
                Ok(Expr::new(ExprKind::Ident(Ident { name, span }), span))
            }
            TokenKind::Keyword(Keyword::This) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::This, self.span_from(start)))
            }
            TokenKind::Keyword(Keyword::Null) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::Null, self.span_from(start)))
            }
            TokenKind::Keyword(Keyword::True) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::Bool(true), self.span_from(start)))
            }
            TokenKind::Keyword(Keyword::False) => {
                self.bump()?;
                Ok(Expr::new(ExprKind::Bool(false), self.span_from(start)))
            }
            TokenKind::Punct(Punct::LParen) => {
                self.bump()?;
                let inner = self.parse_expression(true)?;
                self.expect_punct(Punct::RParen)?;
                Ok(Expr::new(
                    ExprKind::Paren(Box::new(inner)),
                    self.span_from(start),
                ))
            }
            TokenKind::Punct(Punct::Slash) | TokenKind::Punct(Punct::SlashEq) => {
                self.parse_regex_literal(start)
            }
            _ => self.parse_primary_complex(start),
        }
    }

    /// The primary expressions that need a lot of code: literals with structure,
    /// functions, classes, `import`, `super`, `#x in y`.
    #[inline(never)]
    fn parse_primary_complex(&mut self, start: Position) -> Result<Expr> {
        match self.current.kind.clone() {
            TokenKind::BigInt(literal) => {
                self.bump()?;
                Ok(Expr::new(
                    ExprKind::BigInt {
                        digits: literal.digits,
                        radix: literal.radix,
                    },
                    self.span_from(start),
                ))
            }
            TokenKind::Template(_) => {
                let template = self.parse_template()?;
                Ok(Expr::new(
                    ExprKind::Template(Box::new(template)),
                    self.span_from(start),
                ))
            }
            TokenKind::Keyword(Keyword::Function) => {
                let func = self.parse_function(FunctionForm::Expression, false)?;
                Ok(Expr::new(
                    ExprKind::Function(Box::new(func)),
                    self.span_from(start),
                ))
            }
            TokenKind::Keyword(Keyword::Class) => {
                let class = self.parse_class(true)?;
                Ok(Expr::new(
                    ExprKind::Class(Box::new(class)),
                    self.span_from(start),
                ))
            }
            TokenKind::Keyword(Keyword::Import) => self.parse_import_expression(),
            TokenKind::Keyword(Keyword::Super) => self.parse_super(),
            TokenKind::Punct(Punct::LBracket) => self.parse_array_literal(),
            TokenKind::Punct(Punct::LBrace) => self.parse_object_literal(),
            TokenKind::Punct(Punct::Hash) => self.parse_private_in(start),
            _ => Err(self.unexpected()),
        }
    }

    /// `super` or `super(...)`.
    #[inline(never)]
    fn parse_super(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        self.bump()?;
        // Every error below is reported at the token *after* `super`, because the reference
        // consumes the keyword before it looks at what follows (`case TOK_SUPER:
        // next_token(s); …`) and `js_parse_error` always points at the current token. So
        // `super.x` reports the `.`, `super()` the `(`, and a bare `super` the token that
        // ends the expression.
        let after = self.current.span;
        if self.at_punct(Punct::LParen) {
            if !self.ctx.in_derived_constructor {
                return Err(self.error_at(
                    "super() is only valid in a derived class constructor",
                    after,
                ));
            }
            // A `super()` call is positioned on its `(` like any other call: the reference
            // reaches the same `emit_source_pos(op_token_ptr)` on the way to `apply`.
            let arguments = self.parse_arguments()?;
            return Ok(Expr::at_operator(
                ExprKind::SuperCall(arguments),
                self.span_from(start),
                after.start,
            ));
        }
        if !self.ctx.in_method {
            // Three messages, told apart by what follows `super`:
            //   `super` alone        -> invalid use of 'super'
            //   `super.x`, `super[x]` -> 'super' is only valid in a method
            //   `super()`            -> handled above
            // A bare `super` never reaches the member-access code, so the reference names
            // it on its own.
            let message = if self.at_punct(Punct::Dot) || self.at_punct(Punct::LBracket) {
                "'super' is only valid in a method"
            } else {
                "invalid use of 'super'"
            };
            return Err(self.error_at(message, after));
        }
        Ok(Expr::new(ExprKind::SuperProp, self.span_from(start)))
    }

    /// `#name in object`.
    #[inline(never)]
    fn parse_private_in(&mut self, start: Position) -> Result<Expr> {
        self.bump()?;
        let ident = self.expect_ident()?;
        if !self.private.iter().any(|env| env.contains(&ident.name)) {
            return Err(self.error_at(
                format!(
                    "private field '#{}' must be declared in an enclosing class",
                    ident.name
                ),
                ident.span,
            ));
        }
        if !self.at_keyword(Keyword::In) {
            return Err(self.error("expecting 'in' after a private name"));
        }
        self.bump()?;
        let object = self.parse_unary()?;
        Ok(Expr::new(
            ExprKind::PrivateIn {
                name: ident,
                object: Box::new(object),
            },
            self.span_from(start),
        ))
    }

    /// A regular expression literal: the lexer produced `/` (or `/=`), so the
    /// token has to be re-scanned from that position.
    #[inline(never)]
    fn parse_regex_literal(&mut self, start: Position) -> Result<Expr> {
        let token = self.lexer.re_scan_regex(self.current.span.start)?;
        self.current = token;
        let TokenKind::RegExp { pattern, flags } = self.current.kind.clone() else {
            unreachable!("re_scan_regex returns a RegExp token")
        };
        self.bump()?;
        Ok(Expr::new(
            ExprKind::RegExp { pattern, flags },
            self.span_from(start),
        ))
    }

    fn parse_import_expression(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        self.bump()?; // import
        if self.at_punct(Punct::Dot) {
            if !self.module {
                return Err(self.error_at(
                    "import.meta only valid in module code",
                    self.span_from(start),
                ));
            }
            self.bump()?;
            if !self.at_ident("meta") {
                return Err(self.error("expecting 'meta' after 'import.'"));
            }
            self.bump()?;
            return Ok(Expr::new(ExprKind::ImportMeta, self.span_from(start)));
        }
        if self.at_punct(Punct::LParen) {
            self.bump()?;
            let specifier = self.parse_assignment(true)?;
            if self.at_punct(Punct::Comma) {
                return Err(self.error("import() takes exactly one argument"));
            }
            self.expect_punct(Punct::RParen)?;
            return Ok(Expr::new(
                ExprKind::ImportCall(Box::new(specifier)),
                self.span_from(start),
            ));
        }
        Err(self.unexpected())
    }

    fn parse_array_literal(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        self.expect_punct(Punct::LBracket)?;
        let mut elements = Vec::new();
        while !self.at_punct(Punct::RBracket) {
            if self.at_punct(Punct::Comma) {
                self.bump()?;
                elements.push(None);
                continue;
            }
            if self.at_punct(Punct::Ellipsis) {
                let spread_start = self.current.span.start;
                self.bump()?;
                let argument = self.parse_assignment(true)?;
                elements.push(Some(ArrayElement::Spread(Box::new(Expr::new(
                    ExprKind::Spread(Box::new(argument)),
                    self.span_from(spread_start),
                )))));
            } else {
                let expr = self.parse_assignment(true)?;
                elements.push(Some(ArrayElement::Expr(Box::new(expr))));
            }
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RBracket)?;
        Ok(Expr::new(
            ExprKind::Array { elements },
            self.span_from(start),
        ))
    }

    fn parse_object_literal(&mut self) -> Result<Expr> {
        let start = self.current.span.start;
        self.expect_punct(Punct::LBrace)?;
        let mut properties: Vec<ObjectProperty> = Vec::new();
        let mut proto_count = 0;
        while !self.at_punct(Punct::RBrace) {
            let property_start = self.current.span.start;
            if self.at_punct(Punct::Ellipsis) {
                self.bump()?;
                let argument = self.parse_assignment(true)?;
                properties.push(ObjectProperty::Spread(Box::new(argument)));
            } else {
                let mut is_async = false;
                let mut is_generator = false;
                let mut accessor: Option<MethodKind> = None;
                if self.at_ident("async") {
                    let peek = self.peek()?;
                    let is_modifier = !matches!(
                        peek.kind,
                        TokenKind::Punct(Punct::LParen)
                            | TokenKind::Punct(Punct::Colon)
                            | TokenKind::Punct(Punct::Comma)
                            | TokenKind::Punct(Punct::RBrace)
                            | TokenKind::Punct(Punct::Eq)
                    ) && !peek.newline_before;
                    if is_modifier {
                        self.bump()?;
                        is_async = true;
                    }
                }
                if self.at_punct(Punct::Star) {
                    self.bump()?;
                    is_generator = true;
                }
                if accessor.is_none() && (self.at_ident("get") || self.at_ident("set")) {
                    let is_get = self.at_ident("get");
                    let peek = self.peek()?;
                    if is_property_key_start(&peek.kind) {
                        self.bump()?;
                        accessor = Some(if is_get {
                            MethodKind::Get
                        } else {
                            MethodKind::Set
                        });
                    }
                }
                let key = self.parse_property_key()?;
                let is_computed = matches!(key, PropName::Computed(_));
                if self.at_punct(Punct::LParen) {
                    let saved = self.ctx;
                    self.ctx = FnCtx {
                        in_function: true,
                        in_generator: is_generator,
                        in_async: is_async,
                        in_method: true,
                        in_derived_constructor: false,
                    };
                    let saved_top = self.top_level;
                    self.top_level = false;
                    let func_result = (|| -> Result<Function> {
                        let params = self.parse_formal_parameters()?;
                        let (params, params_span, body, strict) =
                            self.parse_function_body(params, true)?;
                        Ok(Function {
                            name: None,
                            params,
                            body,
                            is_async,
                            is_generator,
                            strict,
                            is_arrow: false,
                            span: self.span_from(property_start),
                            params_span,
                        })
                    })();
                    self.ctx = saved;
                    self.top_level = saved_top;
                    let func = func_result?;
                    let kind = match accessor {
                        Some(kind) => {
                            if is_generator || is_async {
                                return Err(self.error_at(
                                    "accessors may not be generators or async",
                                    func.span,
                                ));
                            }
                            if kind == MethodKind::Get && !func.params.is_empty() {
                                return Err(self.error_at(
                                    "invalid number of arguments for getter or setter",
                                    func.span,
                                ));
                            }
                            if kind == MethodKind::Set && func.params.len() != 1 {
                                return Err(self.error_at(
                                    "invalid number of arguments for getter or setter",
                                    func.span,
                                ));
                            }
                            match kind {
                                MethodKind::Get => PropertyKind::Get,
                                MethodKind::Set => PropertyKind::Set,
                                MethodKind::Method | MethodKind::Constructor => {
                                    PropertyKind::Method
                                }
                            }
                        }
                        None => PropertyKind::Method,
                    };
                    let span = self.span_from(property_start);
                    properties.push(ObjectProperty::Property {
                        key,
                        value: Box::new(Expr::new(ExprKind::Function(Box::new(func)), span)),
                        kind,
                        shorthand: false,
                        span,
                    });
                } else if self.eat_punct(Punct::Colon)? {
                    let value = self.parse_assignment(true)?;
                    if !is_computed {
                        if let Some(PropKeyRef::Str("__proto__")) = key.static_name() {
                            proto_count += 1;
                            if proto_count > 1 {
                                return Err(self.error_at(
                                    "duplicate __proto__ property name",
                                    self.span_from(property_start),
                                ));
                            }
                        }
                    }
                    properties.push(ObjectProperty::Property {
                        key,
                        value: Box::new(value),
                        kind: PropertyKind::Value,
                        shorthand: false,
                        span: self.span_from(property_start),
                    });
                } else {
                    // Shorthand `{a}` / `{a = 1}` (the latter only inside a
                    // destructuring pattern).
                    let PropName::Literal(PropKey::Str(name)) = &key else {
                        return Err(self.error("expecting ':' after a property name"));
                    };
                    let ident_span = self.span_from(property_start);
                    let ident = Ident {
                        name: name.clone(),
                        span: ident_span,
                    };
                    self.check_identifier(&ident.name, ident.span)?;
                    let mut value = Expr::new(ExprKind::Ident(ident.clone()), ident_span);
                    if self.at_punct(Punct::Eq) {
                        // CoverInitializedName: only legal once the object turns
                        // out to be a destructuring pattern.
                        self.cover_inits.push(ident_span);
                        self.bump()?;
                        let default = self.parse_assignment(true)?;
                        let target = Pattern::new(PatternKind::Ident(ident), ident_span);
                        value = Expr::new(
                            ExprKind::Assign {
                                op: AssignOp::Assign,
                                target: Box::new(target),
                                value: Box::new(default),
                            },
                            ident_span,
                        );
                    }
                    properties.push(ObjectProperty::Property {
                        key,
                        value: Box::new(value),
                        kind: PropertyKind::Value,
                        shorthand: true,
                        span: ident_span,
                    });
                }
            }
            if !self.eat_punct(Punct::Comma)? {
                break;
            }
        }
        self.expect_punct(Punct::RBrace)?;
        Ok(Expr::new(
            ExprKind::Object(properties),
            self.span_from(start),
        ))
    }

    fn parse_template(&mut self) -> Result<TemplateLiteral> {
        let mut quasis = Vec::new();
        let mut expressions = Vec::new();
        loop {
            let (cooked, raw, kind) = match self.current.kind.clone() {
                TokenKind::Template(chunk) => (chunk.cooked, chunk.raw, chunk.kind),
                _ => return Err(self.error("expecting a template literal")),
            };
            let tail = matches!(kind, TemplateKind::NoSubstitution | TemplateKind::Tail);
            quasis.push(TemplateChunk { cooked, raw, tail });
            if tail {
                self.bump()?;
                break;
            }
            self.bump()?; // move to the first token of the substitution
            let expr = self.parse_expression(true)?;
            expressions.push(expr);
            if !self.at_punct(Punct::RBrace) {
                return Err(self.error("expecting '}' in a template substitution"));
            }
            let brace = self.current.span.start;
            let token = self.lexer.re_scan_template_continue(brace)?;
            self.current = token;
        }
        Ok(TemplateLiteral {
            quasis,
            expressions,
        })
    }
}

/// Which kind of function is being parsed (affects whether a name is required).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionForm {
    Declaration,
    Expression,
}

/// Names of the parameters that participate in declaration-conflict checks.
///
/// Engines only check *simple* (identifier) parameters against the body's
/// lexical declarations: `function f(a) { let a; }` is an early error, but
/// `function f({a}) { let a; }` is not (QuickJS behaves the same way, so this
/// deliberately follows the reference rather than ES2025 §10.2.1).
/// Whether this parameter is a bare identifier, as opposed to a pattern.
fn is_plain_identifier(param: &Pattern) -> bool {
    matches!(param.unparen_kind(), PatternKind::Ident(_))
}

/// Every name `param` binds, in binding order, tagged with whether it sits inside a
/// destructuring pattern.
///
/// A rest parameter is a pattern only when its target is: `...a` binds like a plain name,
/// while `...[a]` does not.
fn parameter_bindings(param: &Pattern) -> Vec<(String, Span, bool)> {
    fn walk(pattern: &Pattern, in_pattern: bool, out: &mut Vec<(String, Span, bool)>) {
        match &pattern.kind {
            PatternKind::Ident(ident) => out.push((ident.name.clone(), ident.span, in_pattern)),
            PatternKind::Assign { target, default } => {
                walk(target, in_pattern, out);
                // A default's own expression can bind nothing, so only the target is walked.
                let _ = default;
            }
            PatternKind::Rest(target) => walk(target, !is_plain_identifier(target), out),
            PatternKind::Array { .. } | PatternKind::Object { .. } => {
                let mut names = Vec::new();
                pattern.bound_names(&mut names);
                for name in names {
                    out.push((name.name, name.span, true));
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    let in_pattern = !is_plain_identifier(param);
    walk(param, in_pattern, &mut out);
    // A pattern's own walk above already flagged its names; a plain identifier or a
    // defaulted one goes through `walk` directly.
    if out.is_empty() {
        let mut names = Vec::new();
        param.bound_names(&mut names);
        out.extend(names.into_iter().map(|name| (name.name, name.span, false)));
    }
    out
}

/// Whether any binding in `pattern` carries a default value.
///
/// This is the closest stand-in we have for the reference's lookahead: it scans the
/// parenthesised parameter text for an `=` (`SKIP_HAS_ASSIGNMENT`) *before* parsing it, and
/// that flag decides which of the two duplicate-name messages a later conflict gets.
fn pattern_has_default(pattern: &Pattern) -> bool {
    let element = |element: &Option<PatternElement>| {
        element.as_ref().is_some_and(|element| match element {
            PatternElement::Pattern(inner) => pattern_has_default(inner),
            PatternElement::Property { value, .. } => pattern_has_default(value),
        })
    };
    let rest = |rest: &Option<Box<RestElement>>| {
        rest.as_ref().is_some_and(|rest| match rest.as_ref() {
            RestElement::Pattern(inner) => pattern_has_default(inner),
            RestElement::Member(_) => false,
        })
    };
    match &pattern.kind {
        PatternKind::Assign { .. } => true,
        PatternKind::Array {
            elements,
            rest: tail,
        } => elements.iter().any(element) || rest(tail),
        PatternKind::Object {
            properties,
            rest: tail,
        } => properties.iter().any(element) || rest(tail),
        PatternKind::Rest(target) => pattern_has_default(target),
        _ => false,
    }
}

fn simple_parameter_names(params: &[Pattern]) -> Vec<String> {
    params
        .iter()
        .filter_map(|param| match &param.kind {
            PatternKind::Ident(id) => Some(id.name.clone()),
            _ => None,
        })
        .collect()
}

/// Collect the names a top-level module item declares, for the exported-name
/// check.
fn collect_module_binding(stmt: &Stmt, out: &mut Vec<String>) {
    match &stmt.kind {
        StmtKind::VarDecl(decl) => {
            // A `using` declaration is a lexical binding of the module's own scope but not
            // a *module variable*, so it cannot be exported: the reference excludes it from
            // `add_global_var` and `export {a}` of one reports
            // `exported variable 'a' does not exist`. Leaving it in this list would make
            // that export silently succeed.
            if decl.kind.is_using() {
                return;
            }
            for declarator in &decl.declarators {
                let mut names = Vec::new();
                declarator.target.bound_names(&mut names);
                out.extend(names.into_iter().map(|n| n.name));
            }
        }
        StmtKind::FuncDecl(func) => {
            if let Some(name) = &func.name {
                out.push(name.name.clone());
            }
        }
        StmtKind::ClassDecl(class) => {
            if let Some(name) = &class.name {
                out.push(name.name.clone());
            }
        }
        StmtKind::Import(decl) => {
            for specifier in &decl.specifiers {
                match specifier {
                    ImportSpecifier::Default(id) => out.push(id.name.clone()),
                    ImportSpecifier::Namespace(id) => out.push(id.name.clone()),
                    ImportSpecifier::Named { local, .. } => out.push(local.name.clone()),
                }
            }
        }
        StmtKind::Export(decl) => match &decl.kind {
            ExportKind::Declaration(inner) => collect_module_binding(inner, out),
            ExportKind::Default(expr) => match &expr.unparen().kind {
                ExprKind::Function(func) => {
                    if let Some(name) = &func.name {
                        out.push(name.name.clone());
                    }
                }
                ExprKind::Class(class) => {
                    if let Some(name) = &class.name {
                        out.push(name.name.clone());
                    }
                }
                _ => {}
            },
            _ => {}
        },
        _ => {}
    }
}

/// Whether `inner` is fully inside `outer` (byte offset comparison only).
fn span_contains(outer: Span, inner: Span) -> bool {
    inner.start.offset >= outer.start.offset && inner.end.offset <= outer.end.offset
}

fn is_property_key_start(kind: &TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Ident(_)
            | TokenKind::Keyword(_)
            | TokenKind::String(_)
            | TokenKind::Number(_)
            | TokenKind::Punct(Punct::LBracket)
            | TokenKind::Punct(Punct::Hash)
    )
}

/// A binary operator with its precedence, as consumed by [`Parser::parse_binary`].
#[derive(Debug, Clone, Copy)]
enum BinOp {
    Binary(BinaryOp),
    Logical(LogicalOp),
}

impl BinOp {
    fn is_right_associative(self) -> bool {
        matches!(self, BinOp::Binary(BinaryOp::Pow))
    }
}

fn binary_operator(kind: &TokenKind, allow_in: bool) -> Option<(u8, BinOp)> {
    use BinaryOp as B;
    use LogicalOp as L;
    let result = match kind {
        TokenKind::Punct(Punct::QuestionQuestion) => (1, BinOp::Logical(L::Coalesce)),
        TokenKind::Punct(Punct::PipePipe) => (2, BinOp::Logical(L::Or)),
        TokenKind::Punct(Punct::AmpAmp) => (3, BinOp::Logical(L::And)),
        TokenKind::Punct(Punct::Pipe) => (4, BinOp::Binary(B::BitOr)),
        TokenKind::Punct(Punct::Caret) => (5, BinOp::Binary(B::BitXor)),
        TokenKind::Punct(Punct::Amp) => (6, BinOp::Binary(B::BitAnd)),
        TokenKind::Punct(Punct::EqEq) => (7, BinOp::Binary(B::Eq)),
        TokenKind::Punct(Punct::NotEq) => (7, BinOp::Binary(B::NotEq)),
        TokenKind::Punct(Punct::EqEqEq) => (7, BinOp::Binary(B::StrictEq)),
        TokenKind::Punct(Punct::NotEqEq) => (7, BinOp::Binary(B::StrictNotEq)),
        TokenKind::Punct(Punct::Lt) => (8, BinOp::Binary(B::Lt)),
        TokenKind::Punct(Punct::LtEq) => (8, BinOp::Binary(B::LtEq)),
        TokenKind::Punct(Punct::Gt) => (8, BinOp::Binary(B::Gt)),
        TokenKind::Punct(Punct::GtEq) => (8, BinOp::Binary(B::GtEq)),
        TokenKind::Keyword(Keyword::Instanceof) => (8, BinOp::Binary(B::Instanceof)),
        TokenKind::Keyword(Keyword::In) if allow_in => (8, BinOp::Binary(B::In)),
        TokenKind::Punct(Punct::Shl) => (9, BinOp::Binary(B::Shl)),
        TokenKind::Punct(Punct::Shr) => (9, BinOp::Binary(B::Shr)),
        TokenKind::Punct(Punct::UShr) => (9, BinOp::Binary(B::UShr)),
        TokenKind::Punct(Punct::Plus) => (10, BinOp::Binary(B::Add)),
        TokenKind::Punct(Punct::Minus) => (10, BinOp::Binary(B::Sub)),
        TokenKind::Punct(Punct::Star) => (11, BinOp::Binary(B::Mul)),
        TokenKind::Punct(Punct::Slash) => (11, BinOp::Binary(B::Div)),
        TokenKind::Punct(Punct::Percent) => (11, BinOp::Binary(B::Mod)),
        TokenKind::Punct(Punct::StarStar) => (12, BinOp::Binary(B::Pow)),
        _ => return None,
    };
    Some(result)
}

fn build_binary(op: BinOp, left: Expr, right: Expr, span: Span, op_pos: Position) -> Result<Expr> {
    match op {
        BinOp::Binary(op) => Ok(Expr::at_operator(
            ExprKind::Binary {
                op,
                left: Box::new(left),
                right: Box::new(right),
            },
            span,
            op_pos,
        )),
        BinOp::Logical(op) => {
            // `??` may not be mixed with `&&`/`||` without parentheses.
            // A parenthesised operand is visibly a different production, so the
            // check must look at the raw node, not through `Paren`.
            let mixes = |expr: &Expr, op: LogicalOp| {
                matches!(&expr.kind, ExprKind::Logical { op: inner, .. }
                    if (op == LogicalOp::Coalesce) != (*inner == LogicalOp::Coalesce))
            };
            if mixes(&left, op) || mixes(&right, op) {
                return Err(SyntaxError::new(
                    // The reference has no quotes in this one; the parenthesised form is the only
                    // way to write it, which the message states rather than implies.
                    "cannot mix ?? with && or ||",
                    span,
                ));
            }
            Ok(Expr::at_operator(
                ExprKind::Logical {
                    op,
                    left: Box::new(left),
                    right: Box::new(right),
                },
                span,
                op_pos,
            ))
        }
    }
}

/// Rebuild an expression from a pattern (used when a pattern has to go back
/// through [`Parser::expr_to_pattern`], e.g. `{a: [b = 1]}`).
fn pattern_to_expr(pattern: Pattern, span: Span) -> Expr {
    match pattern.kind {
        PatternKind::Ident(ident) => Expr::new(ExprKind::Ident(ident), span),
        PatternKind::Assign { target, default } => Expr::new(
            ExprKind::Assign {
                op: AssignOp::Assign,
                target,
                value: default,
            },
            span,
        ),
        PatternKind::Array { elements, rest } => {
            let mut out = Vec::new();
            for element in elements {
                match element {
                    Some(PatternElement::Pattern(pattern)) => {
                        let span = pattern.span;
                        out.push(Some(ArrayElement::Expr(Box::new(pattern_to_expr(
                            pattern, span,
                        )))));
                    }
                    Some(PatternElement::Property { .. }) | None => out.push(None),
                }
            }
            if let Some(rest) = rest {
                let RestElement::Pattern(pattern) = *rest else {
                    return Expr::new(ExprKind::Null, span);
                };
                let span = pattern.span;
                out.push(Some(ArrayElement::Spread(Box::new(pattern_to_expr(
                    pattern, span,
                )))));
            }
            Expr::new(ExprKind::Array { elements: out }, span)
        }
        _ => Expr::new(ExprKind::Null, span),
    }
}

impl Pattern {
    /// Pattern kind, ignoring `Paren` wrappers (patterns never have any, but
    /// this keeps call sites uniform with [`Expr::unparen`]).
    fn unparen_kind(&self) -> &PatternKind {
        &self.kind
    }
}

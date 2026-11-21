//! Structural parser tests: the accept/reject differential lives in
//! `parse_oracle.rs`; this file pins down the *shape* of the tree (precedence,
//! associativity, ASI, desugaring) so the compiler can rely on it.

use js_parse::ast::*;
use js_parse::{parse_expression, parse_module, parse_script};

fn stmt(src: &str) -> Stmt {
    let mut program = parse_script(src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
    assert_eq!(
        program.body.len(),
        1,
        "{src:?} should be a single statement"
    );
    program.body.remove(0)
}

fn expr(src: &str) -> Expr {
    parse_expression(src).unwrap_or_else(|e| panic!("{src:?}: {e}"))
}

fn expr_stmt(src: &str) -> Expr {
    match stmt(src).kind {
        StmtKind::Expr(expr) => *expr,
        other => panic!("{src:?}: expected an expression statement, got {other:?}"),
    }
}

/// Render an expression in a fully parenthesised form, so precedence and
/// associativity can be asserted directly.
fn render(e: &Expr) -> String {
    match &e.kind {
        ExprKind::Ident(id) => id.name.clone(),
        ExprKind::Null => "null".to_string(),
        ExprKind::Bool(b) => b.to_string(),
        ExprKind::Number(n) => {
            if n.fract() == 0.0 && n.abs() < 1e15 {
                format!("{}", *n as i64)
            } else {
                format!("{n}")
            }
        }
        ExprKind::String(s) => format!("\"{s}\""),
        ExprKind::This => "this".to_string(),
        ExprKind::Paren(inner) => format!("({})", render(inner)),
        ExprKind::Sequence(items) => {
            format!(
                "({})",
                items.iter().map(render).collect::<Vec<_>>().join(", ")
            )
        }
        ExprKind::Binary { op, left, right } => {
            format!("({} {} {})", render(left), op.as_str(), render(right))
        }
        ExprKind::Logical { op, left, right } => {
            format!("({} {} {})", render(left), op.as_str(), render(right))
        }
        ExprKind::Unary { op, argument } => {
            let sep = if op.as_str().chars().all(|c| c.is_alphabetic()) {
                " "
            } else {
                ""
            };
            format!("({}{sep}{})", op.as_str(), render(argument))
        }
        ExprKind::Update {
            op,
            argument,
            prefix,
        } => {
            if *prefix {
                format!("({}{})", op.as_str(), render(argument))
            } else {
                format!("({}{})", render(argument), op.as_str())
            }
        }
        ExprKind::Conditional { test, cons, alt } => {
            format!("({} ? {} : {})", render(test), render(cons), render(alt))
        }
        ExprKind::Assign { op, target, value } => {
            format!(
                "({} {} {})",
                render_pattern(target),
                op.as_str(),
                render(value)
            )
        }
        ExprKind::Member {
            object,
            property,
            optional,
        } => {
            let dot = if *optional { "?." } else { "." };
            match property {
                MemberProp::Ident(id) => format!("{}{}{}", render(object), dot, id.name),
                MemberProp::Computed(index) => format!("{}[{}]", render(object), render(index)),
                MemberProp::Private(id) => format!("{}#{}", render(object), id.name),
            }
        }
        ExprKind::Call {
            callee,
            arguments,
            optional,
        } => {
            let args: Vec<String> = arguments.iter().map(render).collect();
            let marker = if *optional { "?." } else { "" };
            format!("{}{}({})", render(callee), marker, args.join(", "))
        }
        ExprKind::New { callee, arguments } => match arguments {
            Some(args) => {
                let args: Vec<String> = args.iter().map(render).collect();
                format!("(new {}({}))", render(callee), args.join(", "))
            }
            None => format!("(new {})", render(callee)),
        },
        ExprKind::Array { elements } => {
            let items: Vec<String> = elements
                .iter()
                .map(|element| match element {
                    None => String::new(),
                    Some(ArrayElement::Expr(e)) => render(e),
                    Some(ArrayElement::Spread(e)) => format!("...{}", render(e)),
                })
                .collect();
            format!("[{}]", items.join(", "))
        }
        ExprKind::Object(properties) => {
            let items: Vec<String> = properties
                .iter()
                .map(|property| match property {
                    ObjectProperty::Spread(e) => format!("...{}", render(e)),
                    ObjectProperty::Property {
                        key,
                        value,
                        shorthand,
                        ..
                    } => {
                        if *shorthand {
                            render(value)
                        } else {
                            format!("{}: {}", render_key(key), render(value))
                        }
                    }
                })
                .collect();
            format!("{{{}}}", items.join(", "))
        }
        ExprKind::Arrow(func) => {
            let params: Vec<String> = func.params.iter().map(render_pattern).collect();
            let body = match &func.body {
                FunctionBody::Expr(e) => render(e),
                FunctionBody::Block(_) => "{…}".to_string(),
            };
            format!("(({}) => {})", params.join(", "), body)
        }
        ExprKind::Spread(inner) => format!("...{}", render(inner)),
        ExprKind::Await(inner) => format!("(await {})", render(inner)),
        ExprKind::Yield { argument, delegate } => {
            let star = if *delegate { "*" } else { "" };
            match argument {
                Some(inner) => format!("(yield{star} {})", render(inner)),
                None => format!("(yield{star})"),
            }
        }
        ExprKind::OptionalChain(inner) => format!("opt({})", render(inner)),
        ExprKind::Template(t) => {
            let mut out = String::from("`");
            for (index, quasi) in t.quasis.iter().enumerate() {
                out.push_str(quasi.cooked.as_deref().unwrap_or("?"));
                if let Some(expression) = t.expressions.get(index) {
                    out.push_str(&format!("${{{}}}", render(expression)));
                }
            }
            out.push('`');
            out
        }
        ExprKind::TaggedTemplate { tag, quasi } => {
            format!(
                "{}{}",
                render(tag),
                render(&Expr::new(ExprKind::Template(quasi.clone()), e.span))
            )
        }
        ExprKind::RegExp { pattern, flags } => format!("/{pattern}/{flags}"),
        ExprKind::BigInt { digits, radix } => format!("{digits}n{radix}"),
        ExprKind::Function(f) => format!(
            "(function {})",
            f.name.as_ref().map(|n| n.name.as_str()).unwrap_or("")
        ),
        ExprKind::Class(_) => "(class)".to_string(),
        ExprKind::NewTarget => "new.target".to_string(),
        ExprKind::ImportMeta => "import.meta".to_string(),
        ExprKind::ImportCall(spec) => format!("import({})", render(spec)),
        ExprKind::SuperCall(args) => {
            let args: Vec<String> = args.iter().map(render).collect();
            format!("super({})", args.join(", "))
        }
        ExprKind::SuperProp => "super".to_string(),
        ExprKind::PrivateIn { name, object } => {
            format!("(#{} in {})", name.name, render(object))
        }
    }
}

fn render_key(key: &PropName) -> String {
    match key {
        PropName::Literal(PropKey::Str(s)) => s.clone(),
        PropName::Literal(PropKey::Num(n)) => format!("{n}"),
        PropName::Computed(e) => format!("[{}]", render(e)),
        PropName::Private(id) => format!("#{}", id.name),
    }
}

fn render_pattern(pattern: &Pattern) -> String {
    match &pattern.kind {
        PatternKind::Ident(id) => id.name.clone(),
        PatternKind::Assign { target, default } => {
            format!("{} = {}", render_pattern(target), render(default))
        }
        PatternKind::Array { elements, rest } => {
            let mut items: Vec<String> = elements
                .iter()
                .map(|element| match element {
                    None => String::new(),
                    Some(PatternElement::Pattern(p)) => render_pattern(p),
                    Some(PatternElement::Property { key, value, .. }) => {
                        format!("{}: {}", render_key(key), render_pattern(value))
                    }
                })
                .collect();
            if let Some(rest) = rest {
                match &**rest {
                    RestElement::Pattern(p) => items.push(format!("...{}", render_pattern(p))),
                    RestElement::Member(m) => {
                        items.push(format!("...{}", render(member_to_expr(m))))
                    }
                }
            }
            format!("[{}]", items.join(", "))
        }
        PatternKind::Object { properties, rest } => {
            let mut items: Vec<String> = properties
                .iter()
                .map(|property| match property {
                    None => String::new(),
                    Some(PatternElement::Property { key, value, .. }) => {
                        format!("{}: {}", render_key(key), render_pattern(value))
                    }
                    Some(PatternElement::Pattern(p)) => render_pattern(p),
                })
                .collect();
            if let Some(rest) = rest {
                match &**rest {
                    RestElement::Pattern(p) => items.push(format!("...{}", render_pattern(p))),
                    RestElement::Member(m) => {
                        items.push(format!("...{}", render(member_to_expr(m))))
                    }
                }
            }
            format!("{{{}}}", items.join(", "))
        }
        PatternKind::Member(member) => render(member_to_expr(member)),
        PatternKind::Rest(inner) => format!("...{}", render_pattern(inner)),
        PatternKind::Invalid(inner) => format!("invalid({})", render(inner)),
    }
}

fn member_to_expr(member: &MemberPattern) -> &Expr {
    // Member patterns only wrap the object; rendering the object plus the
    // property text is enough for assertions.
    &member.object
}

// -- precedence and associativity -----------------------------------------

#[test]
fn arithmetic_precedence() {
    assert_eq!(render(&expr("1 + 2 * 3")), "(1 + (2 * 3))");
    assert_eq!(render(&expr("1 * 2 + 3")), "((1 * 2) + 3)");
    assert_eq!(render(&expr("1 - 2 - 3")), "((1 - 2) - 3)");
    assert_eq!(render(&expr("2 ** 3 ** 2")), "(2 ** (3 ** 2))");
    assert_eq!(render(&expr("1 + 2 << 3")), "((1 + 2) << 3)");
    assert_eq!(render(&expr("a | b ^ c & d")), "(a | (b ^ (c & d)))");
}

#[test]
fn comparison_and_equality_precedence() {
    assert_eq!(render(&expr("a < b == c > d")), "((a < b) == (c > d))");
    assert_eq!(render(&expr("a === b === c")), "((a === b) === c)");
    assert_eq!(
        render(&expr("a in b instanceof c")),
        "((a in b) instanceof c)"
    );
}

#[test]
fn logical_precedence_and_mixing() {
    assert_eq!(render(&expr("a || b && c")), "(a || (b && c))");
    // `??` is left-associative, like `||` and `&&`.
    assert_eq!(render(&expr("a ?? b ?? c")), "((a ?? b) ?? c)");
    assert_eq!(render(&expr("(a ?? b) || c")), "(((a ?? b)) || c)");
    assert_eq!(render(&expr("a && (b ?? c)")), "(a && ((b ?? c)))");
    assert!(parse_expression("a ?? b || c").is_err());
    assert!(parse_expression("a && b ?? c").is_err());
}

#[test]
fn unary_and_power() {
    assert_eq!(render(&expr("!a + b")), "((!a) + b)");
    assert_eq!(render(&expr("typeof a === 'b'")), "((typeof a) === \"b\")");
    // `-a ** 2` is a SyntaxError: a unary expression may not be the base of `**`.
    assert!(parse_expression("-a ** 2").is_err());
    assert_eq!(render(&expr("(-a) ** 2")), "(((-a)) ** 2)");
    assert_eq!(
        render(&expr("++a ** 2")),
        "((++a) ** 2)",
        "an update expression is allowed"
    );
    assert_eq!(render(&expr("2 ** -1")), "(2 ** (-1))");
    assert_eq!(render(&expr("!!a")), "(!(!a))");
    assert_eq!(render(&expr("void 0")), "(void 0)");
    assert_eq!(render(&expr("delete a.b")), "(delete a.b)");
}

#[test]
fn conditional_and_assignment_are_right_associative() {
    assert_eq!(render(&expr("a ? b : c ? d : e")), "(a ? b : (c ? d : e))");
    assert_eq!(render(&expr("a = b = 1")), "(a = (b = 1))");
    assert_eq!(render(&expr("a += b *= 2")), "(a += (b *= 2))");
    assert_eq!(render(&expr("a ? b = 1 : c")), "(a ? (b = 1) : c)");
}

#[test]
fn comma_expression() {
    assert_eq!(render(&expr("(a, b, c)")), "((a, b, c))");
    assert_eq!(render(&expr("a = (1, 2)")), "(a = ((1, 2)))");
}

#[test]
fn update_operators() {
    assert_eq!(render(&expr("++a")), "(++a)");
    assert_eq!(render(&expr("a--")), "(a--)");
    // A line break before a postfix operator makes it a prefix one, so
    // `a\n++b` is two statements.
    let program = parse_script("a\n++b").expect("ASI");
    assert_eq!(program.body.len(), 2);
    assert_eq!(
        render(match &program.body[0].kind {
            StmtKind::Expr(expr) => expr,
            other => panic!("unexpected {other:?}"),
        }),
        "a"
    );
}

// -- member / call / new ---------------------------------------------------

#[test]
fn member_and_call_chains() {
    assert_eq!(render(&expr("a.b.c")), "a.b.c");
    assert_eq!(render(&expr("a[b][c]")), "a[b][c]");
    assert_eq!(render(&expr("a.b(c).d")), "a.b(c).d");
    assert_eq!(render(&expr("f()()")), "f()()");
    assert_eq!(render(&expr("f(1, 2, ...rest)")), "f(1, 2, ...rest)");
}

#[test]
fn optional_chaining() {
    let chain = expr("a?.b");
    match &chain.kind {
        ExprKind::OptionalChain(inner) => match &inner.kind {
            ExprKind::Member { optional, .. } => assert!(*optional),
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("expected an OptionalChain wrapper, got {other:?}"),
    }
    assert_eq!(render(&expr("a?.b?.()")), "opt(a?.b?.())");
    assert_eq!(
        render(&expr("a?.b.c")),
        "opt(a?.b.c)",
        "later links are not optional"
    );
}

#[test]
fn new_expressions() {
    assert_eq!(render(&expr("new A(1)")), "(new A(1))");
    assert_eq!(render(&expr("new A")), "(new A)");
    assert_eq!(render(&expr("new a.b.C(1)")), "(new a.b.C(1))");
    // `new a().b` calls `a`, then reads `.b`; `new a.b()` reads `.b` first.
    assert_eq!(render(&expr("new a().b")), "(new a()).b");
    assert_eq!(render(&expr("new (a())")), "(new (a()))");
    assert!(parse_expression("new a?.b()").is_err());
}

// -- literals and templates ------------------------------------------------

#[test]
fn literals() {
    assert_eq!(render(&expr("1.5")), "1.5");
    assert_eq!(render(&expr("0x10")), "16");
    assert_eq!(render(&expr("'a' + \"b\"")), "(\"a\" + \"b\")");
    assert_eq!(render(&expr("null")), "null");
    assert_eq!(render(&expr("true")), "true");
    assert_eq!(render(&expr("/a+/gi")), "/a+/gi");
    assert_eq!(render(&expr("[1, , 3]")), "[1, , 3]");
    assert_eq!(render(&expr("({a: 1, b, ...c})")), "({a: 1, b, ...c})");
    assert_eq!(render(&expr("({['k']: 1}).k")), "({[\"k\"]: 1}).k");
    assert_eq!(render(&expr("({1: 'x'}).x")), "({1: \"x\"}).x");
    assert_eq!(
        render(&expr("({if: 1}).x")),
        "({if: 1}).x",
        "reserved words are valid keys"
    );
}

#[test]
fn templates() {
    assert_eq!(render(&expr("`a${b}c`")), "`a${b}c`");
    let tagged = expr("tag`a${b}c`");
    match tagged.kind {
        ExprKind::TaggedTemplate { quasi, .. } => {
            assert_eq!(quasi.quasis.len(), 2);
            assert_eq!(quasi.expressions.len(), 1);
            assert_eq!(quasi.quasis[0].raw, "a");
            assert!(quasi.quasis[1].tail);
        }
        other => panic!("unexpected {other:?}"),
    }
}

// -- functions and arrows --------------------------------------------------

#[test]
fn arrow_function_shapes() {
    assert_eq!(render(&expr("x => x")), "((x) => x)");
    assert_eq!(render(&expr("(x) => x")), "((x) => x)");
    assert_eq!(render(&expr("(a, b) => a")), "((a, b) => a)");
    assert_eq!(render(&expr("(a = 1) => a")), "((a = 1) => a)");
    assert_eq!(render(&expr("({a}) => a")), "(({a: a}) => a)");
    assert_eq!(render(&expr("([a, b]) => a")), "(([a, b]) => a)");
    assert_eq!(render(&expr("() => ({a: 1})")), "(() => ({a: 1}))");
    let block = expr("() => { return 1; }");
    match block.kind {
        ExprKind::Arrow(func) => {
            assert!(func.is_arrow);
            assert!(matches!(func.body, FunctionBody::Block(_)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn arrow_is_async_and_or_generator_flag() {
    let func = expr("async (a) => a");
    match func.kind {
        ExprKind::Arrow(f) => {
            assert!(f.is_async);
            assert!(!f.is_generator);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn parenthesised_expression_is_not_an_arrow() {
    // `(a, b)` is a sequence, not a parameter list, once `=>` is absent.
    assert_eq!(render(&expr("(a, b)")), "((a, b))");
    assert_eq!(render(&expr("((a))")), "((a))");
}

#[test]
fn function_declarations_and_expressions() {
    let decl = stmt("function f(a, {b}, [c], ...rest) { return a; }");
    match decl.kind {
        StmtKind::FuncDecl(func) => {
            assert_eq!(func.name.as_ref().unwrap().name, "f");
            assert_eq!(func.params.len(), 4);
            assert!(matches!(func.params[3].kind, PatternKind::Rest(_)));
        }
        other => panic!("unexpected {other:?}"),
    }
    let func = expr("function* g() { yield 1; }");
    match func.kind {
        ExprKind::Function(f) => assert!(f.is_generator),
        other => panic!("unexpected {other:?}"),
    }
}

// -- statements ------------------------------------------------------------

#[test]
fn statement_kinds() {
    assert!(matches!(stmt(";").kind, StmtKind::Empty));
    assert!(matches!(stmt("{ }").kind, StmtKind::Block(_)));
    assert!(matches!(
        stmt("if (a) b;").kind,
        StmtKind::If { alt: None, .. }
    ));
    assert!(matches!(
        stmt("if (a) b; else c;").kind,
        StmtKind::If { alt: Some(_), .. }
    ));
    assert!(matches!(
        stmt("do a; while (b);").kind,
        StmtKind::DoWhile { .. }
    ));
    assert!(matches!(stmt("while (a) b;").kind, StmtKind::While { .. }));
    assert!(matches!(
        stmt("switch (a) { case 1: }").kind,
        StmtKind::Switch { .. }
    ));
    assert!(matches!(
        stmt("try { } catch (e) { }").kind,
        StmtKind::Try { .. }
    ));
    assert!(matches!(stmt("debugger;").kind, StmtKind::Debugger));
    assert!(matches!(stmt("throw a;").kind, StmtKind::Throw(_)));
    assert!(matches!(stmt("l: a;").kind, StmtKind::Labeled { .. }));
}

#[test]
fn variable_declarations() {
    let decl = stmt("var a = 1, b, c = 3;");
    match decl.kind {
        StmtKind::VarDecl(decl) => {
            assert_eq!(decl.kind, VarDeclKind::Var);
            assert_eq!(decl.declarators.len(), 3);
            assert!(decl.declarators[1].init.is_none());
        }
        other => panic!("unexpected {other:?}"),
    }
    for (src, kind) in [
        ("var x = 1;", VarDeclKind::Var),
        ("let x = 1;", VarDeclKind::Let),
        ("const x = 1;", VarDeclKind::Const),
    ] {
        match stmt(src).kind {
            StmtKind::VarDecl(decl) => assert_eq!(decl.kind, kind),
            other => panic!("unexpected {other:?}"),
        }
    }
}

#[test]
fn destructuring_declarations() {
    let decl = stmt("let [a, , b = 2, ...c] = xs;");
    match decl.kind {
        StmtKind::VarDecl(decl) => match &decl.declarators[0].target.kind {
            PatternKind::Array { elements, rest } => {
                assert_eq!(elements.len(), 3);
                assert!(elements[1].is_none());
                assert!(rest.is_some());
                assert!(matches!(
                    elements[2].as_ref().unwrap(),
                    PatternElement::Pattern(Pattern {
                        kind: PatternKind::Assign { .. },
                        ..
                    })
                ));
            }
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }
    let decl = stmt("let {a, b: c, d = 1, ...rest} = obj;");
    match decl.kind {
        StmtKind::VarDecl(decl) => match &decl.declarators[0].target.kind {
            PatternKind::Object { properties, rest } => {
                assert_eq!(properties.len(), 3);
                assert!(rest.is_some());
            }
            other => panic!("unexpected {other:?}"),
        },
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn for_loops() {
    assert!(matches!(
        stmt("for (;;) ;").kind,
        StmtKind::For { init: None, .. }
    ));
    match stmt("for (var i = 0; i < 1; i++) ;").kind {
        StmtKind::For {
            init: Some(ForInit::VarDecl(_)),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    match stmt("for (i = 0; i < 1; i++) ;").kind {
        StmtKind::For {
            init: Some(ForInit::Expr(_)),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    match stmt("for (const x of xs) ;").kind {
        StmtKind::ForOf {
            is_await: false,
            left: ForHead::VarDecl(_),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    match stmt("for (x of xs) ;").kind {
        StmtKind::ForOf {
            left: ForHead::Assign(_),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    match stmt("for (const k in o) ;").kind {
        StmtKind::ForIn {
            left: ForHead::VarDecl(_),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
    // The head uses AssignmentExpression[~In], so `1 in {}` is *not* a binary
    // expression there; the `in` starts a for-in clause (legacy sloppy form).
    assert!(parse_script("for (var x = 1 in {}; ; ) ;").is_err());
    match stmt("for (var x = 1 in {});").kind {
        StmtKind::ForIn {
            left: ForHead::VarDecl(decl),
            ..
        } => {
            assert!(
                decl.declarators[0].init.is_some(),
                "Annex B allows an initialiser here"
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    // Parenthesised, `in` is a binary operator again.
    match stmt("for (var x = (1 in {}); ; ) ;").kind {
        StmtKind::For {
            init: Some(ForInit::VarDecl(_)),
            ..
        } => {}
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn assignment_targets() {
    assert_eq!(render(&expr_stmt("[a, b] = [b, a]")), "([a, b] = [b, a])");
    assert_eq!(render(&expr_stmt("({a} = o)")), "(({a: a} = o))");
    let assign = expr_stmt("a.b = 1");
    match assign.kind {
        ExprKind::Assign { target, .. } => {
            assert!(matches!(target.kind, PatternKind::Member(_)));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn automatic_semicolon_insertion() {
    for (src, count) in [
        ("a\nb\nc", 3),
        ("return_ = 1\n1", 2),
        ("var a = 1\nvar b = 2", 2),
        ("a = 1;", 1),
        ("if (a) b\nc", 2),
        ("{}\n{}", 2),
        ("do ; while (0)\nx", 2),
    ] {
        let program = parse_script(src).unwrap_or_else(|e| panic!("{src:?}: {e}"));
        assert_eq!(program.body.len(), count, "{src:?}");
    }
}

#[test]
fn restricted_productions() {
    // `return` / `throw` / `yield` may not span a line break.
    let func = stmt("function f() { return\n1; }");
    match func.kind {
        StmtKind::FuncDecl(f) => {
            let body = f.body.statements().expect("block body");
            assert!(
                matches!(body[0].kind, StmtKind::Return(None)),
                "ASI after return"
            );
            assert_eq!(body.len(), 2);
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(parse_script("throw\n1").is_err());
}

#[test]
fn classes() {
    let class = stmt(
        "class A extends B { constructor() { super(); } m() {} get x() { return 1 } set x(v) {} static s() {} #p = 1; static { } }",
    );
    match class.kind {
        StmtKind::ClassDecl(class) => {
            assert!(class.super_class.is_some());
            let kinds: Vec<&str> = class
                .elements
                .iter()
                .map(|element| match element {
                    ClassElement::Method {
                        kind, is_static, ..
                    } => {
                        if *is_static {
                            "static method"
                        } else {
                            kind_name(*kind)
                        }
                    }
                    ClassElement::Field { is_static, .. } => {
                        if *is_static {
                            "static field"
                        } else {
                            "field"
                        }
                    }
                    ClassElement::StaticBlock { .. } => "static block",
                })
                .collect();
            assert_eq!(
                kinds,
                vec![
                    "constructor",
                    "method",
                    "get",
                    "set",
                    "static method",
                    "field",
                    "static block"
                ]
            );
        }
        other => panic!("unexpected {other:?}"),
    }
}

fn kind_name(kind: MethodKind) -> &'static str {
    match kind {
        MethodKind::Constructor => "constructor",
        MethodKind::Method => "method",
        MethodKind::Get => "get",
        MethodKind::Set => "set",
    }
}

#[test]
fn class_fields_vs_methods() {
    // `static`, `get`, `set` and `async` are ordinary field names when followed
    // by `=`, `;` or `(`.
    let class = stmt("class A { static = 1; get = 2; set = 3; async = 4; }");
    match class.kind {
        StmtKind::ClassDecl(class) => assert_eq!(class.elements.len(), 4),
        other => panic!("unexpected {other:?}"),
    }
    let class = stmt("class A { static() {} get() {} }");
    match class.kind {
        StmtKind::ClassDecl(class) => {
            assert_eq!(class.elements.len(), 2);
            assert!(matches!(
                class.elements[0],
                ClassElement::Method {
                    kind: MethodKind::Method,
                    is_static: false,
                    ..
                }
            ));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn modules() {
    let program =
        parse_module("import a, {b as c} from 'm'; export const d = 1; export default 2;")
            .expect("valid module");
    assert_eq!(program.kind, ProgramKind::Module);
    assert!(program.strict);
    assert_eq!(program.body.len(), 3);
    match &program.body[0].kind {
        StmtKind::Import(decl) => {
            assert_eq!(decl.source, "m");
            assert_eq!(decl.specifiers.len(), 2);
        }
        other => panic!("unexpected {other:?}"),
    }
    match &program.body[2].kind {
        StmtKind::Export(decl) => assert!(matches!(decl.kind, ExportKind::Default(_))),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn module_early_errors() {
    assert!(parse_module("export {missing};").is_err());
    assert!(
        parse_module("export {a}; const a = 1;").is_ok(),
        "forward reference is legal"
    );
    assert!(parse_module("import {a} from 1;").is_err());
    assert!(parse_module("with (o) {}").is_err(), "modules are strict");
}

// -- positions -------------------------------------------------------------

#[test]
fn spans_cover_the_source() {
    let program = parse_script("var x = 1;\nfunction f() {}").expect("parse");
    assert_eq!(program.body.len(), 2);
    assert_eq!(program.body[0].span.start.line, 1);
    assert_eq!(program.body[0].span.end.column, 11);
    assert_eq!(program.body[1].span.start.line, 2);
    assert_eq!(program.body[1].span.start.column, 1);
    // Identifiers carry their own span.
    match &program.body[0].kind {
        StmtKind::VarDecl(decl) => {
            let PatternKind::Ident(id) = &decl.declarators[0].target.kind else {
                panic!()
            };
            assert_eq!(id.span.start.column, 5);
            assert_eq!(id.span.end.column, 6);
        }
        other => panic!("unexpected {other:?}"),
    }
}

// -- early errors ----------------------------------------------------------

#[test]
fn strict_mode_early_errors() {
    for src in [
        "'use strict'; with (o) {}",
        "'use strict'; var eval = 1;",
        "'use strict'; arguments = 1;",
        "'use strict'; delete x;",
        "'use strict'; 010;",
        "'use strict'; var implements = 1;",
        "function f(a, a) { 'use strict'; }",
        "'use strict'; function f(a, a) {}",
    ] {
        assert!(parse_script(src).is_err(), "{src:?} should be rejected");
    }
    // The same constructs are legal in sloppy mode.
    for src in [
        "with (o) {}",
        "var eval = 1;",
        "010;",
        "function f(a, a) {}",
    ] {
        assert!(parse_script(src).is_ok(), "{src:?} should be accepted");
    }
}

#[test]
fn parameter_early_errors() {
    for src in [
        "function f(...a, b) {}",
        "function f(...a = 1) {}",
        "var f = (a, a) => 1;",
        "function f({a}, {a}) {}",
        "var f = (a, a) => 1;",
    ] {
        assert!(parse_script(src).is_err(), "{src:?} should be rejected");
    }
}

#[test]
fn control_flow_early_errors() {
    for src in [
        "break;",
        "continue;",
        "for (;;) { continue l; }",
        "function f() { return 1; } return 2;",
        "l: { l: ; }",
        "switch (a) { default: ; default: ; }",
    ] {
        assert!(parse_script(src).is_err(), "{src:?} should be rejected");
    }
    for src in [
        "l: for (;;) { continue l; }",
        "a: b: for (;;) { break a; break b; }",
        "switch (a) { default: break; }",
        "for (;;) { break; }",
    ] {
        assert!(parse_script(src).is_ok(), "{src:?} should be accepted");
    }
}

#[test]
fn object_and_class_early_errors() {
    for src in [
        "({__proto__: 1, __proto__: 2});",
        "({a = 1});",
        "class A { constructor() {} constructor() {} }",
        "class A { #x; #x; }",
        "class A { m() { return this.#y; } }",
        "class A { static prototype() {} }",
    ] {
        assert!(parse_script(src).is_err(), "{src:?} should be rejected");
    }
    // `__proto__` twice is fine when one is computed or a method.
    for src in [
        "({__proto__: 1, ['__proto__']: 2});",
        "({__proto__: 1, __proto__() {}});",
        "class A { get #x() {} set #x(v) {} }",
    ] {
        assert!(parse_script(src).is_ok(), "{src:?} should be accepted");
    }
}

#[test]
fn super_and_new_target_early_errors() {
    assert!(parse_script("super.x;").is_err());
    assert!(parse_script("function f() { super(); }").is_err());
    assert!(parse_script("new.target;").is_err());
    // `super()` is only legal in a *derived* constructor.
    assert!(parse_script("class A extends B { constructor() { super(); } }").is_ok());
    assert!(parse_script("class A { m() { return super.x; } }").is_ok());
    assert!(parse_script("function f() { new.target; }").is_ok());
}

#[test]
fn error_positions_are_reported() {
    let err = parse_script("var x = ;").expect_err("syntax error");
    assert_eq!(err.line(), 1);
    assert_eq!(err.column(), 9);
    let err = parse_script("let a = 1;\nlet b = ;").expect_err("syntax error");
    assert_eq!(err.line(), 2);
    assert_eq!(err.column(), 9);
}

/// Parse `src` on a thread with an explicit stack, so the nesting tests do not
/// depend on whatever stack the test harness hands out.
fn parse_on_big_stack(src: &str, max_depth: u32) -> Result<(), js_parse::SyntaxError> {
    let source = src.to_string();
    let handle = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut parser = js_parse::Parser::new(&source, false)?;
            parser.set_max_depth(max_depth);
            parser.parse_program().map(|_| ())
        })
        .expect("spawn");
    handle.join().expect("parse thread")
}

#[test]
fn deep_nesting_is_supported_up_to_the_limit() {
    let depth = 400;
    let src = format!("{}1{}", "(".repeat(depth), ")".repeat(depth));
    assert!(
        parse_on_big_stack(&src, 500).is_ok(),
        "{depth} levels should parse"
    );

    // A long chain of binary operators builds a left-leaning tree, but the
    // precedence climber is iterative there, so its depth does not matter.
    let src = (0..500)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(" + ");
    assert!(parse_on_big_stack(&src, 500).is_ok());
}

#[test]
fn nesting_beyond_the_limit_is_a_syntax_error_not_a_crash() {
    // Matches QuickJS, which reports `SyntaxError: stack overflow` rather than
    // dying when the parser runs out of stack.
    let src = format!("{}1{}", "(".repeat(120), ")".repeat(120));
    let err = parse_on_big_stack(&src, 40).expect_err("should hit the nesting limit");
    assert_eq!(err.message, "stack overflow");
}

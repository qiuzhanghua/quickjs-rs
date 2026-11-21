//! The example in README.md, kept compilable so the two cannot drift.
use js_parse::{ast::StmtKind, parse_script};

fn main() -> Result<(), js_parse::ParseError> {
    let program = parse_script("var x = 1 + 2;")?;
    for stmt in &program.body {
        if let StmtKind::VarDecl(decl) = &stmt.kind {
            assert_eq!(decl.declarators.len(), 1);
        }
    }
    println!("{} statement(s)", program.body.len());
    Ok(())
}

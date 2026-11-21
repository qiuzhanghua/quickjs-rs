//! The example in README.md, kept compilable so the two cannot drift.
use js_compile::{compile_script, disassemble};
use js_parse::parse_script;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let program = parse_script("var x = 1; x + 2;")?;
    let function = compile_script(&program)?;
    print!("{}", disassemble(&function));
    Ok(())
}

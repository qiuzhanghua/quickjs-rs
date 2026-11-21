fn main() {
    let source = std::env::args().nth(1).expect("source");
    let program = js_parse::parse_script(&source).unwrap();
    let code = js_compile::compile_script(&program).unwrap();
    print!("{}", js_compile::disassemble(&code));
}

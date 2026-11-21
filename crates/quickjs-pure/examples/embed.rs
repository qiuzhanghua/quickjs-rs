//! The example in README.md, kept compilable so the two cannot drift.
use quickjs::{Context, Runtime};

fn main() -> Result<(), quickjs::Error> {
    let runtime = Runtime::new();
    let mut context = Context::new(&runtime);
    context.eval("print([1, 2, 3].map(x => x * 2).join('-'));")?;
    assert_eq!(context.output(), "2-4-6\n");
    print!("{}", context.output());
    Ok(())
}

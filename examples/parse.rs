use std::path::Path;

use anyhow::Context;
use tree_sitter_wasm::Registry;

fn main() -> anyhow::Result<()> {
    let filename = std::env::args()
        .nth(1)
        .context("Usage: cargo run --example parse -- <file>")?;
    let source = std::fs::read_to_string(&filename)?;
    let registry = Registry::new()?;
    let resolution = registry.resolve(Path::new(&filename), &source)?;
    if let Some(warning) = resolution.warning {
        eprintln!("{warning}");
    }
    let grammar = resolution.grammar.context("No grammar matches this file")?;
    let workers = grammar.workers.clone();
    workers.run_blocking(move || {
        let mut parser = grammar.new_parser()?;
        let tree = parser.parse(&source, None).context("Parse cancelled")?;
        println!("{}", tree.root_node().to_sexp());
        Ok(())
    })
}

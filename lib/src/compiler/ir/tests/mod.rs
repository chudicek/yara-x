use std::fs;
use std::io::BufWriter;
use std::mem::size_of;

use crate::compiler::Expr;
use crate::Compiler;

#[test]
fn expr_size() {
    // Sentinel test for making sure tha Expr doesn't grow in future
    // changes.
    assert_eq!(size_of::<Expr>(), 32);
}

#[test]
fn ir() {
    let files: Vec<_> = globwalk::glob("src/compiler/ir/tests/testdata/*.in")
        .unwrap()
        .flatten()
        .map(|entry| entry.into_path())
        .collect();

    files.into_iter().for_each(|path| {
        let mut mint = goldenfile::Mint::new(".");

        let output_path = if cfg!(feature = "constant-folding") {
            path.with_extension("folding.ir")
        } else {
            path.with_extension("no-folding.ir")
        };

        let output_file = mint.new_goldenfile(output_path).unwrap();

        println!("file: {:?}", path);
        let source = fs::read_to_string(path).unwrap();

        let mut compiler = Compiler::new();

        let w = BufWriter::new(output_file);

        compiler.set_ir_writer(w).add_source(source.as_str()).unwrap();
    });
}

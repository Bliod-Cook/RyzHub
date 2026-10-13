//! Writes the grammars the web client loads on first use (`highlight::load_syntaxes`):
//! `cargo run -p ryzhub-core --example syntax-dump -- client/web/syntaxes.bin`.
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: syntax-dump <output file>");
    syntect::dumps::dump_to_uncompressed_file(&two_face::syntax::extra_newlines(), &path)
        .expect("write the grammars");
}

//! Headless preview of the Rust-script pseudocode for a `composition_tree` answer.
//!
//! The tree panel's 「脚本」 view as text, so a document's pseudocode can be read
//! without a window: capture a `composition_tree` answer to a JSON file and run
//!
//! ```text
//! cargo run -p rusd-viewer --example pseudocode -- tree.json
//! ```
//!
//! Each line is a clickable variable in the GUI — the label, then the layer id a
//! click would solo (the USD it renders).

use rusd_viewer::tree::CompositionTree;

fn main() {
    let path = std::env::args().nth(1).expect("usage: pseudocode <tree.json>");
    let text = std::fs::read_to_string(&path).expect("read tree json");
    let value: serde_json::Value = serde_json::from_str(&text).expect("tree json parses");
    let tree = CompositionTree::parse(&value).expect("composition_tree parses");
    for row in rusd_viewer::script::rows(&tree) {
        println!("{}", row.label);
        if let Some(secondary) = row.secondary {
            println!("    {secondary}");
        }
        println!("    click → solo {}", row.id);
    }
}

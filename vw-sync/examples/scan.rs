//! Show what a workspace would synchronize, without synchronizing it.
//!
//! ```text
//! cargo run -p vw-sync --example scan -- ~/sketch/metroid
//! ```
//!
//! Useful for answering "why is my sync so large" before blaming the network.
//! The answer is nearly always an ignore rule that is not doing what it looks
//! like it does.

fn main() {
    let Some(root) = std::env::args().nth(1) else {
        eprintln!("usage: scan <workspace>");
        std::process::exit(1);
    };
    let root = camino::Utf8PathBuf::from(root);

    let manifest = match vw_sync::scan(&root) {
        Ok(manifest) => manifest,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    let bytes: u64 = manifest
        .entries
        .iter()
        .filter_map(|entry| std::fs::metadata(root.join(&entry.path)).ok())
        .map(|meta| meta.len())
        .sum();

    println!(
        "{} files, {:.1} MiB",
        manifest.entries.len(),
        bytes as f64 / (1024.0 * 1024.0),
    );
    for entry in &manifest.entries {
        println!("  {}", entry.path);
    }
}

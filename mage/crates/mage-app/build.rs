//! Embeds the `mage/web-app` production bundle (`web-app/dist`, built with `npm run build`) into
//! the binary: `$OUT_DIR/web_assets.rs` lists every file of the bundle with `include_bytes!`.
//! Without a bundle (no `dist/index.html` at build time) the list is empty and `mage start server`
//! serves the API only.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect(root, &path, out)?;
        } else if let Ok(rel) = path.strip_prefix(root) {
            let rel = rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/");
            out.push((rel, path.canonicalize()?));
        }
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let dist = manifest.join("../../web-app/dist");
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.join("index.html").is_file() {
        collect(&dist, &dist, &mut files)?;
    }
    let mut code = String::from("/// `(path relative to web-app/dist, bytes)` of the embedded bundle; empty = none at build time.\n");
    code.push_str("pub static WEB_ASSETS: &[(&str, &[u8])] = &[\n");
    for (rel, abs) in &files {
        let abs = abs.to_str().ok_or("web-app/dist path is not UTF-8")?;
        writeln!(code, "    ({rel:?}, include_bytes!({abs:?})),")?;
    }
    code.push_str("];\n");
    std::fs::write(PathBuf::from(std::env::var("OUT_DIR")?).join("web_assets.rs"), code)?;
    Ok(())
}

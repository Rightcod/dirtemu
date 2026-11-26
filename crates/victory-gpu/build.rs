use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_files(&path, files)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
    Ok(())
}

fn main() {
    let package = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let workspace = package.parent().unwrap().parent().unwrap();
    let shader = workspace.join("crates/victory-shader");
    let mut files = Vec::new();
    rust_files(&shader.join("src"), &mut files).expect("read translator sources");
    files.extend([
        shader.join("Cargo.toml"),
        package.join("src/shader_cache.rs"),
        package.join("Cargo.toml"),
        package.join("build.rs"),
        workspace.join("Cargo.lock"),
    ]);
    files.sort();
    let mut hash = blake3::Hasher::new();
    let mut switches = BTreeSet::new();
    for path in files {
        println!("cargo:rerun-if-changed={}", path.display());
        let name = path
            .strip_prefix(workspace)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = std::fs::read(&path).expect("read translator input");
        hash.update(&(name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update(&(bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
        let text = String::from_utf8_lossy(&bytes);
        for (at, _) in text.match_indices("\"VICTORY_") {
            let tail = &text[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                switches.insert(tail[..end].to_owned());
            }
        }
    }
    for name in ["TARGET", "CARGO_FEATURE_DIAGNOSTICS"] {
        hash.update(name.as_bytes());
        hash.update(std::env::var(name).unwrap_or_default().as_bytes());
    }
    println!(
        "cargo:rustc-env=VICTORY_TRANSLATOR_SOURCE={}",
        hash.finalize().to_hex()
    );
    println!(
        "cargo:rustc-env=VICTORY_TRANSLATOR_SWITCHES={}",
        switches.into_iter().collect::<Vec<_>>().join(",")
    );
}

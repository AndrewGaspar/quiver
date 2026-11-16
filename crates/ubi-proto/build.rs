use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "proto") {
            out.push(path);
        }
    }
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../proto");
    let mut protos = Vec::new();
    collect(&root, &mut protos);
    protos.sort();
    println!("cargo:rerun-if-changed={}", root.display());
    prost_build::Config::new()
        .compile_protos(&protos, &[&root])
        .expect("compile Ubisoft protos");
}

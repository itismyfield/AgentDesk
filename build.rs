use std::{fs, path::Path};

fn main() {
    rerun_if_changed_recursively(Path::new("migrations/postgres"));
}

fn rerun_if_changed_recursively(path: &Path) {
    println!("cargo:rerun-if-changed={}", path.display());

    let Ok(entries) = fs::read_dir(path) else {
        return;
    };

    for entry in entries.flatten() {
        let child_path = entry.path();
        if child_path.is_dir() {
            rerun_if_changed_recursively(&child_path);
        } else {
            println!("cargo:rerun-if-changed={}", child_path.display());
        }
    }
}

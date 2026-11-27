use std::{env, fs, path::Path};

fn main() {
    // The source checkout root (two levels above this manifest) for `runtime_layout::REPO_ROOT`.
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let repo_root = Path::new(&manifest_dir)
        .parent()
        .and_then(Path::parent)
        .expect("crates/adk-runtime sits two levels below the checkout root");
    println!("cargo:rustc-env=ADK_REPO_ROOT={}", repo_root.display());
    rerun_if_changed_recursively(&repo_root.join("migrations/postgres"));
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

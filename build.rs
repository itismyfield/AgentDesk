use std::{env, fs, path::Path};

const CHECKOUT_SHA_ENV: &str = "ADK_RELEASE_CHECKOUT_SHA";

fn main() {
    rerun_if_changed_recursively(Path::new("migrations/postgres"));
    println!("cargo:rerun-if-env-changed={CHECKOUT_SHA_ENV}");
    let source = env::var(CHECKOUT_SHA_ENV).ok().filter(|value| {
        value.len() == 40
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    let target = env::var("TARGET").ok().filter(|value| {
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    });
    let (source, target) = source.zip(target).unwrap_or_default();
    // Keep unwitnessed builds from inheriting a stamp meant for rustc.
    println!("cargo:rustc-env=ADK_RUNNING_SOURCE_SHA={source}");
    println!("cargo:rustc-env=ADK_RUNNING_SOURCE_TARGET={target}");
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

use std::{env, fs, path::Path, process::Command};

fn main() {
    rerun_if_changed_recursively(Path::new("migrations/postgres"));
    let root = env::var_os("CARGO_MANIFEST_DIR");
    let source = root.as_deref().map(Path::new).and_then(running_source);
    let target = env::var("TARGET").ok().filter(|value| {
        !value.is_empty()
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    });
    let (source, target) = source.zip(target).unwrap_or_default();
    // Empty values mask any inherited environment values on unwitnessed builds.
    println!("cargo:rustc-env=ADK_RUNNING_SOURCE_SHA={source}");
    println!("cargo:rustc-env=ADK_RUNNING_SOURCE_TARGET={target}");
}

fn running_source(root: &Path) -> Option<String> {
    let git_root = git(root, &["rev-parse", "--show-toplevel"])?;
    let git_root = Path::new(
        std::str::from_utf8(&git_root)
            .ok()?
            .trim_end_matches(['\r', '\n']),
    );
    if git_root.canonicalize().ok()? != root.canonicalize().ok()? {
        return None;
    }

    // Cargo's existing migration watch covers new migration files; track the checkout as well.
    if root.join(".git").is_file() {
        watch(&root.join(".git"))?;
    }
    for name in ["HEAD", "index", "packed-refs"] {
        let path = git_path(root, name)?;
        if path.exists() {
            watch(&path)?;
        }
    }
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"]) {
        let name = std::str::from_utf8(&reference)
            .ok()?
            .trim_end_matches(['\r', '\n']);
        let path = git_path(root, name)?;
        // A packed branch may gain a loose ref without rewriting packed-refs.
        let watched = path.ancestors().find(|path| path.exists())?;
        watch(watched)?;
    }

    let files = git(root, &["ls-files", "-v", "-z"])?;
    let mut visible = true;
    for file in files
        .split(|byte| *byte == 0)
        .filter(|file| !file.is_empty())
    {
        let (tag, path) = file.split_at_checked(2)?;
        let path = root.join(std::str::from_utf8(path).ok()?);
        visible &= tag == b"H " && path.symlink_metadata().is_ok_and(|meta| meta.is_file());
        watch(&path)?;
    }
    let head = git(root, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let dirty = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )?;
    if !visible
        || !dirty.is_empty()
        || git(root, &["rev-parse", "--verify", "HEAD^{commit}"])? != head
    {
        return None;
    }
    let sha = std::str::from_utf8(&head)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    (sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| sha.to_owned())
}

fn git_path(root: &Path, name: &str) -> Option<std::path::PathBuf> {
    let path = git(
        root,
        &["rev-parse", "--path-format=absolute", "--git-path", name],
    )?;
    let path = std::str::from_utf8(&path)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    Path::new(path).is_absolute().then(|| path.into())
}

fn watch(path: &Path) -> Option<()> {
    let path = path.to_str()?;
    if path.contains(['\n', '\r']) {
        return None;
    }
    println!("cargo:rerun-if-changed={path}");
    Some(())
}

fn git(root: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.trustctime=true",
            "-c",
            "core.checkStat=default",
        ])
        .args(args)
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
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

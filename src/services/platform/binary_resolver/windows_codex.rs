//! Windows stock npm Codex launcher resolution, confined to its own install.
use std::path::{Path, PathBuf};

pub(super) fn native_from_npm_shim(shim: &Path, arch: &str) -> Option<PathBuf> {
    if !shim
        .file_name()?
        .to_str()?
        .eq_ignore_ascii_case("codex.cmd")
    {
        return None;
    }
    let (package, target) = match arch {
        "x86_64" => ("codex-win32-x64", "x86_64-pc-windows-msvc"),
        "aarch64" => ("codex-win32-arm64", "aarch64-pc-windows-msvc"),
        _ => return None,
    };
    // Only recognize npm's direct package launcher. Do not reinterpret arbitrary
    // custom batch files, other package managers, or explicit operator overrides.
    if std::fs::metadata(shim).ok()?.len() > 16 * 1024 {
        return None;
    }
    let text = std::fs::read_to_string(shim).ok()?;
    if !text.contains(r#""%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*"#) {
        return None;
    }
    let modules = shim.parent()?.join("node_modules");
    let root = modules.join("@openai/codex");
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("package.json")).ok()?).ok()?;
    if manifest["name"] != "@openai/codex" || manifest["bin"]["codex"] != "bin/codex.js" {
        return None;
    }
    let relative = Path::new("vendor").join(target).join("bin/codex.exe");
    // Node resolution prefers a nested optional dependency, then a hoisted one;
    // older npm Codex installs kept vendor directly in the main package.
    [
        root.join("node_modules/@openai").join(package),
        modules.join("@openai").join(package),
        root,
    ]
    .into_iter()
    .map(|base| base.join(&relative))
    .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(root: &Path) -> PathBuf {
        let shim = root.join("codex.cmd");
        std::fs::write(&shim,
            r#"@ECHO off
endLocal & goto #_undefined_# 2>NUL || title %COMSPEC% & "%_prog%"  "%dp0%\node_modules\@openai\codex\bin\codex.js" %*"#,
        ).unwrap();
        let package = root.join("node_modules/@openai/codex");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#,
        )
        .unwrap();
        shim
    }

    #[test]
    fn resolves_same_install_native_binary_and_preserves_architecture() {
        for (arch, package, target) in [
            ("x86_64", "codex-win32-x64", "x86_64-pc-windows-msvc"),
            ("aarch64", "codex-win32-arm64", "aarch64-pc-windows-msvc"),
        ] {
            for layout in ["nested", "hoisted", "legacy"] {
                let dir = tempfile::tempdir().unwrap();
                let shim = fixture(dir.path());
                let modules = dir.path().join("node_modules");
                let base = match layout {
                    "nested" => modules
                        .join("@openai/codex/node_modules/@openai")
                        .join(package),
                    "hoisted" => modules.join("@openai").join(package),
                    _ => modules.join("@openai/codex"),
                };
                let native = base.join("vendor").join(target).join("bin/codex.exe");
                std::fs::create_dir_all(native.parent().unwrap()).unwrap();
                std::fs::write(&native, b"fixture").unwrap();
                assert_eq!(native_from_npm_shim(&shim, arch), Some(native));
                let other = if arch == "x86_64" {
                    "aarch64"
                } else {
                    "x86_64"
                };
                assert_eq!(native_from_npm_shim(&shim, other), None);
            }
        }
    }

    #[test]
    fn preserves_unknown_custom_missing_and_mismatched_launchers() {
        let dir = tempfile::tempdir().unwrap();
        let shim = fixture(dir.path());
        assert_eq!(native_from_npm_shim(&shim, "x86_64"), None);
        let native = dir
            .path()
            .join("node_modules/@openai/codex/vendor/x86_64-pc-windows-msvc/bin/codex.exe");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(&native, b"fixture").unwrap();
        assert_eq!(native_from_npm_shim(&shim, "x86_64"), Some(native));
        std::fs::write(&shim, "@echo custom-launcher").unwrap();
        assert_eq!(native_from_npm_shim(&shim, "x86_64"), None);
        assert_eq!(native_from_npm_shim(&shim, "unsupported"), None);
        fixture(dir.path());
        std::fs::write(
            dir.path().join("node_modules/@openai/codex/package.json"),
            r#"{"name":"different-package","bin":{"codex":"bin/codex.js"}}"#,
        )
        .unwrap();
        assert_eq!(native_from_npm_shim(&shim, "x86_64"), None);
    }
}

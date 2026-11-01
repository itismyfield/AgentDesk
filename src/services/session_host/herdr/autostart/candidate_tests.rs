//! Binary discovery and the checks a candidate must pass before AgentDesk would start it.

use std::os::unix::fs::PermissionsExt;

use super::*;

/// A Mach-O 64-bit header for `cputype`, little-endian, as the first eight bytes of an image.
fn header(magic: u32, cputype: u32) -> Vec<u8> {
    let mut bytes = magic.to_le_bytes().to_vec();
    bytes.extend(cputype.to_le_bytes());
    bytes
}

#[test]
fn search_order_is_path_then_known_install_dirs_without_repeats() {
    let dirs = search_dirs(
        Path::new("/h"),
        Some(OsStr::new(
            "/opt/homebrew/bin:relative:/usr/bin::/opt/homebrew/bin",
        )),
    );
    let expected: Vec<PathBuf> = [
        "/opt/homebrew/bin",
        "/usr/bin",
        "/usr/local/bin",
        "/h/.local/bin",
        "/h/.cargo/bin",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    assert_eq!(dirs, expected);
}

#[test]
fn only_a_thin_arm64_image_is_native() {
    assert!(native_arm64(&header(0xfeed_facf, 0x0100_000c)));
    assert!(!native_arm64(&header(0xfeed_facf, 0x0100_0007)), "x86_64");
    assert!(!native_arm64(&0xcafe_babe_u32.to_be_bytes()), "fat");
    assert!(!native_arm64(b"#!/bin/sh\n"), "script");
    assert!(!native_arm64(&[0xcf, 0xfa]), "short");
}

#[test]
fn only_a_stable_supported_version_is_accepted() {
    assert_eq!(supported_version("herdr 0.9.5\n"), Some("0.9.5".into()));
    assert_eq!(supported_version("herdr 0.9.3"), Some("0.9.3".into()));
    for refused in [
        "herdr 0.9.2",
        "herdr 0.9.3-rc1",
        "herdr",
        "zellij 0.9.5",
        "",
    ] {
        assert_eq!(supported_version(refused), None, "{refused}");
    }
}

#[test]
fn unacceptable_files_are_rejected_with_their_reason() {
    let dir = tempfile::tempdir().unwrap();
    let euid = unsafe { libc::geteuid() };
    let make = |sub: &str, bytes: &[u8], mode: u32| {
        let d = dir.path().join(sub);
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join("herdr");
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        d
    };
    let script = make("script", b"#!/bin/sh\necho herdr 0.9.5\n", 0o755);
    let writable = make("writable", &header(0xfeed_facf, 0x0100_000c), 0o775);
    let excluded = make("adk", &header(0xfeed_facf, 0x0100_000c), 0o755);
    let empty = dir.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let (found, rejected) = find(
        &[empty, script.clone(), writable.clone(), excluded.clone()],
        std::slice::from_ref(&excluded),
        euid,
    );
    assert_eq!(found, None);
    let why = |d: &Path| {
        rejected
            .iter()
            .find(|r| r.path == d.join("herdr"))
            .map(|r| r.why.clone())
            .unwrap_or_default()
    };
    assert!(why(&script).contains("arm64"), "{rejected:?}");
    assert!(why(&writable).contains("writable"), "{rejected:?}");
    assert!(why(&excluded).contains("AgentDesk"), "{rejected:?}");
    assert_eq!(rejected.len(), 3, "a dir without herdr is no rejection");
}

#[test]
fn a_version_run_must_exit_zero_within_the_output_limit() {
    let dir = tempfile::tempdir().unwrap();
    let script = |name: &str, body: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        path
    };
    let version = |path: &Path| version_output(path).map(|out| supported_version(&out));
    let ok = script("ok", "echo 'herdr 0.9.5'");
    assert_eq!(version(&ok), Ok(Some("0.9.5".into())));
    let failed = script("failed", "echo 'herdr 0.9.5'; exit 7");
    assert!(version(&failed).is_err_and(|e| e.contains("exited")));
    // A valid line padded past the limit would still trim to a supported version if cut short.
    let long = script(
        "long",
        "echo 'herdr 0.9.5'; i=0; while [ $i -lt 5000 ]; do printf ' '; i=$((i+1)); done; echo; echo extra",
    );
    assert!(version(&long).is_err_and(|e| e.contains("too much")));
}

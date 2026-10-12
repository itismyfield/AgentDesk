use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PATTERN: &str = r#""type"[[:space:]]*:[[:space:]]*"not_sent""#;

fn literal_rg(path: &Path) -> io::Result<Output> {
    Command::new("rg")
        .env_remove("RIPGREP_CONFIG_PATH")
        .args(["-n", PATTERN])
        .arg(path)
        .output()
        .map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                io::Error::new(error.kind(), "rg binary required (install ripgrep)")
            } else {
                error
            }
        })
}

pub(crate) fn not_sent_tags(path: &Path) -> io::Result<usize> {
    let output = literal_rg(path)?;
    if !matches!(output.status.code(), Some(0 | 1)) || !output.stderr.is_empty() {
        return Err(io::Error::other(format!(
            "rg read failed: {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    // Exit 1 alone is insufficient: ripgrep can skip binary input or search a directory.
    let bytes = std::fs::read(path)?;
    if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
        return Err(io::Error::other(
            "ledger was not completely searched as text",
        ));
    }
    match output.status.code() {
        Some(1) if output.stdout.is_empty() => Ok(0),
        Some(0) if !output.stdout.is_empty() => {
            Ok(String::from_utf8_lossy(&output.stdout).lines().count())
        }
        _ => Err(io::Error::other("rg result is incomplete")),
    }
}

pub(crate) fn scan_activated_ledgers(
    paths: &[PathBuf],
    off: bool,
    joined: bool,
    complete_list: bool,
    read_tags: impl Fn(&Path) -> io::Result<usize>,
) -> Result<Vec<(PathBuf, io::Result<usize>)>, &'static str> {
    if !off || !joined {
        return Err("confirm OFF and wait for writer quiescence before scanning");
    }
    if !complete_list || paths.is_empty() {
        return Err("the complete activation list is required");
    }
    // Keep every file's result; one absent tag or a partial read cannot pass the whole list.
    Ok(paths
        .iter()
        .map(|path| (path.clone(), read_tags(path)))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permits_old(results: &[(PathBuf, io::Result<usize>)]) -> bool {
        !results.is_empty() && results.iter().all(|(_, tags)| matches!(tags, Ok(0)))
    }

    #[test]
    fn d4act3_literal_rg_missing_binary_requires_install() {
        const CHILD: &str = "AGENTDESK_TEST_RG_MISSING_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let error = literal_rg(Path::new("ledger.jsonl")).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::NotFound);
            assert_eq!(error.to_string(), "rg binary required (install ripgrep)");
            return;
        }
        let empty_path = tempfile::tempdir().unwrap();
        let test = format!(
            "{}::d4act3_literal_rg_missing_binary_requires_install",
            module_path!()
        );
        let (_, test) = test.split_once("::").unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--nocapture"])
            .env("PATH", empty_path.path())
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed"),
            "missing-rg child failed: {stdout} {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn d4act3_literal_rg_finds_spaced_not_sent_tags() {
        let runbook = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/tui-o-writer-canary-runbook.md"),
        )
        .unwrap();
        assert!(
            runbook.contains(&format!("rg -n '{PATTERN}'")),
            "execute the documented literal command"
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.jsonl");
        for (ledger, code, tags) in [
            ("{\"entry\":{\"type\":\"not_sent\"}}\n", 0, 1),
            ("{\"entry\":{\"type\" : \"not_sent\"}}\n", 0, 1),
            ("{\"entry\":{\"type\"\t:\t\"not_sent\"}}\n", 0, 1),
            ("{\"entry\":{\"type\":\"not_found\"}}\n", 1, 0),
            ("", 1, 0),
        ] {
            std::fs::write(&path, ledger).unwrap();
            assert_eq!(
                literal_rg(&path).unwrap().status.code(),
                Some(code),
                "{ledger:?}"
            );
            assert_eq!(not_sent_tags(&path).unwrap(), tags, "{ledger:?}");
        }
    }

    #[test]
    fn d4act3_literal_rg_read_failure_and_incomplete_text_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.jsonl");
        assert!(literal_rg(&missing).unwrap().status.code().unwrap() >= 2);
        assert!(not_sent_tags(&missing).is_err(), "exit >=2 is never absent");
        let binary = dir.path().join("binary.jsonl");
        std::fs::write(&binary, b"\0{\"type\":\"not_sent\"}\n").unwrap();
        assert!(
            not_sent_tags(&binary).is_err(),
            "a binary-skipped search is not complete"
        );
        std::fs::write(&binary, b"\xff\n").unwrap();
        assert!(
            not_sent_tags(&binary).is_err(),
            "unsearchable bytes never mean no tags"
        );
        let empty_dir = dir.path().join("empty-directory");
        std::fs::create_dir(&empty_dir).unwrap();
        assert_eq!(literal_rg(&empty_dir).unwrap().status.code(), Some(1));
        assert!(
            not_sent_tags(&empty_dir).is_err(),
            "rg exit 1 does not prove a ledger was read"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let unreadable = dir.path().join("unreadable.jsonl");
            std::fs::write(&unreadable, "{\"type\":\"not_found\"}\n").unwrap();
            let old = std::fs::metadata(&unreadable).unwrap().permissions();
            std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o0)).unwrap();
            let output = literal_rg(&unreadable).unwrap();
            let result = not_sent_tags(&unreadable);
            std::fs::set_permissions(&unreadable, old).unwrap();
            assert!(
                output.status.code().unwrap() >= 2,
                "permission read failure must run as an unprivileged user"
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn d4act3_rollback_scan_requires_wait_and_every_activated_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let clean = dir.path().join("clean.jsonl");
        let tagged = dir.path().join("tagged.jsonl");
        let missing = dir.path().join("missing.jsonl");
        std::fs::write(&clean, "{\"type\":\"not_found\"}\n").unwrap();
        std::fs::write(&tagged, "{\"type\" : \"not_sent\"}\n").unwrap();
        let paths = vec![clean.clone(), tagged.clone(), missing.clone()];
        for (off, joined) in [(false, false), (false, true), (true, false)] {
            assert_eq!(
                scan_activated_ledgers(&paths, off, joined, true, not_sent_tags).unwrap_err(),
                "confirm OFF and wait for writer quiescence before scanning"
            );
        }
        assert!(scan_activated_ledgers(&paths, true, true, false, not_sent_tags).is_err());
        assert!(scan_activated_ledgers(&[], true, true, true, not_sent_tags).is_err());
        let reads = scan_activated_ledgers(&paths, true, true, true, not_sent_tags).unwrap();
        assert_eq!(
            reads.len(),
            3,
            "retain all results even after a tag or read failure"
        );
        assert!(matches!(reads[0].1, Ok(0)));
        assert!(matches!(reads[1].1, Ok(1)));
        assert!(reads[2].1.is_err());
        assert!(!permits_old(&reads), "a partial set of reads cannot pass");
        assert!(permits_old(
            &scan_activated_ledgers(&[clean], true, true, true, not_sent_tags).unwrap()
        ));
        assert!(!permits_old(
            &scan_activated_ledgers(&[tagged], true, true, true, not_sent_tags).unwrap()
        ));
        assert!(!permits_old(
            &scan_activated_ledgers(&[missing], true, true, true, not_sent_tags).unwrap()
        ));
    }
}

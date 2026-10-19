#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::os::unix::fs::PermissionsExt;

use super::*;

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        fs::write(
            dir.path().join("screen"),
            "────────────────────\n❯ \n────────────────────\n",
        )
        .unwrap();
        let script = format!(
            "#!/bin/sh\nset -e\ncd '{root}' || exit 9\necho \"$2\" >> log\n\
             case \"$2\" in\n\
             load-buffer) for last do :; done; cp \"$last\" buffer ;;\n\
             paste-buffer) {{ printf '────────────────────\\n❯ '; cat buffer; printf '\\n────────────────────\\n'; }} > screen ;;\n\
             capture-pane) cat screen ;;\n\
             esac\nexit 0\n",
            root = dir.path().display(),
        );
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self(dir)
    }

    fn pane(&self) -> TmuxPane {
        let mut pane = TmuxPane::with_program(
            &format!("submit-{}", self.0.path().display()),
            self.0.path().join("tmux"),
            Duration::from_secs(5),
        );
        pane.attest_test_nonce("submit-test");
        pane
    }

    fn calls(&self, command: &str) -> usize {
        fs::read_to_string(self.0.path().join("log"))
            .unwrap_or_default()
            .lines()
            .filter(|line| *line == command)
            .count()
    }
}

#[test]
fn an_ungated_submit_loads_pastes_and_enters_once() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.pane().with_composer(|p| p.submit("hello")),
        Some(SendOutcome::Sent)
    );
    assert_eq!(fixture.calls("load-buffer"), 1);
    assert_eq!(fixture.calls("paste-buffer"), 1);
    assert_eq!(fixture.calls("send-keys"), 1);
}

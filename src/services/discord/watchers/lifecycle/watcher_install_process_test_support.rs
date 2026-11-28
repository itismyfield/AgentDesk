//! Isolated production entry fixtures; every tmux command goes to the child executable.
use std::path::PathBuf;

const CHILD: &str = "ADK_WATCHER_FIXTURE_CHILD";
const FAKE_TMUX: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "ROOT/tmux-calls"
while [ "${1#-}" != "$1" ]; do
  case "$1" in -V) echo 'tmux fixture'; exit 0 ;; -L|-S) shift ;; esac
  shift
done
read -r state < "ROOT/pane" || state="unreadable"
case "$state" in busy|idle|dead|deadpane|unanswered|listfail) ;; *) printf '%s\n' "$* on pane '$state'" >> "ROOT/tmux-errors"; exit 97 ;; esac
case "$1" in
  list-sessions) /bin/cat "ROOT/tmux-sessions"; exit $? ;;
  has-session) [ "$state" = dead ] && { echo "can't find session" >&2; exit 1; }
    [ "$state" = unanswered ] && { echo 'error connecting to fixture socket' >&2; exit 1; }; exit 0 ;;
  list-panes) [ "$state" = listfail ] && exit 1
    case "$state" in dead|deadpane) echo 1 ;; *) echo 0 ;; esac; exit 0 ;;
  capture-pane) [ -s "ROOT/draft" ] && { /usr/bin/awk -v draft="ROOT/draft" 'index($0, "Ask Codex") {
        lead = "\033[1m›\033[0m "; while ((getline row < draft) > 0) { print lead row; lead = "  " }; next } { print }' "ROOT/composer"; exit 0; }
    [ -f "ROOT/composer" ] && { /bin/cat "ROOT/composer"; exit 0; }
    [ "$state" = busy ] && printf '%s\n' '⏺ Running a fixture command'; exit 0 ;;
  send-keys|paste-buffer|delete-buffer|load-buffer)
    [ -f "ROOT/composer" ] || { printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97; }
    printf '%s\n' "$*" >> "ROOT/tmux-input"
    buffer=; target=; prev=
    for arg; do case "$prev" in -b) buffer=$arg ;; -t) target=$arg ;; esac; prev=$arg; last=$arg; done
    refuse() { printf '%s\n' "refused $*" >> "ROOT/tmux-errors"; exit 97; }
    [ "$1" = load-buffer ] || [ "$1" = delete-buffer ] || [ "$target" = "$(/bin/cat "ROOT/composer-target")" ] || refuse "$@"
    /bin/mkdir -p "ROOT/buffers"
    case "$1 $*" in
      load-buffer*) [ -n "$buffer" ] || refuse "$@"; /bin/cat "$last" > "ROOT/buffers/$buffer" ;;
      delete-buffer*) /bin/rm -f "ROOT/buffers/$buffer" ;;
      paste-buffer*) [ -f "ROOT/buffers/$buffer" ] || refuse "$@"; /bin/cat "ROOT/buffers/$buffer" >> "ROOT/draft"
        case " $* " in *" -d "*) /bin/rm -f "ROOT/buffers/$buffer" ;; esac ;;
      *" -l "*) printf '%s' "$last" >> "ROOT/draft" ;;
      *) for key; do case "$key" in
           C-u) /bin/rm -f "ROOT/draft" ;;
           Enter|C-m) [ -s "ROOT/draft" ] && { /bin/cat "ROOT/draft"; printf '\n%s\n' "--8<--"; } >> "ROOT/tmux-submits"
             /bin/rm -f "ROOT/draft" ;; esac; done ;;
    esac; exit 0 ;;
  show-options|show-option) exit 0 ;;
  display-message) [ -f "ROOT/composer" ] && case "$*" in *pane_dead*) echo 0; exit 0 ;;
      *pane_width*) echo '120 40'; exit 0 ;; esac
    [ -f "ROOT/pane.pid" ] && /bin/cat "ROOT/pane.pid"; exit 0 ;;
  kill-session) printf '%s\n' "$*" >> "ROOT/tmux-kills"; exit 0 ;;
  *) printf '%s\n' "$*" >> "ROOT/tmux-errors"; exit 97 ;;
esac
"#;

pub(in crate::services::discord) fn root() -> PathBuf {
    PathBuf::from(std::env::var_os("AGENTDESK_ROOT_DIR").unwrap())
}

pub(in crate::services::discord) fn isolated(module: &str, name: &str) -> bool {
    if std::env::var_os(CHILD).is_some() {
        crate::services::discord::terminal_delivery_custody::install_dl_ports_for_tests();
        return true;
    }
    use std::os::unix::fs::PermissionsExt;
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path();
    for dir in ["codex/sessions", "claude/projects"] {
        std::fs::create_dir_all(path.join(dir)).unwrap();
    }
    std::fs::write(path.join("pane"), "busy\n").unwrap();
    std::fs::write(path.join("tmux-sessions"), []).unwrap();
    let tmux = path.join("tmux");
    std::fs::write(
        &tmux,
        FAKE_TMUX.replace("ROOT", &path.display().to_string()),
    )
    .unwrap();
    std::fs::set_permissions(tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    let module = module.split_once("::").unwrap().1;
    let exact = format!("{module}::{name}");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &exact, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", path)
        .env("TMPDIR", path)
        .env("CODEX_HOME", path.join("codex"))
        .env("CLAUDE_CONFIG_DIR", path.join("claude"))
        .env("PATH", format!("{}:/usr/bin:/bin", path.display()))
        .env("AGENTDESK_STATUS_INTERVAL_SECS", "0")
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9")
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env_remove("DATABASE_URL")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{exact}:\n{stdout}\n{stderr}");
    assert!(stdout.contains("running 1 test"), "{stdout}");
    assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
    let errors = std::fs::read_to_string(path.join("tmux-errors")).unwrap_or_default();
    assert!(errors.is_empty(), "unexpected fake tmux command: {errors}");
    false
}

pub(in crate::services::discord) fn list_sessions(sessions: &[&str]) {
    std::fs::write(root().join("tmux-sessions"), sessions.join("\n")).unwrap();
}

/// A ready Codex composer for `session` on `capture-pane`. Writes must name its pane and a loaded
/// buffer; typed or pasted text shows as the draft, and Enter on a non-empty draft submits it.
pub(in crate::services::discord) fn ready_codex_composer(session: &str) {
    std::fs::write(root().join("composer-target"), format!("={session}:")).unwrap();
    std::fs::write(
        root().join("composer"),
        include_str!("../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi"),
    )
    .unwrap();
}

/// Every pane write the composer received, one command per line.
pub(in crate::services::discord) fn pane_input() -> String {
    std::fs::read_to_string(root().join("tmux-input")).unwrap_or_default()
}

/// Each whole draft Enter submitted to the composer's pane, in order.
pub(in crate::services::discord) fn pane_submits() -> Vec<String> {
    let submits = std::fs::read_to_string(root().join("tmux-submits")).unwrap_or_default();
    submits
        .split_terminator("\n--8<--\n")
        .map(str::to_owned)
        .collect()
}

pub(in crate::services::discord) fn pane(state: &str) {
    std::fs::write(root().join("pane.next"), format!("{state}\n")).unwrap();
    std::fs::rename(root().join("pane.next"), root().join("pane")).unwrap();
}

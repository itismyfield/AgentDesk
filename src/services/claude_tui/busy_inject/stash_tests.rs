use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use super::*;

thread_local! {
    pub(super) static USE_PRODUCTION_CHANNELS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

static STASH_CHANNELS: LazyLock<Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

/// Opens only this test's channel; unrelated parallel tests retain their own switches.
pub(crate) struct StashChannelGuard(u64);

impl StashChannelGuard {
    pub(crate) fn new(channel: u64) -> Self {
        let mut channels = STASH_CHANNELS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(
            channels.insert(channel),
            "stash channel already enabled by a test"
        );
        Self(channel)
    }
}

impl Drop for StashChannelGuard {
    fn drop(&mut self) {
        STASH_CHANNELS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&self.0);
    }
}

pub(crate) fn configured_channels() -> Vec<u64> {
    STASH_CHANNELS
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .iter()
        .copied()
        .collect()
}

const TEXT: &str = "are you there?";

/// Windows only bound the waits for evidence that never comes; found evidence returns at once.
/// Lock retries ride out a map shard another test holds for an instant.
const FAST: Timing = Timing {
    lock_retries: &[
        Duration::ZERO,
        Duration::from_millis(5),
        Duration::from_millis(10),
    ],
    settle: Duration::ZERO,
    confirm_window: Duration::ZERO,
    confirm_poll: Duration::from_millis(20),
};

/// A scripted Claude TUI behind `tmux`: composer, stash, the single-slot C-s, a submit handing the
/// stash back, attach generation, a person, a barrier, and a faint idle placeholder only `-e` keeps.
pub(crate) const FAKE_TUI: &str = r#"#!/bin/sh
d='@D@'
echo "$*" >> "$d/log"
cursor() {
  if [ -f "$d/chip" ]; then
    wc -m < "$d/chip" | tr -d ' ' > "$d/cursor_x"; echo 0 > "$d/cursor_y"
  else
    tail -n 1 "$d/composer" | wc -m | tr -d ' ' > "$d/cursor_x"
    wc -l < "$d/composer" | tr -d ' ' > "$d/cursor_y"
  fi
}
restore() {
  mv "$d/stash" "$d/composer"
  if [ -f "$d/stash-chip" ]; then mv "$d/stash-chip" "$d/chip"; else rm -f "$d/chip"; fi
}
cs() {
  if [ -s "$d/composer" ]; then
    mv "$d/composer" "$d/stash"; : > "$d/composer"
    if [ -f "$d/chip" ]; then mv "$d/chip" "$d/stash-chip"; else rm -f "$d/stash-chip"; fi
  elif [ -f "$d/stash" ]; then restore; fi
}
enter() {
  [ -s "$d/composer" ] || return 0
  # The acceptance record carries what the composer held at Enter.
  awk 'BEGIN { printf "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"content\":\"" }
    { gsub(/"/, "\""); printf "%s%s", (NR > 1 ? "\\n" : ""), $0 } END { print "\"}" }' "$d/composer" >> '@T@'
  : > "$d/composer"
  rm -f "$d/chip"
  [ -f "$d/stash" ] || return 0
  if [ -f "$d/restore_after" ]; then cp "$d/restore_after" "$d/restore_in"; else restore; fi
}
paste() {
  cat "$d/buffer" >> "$d/composer"
  if [ -f "$d/fold" ]; then
    lines=$(wc -l < "$d/buffer" | tr -d ' ')
    if [ "$lines" = 0 ]; then printf '[Pasted text #1]' > "$d/chip"
    else printf '[Pasted text #1 +%s lines]' "$lines" > "$d/chip"; fi
  fi
}
apply() {
  [ "$1" != Enter ] || [ ! -f "$d/human.before-Enter" ] || . "$d/human.before-Enter"
  echo "$1" >> "$d/applied"
  case "$1" in C-s) cs ;; Enter) enter ;; paste) paste ;; esac
  cursor
  [ "$1" != paste ] || [ ! -f "$d/human.after-paste" ] || . "$d/human.after-paste"
  [ "$1" != C-s ] || [ ! -f "$d/human.after-C-s" ] || . "$d/human.after-C-s"
  # The key took effect but the reply never came back.
  [ ! -f "$d/lost.$1" ] || exit 1
}
attach() { echo 1 > "$d/attached"; echo "$1" > "$d/last"; }
detach() { echo 0 > "$d/attached"; }
type_text() { printf '%s' "$1" >> "$d/composer"; [ ! -f "$d/chip" ] || printf '%s' "$1" >> "$d/chip"; cursor; }
render() {
  cat "$d/head"
  if [ -f "$d/status" ]; then cat "$d/status"
  elif [ -f "$d/stash" ]; then cat "$d/stashrow"
  elif [ -f "$d/effortrow" ]; then cat "$d/effortrow"; else echo; fi
  cat "$d/border"
  if [ -s "$d/composer" ]; then
    if [ -f "$d/chip" ]; then printf '%s%s\n' "$(cat "$d/prompt")" "$(cat "$d/chip")"
    else awk -v p="$(cat "$d/prompt")" 'NR==1{print p $0; next} {print ($0 == "" ? "" : "  " $0)}' "$d/composer"; fi
  elif [ -s "$d/placeholder" ] && ! grep -q 'esc to interrupt' "$d/head"; then
    printf '%s\033[2m%s\033[0m\n' "$(cat "$d/prompt")" "$(cat "$d/placeholder")"
  else cat "$d/prompt"; echo; fi
  cat "$d/border" "$d/footer"
}
case "$2" in
display-message) echo "$(cat "$d/attached"),$(cat "$d/last"),$(cat "$d/width"),$(cat "$d/height")" ;;
capture-pane)
  n=$(($(cat "$d/cap.n" 2>/dev/null || echo 0) + 1)); echo $n > "$d/cap.n"
  [ -f "$d/fail.cap.$n" ] && exit 1
  # With a barrier, the first capture after Enter waits for go, 3s at most.
  if [ -f "$d/barrier" ] && grep -qx Enter "$d/applied" 2>/dev/null; then
    rm "$d/barrier"; : > "$d/reached"; i=0
    while [ ! -f "$d/go" ] && [ $i -lt 300 ]; do sleep 0.01; i=$((i+1)); done
  fi
  if [ -f "$d/restore_in" ]; then
    k=$(cat "$d/restore_in")
    if [ "$k" = 0 ]; then restore; rm "$d/restore_in"
    elif [ "$k" != never ]; then echo $((k-1)) > "$d/restore_in"; fi
  fi
  case " $* " in *" -e "*) render ;; *) render | sed "s/$(printf '\033')\[[0-9;]*m//g" ;; esac
  case " $* " in *agentdesk-busy-inject-cursor*)
    echo "agentdesk-busy-inject-cursor $(cat "$d/cursor_x" 2>/dev/null || echo 0),$(cat "$d/cursor_y" 2>/dev/null || echo 0)" ;;
  esac
  [ -f "$d/human.$n" ] && . "$d/human.$n"
  ;;
load-buffer) for last do :; done; cp "$last" "$d/buffer"; printf '%s\n' "${4#agentdesk-busy-inject-}" > "$d/nonce" ;;
if-shell)
  a=$(cat "$d/attached"); l=$(cat "$d/last"); ok=1
  expected=$(printf '%s' "$6" | sed -n 's/.*session_attached},\([0-9]*\)}.*/\1/p')
  [ -z "$expected" ] || [ "$a" = "$expected" ] || ok=0
  case "$6" in *session_last_attached*)
    g0=$(printf '%s' "$6" | sed 's/.*session_last_attached},\([0-9]*\)}.*/\1/')
    [ "$l" = "$g0" ] || ok=0 ;;
  esac
  case "$6" in *pane_width*)
    w=$(printf '%s' "$6" | sed -n 's/.*pane_width},\([0-9]*\)}.*/\1/p')
    h=$(printf '%s' "$6" | sed -n 's/.*pane_height},\([0-9]*\)}.*/\1/p')
    [ "$(cat "$d/width")" = "$w" ] && [ "$(cat "$d/height")" = "$h" ] || ok=0 ;;
  esac
  case "$6" in *cursor_x*)
    x=$(printf '%s' "$6" | sed -n 's/.*cursor_x},\([0-9]*\)}.*/\1/p')
    y=$(printf '%s' "$6" | sed -n 's/.*cursor_y},\([0-9]*\)}.*/\1/p')
    [ "$(cat "$d/cursor_x" 2>/dev/null || echo 0)" = "$x" ] && [ "$(cat "$d/cursor_y" 2>/dev/null || echo 0)" = "$y" ] || ok=0 ;;
  esac
  [ "$ok" = 1 ] || { echo "agentdesk-busy-inject-vetoed $a $l"; exit 0; }
  set -- $7; eval "key=\${$#}"; [ "$1" = paste-buffer ] && key=paste
  apply "$key" ;;
paste-buffer) apply paste ;;
send-keys) for last do :; done; apply "$last" ;;
esac
exit 0
"#;

struct Tui {
    dir: tempfile::TempDir,
    transcript: PathBuf,
}

impl Tui {
    /// A busy 160x50 pane, wide enough for the input header to render flat, whose composer holds
    /// `draft` (empty for none) and no stash.
    fn new(draft: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/busy-inject-tmp");
        fs::create_dir_all(&root).unwrap();
        let dir = tempfile::tempdir_in(root).unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        let busy_turn = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
            {\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"working\"}]}}\n";
        fs::write(&transcript, busy_turn).unwrap();
        let script = FAKE_TUI
            .replace("@D@", &dir.path().display().to_string())
            .replace("@T@", &transcript.display().to_string());
        let program = dir.path().join("tmux");
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        let tui = Self { dir, transcript };
        for (name, value) in [
            ("attached", "0".to_string()),
            ("last", "100".to_string()),
            ("width", "160".to_string()),
            ("height", "50".to_string()),
            ("cursor_x", "0".to_string()),
            ("cursor_y", "0".to_string()),
            (
                "head",
                "⏺ Working on it.\n\n✻ Thinking… (12s · esc to interrupt)\n".to_string(),
            ),
            ("border", format!("{}\n", "─".repeat(60))),
            ("prompt", "❯\u{00a0}".to_string()),
            (
                "footer",
                "  ⏵⏵ bypass permissions on (shift+tab to cycle)\n".to_string(),
            ),
            ("stashrow", format!("{:>58}\n", "› stashed")),
            ("placeholder", "Try \"edit <filepath> to...\"".to_string()),
            ("composer", draft.to_string()),
        ] {
            tui.put(name, &value);
        }
        tui
    }

    fn put(&self, name: &str, value: &str) {
        fs::write(self.dir.path().join(name), value).unwrap();
    }

    fn get(&self, name: &str) -> Option<String> {
        fs::read_to_string(self.dir.path().join(name)).ok()
    }

    fn session(&self) -> String {
        let name = self.dir.path().file_name().unwrap().to_string_lossy();
        let name: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        format!("stash-{name}")
    }

    fn next_nonce(&self) -> String {
        let nonce = fresh_nonce();
        self.put("attempt.nonce", &nonce);
        nonce
    }

    fn run_text(&self, text: &str, timing: &Timing) -> Report {
        let session = self.session();
        let pane = Pane::with_program(&session, self.dir.path().join("tmux"));
        let nonce = self.next_nonce();
        let request = Request {
            session: &session,
            transcript: &self.transcript,
            source: "iMessage",
            author: "200",
            name: "ann",
            nonce: &nonce,
            text,
        };
        inject_gated(&pane, &request, timing, true)
    }

    fn run(&self) -> Report {
        self.run_text(TEXT, &FAST)
    }

    /// Keys the server applied for AgentDesk, in order.
    fn applied(&self) -> Vec<String> {
        let log = self.get("applied").unwrap_or_default();
        log.lines().map(str::to_string).collect()
    }

    /// (composer, stash) as the person would find them.
    fn drafts(&self) -> (String, Option<String>) {
        (self.get("composer").unwrap_or_default(), self.get("stash"))
    }

    /// Every submission the fake recorded, as the composer held it at Enter.
    fn records(&self) -> Vec<String> {
        let transcript = fs::read_to_string(&self.transcript).unwrap();
        let records = transcript.lines().skip(2).map(|line| {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            record["content"].as_str().unwrap().to_string()
        });
        records.collect()
    }

    fn alerts(&self) -> Vec<serde_json::Value> {
        let session = self.session();
        crate::services::observability::events::recent(usize::MAX)
            .into_iter()
            .filter(|event| event.event_type == "busy_inject_draft_unrecovered")
            .map(|event| event.payload)
            .filter(|payload| payload["tmux_session"] == session.as_str())
            .collect()
    }
}

#[test]
fn visible_draft_is_stashed_once_and_submitted_without_post_reads() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for draft in ["human draft A", "first line\n\n  들여쓰기"] {
        let tui = Tui::new(draft);
        tui.put("attached", "4");
        tui.put(
            "human.after-paste",
            r#"echo 101 > "$d/last"; echo 20 > "$d/width""#,
        );
        assert_eq!(tui.run().outcome, Outcome::Injected);
        assert_eq!(tui.applied(), ["C-s", "paste", "Enter"]);
        assert_eq!(tui.drafts(), (draft.into(), None));
        assert_eq!(tui.get("cap.n").as_deref(), Some("1\n"));
        assert_eq!(tui.records(), [tui.get("buffer").unwrap()]);
        assert!(tui.alerts().is_empty());
        assert!(!super::super::composer_lock::draft_guarded(&tui.session()));
    }
}

/// Also when streamed output above the box hides the `› stashed` marker.
#[test]
fn empty_never_toggles_an_old_stash_back_before_pasting() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for status in [None, Some("  12. streamed line\n")] {
        let tui = Tui::new("");
        tui.put("stash", "old stashed draft");
        if let Some(status) = status {
            tui.put("status", status);
        }
        assert_eq!(tui.run().outcome, Outcome::Injected, "{status:?}");
        assert_eq!(tui.applied(), ["paste", "Enter"], "{status:?}");
        assert_eq!(tui.records(), [tui.get("buffer").unwrap()]);
        assert_eq!(tui.drafts(), ("old stashed draft".into(), None));
    }
}

#[test]
fn a_shown_stash_is_never_overwritten_by_parking_a_new_draft() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tui = Tui::new("B");
    tui.put("stash", "A");
    assert_eq!(tui.run().outcome, Outcome::Injected);
    assert_eq!(tui.applied(), ["paste", "Enter"]);
    assert_eq!(tui.records(), [format!("B{}", tui.get("buffer").unwrap())]);
    assert_eq!(tui.drafts(), ("A".into(), None));
}

#[test]
fn unread_box_sends_without_stashing_and_never_holds_the_next_input() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let tui = Tui::new("person draft");
    tui.put("attached", "4");
    tui.put("border", "incomplete border\n");
    for payload in ["first input", "later input"] {
        assert_eq!(tui.run_text(payload, &FAST).outcome, Outcome::Injected);
        assert!(!super::super::composer_lock::draft_guarded(&tui.session()));
    }
    assert_eq!(tui.applied(), ["paste", "Enter", "paste", "Enter"]);
    assert!(tui.records()[0].starts_with("person draft[User:"));
    assert!(tui.alerts().is_empty());
}

#[test]
fn an_unlisted_channel_sends_without_stashing_a_visible_draft() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for channel in [None, Some(2)] {
        let tui = Tui::new("draft");
        let session = tui.session();
        let pane = Pane::with_program(&session, tui.dir.path().join("tmux"));
        let nonce = tui.next_nonce();
        let request = Request {
            session: &session,
            transcript: &tui.transcript,
            source: "iMessage",
            author: "200",
            name: "ann",
            nonce: &nonce,
            text: TEXT,
        };
        assert_eq!(
            inject_listed(&pane, &request, channel, &[1], &FAST).outcome,
            Outcome::Injected
        );
        assert_eq!(tui.applied(), ["paste", "Enter"]);
    }
}

#[test]
fn menus_and_permission_cards_precede_stash_and_paste() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    for menu in [
        "Run this tool? allow / deny",
        "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel",
    ] {
        let tui = Tui::new("draft");
        tui.put("head", &format!("{menu}\n"));
        assert_eq!(tui.run().outcome, Outcome::NotSent(Veto::Modal));
        assert!(tui.applied().is_empty());
        assert_eq!(tui.drafts(), ("draft".into(), None));
    }
}

mod environment_tests {
    use super::*;
    #[test]
    fn production_environment_initialization_selects_only_the_listed_channel() {
        const CHILD: &str = "ADK_BUSY_INJECT_STASH_CONFIG_SUBPROCESS";
        if std::env::var_os(CHILD).is_none() {
            let test = format!(
                "{}::production_environment_initialization_selects_only_the_listed_channel",
                module_path!().split_once("::").unwrap().1
            );
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env(CHILD, "1")
                .env("ADK_BUSY_INJECT_STASH_CHANNELS", "invalid, 6245, , 6245")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
            return;
        }
        let _runtime = crate::config::TestRuntimeRootGuard::new();
        USE_PRODUCTION_CHANNELS.set(true);
        for channel in [Some(6245), Some(6246), None] {
            let tui = Tui::new("human draft A");
            tui.put("attached", "4");
            let session = tui.session();
            let pane = Pane::with_program(&session, tui.dir.path().join("tmux"));
            let nonce = tui.next_nonce();
            let request = Request {
                session: &session,
                transcript: &tui.transcript,
                source: "iMessage",
                author: "200",
                name: "ann",
                nonce: &nonce,
                text: TEXT,
            };
            assert_eq!(
                inject_report(&pane, &request, channel, &FAST).outcome,
                Outcome::Injected
            );
            let expected = if channel == Some(6245) {
                vec!["C-s", "paste", "Enter"]
            } else {
                vec!["paste", "Enter"]
            };
            assert_eq!(tui.applied(), expected);
        }
    }
}

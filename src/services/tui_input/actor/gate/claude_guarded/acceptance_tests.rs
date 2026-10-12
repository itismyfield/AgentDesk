use super::*;
use crate::services::claude_tui::host_input::guarded_submission::folded_payload;
use crate::services::claude_tui::input::plan_prompt_submit;
use crate::services::tui_o::shadow::ShadowProvider;

fn composer(body: &str) -> String {
    format!(
        "────────────────────\n❯ {}\n────────────────────\n",
        body.replace('\n', "\n  ")
    )
}

fn owns(body: &str, frame: &str) -> bool {
    let payload = [TuiInputAction::PasteBuffer(frame.to_string())];
    own_claude_guarded_draft(
        &composer(body),
        DraftEvidence {
            frame,
            payload: &payload,
            pre_empty: true,
            exclusive_writer: true,
        },
    )
}

#[test]
fn cg_b1_utf16_800_801_includes_emoji_and_hangul() {
    for (frame, expected) in [
        ("a".repeat(799), false),
        ("a".repeat(800), false),
        ("a".repeat(801), true),
        (format!("{}😀", "a".repeat(798)), false),
        (format!("{}😀", "a".repeat(799)), true),
        ("😀".repeat(400), false),
        ("😀".repeat(401), true),
        ("가".repeat(800), false),
        ("가".repeat(801), true),
    ] {
        assert_eq!(
            claude_should_fold(&frame),
            expected,
            "UTF16={}",
            frame.encode_utf16().count()
        );
        assert_eq!(owns("[Pasted text #7]", &frame), expected);
    }
}

#[test]
fn cg_b1_lf2_lf3_and_lf_in_utf16_budget() {
    for (frame, folded) in [
        ("a\nb".to_string(), false),
        ("a\nb\nc".to_string(), false),
        ("a\nb\nc\nd".to_string(), true),
        (format!("{}\nb", "a".repeat(798)), false),
        (format!("{}\nb", "a".repeat(799)), true),
    ] {
        let lf = frame.bytes().filter(|b| *b == b'\n').count();
        assert_eq!(claude_should_fold(&frame), folded, "{lf} LF");
        assert_eq!(
            owns(&format!("[Pasted text #9 +{lf} lines]"), &frame),
            folded
        );
        assert!(!owns(
            &format!("[Pasted text #9 +{} lines]", lf + 1),
            &frame
        ));
        assert!(owns(&frame, &frame));
    }
}

#[test]
fn cg_b1_whitespace_is_exact_and_unknown_wraps_hold() {
    for frame in [
        "a b",
        "a  b",
        " a b ",
        "가 😀  b",
        "a\n b",
        "a\n\nb",
        "a\t b",
        "a\n",
    ] {
        assert!(owns(frame, frame), "exact {frame:?}");
        for other in ["ab", "a b", "a  b", "a\nb", "a\n b", "a\n\nb", "a\tb"] {
            if frame != other {
                assert!(!owns(other, frame), "foreign {other:?} for {frame:?}");
            }
        }
    }
    assert!(!owns("hello\nworld", "helloworld"));
    assert!(!owns("hello \nworld", "hello world"));
    assert!(!owns("a    b", "a\tb"));
}

#[test]
fn cg_b1_folded_requires_exact_empty_exclusive_and_single_same_frame_paste() {
    let frame = "a\nb\nc\nd";
    let same = TuiInputAction::PasteBuffer(frame.into());
    let variants = [
        (vec![same.clone()], true, true, true),
        (vec![same.clone()], false, true, false),
        (vec![same.clone()], true, false, false),
        (
            vec![TuiInputAction::Literal(frame.into())],
            true,
            true,
            false,
        ),
        (
            vec![TuiInputAction::PasteBuffer("foreign\nx\ny\nz".into())],
            true,
            true,
            false,
        ),
        (vec![same.clone(), same], true, true, false),
        (vec![], true, true, false),
    ];
    for (payload, pre_empty, exclusive_writer, expected) in variants {
        assert_eq!(
            own_claude_guarded_draft(
                &composer("[Pasted text #8 +3 lines]"),
                DraftEvidence {
                    frame,
                    payload: &payload,
                    pre_empty,
                    exclusive_writer,
                }
            ),
            expected,
            "{payload:?}, empty={pre_empty}, exclusive={exclusive_writer}"
        );
    }
    for body in [
        "[Pasted text # +3 lines]",
        "[Pasted text #x +3 lines]",
        "[Pasted text #8 +2 lines]",
        "[Pasted text #8]",
        "[Pasted text #8 +3 lines]foreign",
        " [Pasted text #8 +3 lines]",
        "[Pasted text #8 +3 lines]\n[Pasted text #9 +3 lines]",
    ] {
        assert!(!owns(body, frame), "{body}");
    }
    assert!(!owns("[Pasted text #8 +3 lines]", "short\nLF2\nframe"));
}

#[test]
fn cg_b1_exact_reader_rejects_modal_scrollback_and_missing_capture() {
    let frame = "exact body";
    assert!(owns(frame, frame));
    let payload = [TuiInputAction::Literal(frame.into())];
    for capture in [
        "".into(),
        format!("❯ {frame}\n{}", composer("foreign")),
        format!(
            "{}\nDo you want to proceed?\n1. Yes\n2. No\nEnter to confirm · esc to cancel",
            composer(frame)
        ),
        composer("foreign"),
    ] {
        assert!(
            !own_claude_guarded_draft(
                &capture,
                DraftEvidence {
                    frame,
                    payload: &payload,
                    pre_empty: true,
                    exclusive_writer: true,
                }
            ),
            "{capture}"
        );
    }
}

#[test]
fn cg_b1_guarded_plan_replans_utf16_fold_only_and_preserves_frame_bytes() {
    for (frame, paste) in [
        ("a".repeat(532), false),
        ("a".repeat(533), false),
        ("a".repeat(799), false),
        ("a".repeat(800), false),
        ("a".repeat(801), true),
        (format!("{}😀", "a".repeat(798)), false),
        (format!("{}😀", "a".repeat(799)), true),
        ("😀".repeat(401), true),
        ("가".repeat(800), false),
        ("가".repeat(801), true),
        ("가  😀".into(), false),
        ("a\nb\nc".into(), true),
        ("a\nb\nc\nd".into(), true),
    ] {
        let legacy = plan_prompt_submit(&frame).unwrap();
        assert_eq!(legacy.last(), Some(&TuiInputAction::Enter));
        let (_, payload) = legacy.split_last().unwrap();
        let folded = folded_payload(&frame);
        let selected = folded.as_ref().map_or(payload, |paste| &paste[..]);
        if paste {
            assert_eq!(
                selected,
                [TuiInputAction::PasteBuffer(frame.clone())],
                "{frame}"
            );
        } else {
            assert_eq!(selected, payload);
            assert!(
                selected
                    .iter()
                    .all(|a| matches!(a, TuiInputAction::Literal(_)))
            );
        }
        let sent: String = selected
            .iter()
            .map(|a| match a {
                TuiInputAction::Literal(s) | TuiInputAction::PasteBuffer(s) => s.as_str(),
                _ => panic!("non-payload"),
            })
            .collect();
        assert_eq!(sent.as_bytes(), frame.as_bytes());
    }
}

#[test]
fn cg_b1_off_keeps_legacy_own_draft_consumers_and_plans_byte_identical() {
    let mut results = Vec::new();
    for provider in [ShadowProvider::Claude, ShadowProvider::Codex] {
        for (body, frame, empty) in [
            ("a b", "a  b", true),
            ("a\nb", "ab", true),
            ("foreign", "frame", true),
            ("a", "a", false),
            ("a", "a", true),
            ("[Pasted text #7 +2 lines]", "a\nb\nc", true),
            ("[Pasted Content 900 chars]", "a", true),
        ] {
            let capture = if provider == ShadowProvider::Claude {
                composer(body)
            } else {
                format!(
                    "› {}\n\n  gpt-5.4 · Fast off · Context 100% left",
                    body.replace('\n', "\n  ")
                )
            };
            results.push(u8::from(super::super::own_draft(
                provider, &capture, frame, empty,
            )));
        }
    }
    assert_eq!(
        results.as_slice(),
        b"\x01\x01\x00\x00\x01\x01\x00\x01\x01\x00\x00\x01\x00\x00"
    );
    for (raw, expected) in [
        (
            "a  b",
            vec![
                TuiInputAction::Literal("a  b".into()),
                TuiInputAction::Enter,
            ],
        ),
        (
            "a\r\nb\rc",
            vec![
                TuiInputAction::PasteBuffer("a\nb\nc".into()),
                TuiInputAction::Enter,
            ],
        ),
    ] {
        assert_eq!(plan_prompt_submit(raw).unwrap(), expected);
    }
    let frame = format!("{}😀", "a".repeat(799));
    let legacy = plan_prompt_submit(&frame).unwrap();
    assert!(
        legacy[..legacy.len() - 1]
            .iter()
            .all(|a| matches!(a, TuiInputAction::Literal(_)))
    );
}

#[cfg(unix)]
#[test]
fn cg_b1_real_tmux_plan_transport_and_capture_draft_without_spy() {
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::claude_tui::host_input::InputTransport;
    use crate::services::tui_input::submission_tmux::SubmissionTmux;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dir = tempfile::tempdir().unwrap();
    let path = std::env::var("PATH").unwrap();
    let real = std::env::split_paths(&path)
        .map(|p| p.join("tmux"))
        .find(|p| p.is_file())
        .expect("real tmux required");
    let socket = format!("cgb1-{}", std::process::id());
    let shim = dir.path().join("tmux");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nexec '{}' -L {socket} -f /dev/null \"$@\"\n",
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
    let _guards = [
        Guard::set_value_after_shared_test_env_lock(
            "PATH",
            format!("{}:{path}", dir.path().display()).as_ref(),
        ),
        Guard::set_path_after_shared_test_env_lock("TMUX_TMPDIR", dir.path()),
    ];
    struct Server(std::path::PathBuf);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = std::process::Command::new(&self.0)
                .arg("kill-server")
                .output();
        }
    }
    let _server = Server(shim.clone());
    let script = dir.path().join("renderer.py");
    std::fs::write(&script, r#"import os, pathlib, sys, time, tty
tty.setraw(0)
os.write(1, ('\x1b[?2004h' + '─' * 160 + '\r\n❯ \r\n' + '─' * 160 + '\r\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\r\n').encode())
root = pathlib.Path(sys.argv[1])
(root / 'ready').write_text('ready')
data = b''
while not (root / 'render').exists():
    import select
    if select.select([0], [], [], .01)[0]:
        data += os.read(0, 65536)
while select.select([0], [], [], .02)[0]:
    data += os.read(0, 65536)
paste = data.startswith(b'\x1b[200~') and data.endswith(b'\x1b[201~')
frame = data[6:-6] if paste else data
(root / 'received').write_bytes(frame)
(root / 'mode').write_text('paste' if paste else 'literal')
text = frame.decode()
lf = text.count('\n')
fold = len(text.encode('utf-16-le')) // 2 > 800 or lf >= 3
body = ('[Pasted text #7' + (f' +{lf} lines]' if lf else ']')) if paste and fold else text
screen = '\x1b[2J\x1b[H' + '─' * 160 + '\r\n\x1b[39m❯ ' + body.replace('\n', '\r\n  ') + '\r\n' + '─' * 160 + '\r\n'
os.write(1, screen.encode())
time.sleep(30)
"#).unwrap();
    let mut transport = SubmissionTmux;
    for (i, frame) in [
        "a  b".into(),
        "a\nb\nc".into(),
        "a\nb\nc\nd".into(),
        format!("{}😀", "a".repeat(799)),
    ]
    .into_iter()
    .enumerate()
    {
        let root = dir.path().join(format!("case-{i}"));
        std::fs::create_dir(&root).unwrap();
        let session = format!("cgb1-{i}");
        let command = format!("python3 '{}' '{}'", script.display(), root.display());
        let output = std::process::Command::new(&shim)
            .args([
                "new-session",
                "-d",
                "-s",
                &session,
                "-x",
                "160",
                "-y",
                "40",
                &command,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let until = Instant::now() + Duration::from_secs(5);
        while !root.join("ready").exists() {
            assert!(Instant::now() < until, "renderer not ready");
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = transport
            .capture_draft(&session)
            .expect("actual preflight capture");
        assert_eq!(
            super::super::judge_pane(ShadowProvider::Claude, &before),
            super::super::PaneVerdict::Ready
        );
        let plan = plan_prompt_submit(&frame).unwrap();
        let (_, payload) = plan.split_last().unwrap();
        let folded = folded_payload(&frame);
        let selected = folded.as_ref().map_or(payload, |paste| &paste[..]);
        for action in selected {
            let output = match action {
                TuiInputAction::Literal(text) => transport.send_literal(&session, text).unwrap(),
                TuiInputAction::PasteBuffer(text) => {
                    assert!(
                        transport
                            .load_buffer("cgb1", text)
                            .unwrap()
                            .status
                            .success()
                    );
                    transport.paste_buffer(&session, "cgb1", true).unwrap()
                }
                _ => panic!("unexpected action"),
            };
            assert!(output.status.success());
        }
        std::fs::write(root.join("render"), "render").unwrap();
        loop {
            let capture = transport.capture_draft(&session);
            if capture.as_ref().is_some_and(|c| {
                own_claude_guarded_draft(
                    c,
                    DraftEvidence {
                        frame: &frame,
                        payload: selected,
                        pre_empty: true,
                        exclusive_writer: true,
                    },
                )
            }) {
                break;
            }
            assert!(
                Instant::now() < until,
                "fresh draft not captured: {capture:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            std::fs::read(root.join("received")).unwrap(),
            frame.as_bytes()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("mode")).unwrap(),
            if frame.contains('\n') || claude_should_fold(&frame) {
                "paste"
            } else {
                "literal"
            }
        );
    }
    assert!(transport.capture_draft("missing-session").is_none());
}

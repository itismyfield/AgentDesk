//! Real Codex and Claude screens through `/stop`, the Herdr socket's `pane.read` and Escape.

use super::*;
use crate::services::discord::commands::stop::run_slash_stop;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::AtomicBool;

const WORKING: &str = "\x1b[2m•\x1b[0m \x1b[1mWorking\x1b[0m \x1b[2m(3s • esc to interrupt)\x1b[0m";
/// Herdr 0.9.3's own `format: "ansi"` rendering of the 46-column Working screen, rows LF-joined.
const HERDR_RENDERED_46: &str = "\x1b[0m\x1b[2m•\x1b[0m \x1b[0m\x1b[1mWorking\x1b[0m \x1b[0m\x1b[2m(3s • esc to interrupt)\x1b[0m\n\n\x1b[0m\x1b[1m›\x1b[0m \x1b[0m\x1b[2mAsk Codex to do anything\x1b[0m\n\n  \x1b[0m\x1b[38;2;246;226;183mGPT-6.1-Sol xhigh\x1b[0m · \x1b[0m\x1b[38;2;233;144;169mweekly 50% l…\x1b[0m  ⚠ \x1b[0m\x1b[38;2;196;167;103m1\x1b[0m · \x1b[0m\x1b[1mf2\x1b[0m\n";
const PLACEHOLDER: &str = "\x1b[2mAsk Codex to do anything\x1b[0m";
const CUT_WIDTHS: [(usize, &str); 3] = [
    (
        46,
        include_str!("../../../../../tests/fixtures/tui_input/codex-status-sol-46-idle.ansi"),
    ),
    (
        80,
        include_str!("../../../../../tests/fixtures/tui_input/codex-status-sol-80-idle.ansi"),
    ),
    (
        113,
        include_str!("../../../../../tests/fixtures/tui_input/codex-status-astra-113-idle.ansi"),
    ),
];

macro_rules! claude_capture {
    ($name:literal) => {
        include_str!(concat!(
            "../../../../../tests/fixtures/tui_input/claude-2.1.293-",
            $name,
            ".txt"
        ))
    };
}

/// Fronts the rig's socket as Herdr 0.9.3 answers: only a `format: "ansi"` read keeps SGR, with
/// CRLF rows; any other read is plain whatever its `strip_ansi`.
struct HerdrStrips {
    backend: PathBuf,
    /// Each `pane.read` request line as the client wrote it, its id masked.
    reads: Arc<std::sync::Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HerdrStrips {
    fn over(rig: &HerdrRig) -> Self {
        let front = rig.socket().to_path_buf();
        let backend = front.with_extension("rig");
        std::fs::rename(&front, &backend).unwrap();
        let listener = UnixListener::bind(&front).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let reads = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (stopped, rig_socket, log) = (stop.clone(), backend.clone(), reads.clone());
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let Ok((client, _)) = listener.accept() else {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                };
                relay(client, &rig_socket, &log);
            }
        });
        Self {
            backend,
            reads,
            stop,
            thread: Some(thread),
        }
    }
}

fn relay(client: UnixStream, rig_socket: &Path, reads: &std::sync::Mutex<Vec<String>>) {
    let _ = client.set_nonblocking(false);
    let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let mut writer = client.try_clone().unwrap();
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    let _ = reader.read_line(&mut line);
    let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
        return;
    };
    if request["method"] == "pane.read" {
        let id = format!("\"id\":{}", request["id"]);
        let line = line.trim_end().replacen(&id, "\"id\":\"ID\"", 1);
        reads.lock().unwrap().push(line);
    }
    let mut rig = UnixStream::connect(rig_socket).unwrap();
    rig.write_all(line.as_bytes()).unwrap();
    let _ = rig.shutdown(Shutdown::Write);
    let mut reply = String::new();
    let _ = rig.read_to_string(&mut reply);
    if request["method"] == "pane.read" && !reply.trim().is_empty() {
        let ansi = request["params"]["format"] == "ansi";
        let mut answer: serde_json::Value = serde_json::from_str(&reply).unwrap();
        let text = &mut answer["result"]["read"]["text"];
        if let Some(styled) = text.as_str() {
            *text = json!(match ansi {
                true => styled.replace('\n', "\r\n"),
                false => crate::services::codex_tui::input::strip_ansi_escape_sequences(styled),
            });
        }
        answer["result"]["read"]["format"] = json!(if ansi { "ansi" } else { "text" });
        reply = format!("{answer}\n");
    }
    let _ = writer.write_all(reply.as_bytes());
    let _ = writer.shutdown(Shutdown::Write);
    let _ = reader.read_to_end(&mut Vec::new());
}

impl Drop for HerdrStrips {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_file(&self.backend);
    }
}

/// A fresh turn per screen: the `/stop` reply, the Escapes sent and each `pane.read` line sent.
fn stop_on(
    pool: &sqlx::PgPool,
    root: &Path,
    runtime: &tokio::runtime::Runtime,
    provider: ProviderKind,
    n: u64,
    screen: &str,
) -> (&'static str, usize, Vec<String>) {
    stop_after(pool, root, runtime, provider, n, screen, &[])
}

/// `stop_on` with `records` appended to the fresh turn's transcript first.
fn stop_after(
    pool: &sqlx::PgPool,
    root: &Path,
    runtime: &tokio::runtime::Runtime,
    provider: ProviderKind,
    n: u64,
    screen: &str,
    records: &[serde_json::Value],
) -> (&'static str, usize, Vec<String>) {
    let case = runtime.block_on(Case::new(pool, root, provider, n));
    let mut transcript = std::fs::OpenOptions::new()
        .append(true)
        .open(&case.path)
        .unwrap();
    for record in records {
        writeln!(transcript, "{record}").unwrap();
    }
    let _registry = case.rig.registry_on_this_thread();
    let strips = HerdrStrips::over(&case.rig);
    case.rig.answer("pane.read", json!({"type":"pane_read","read":{"pane_id":PANE,"workspace_id":"w1","tab_id":"w1:1","source":"recent_unwrapped","format":"text","text":screen,"revision":3,"truncated":false}}));
    let reply = runtime.block_on(run_slash_stop(&case.shared, &case.provider, case.channel));
    let text = reply.text();
    runtime.block_on(reply.finish(&case.shared, &case.provider, case.channel));
    let reads = strips.reads.lock().unwrap().clone();
    (text, case.escapes(), reads)
}

fn with_pool(check: impl FnOnce(&sqlx::PgPool, &Path, &tokio::runtime::Runtime)) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let fx = Fixture::new();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _switch = Switch::on();
    let root = tempfile::tempdir().unwrap();
    let _log = TestBindingRoot::enter(Some(root.path()));
    let db = runtime.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = runtime.block_on(db.connect_and_migrate());
    check(&pool, root.path(), &runtime);
    assert!(fx.take_calls().is_empty(), "no tmux call on a Herdr stop");
    runtime.block_on(pool.close());
    runtime.block_on(db.drop());
}

/// The plain read as `HerdrTarget::capture` has always written it, byte for byte.
fn plain_read() -> Vec<String> {
    vec![format!(
        r#"{{"id":"ID","method":"pane.read","params":{{"pane_id":"{PANE}","source":"recent_unwrapped","lines":160,"strip_ansi":true}}}}"#
    )]
}

fn ansi_read() -> Vec<String> {
    vec![format!(
        r#"{{"id":"ID","method":"pane.read","params":{{"pane_id":"{PANE}","source":"recent_unwrapped","lines":160,"strip_ansi":false,"format":"ansi"}}}}"#
    )]
}

fn sent() -> &'static str {
    HerdrStop::Requested(HerdrDelivery::Sent).reply()
}

fn idle() -> &'static str {
    HerdrStop::Requested(HerdrDelivery::NotSent(HerdrNotSent::Idle)).reply()
}

// A Working Codex turn at a cut width gets one Escape though its composer shows the dim
// placeholder; idle, modal, typed and partly typed composers get none, even beside a dim row.
#[test]
fn herdr_codex_stop_reads_the_cut_width_composer_from_one_styled_read() {
    with_pool(|pool, root, runtime| {
        let (mut n, mut observed, mut expected) = (600, Vec::new(), Vec::new());
        for (width, idle_screen) in CUT_WIDTHS {
            let working = format!("{WORKING}\n{idle_screen}");
            let modal = working.replace("\x1b[1m›", "Allow command `cargo test`?\n\x1b[1m›");
            let draft = working.replace(PLACEHOLDER, "fix the flaky test");
            let mixed = format!(
                "\x1b[2m› Explain this codebase\x1b[0m\n\n{}",
                working.replace(PLACEHOLDER, "\x1b[2mAsk Codex\x1b[0m to do anything")
            );
            // Herdr answering the styled read in plain text: the placeholder reads as a draft.
            let plain = crate::services::codex_tui::input::strip_ansi_escape_sequences(&working);
            let edge = "─".repeat(30);
            let boxed = format!(
                "{WORKING}\n\n╭{edge}╮\n│ fix the flaky test ▌ │\n╰{edge}╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send"
            );
            let rows = [
                ("working", working.as_str(), sent(), 1),
                ("idle", idle_screen, idle(), 0),
                ("modal", modal.as_str(), idle(), 0),
                ("draft", draft.as_str(), idle(), 0),
                ("mixed", mixed.as_str(), idle(), 0),
                ("plain answer", plain.as_str(), idle(), 0),
                ("boxed draft", boxed.as_str(), idle(), 0),
            ];
            for (name, screen, reply, escapes) in rows {
                n += 1;
                let seen = stop_on(pool, root, runtime, ProviderKind::Codex, n, screen);
                observed.push((width, name, seen));
                expected.push((width, name, (reply, escapes, ansi_read())));
            }
        }
        let seen = stop_on(
            pool,
            root,
            runtime,
            ProviderKind::Codex,
            651,
            HERDR_RENDERED_46,
        );
        observed.push((46, "herdr 0.9.3 render", seen));
        expected.push((46, "herdr 0.9.3 render", (sent(), 1, ansi_read())));
        assert_eq!(observed, expected);
    });
}

// Claude's Herdr stop reads the pane as the tmux stop does: spinner chrome over an exactly
// empty composer is a live turn; a busy draft, a finished and an interrupted turn are not.
#[test]
fn herdr_claude_stop_shares_the_tmux_pane_evidence() {
    with_pool(|pool, root, runtime| {
        let rows = [
            ("thinking", claude_capture!("thinking"), sent(), 1),
            ("tool-running", claude_capture!("tool-running"), sent(), 1),
            ("busy-draft", claude_capture!("busy-draft"), idle(), 0),
            ("done", claude_capture!("done"), idle(), 0),
            ("interrupted", claude_capture!("interrupted"), idle(), 0),
        ];
        let (mut observed, mut expected) = (Vec::new(), Vec::new());
        for (n, (name, screen, reply, escapes)) in rows.into_iter().enumerate() {
            let n = 700 + n as u64;
            let seen = stop_on(pool, root, runtime, ProviderKind::Claude, n, screen);
            observed.push((name, seen));
            expected.push((name, (reply, escapes, plain_read())));
        }
        assert_eq!(observed, expected);
    });
}

// A Claude stop refused as idle logs once what it read and judged, after the same reply and no
// Escape; a sent stop logs none.
#[test]
fn herdr_claude_idle_refusal_logs_its_judged_inputs_once() {
    use sha2::Digest;
    with_pool(|pool, root, runtime| {
        let rows = [
            ("thinking", claude_capture!("thinking"), sent(), 1, 0),
            (
                "tool-running",
                claude_capture!("tool-running"),
                sent(),
                1,
                0,
            ),
            ("busy-draft", claude_capture!("busy-draft"), idle(), 0, 1),
            ("done", claude_capture!("done"), idle(), 0, 1),
            ("interrupted", claude_capture!("interrupted"), idle(), 0, 1),
        ];
        let (mut observed, mut expected) = (Vec::new(), Vec::new());
        for (n, (name, screen, reply, escapes, lines)) in rows.into_iter().enumerate() {
            let (logs, _capture) = capture_stop_events();
            let n = 720 + n as u64;
            let (text, sent_escapes, _) =
                stop_on(pool, root, runtime, ProviderKind::Claude, n, screen);
            let evidence: Vec<String> = logs
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.contains("herdr_stop_idle_evidence"))
                .cloned()
                .collect();
            observed.push((name, text, sent_escapes, evidence.len()));
            expected.push((name, reply, escapes, lines));
            let read = crate::services::codex_tui::input::strip_ansi_escape_sequences(screen);
            let sha = format!("{:x}", sha2::Sha256::digest(read.as_bytes()));
            for line in &evidence {
                for field in [
                    "structured=Some(Streaming)".to_owned(),
                    format!("draft={}", name == "busy-draft"),
                    format!("screen_sha=\"{}\"", &sha[..12]),
                    "phase=\"ambiguous\"".to_owned(),
                    "runtime_kind=\"claude_tui\"".to_owned(),
                    "sources_same=true".to_owned(),
                    "truncated=false".to_owned(),
                ] {
                    // The collector lists each field as one Debug-quoted element.
                    let element = format!("{field:?}");
                    assert!(line.contains(&element), "{name}: {field} missing in {line}");
                }
                for field in [
                    "ready=",
                    "active=",
                    "modal=",
                    "veto=",
                    "legacy_active=",
                    "foreground_tool_running=",
                    "exact_empty_composer=",
                    "tail=[",
                    "turn_nonce=",
                ] {
                    assert!(line.contains(field), "{name}: {field} missing in {line}");
                }
            }
        }
        assert_eq!(observed, expected);
    });
}

// A Claude tool call trailed by the session records Claude writes mid-turn stops from the
// production screen; the screen still fences idle and draft panes, and a finished turn stays idle.
#[test]
fn herdr_claude_stop_reads_a_tool_turn_past_trailing_session_metadata() {
    let probe = include_str!(
        "../../../../../tests/fixtures/tui_input/claude-herdr-sleep90-update-notice.txt"
    );
    let pane = super::super::super::claude_stop_pane::legacy_evidence(probe);
    let fenced = super::super::super::claude_stop_pane::veto(probe);
    let foreground = super::super::super::claude_stop_pane::foreground_tool_running(probe);
    let judged = (
        claude_tui_stop_pane_evidence(probe),
        pane.1,
        fenced,
        foreground,
    );
    assert_eq!(judged, ((true, true, false), true, false, false));
    let tool = json!({"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"python3 -c \"import time; time.sleep(90)\""}}],"stop_reason":"tool_use"}});
    let meta = [
        json!({"type":"last-prompt","lastPrompt":"run the 90 second sleep probe","sessionId":"s1"}),
        json!({"type":"ai-title","aiTitle":"Sleep probe","sessionId":"s1"}),
        json!({"type":"mode","mode":"normal","sessionId":"s1"}),
        json!({"type":"permission-mode","permissionMode":"bypassPermissions","sessionId":"s1"}),
        json!({"type":"atis-latch","sessionId":"s1"}),
    ];
    let running: Vec<_> = std::iter::once(tool.clone()).chain(meta.clone()).collect();
    let finished: Vec<_> = [
        tool,
        json!({"type":"user","uuid":"u2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"done"}]}}),
        json!({"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"text","text":"DONE"}],"stop_reason":"end_turn"}}),
        json!({"type":"system","subtype":"stop_hook_summary"}),
        json!({"type":"system","subtype":"turn_duration","durationMs":93000}),
    ]
    .into_iter()
    .chain(meta)
    .collect();
    with_pool(|pool, root, runtime| {
        let rows = [
            ("running tool", probe, &running, sent(), 1, None),
            (
                "done",
                claude_capture!("done"),
                &running,
                idle(),
                0,
                Some("Streaming"),
            ),
            (
                "idle",
                claude_capture!("idle"),
                &running,
                idle(),
                0,
                Some("Streaming"),
            ),
            (
                "busy-draft",
                claude_capture!("busy-draft"),
                &running,
                idle(),
                0,
                Some("Streaming"),
            ),
            (
                "finished turn",
                probe,
                &finished,
                idle(),
                0,
                Some("Unknown"),
            ),
        ];
        let (mut observed, mut expected) = (Vec::new(), Vec::new());
        for (n, (name, screen, records, reply, escapes, structured)) in rows.into_iter().enumerate()
        {
            let (logs, _capture) = capture_stop_events();
            let n = 740 + n as u64;
            let seen = stop_after(
                pool,
                root,
                runtime,
                ProviderKind::Claude,
                n,
                screen,
                records,
            );
            // The refusal names what the transcript read, so a screen fence is told from a transcript one.
            let refused: Vec<Option<String>> = logs
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.contains("herdr_stop_idle_evidence"))
                .map(|event| {
                    ["Streaming", "Unknown", "Idle", "UserSubmitted"]
                        .into_iter()
                        .find(|state| {
                            event.contains(&format!("{:?}", format!("structured=Some({state})")))
                        })
                        .map(str::to_owned)
                })
                .collect();
            observed.push((name, seen, refused));
            let structured = structured.map(|state: &str| Some(state.to_owned()));
            expected.push((
                name,
                (reply, escapes, plain_read()),
                structured.into_iter().collect(),
            ));
        }
        assert_eq!(observed, expected);
    });
}

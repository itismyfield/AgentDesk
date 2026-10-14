//! Native passthrough input admission: a protected channel's pane sees no probe and no write.
use super::*;
use crate::services::discord::input_runtime::fence::Mode;
use crate::services::discord::turn_bridge::recovery_text::input_fence_tests::gate_in;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const COMMANDS: [ClaudeSlashPassthrough; 4] = [
    ClaudeSlashPassthrough::Effort(EffortLevel::Max),
    ClaudeSlashPassthrough::Compact,
    ClaudeSlashPassthrough::Cost,
    ClaudeSlashPassthrough::Context,
];

/// Every pane write the substitute sends saw, as `(tmux name, write)`.
static SENT: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

fn record(name: &str, write: String) {
    let mut sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
    sent.push((name.to_string(), write));
}

fn sent(name: &str) -> Vec<String> {
    let sent = SENT.lock().unwrap_or_else(|e| e.into_inner());
    let writes = sent.iter().filter(|(sent_to, _)| sent_to == name);
    writes.map(|(_, write)| write.clone()).collect()
}

const RECORD: NativeSend = NativeSend {
    selector: |name, nav| {
        record(name, format!("selector {}", nav.target_index));
        Ok(())
    },
    prompt: |name, prompt| {
        record(name, format!("prompt {prompt}"));
        Ok(())
    },
};

fn expected_write(command: ClaudeSlashPassthrough) -> String {
    match command.selector_navigation() {
        Some(nav) => format!("selector {}", nav.target_index),
        None => format!("prompt {}", command.prompt()),
    }
}

fn ready() -> std::future::Ready<Result<(), poise::serenity_prelude::Error>> {
    std::future::ready(Ok(()))
}

// Every non-Legacy mode, including another provider's Gate on the channel, refuses the selector
// and the prompt commands on an idle live pane before the defer, the probe and the write.
#[tokio::test]
async fn g2e_protected_native_commands_never_send() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install("-");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let cases = [
        (ProviderKind::Claude, Mode::Closing),
        (ProviderKind::Claude, Mode::LedgerOpen),
        (ProviderKind::Claude, Mode::Held),
        (ProviderKind::Codex, Mode::Held),
    ];
    for (n, (owner, mode)) in cases.into_iter().enumerate() {
        let channel = 6_325_960_000 + n as u64;
        let _gate = gate_in(&shared, &owner, channel, mode).await;
        let name = provider.build_tmux_session_name(&format!("g2e-pa-protected-{n}"));
        for command in COMMANDS {
            tmux.take_calls();
            let deferred = AtomicBool::new(false);
            let defer = async {
                deferred.store(true, Ordering::SeqCst);
                Ok(())
            };
            let delivery = deliver(&shared, &provider, channel, &name, command, defer, RECORD);
            let case = format!("{owner:?} {mode:?} {command:?}");
            let Delivery::Refused(notice) = delivery.await.unwrap() else {
                panic!("{case}: the command reached the pane");
            };
            let refused = format!("{}를 전달하지 않았어요: 입력 보호", command.slash_name());
            assert!(notice.starts_with(&refused), "{case}: {notice}");
            assert_eq!(tmux.take_calls(), Vec::<String>::new(), "{case}: no tmux");
            assert!(!deferred.load(Ordering::SeqCst), "{case}: no defer");
        }
        assert_eq!(
            sent(&name),
            Vec::<String>::new(),
            "{owner:?} {mode:?}: no write"
        );
    }
}

// Without a Gate and under a Legacy-open one, each command probes the pane and takes its native
// write as in main; a dead pane and a readiness timeout keep their outcomes.
#[tokio::test]
async fn g2e_legacy_open_and_off_keep_native_send() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let tmux = FakeTmux::install("-");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let timeout = NativeSend {
        selector: |_, _| Err("timeout waiting for claude tui selector".into()),
        prompt: |_, _| Err("timeout waiting for claude tui prompt".into()),
    };
    let mut channels = Vec::new();
    for (n, protected) in [false, true].into_iter().enumerate() {
        let channel = 6_325_960_100 + n as u64;
        let gate = match protected {
            true => Some(gate_in(&shared, &provider, channel, Mode::LegacyOpen).await),
            false => None,
        };
        let name = provider.build_tmux_session_name(&format!("g2e-pa-legacy-{n}"));
        for command in COMMANDS {
            tmux.take_calls();
            let delivery = deliver(&shared, &provider, channel, &name, command, ready(), RECORD);
            let case = format!("{protected} {command:?}");
            let Delivery::Sent(result) = delivery.await.unwrap() else {
                panic!("{case}: a Legacy passthrough was refused");
            };
            assert_eq!(result, Ok(()), "{case}");
            let calls = tmux.take_calls();
            assert!(calls[0].starts_with("has-session"), "{case}: {calls:?}");
            let written = sent(&name).pop();
            assert_eq!(written, Some(expected_write(command)), "{case}");
            let delivery = deliver(
                &shared,
                &provider,
                channel,
                &name,
                command,
                ready(),
                timeout,
            );
            let Delivery::Sent(Err(error)) = delivery.await.unwrap() else {
                panic!("{case}: the timeout was not returned");
            };
            let is_timeout = crate::services::claude_tui::input::is_prompt_ready_timeout_error;
            assert!(is_timeout(&error), "{case}: {error}");
        }
        assert_eq!(sent(&name).len(), COMMANDS.len(), "{protected}");
        channels.push((channel, name, gate));
    }
    tmux.mark_missing();
    for (channel, name, _gate) in &channels {
        let command = ClaudeSlashPassthrough::Compact;
        let delivery = deliver(&shared, &provider, *channel, name, command, ready(), RECORD);
        let Delivery::Refused(notice) = delivery.await.unwrap() else {
            panic!("{channel}: a dead pane was written");
        };
        assert_eq!(notice, live_session_required_notice(command));
    }
}

static ENTERED: Mutex<Option<std::sync::mpsc::Sender<()>>> = Mutex::new(None);
static RELEASE: Mutex<Option<std::sync::mpsc::Receiver<()>>> = Mutex::new(None);

// An admitted write keeps its permit inside the blocking worker: after the command future is
// cancelled mid-write, a close still waits until the pane write returns.
#[tokio::test]
async fn g2e_native_worker_keeps_permit_until_exit() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _tmux = FakeTmux::install("-");
    let shared = crate::services::discord::make_shared_data_for_tests();
    let provider = ProviderKind::Claude;
    let channel = 6_325_960_150;
    let (gate, _health) = gate_in(&shared, &provider, channel, Mode::LegacyOpen).await;
    let (entered, on_entered) = std::sync::mpsc::channel();
    let (release, on_release) = std::sync::mpsc::channel();
    *ENTERED.lock().unwrap() = Some(entered);
    *RELEASE.lock().unwrap() = Some(on_release);
    let held = NativeSend {
        selector: |_, _| Err("unexpected selector".into()),
        prompt: |name, prompt| {
            let entered = ENTERED.lock().unwrap().take().unwrap();
            let release = RELEASE.lock().unwrap().take().unwrap();
            entered.send(()).unwrap();
            release.recv().unwrap();
            record(name, format!("prompt {prompt}"));
            Ok(())
        },
    };
    let name = provider.build_tmux_session_name("g2e-pa-worker");
    let command = {
        let (shared, provider, name) = (shared.clone(), provider.clone(), name.clone());
        let compact = ClaudeSlashPassthrough::Compact;
        tokio::spawn(async move {
            let delivery = deliver(&shared, &provider, channel, &name, compact, ready(), held);
            delivery.await.map(drop).map_err(|error| error.to_string())
        })
    };
    let wait = move || on_entered.recv_timeout(Duration::from_secs(10));
    let reached = tokio::task::spawn_blocking(wait).await.unwrap();
    reached.expect("the pane write started");
    command.abort();
    assert!(command.await.unwrap_err().is_cancelled());
    let closing = gate.close().unwrap();
    let early = tokio::time::timeout(Duration::from_millis(300), closing.drain()).await;
    assert!(early.is_err(), "close waits for the running pane write");
    release.send(()).unwrap();
    let drained = tokio::time::timeout(Duration::from_secs(10), closing.drain()).await;
    drained.expect("the effect ends with the pane write");
    assert_eq!(sent(&name), vec!["prompt /compact".to_string()]);
}

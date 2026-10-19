//! A clear records a Claude pane's end for O only once it confirmed that end, without taking the
//! pane's source authority twice; any other end leaves no record.

use std::os::unix::process::CommandExt;
use std::path::Path;

use poise::serenity_prelude::ChannelId;

use super::*;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::provider_teardown::tests::test_support::FakeTmux;
use crate::services::tmux_common as tc;
use crate::services::tui_o::store::clear_end;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::binding_events::{self as p5, BindingTarget};
use crate::services::tui_prompt_dedupe::{self as dedupe, TuiRuntimeBinding};

/// A live Claude pane of its own channel: a process tmux reports as the pane, the execution's
/// launch context and spawn nonce, and the source its binding log names.
struct Pane {
    channel: u64,
    tmux: String,
    nonce: String,
    pid: u32,
    fake: FakeTmux,
}

fn binding(path: &Path, session: &str) -> TuiRuntimeBinding {
    TuiRuntimeBinding {
        runtime_kind: ClaudeTui,
        output_path: path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session.to_owned()),
        last_offset: 0,
        relay_last_offset: None,
    }
}

impl Pane {
    fn start(dir: &Path) -> Self {
        let id = uuid::Uuid::new_v4();
        let channel = 1_479_671_305_342_000_000 + u64::from(id.as_fields().1);
        let tmux = ProviderKind::Claude
            .build_tmux_session_name(&format!("adk-clear-record-{}", id.simple()));
        let fake = FakeTmux::install(&tmux);
        fake.become_missing_after_kill();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        std::thread::spawn(move || child.wait());
        fake.run_pane(pid);
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        PreparedIncarnation::create(BindingContext {
            schema: 1,
            provider: "claude".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: nonce.clone(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: Some("s1".into()),
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        })
        .unwrap();
        std::fs::write(tc::session_temp_path(&tmux, "spawn_nonce"), &nonce).unwrap();
        let transcript = dir.join(format!("{}.jsonl", id.simple()));
        std::fs::write(&transcript, b"{}\n").unwrap();
        dedupe::register_tmux_channel(&tmux, channel);
        dedupe::register_provider_session("claude", "s1", &tmux);
        dedupe::register_launched_tmux_runtime_binding(&tmux, binding(&transcript, "s1"));
        assert_eq!(p5::binding_events_since(channel, 0).unwrap().len(), 1);
        Self {
            channel,
            tmux,
            nonce,
            pid,
            fake,
        }
    }

    /// Runs the clear's session end, returning the source authority it would have contended on.
    fn end(&self, provider: ProviderKind, guarded: bool) -> Option<String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        tc::source_authority_contention_key_for_tests(|| {
            runtime.block_on(async {
                let shared =
                    crate::services::discord::make_shared_data_for_tests_with_storage(None);
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                let guard = guarded.then(|| lock.try_lock_owned().unwrap());
                let session = (Some(self.tmux.as_str()), None);
                let mut report = ClearReport::default();
                let channel = ChannelId::new(self.channel);
                end_session(&shared, &provider, channel, session, guard, &mut report).await;
            })
        })
    }

    fn records(&self) -> Vec<clear_end::ClearEnd> {
        clear_end::read(&crate::config::runtime_root().unwrap(), self.channel).unwrap()
    }
}

impl Drop for Pane {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/bin/kill")
            .args(["-9", &self.pid.to_string()])
            .output();
    }
}

#[test]
fn only_a_confirmed_claude_pane_end_is_recorded() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _state = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _rotations = dedupe::lock_claude_session_rotations_for_tests();
    dedupe::reset_state_for_tests();
    let logs = tempfile::tempdir().unwrap();
    p5::set_test_root(Some(logs.path()));
    let cases = [
        "confirmed",
        "lost_kill_ack",
        "unguarded",
        "codex",
        "survivor",
        "rebound",
    ];
    for case in cases {
        let pane = Pane::start(logs.path());
        let provider = match case {
            "codex" => ProviderKind::Codex,
            _ => ProviderKind::Claude,
        };
        match case {
            "lost_kill_ack" => pane.fake.lose_kill_ack_after_exit(),
            "survivor" => pane.fake.keep_pane_alive(),
            "rebound" => pane.fake.hold_kills(),
            _ => {}
        }
        let contended = std::thread::scope(|scope| {
            if case == "rebound" {
                // The pane binds a new source while its kill is still in flight.
                scope.spawn(|| {
                    while !pane.fake.killing() {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    p5::set_test_root(Some(logs.path()));
                    let next = logs.path().join(format!("{}-next.jsonl", pane.nonce));
                    std::fs::write(&next, b"{}\n").unwrap();
                    dedupe::register_launched_tmux_runtime_binding(
                        &pane.tmux,
                        binding(&next, "s2"),
                    );
                    p5::set_test_root(None);
                    pane.fake.release_kills();
                });
            }
            pane.end(provider, case != "unguarded")
        });
        assert_eq!(
            contended, None,
            "{case}: the source authority is never taken twice"
        );
        let records = pane.records();
        if case == "rebound" {
            assert_eq!(p5::binding_events_since(pane.channel, 0).unwrap().len(), 2);
        }
        if case != "confirmed" {
            assert_eq!(records, [], "{case}");
            continue;
        }
        let logged = p5::binding_events_since(pane.channel, 0).unwrap();
        let BindingTarget::Source(source) = &logged[0].new else {
            panic!("the launch logged its source");
        };
        let [record] = records.as_slice() else {
            panic!("one record: {records:?}");
        };
        assert_eq!(
            (
                record.provider.as_str(),
                record.tmux_session.as_str(),
                record.basis.as_str()
            ),
            ("claude", pane.tmux.as_str(), "confirmed_clear")
        );
        assert_eq!(
            (
                &record.source,
                record.execution_nonce.as_str(),
                record.binding_seq
            ),
            (source, pane.nonce.as_str(), 1)
        );
    }
    p5::set_test_root(None);
}

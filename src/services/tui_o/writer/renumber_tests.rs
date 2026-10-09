//! A reboot renumbers a transcript's dev while its inode stays: a restarted writer reads it as the
//! source it stored, and a replaced or rewritten file still halts.

use std::io::{Seek, SeekFrom};

use super::*;
use crate::services::tui_o::shadow::capture::{renumber, same_file};
use crate::services::tui_o::store::spool::source_key;

const REBOOT: u64 = 1 << 40;
const SECOND_REBOOT: u64 = 1 << 41;

/// `stored`'s file as a stat names it now, after a renumber.
fn live(path: &Path, stored: &SourceId) -> SourceId {
    let now = source_id_for(&stored.session_id, path).unwrap();
    assert!(
        now.dev != stored.dev && same_file(&now, stored),
        "{now:?} vs {stored:?}"
    );
    now
}

fn copies(harness: &Harness, source: &SourceId) -> usize {
    let cursors = harness.channel().cursors().cloned().collect::<Vec<_>>();
    cursors
        .iter()
        .filter(|c| same_file(&c.source, source))
        .count()
}

fn halted_with(harness: &Harness, wanted: &str) -> bool {
    let alarms = harness.alarms.taken();
    matches!(alarms.as_slice(), [WriterAlarm::Halted { detail }] if detail.contains(wanted))
}

#[tokio::test(start_paused = true)]
async fn a_writer_restarted_after_the_volume_renumbered_continues_from_its_cursor() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    halt(stop, task).await;

    let reboot = renumber::shift(&a_path, REBOOT);
    let a1 = live(&a_path, &a);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m2", "second"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    assert_eq!(harness.alarms.taken(), []);
    // The resume logged after the reboot names the same file with its new dev.
    bindings.commit(rotate(2, &a, &a1, BindingCause::Resume));
    append(&a_path, &row("m3", "third"));
    polls(12).await;
    assert_eq!(harness.port.posts(), ["first", "second", "third"]);
    assert_eq!(copies(&harness, &a), 1);
    assert!(harness.channel().rotation().unwrap().successors.is_empty());
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(2));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;

    drop(reboot);
    let _again = renumber::shift(&a_path, SECOND_REBOOT);
    let a2 = live(&a_path, &a);
    bindings.commit(rotate(3, &a1, &a2, BindingCause::Resume));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&a_path, &row("m4", "fourth"));
    polls(12).await;
    assert_eq!(harness.port.posts(), ["first", "second", "third", "fourth"]);
    assert_eq!(copies(&harness, &a), 1);
    assert!(harness.channel().rotation().unwrap().successors.is_empty());
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn dev_variants_of_a_file_first_bound_in_one_batch_share_one_cursor() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    halt(stop, task).await;
    // Logged before the reboot, applied only after it, with the resume the reboot logged.
    let (b_path, b0) = transcript(&a_path, "b.jsonl", "s2", &row("n0", "b zero"));
    let target = BindingTarget::Source(b0.clone());
    bindings.commit(bound(2, Some(&a), target, BindingCause::Clear, None));
    let _a_reboot = renumber::shift(&a_path, REBOOT);
    let b_reboot = renumber::shift(&b_path, REBOOT);
    let b1 = live(&b_path, &b0);
    bindings.commit(rotate(3, &b0, &b1, BindingCause::Resume));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    append(&b_path, &row("n1", "b one"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "b zero", "b one"]);
    assert_eq!(copies(&harness, &b0), 1);
    let rotation = harness.channel().rotation().unwrap();
    assert!(!rotation.successors.contains_key(&source_key(&b0)));
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(3));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;

    drop(b_reboot);
    let _b_again = renumber::shift(&b_path, SECOND_REBOOT);
    let b2 = live(&b_path, &b0);
    bindings.commit(rotate(4, &b1, &b2, BindingCause::Resume));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&b_path, &row("n2", "b two"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "b zero", "b one", "b two"]);
    assert_eq!(copies(&harness, &b0), 1);
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), Some(4));
    assert_eq!(harness.alarms.taken(), []);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn a_renumbered_source_replaced_shrunk_or_rewritten_still_halts_the_restart() {
    let wanted = [
        ("replaced", "source reopen: source replaced"),
        ("same bytes, new inode", "source reopen: source replaced"),
        ("shrunk", "source reopen: source holds"),
        ("rewritten", "source bytes before the cursor changed"),
    ];
    for (case, wanted) in wanted {
        let (harness, a_path, _, bindings) = started(&row("m0", "before the switch"));
        let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
        append(&a_path, &row("m1", "first"));
        polls(3).await;
        halt(stop, task).await;
        let bytes = std::fs::read(&a_path).unwrap();
        // The renumbered inode stays linked, so no other test's file takes its number.
        std::fs::hard_link(&a_path, a_path.with_file_name("kept.jsonl")).unwrap();
        let _reboot = renumber::shift(&a_path, REBOOT);
        let next = a_path.with_file_name("next.jsonl");
        match case {
            "replaced" => {
                std::fs::write(&next, &row("x0", "another session")).unwrap();
                std::fs::rename(&next, &a_path).unwrap();
            }
            "same bytes, new inode" => {
                std::fs::write(&next, &bytes).unwrap();
                std::fs::rename(&next, &a_path).unwrap();
            }
            "shrunk" => {
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&a_path)
                    .unwrap();
                file.set_len(bytes.len() as u64 - 5).unwrap();
            }
            _ => {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&a_path)
                    .unwrap();
                file.seek(SeekFrom::Start(10)).unwrap();
                file.write_all(&[bytes[10] ^ 1]).unwrap();
            }
        }
        let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
        polls(3).await;
        assert!(task.is_finished(), "{case}");
        assert!(halted_with(&harness, wanted), "{case}");
        assert_eq!(harness.port.posts(), ["first"], "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_retired_renumbered_source_whose_bytes_changed_still_halts() {
    let (harness, a_path, a, bindings) = started(&row("m0", "before the switch"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    let (b_path, b) = transcript(&a_path, "b.jsonl", "s2", b"");
    bindings.commit(rotate(2, &a, &b, BindingCause::Clear));
    append(&b_path, &row("n1", "b one"));
    polls(15).await;
    assert!(retired(&harness, &a));
    halt(stop, task).await;
    let _a_reboot = renumber::shift(&a_path, REBOOT);
    let _b_reboot = renumber::shift(&b_path, REBOOT);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(&a_path)
        .unwrap();
    file.seek(SeekFrom::Start(10)).unwrap();
    file.write_all(b"#").unwrap();
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    polls(3).await;
    assert!(task.is_finished());
    assert!(halted_with(
        &harness,
        "source bytes before the cursor changed"
    ));
}

#[tokio::test(start_paused = true)]
async fn a_renumbered_source_replaced_while_it_is_read_halts_on_the_next_poll() {
    let (harness, a_path, _, bindings) = started(&row("m0", "before the switch"));
    let _reboot = renumber::shift(&a_path, REBOOT);
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&a_path, &row("m1", "first"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);
    std::fs::hard_link(&a_path, a_path.with_file_name("kept.jsonl")).unwrap();
    let next = a_path.with_file_name("next.jsonl");
    std::fs::write(&next, std::fs::read(&a_path).unwrap()).unwrap();
    std::fs::rename(&next, &a_path).unwrap();
    polls(3).await;
    assert!(task.is_finished());
    assert!(halted_with(&harness, "source Replaced"));
}

#[tokio::test(start_paused = true)]
async fn a_store_holding_two_dev_variants_of_one_file_halts_before_reading_either() {
    let mut variants = None;
    let harness = Harness::build(|root| {
        let path = root.join("t.jsonl");
        std::fs::write(&path, b"").unwrap();
        let first = source_id_for("s1", &path).unwrap();
        let second = SourceId {
            dev: first.dev ^ REBOOT,
            ..first.clone()
        };
        variants = Some((path, first.clone(), second.clone()));
        let empty = hex::encode(Sha256::digest(b""));
        [first, second]
            .into_iter()
            .map(|source_id| InitSource {
                source_id,
                delivery_start: 0,
                prefix_hash: empty.clone(),
            })
            .collect()
    });
    let (path, first, _) = variants.unwrap();
    harness.gate.acquired();
    let bindings = Arc::new(FakeBindings::new());
    let target = BindingTarget::Source(first);
    bindings.commit(bound(1, None, target, BindingCause::Startup, None));
    let (_stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings);
    append(&path, &row("m1", "never read"));
    polls(3).await;
    assert!(task.is_finished());
    assert!(halted_with(&harness, "names more than one stored source"));
    assert!(harness.port.posts().is_empty());
    assert_eq!(harness.channel().binding_checkpoint().unwrap(), None);
    assert!(harness.channel().rotation().unwrap().successors.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_codex_proof_read_after_a_renumber_still_allows_the_stored_cursor() {
    if !crate::services::tui_o::cutover::test_override::isolated_binding_case(concat!(
        module_path!(),
        "::a_codex_proof_read_after_a_renumber_still_allows_the_stored_cursor"
    )) {
        return;
    }
    use crate::services::tui_prompt_dedupe::binding_context::{
        BINDING_HEADER, BindingContext, CapturedContext, HookBindingEnvelope, ObservedHookProcess,
        PreparedIncarnation,
    };
    use crate::services::tui_prompt_dedupe::{self as dedupe};
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe_lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _env) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    let tmux = format!("o-renumber-{}", uuid::Uuid::new_v4().simple());
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: tmux.clone(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: crate::services::tmux_common::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.path().canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: Some("verified".into()),
    };
    PreparedIncarnation::create(context.clone()).unwrap();
    let marker = crate::services::tmux_common::session_temp_path(&tmux, "spawn_nonce");
    std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
    std::fs::write(marker, &context.execution_nonce).unwrap();
    dedupe::register_tmux_channel(&tmux, CHANNEL);
    let id = "019e660d-4859-7522-9cee-8ba7c4e7c743";
    let rollout = (context.provider_root.as_ref().unwrap()).join(format!("rollout-{id}.jsonl"));
    let timestamp = (context.created_at + chrono::Duration::seconds(1)).to_rfc3339();
    let header = serde_json::json!({"type":"session_meta", "timestamp":timestamp,
        "payload":{"id":id,"timestamp":timestamp,"source":"cli",
        "cwd":root.path(),"originator":"codex_cli_rs"}});
    std::fs::write(&rollout, format!("{header}\n")).unwrap();
    // O stored the rollout before the reboot; the hook reads it after.
    let stored = source_id_for(id, &rollout).unwrap();
    let _reboot = renumber::shift(&rollout, REBOOT);
    dedupe::set_codex_delivery_permission_for_tests(
        &context,
        dedupe::CodexDeliveryPermissionForTests::Allowed,
    );
    let envelope = HookBindingEnvelope {
        context: CapturedContext::Captured(context.clone()),
        observed: ObservedHookProcess::default(),
    };
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(BINDING_HEADER, envelope.encode().unwrap().parse().unwrap());
    let ingress =
        crate::services::claude_tui::hook_server::observation_ingress::observe_binding_hook(
            "codex",
            "session_start",
            Some(id),
            Some(id),
            &serde_json::json!({"session_id":id,"transcript_path":rollout,"source":"startup"}),
            &headers,
        );
    let fold = dedupe::binding_events::codex::read_ownership(&context).unwrap();
    let proof = (fold.verified.as_ref())
        .unwrap_or_else(|| panic!("{ingress:?} {fold:?}"))
        .source
        .clone();
    assert!(
        proof.dev != stored.dev && same_file(&proof, &stored),
        "{proof:?}"
    );
    assert!(dedupe::codex_verified_channel_delivery_allowed(CHANNEL));
    assert!(dedupe::codex_verified_o_source_allowed(CHANNEL, &stored));
    let other_ino = SourceId {
        ino: stored.ino + 1,
        ..stored.clone()
    };
    let other_session = SourceId {
        session_id: "another".into(),
        ..stored.clone()
    };
    let other_path = SourceId {
        path: rollout.with_file_name("other.jsonl"),
        ..stored.clone()
    };
    for other in [other_ino, other_session, other_path] {
        assert!(
            !dedupe::codex_verified_o_source_allowed(CHANNEL, &other),
            "{other:?}"
        );
    }
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(None);
}

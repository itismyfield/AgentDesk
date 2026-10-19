use super::*;
use crate::services::discord::{mailbox_snapshot, mailbox_take_next_automatic_intervention};
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{MessageId, UserId};

pub(crate) fn queued(id: u64) -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: 1,
        source_message_ids: Vec::new(),
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("eligible {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}
async fn gate(shared: &Arc<SharedData>, provider: &ProviderKind, channel: u64, idle: bool) {
    let channel = ChannelId::new(channel);
    let first = channel.get() + 1_000_000;
    let context = crate::services::discord::queue_persistence_context(shared, provider, channel);
    shared
        .mailbox(channel)
        .replace_queue(vec![queued(first), queued(first + 1)], context)
        .await;
    let take = mailbox_take_next_automatic_intervention(shared, provider, channel).await;
    assert_eq!(
        take.intervention.as_ref().map(|i| i.message_id.get()),
        idle.then_some(first)
    );
    assert_eq!(
        take.dispatch_lease.is_some(),
        idle,
        "eligible promotion must own its lease"
    );
    assert!(take.persistence_error.is_none());
    let snapshot = mailbox_snapshot(shared, channel).await;
    assert_eq!(snapshot.intervention_queue.len(), if idle { 1 } else { 2 });
    assert_eq!(
        snapshot.intervention_queue[0].message_id.get(),
        if idle { first + 1 } else { first }
    );
    assert_eq!(
        snapshot.pending_user_dispatch.map(|id| id.get()),
        idle.then_some(first)
    );
    assert_eq!(snapshot.pending_user_dispatch_lease_held_by_caller, idle);
}
fn codex_event(kind: &str, id: Option<&str>) -> serde_json::Value {
    let mut row = json!({"type":"event_msg","payload":{"type":kind}});
    if let Some(id) = id {
        row["payload"]["turn_id"] = id.into();
    }
    row
}

#[tokio::test(flavor = "current_thread")]
async fn s1_tail_effects_require_current_complete_closed_evidence() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, rows, idle) in [
        (0, vec![prompt("A"), end()], true),
        (1, vec![prompt("A")], false),
        (2, vec![end(), prompt("B")], false),
        (
            3,
            vec![json!({"type":"summary","summary":"metadata"})],
            false,
        ),
        (4, vec![], false),
    ] {
        let channel = 69_183_000 + index;
        let _limits =
            tail::overrides::Guard::new(channel, 0, if rows.is_empty() { 0 } else { 4096 }, 37, 59);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        std::fs::write(&path, "").unwrap();
        if rows.is_empty() {
            write(
                &path,
                &[json!({"type":"summary","summary":"outside the empty window"})],
            );
        }
        write(&path, &rows);
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Claude,
            channel,
            &format!("s1-effect-{channel}"),
            &path,
        );
        let _pane = ReadyPane::mark(&format!("s1-effect-{channel}"));
        let _ = settled_reason(&shared, &ProviderKind::Claude, channel).await;
        let read = activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
        gate(&shared, &ProviderKind::Claude, channel, idle).await;
        assert_eq!(read.activity == Activity::Idle, idle, "{rows:?}: {read:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_codex_boundary_proof_crosses_actual_worker_install_and_resume() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let blocked = json!({"type":"response_item","payload":{"type":"unknown_item"}});
    for (index, middle, end_kind) in [
        (0, vec![], "task_complete"),
        (1, vec![blocked.clone(), blocked.clone()], "task_complete"),
        (
            2,
            vec![codex_event("turn_aborted", Some("B"))],
            "task_complete",
        ),
    ] {
        let channel = 69_183_100 + index;
        let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 19);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        write(
            &path,
            &[
                json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}}),
                codex_event("task_started", Some("A")),
            ],
        );
        write(&path, &middle);
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Codex,
            channel,
            &format!("s1-codex-{channel}"),
            &path,
        );
        let reason = settled_reason(&shared, &ProviderKind::Codex, channel).await;
        assert_ne!(reason, "closed");
        // Installation retains A's opener, including one preceding a typed error poll.
        write(&path, &[blocked.clone(), codex_event(end_kind, Some("A"))]);
        for _ in 0..8 {
            let _ = settled_reason(&shared, &ProviderKind::Codex, channel).await;
            if activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
                .await
                .activity
                == Activity::Idle
            {
                break;
            }
        }
        assert_eq!(
            activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
                .await
                .activity,
            Activity::Idle
        );
        gate(&shared, &ProviderKind::Codex, channel, true).await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_anonymous_or_foreign_boundary_cannot_reuse_closed_a_at_effect() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, row) in [
        codex_event("task_started", None),
        codex_event("task_complete", None),
        codex_event("task_complete", Some("B")),
    ]
    .into_iter()
    .enumerate()
    {
        let channel = 69_183_200 + index as u64;
        let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 35);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        write(
            &path,
            &[
                json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}}),
                codex_event("task_started", Some("A")),
                codex_event("task_complete", Some("A")),
            ],
        );
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Codex,
            channel,
            &format!("s1-anon-{channel}"),
            &path,
        );
        assert_eq!(
            settled_reason(&shared, &ProviderKind::Codex, channel).await,
            "closed"
        );
        write(&path, &[row, codex_event("task_complete", Some("A"))]);
        gate(&shared, &ProviderKind::Codex, channel, false).await;
        assert_eq!(
            activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
                .await
                .activity,
            Activity::Unknown
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_partial_eof_and_append_after_snapshot_hold_effects() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let channel = 69_183_300;
    let _limits = tail::overrides::Guard::new(channel, 1, 4096, 7, 31);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let path = root.path().join("partial.jsonl");
    write(&path, &[prompt("A"), end()]);
    let _turn =
        bind_turn_mode_transcript(&shared, &ProviderKind::Claude, channel, "s1-partial", &path);
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Claude, channel).await,
        "closed"
    );
    let bytes = prompt("B").to_string();
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&bytes.as_bytes()[..10]).unwrap();
    gate(&shared, &ProviderKind::Claude, channel, false).await;
    assert_eq!(
        activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel))
            .await
            .activity,
        Activity::Unknown
    );
    file.write_all(&bytes.as_bytes()[10..]).unwrap();
    file.write_all(b"\n").unwrap();
    let reason = settled_reason(&shared, &ProviderKind::Claude, channel).await;
    assert_ne!(reason, "closed");
    write(&path, &[end()]);
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Claude, channel).await,
        "closed"
    );
}
#[tokio::test(flavor = "current_thread")]
async fn s1_snapshot_shrink_and_resume_tamper_hold_actual_effects() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, tamper, shrink) in [(0, false, false), (1, true, false), (2, false, true)] {
        let channel = 69_183_400 + index;
        let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 4096);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        let blocked = json!({"type":"response_item","payload":{"type":"unknown_item"}});
        write(
            &path,
            &[
                json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}}),
                codex_event("task_started", Some("A")),
                blocked,
                codex_event("task_complete", Some("A")),
            ],
        );
        let cut = std::fs::metadata(&path).unwrap().len();
        if shrink {
            write(&path, &[codex_event("task_started", Some("B"))]);
        }
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Codex,
            channel,
            &format!("s1-race-{channel}"),
            &path,
        );
        let mutate = path.clone();
        let _hook = tail::tests::HandoffHook::new(
            channel,
            shrink.then(|| {
                Box::new(move || {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(mutate)
                        .unwrap()
                        .set_len(cut)
                        .unwrap();
                }) as Job
            }),
            tamper.then(|| {
                let mutate = path.clone();
                Box::new(move || {
                    let mut bytes = std::fs::read(&mutate).unwrap();
                    let at = bytes.windows(3).position(|w| w == b"\"A\"").unwrap();
                    bytes[at + 1] = b'B';
                    std::fs::write(mutate, bytes).unwrap();
                }) as Job
            }),
        );
        let _ = settled_reason(&shared, &ProviderKind::Codex, channel).await;
        let expected = if tamper || shrink {
            Activity::Unknown
        } else {
            Activity::Idle
        };

        assert!(
            _hook.calls() > 0 || (!tamper && !shrink),
            "race hook never ran"
        );
        gate(
            &shared,
            &ProviderKind::Codex,
            channel,
            expected == Activity::Idle,
        )
        .await;
        assert_eq!(
            activity_now(&shared, &ProviderKind::Codex, ChannelId::new(channel))
                .await
                .activity,
            expected
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_full_cold_boundary_and_installed_growth_remain_full() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, extra) in [(0, 0usize), (1, 1)] {
        let channel = 69_183_500 + index;
        let shared = crate::services::discord::make_shared_data_for_tests();
        let path = root.path().join(format!("{channel}.jsonl"));
        write(&path, &[prompt("A"), end()]);
        let boundary = std::fs::metadata(&path).unwrap().len();
        let _limits =
            tail::overrides::Guard::new(channel, boundary + extra as u64, 4096, 4096, 4096);
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Claude,
            channel,
            &format!("s1-full-{channel}"),
            &path,
        );
        assert_eq!(
            settled_reason(&shared, &ProviderKind::Claude, channel).await,
            "closed"
        );
        let watch = lock(&WATCHES).get(&channel).unwrap().clone();
        assert!(matches!(lock(&watch).outcome, Outcome::Source { .. }));
        for _ in 0..4 {
            write(
                &path,
                &[json!({"type":"summary","summary":"installed growth"})],
            );
        }
        assert!(std::fs::metadata(&path).unwrap().len() > boundary + extra as u64);

        assert!(matches!(lock(&watch).outcome, Outcome::Source { .. }));
        gate(&shared, &ProviderKind::Claude, channel, true).await;
        assert_eq!(
            settled_reason(&shared, &ProviderKind::Claude, channel).await,
            "closed"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_tail_alignment_preserves_complete_record_and_never_expands_no_lf_window() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, utf8) in [false, true].into_iter().enumerate() {
        let channel = 69_183_900 + index as u64;
        let path = root.path().join(format!("{channel}.jsonl"));
        let prefix = json!({"type":"summary","summary":"🦀".repeat(200)});
        write(&path, &[prefix]);
        let boundary = std::fs::metadata(&path).unwrap().len();
        write(&path, &[end()]);
        let size = std::fs::metadata(&path).unwrap().len();
        let start = if utf8 {
            let bytes = std::fs::read(&path).unwrap();
            bytes.iter().position(|b| *b >= 0xf0).unwrap() as u64 + 1
        } else {
            boundary
        };
        let _limits = tail::overrides::Guard::new(channel, 1, size - start, 4096, 23);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Claude,
            channel,
            &format!("s1-aligned-{channel}"),
            &path,
        );
        assert_eq!(
            settled_reason(&shared, &ProviderKind::Claude, channel).await,
            "closed",
            "utf8={utf8}: fragment handling discarded the first complete record"
        );
        gate(&shared, &ProviderKind::Claude, channel, true).await;
    }
    let channel = 69_183_902;
    let path = root.path().join("no-lf.jsonl");
    std::fs::write(
        &path,
        format!("{{\"type\":\"summary\",\"summary\":\"{}", "🦀".repeat(200)),
    )
    .unwrap();
    let snapshot = std::fs::metadata(&path).unwrap().len();
    let _limits = tail::overrides::Guard::new(channel, 1, 31, 7, 11);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let _turn =
        bind_turn_mode_transcript(&shared, &ProviderKind::Claude, channel, "s1-no-lf", &path);
    let _ = activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
    let watch = lock(&WATCHES).get(&channel).unwrap().clone();
    super::installed(&watch).await;
    assert_eq!(
        activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel))
            .await
            .activity,
        Activity::Unknown
    );
    gate(&shared, &ProviderKind::Claude, channel, false).await;
    assert_eq!(
        match &lock(&watch).outcome {
            Outcome::Tail(reader) => reader.diagnostics().0,
            _ => panic!("no-LF Tail changed mode"),
        },
        snapshot - 31
    );
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(b"\"}\n").unwrap();
    write(&path, &[end()]);
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Claude, channel).await,
        "closed"
    );
    assert_eq!(
        match &lock(&watch).outcome {
            Outcome::Tail(reader) => reader.diagnostics().0,
            _ => panic!("no-LF Tail expanded its window"),
        },
        snapshot - 31
    );
}

#[tokio::test(flavor = "current_thread")]
async fn s1_append_after_bootstrap_snapshot_cannot_release_old_closed_fact() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    for (index, append) in [false, true].into_iter().enumerate() {
        let channel = 69_183_910 + index as u64;
        let path = root.path().join(format!("{channel}.jsonl"));
        write(&path, &[prompt("A"), end()]);
        let snapshot = std::fs::metadata(&path).unwrap().len();
        let _limits = tail::overrides::Guard::new(channel, 1, 4096, 7, 4096);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let trace = super::ReadTrace::new(channel);
        let _turn = bind_turn_mode_transcript(
            &shared,
            &ProviderKind::Claude,
            channel,
            &format!("s1-append-{channel}"),
            &path,
        );
        let changed = path.clone();
        let _hook = tail::tests::HandoffHook::new(
            channel,
            append.then(|| Box::new(move || write(&changed, &[prompt("B")])) as Job),
            None,
        );
        let _ = activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
        let watch = lock(&WATCHES).get(&channel).unwrap().clone();
        super::installed(&watch).await;
        let stages = trace.take();
        let initial: Vec<_> = stages
            .iter()
            .filter(|(stage, _)| *stage == "initial")
            .flat_map(|(_, reads)| reads)
            .collect();
        assert!(initial.iter().any(|(a, b, _)| b > a));
        assert!(
            initial.iter().all(|(_, b, _)| *b <= snapshot),
            "bootstrap chased bytes after L"
        );
        let answer = activity_now(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
        gate(&shared, &ProviderKind::Claude, channel, !append).await;
        assert_eq!(
            answer.activity,
            if append {
                Activity::Unknown
            } else {
                Activity::Idle
            },
            "{answer:?}"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn s1_new_binding_source_cannot_reuse_previous_codex_opener() {
    use crate::services::tui_prompt_dedupe::binding_events as p5;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let root = tempfile::tempdir().unwrap();
    let _binding = BindingRoot::enter(root.path());
    let channel = 69_183_920;
    let _limits = tail::overrides::Guard::new(channel, 1, 4096, 4096, 31);
    let shared = crate::services::discord::make_shared_data_for_tests();
    let old = root.path().join("old.jsonl");
    let header = json!({"type":"session_meta","payload":{"id":"parent","cwd":root.path()}});
    write(
        &old,
        &[
            header.clone(),
            codex_event("task_started", Some("A")),
            codex_event("task_complete", Some("A")),
        ],
    );
    let session = "s1-new-source";
    let _turn = bind_turn_mode_transcript(&shared, &ProviderKind::Codex, channel, session, &old);
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Codex, channel).await,
        "closed"
    );
    let next = root.path().join("new.jsonl");
    let mut header = header;
    header["payload"]["id"] = "replacement".into();
    write(&next, &[header, codex_event("task_complete", Some("A"))]);
    let proposal = p5::Proposal {
        channel_id: channel,
        provider: "codex",
        tmux_session: session,
        session_id: Some("replacement"),
        path: next.to_str().unwrap(),
        replaced: Some((old.to_str().unwrap(), Some("parent"))),
        cause: p5::CauseSource::Hook(p5::BindingCause::Startup),
        hook: None,
    };
    let source = source_id_for("replacement", &next).unwrap();
    assert_eq!(
        p5::record_verified(&proposal, &source).unwrap(),
        p5::Committed::Appended
    );
    assert_eq!(*p5::subscribe_binding_events(channel).unwrap().borrow(), 2);
    let reason = settled_reason(&shared, &ProviderKind::Codex, channel).await;
    gate(&shared, &ProviderKind::Codex, channel, false).await;
    assert_eq!(reason, "no_turn_boundary");
    write(
        &next,
        &[
            codex_event("task_started", Some("B")),
            codex_event("task_complete", Some("B")),
        ],
    );
    assert_eq!(
        settled_reason(&shared, &ProviderKind::Codex, channel).await,
        "closed"
    );
}

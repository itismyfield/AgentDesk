use super::*;
use crate::services::discord::turn_view_reconciler as view;
use crate::test_support::wait::{WAIT_CAP, await_within};
use poise::serenity_prelude::{self as serenity, MessageId, UserId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn active_root() -> crate::config::TestRuntimeRootGuard {
    let root = crate::config::TestRuntimeRootGuard::new();
    let generation = discord::runtime_store::generation_path().unwrap();
    fs::create_dir_all(generation.parent().unwrap()).unwrap();
    fs::write(generation, "1").unwrap();
    root
}

async fn busy_active_arrival(rig: &IntakeRig, bot: bool) -> MessageId {
    let persistence =
        discord::queue_persistence_context(&rig.shared, &rig.shared.provider, rig.channel);
    assert_eq!(
        rig.mailbox
            .clear(persistence)
            .await
            .discarded_message_ids
            .len(),
        1
    );
    discord::queue_io::cancel_backstop_test_support::set_owner_wake_for_tests(
        rig.channel,
        Some(Arc::new(|| {})),
    );
    assert!(
        rig.mailbox
            .try_start_turn(
                Arc::new(CancelToken::new()),
                UserId::new(7),
                MessageId::new(1_479_671_302_387_076_000 + rig.channel.get()),
            )
            .await
    );
    let (session, native) = {
        let core = rig.shared.core.lock().await;
        let session = core.sessions.get(&rig.channel).unwrap();
        (
            rig.shared
                .provider
                .build_tmux_session_name(session.channel_name.as_deref().unwrap()),
            session.session_id.clone(),
        )
    };
    let row = discord::inflight::InflightTurnState::new(
        rig.shared.provider.clone(),
        rig.channel.get(),
        None,
        1,
        1_479_671_302_387_076_000 + rig.channel.get(),
        0,
        "active fixture".into(),
        native,
        Some(session),
        Some(rig.transcript.display().to_string()),
        None,
        0,
    );
    let path = discord::inflight::inflight_runtime_root()
        .unwrap()
        .join(rig.shared.provider.as_str())
        .join(format!("{}.json", rig.channel.get()));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(&row).unwrap()).unwrap();
    let message_id = MessageId::new(1_479_671_302_387_077_000 + rig.channel.get());
    let mut message = discord_mock::user_message(
        message_id.get(),
        if bot {
            "DISPATCH: queued bot input"
        } else {
            PROMPT
        },
    );
    message.channel_id = rig.channel;
    message.guild_id = Some(serenity::GuildId::new(96_326_000_999));
    message.author.bot = bot;
    {
        let mut settings = rig.shared.settings.write().await;
        settings.owner_user_id = Some(message.author.id.get());
        settings.allow_all_users = true;
        settings.allowed_channel_ids = vec![rig.channel.get()];
        if bot {
            settings.allowed_bot_ids.push(message.author.id.get());
        }
    }
    rig.shared
        .restart
        .reconcile_done
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let data = Arc::new(discord::Data {
        shared: rig.shared.clone(),
        token: "test-token".into(),
        provider: rig.shared.provider.clone(),
    });
    let event = serenity::FullEvent::Message {
        new_message: message,
    };
    let ctx = rig.shared.http.cached_serenity_ctx.get().unwrap();
    await_within(
        Box::pin(discord::router::handle_event(ctx, &event, &data)),
        WAIT_CAP,
    )
    .await
    .unwrap();
    assert!(
        rig.trace.text().contains(if bot {
            "dispatch_guard"
        } else {
            "busy_active_turn"
        }),
        "{}",
        rig.trace.text()
    );
    assert_eq!(
        rig.state().await["items"].as_array().unwrap().len(),
        1,
        "{}",
        rig.trace.text()
    );
    message_id
}

fn active_message_ops(rig: &IntakeRig, message: MessageId) -> Vec<(char, bool)> {
    rig.shared
        .turn_view_reconciler
        .ops()
        .into_iter()
        .filter(|op| op.target.message_id == message && op.target.channel_id == rig.channel)
        .map(|op| (op.emoji, op.add))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_success_has_only_one_plus_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_success_has_only_one_plus_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_101, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Ok(()),
        "{}",
        rig.trace.text()
    );
    assert_eq!(active_message_ops(&rig, message), [('➕', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_handback_has_only_one_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_handback_has_only_one_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::busy_with_screen(
        8_113_102,
        "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n".into(),
    )
    .await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Err("NotSent(\"Modal\")".into())
    );
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_merged_handback_marks_each_generation_once() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_merged_handback_marks_each_generation_once"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::busy_with_screen(
        8_113_111,
        "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n".into(),
    )
    .await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let first = busy_active_arrival(&rig, false).await;
    let second = MessageId::new(first.get() + 1);
    rig.mailbox
        .v2_request(move |slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let mut follower = owner.snapshot.items[0].clone();
            follower.message_id = second.get();
            follower.queued_generation += 1;
            follower.source_message_ids = vec![second.get()];
            follower.source_text_segments.clear();
            for source in &mut follower.source_message_queued_generations {
                source.message_id = second.get();
                source.queued_generation = follower.queued_generation;
            }
            assert!(owner.enqueue_fixture(&follower).map_err(String::from)?);
            assert_eq!(owner.followers().len(), 1);
            Ok(())
        })
        .await
        .unwrap();
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Err("NotSent(\"Modal\")".into())
    );
    assert_eq!(active_message_ops(&rig, first), [('📬', true)]);
    assert_eq!(active_message_ops(&rig, second), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_unconfirmed_has_only_one_question_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_unconfirmed_has_only_one_question_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_103, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let shim = rig._tmux.root.path().join("tmux");
    let original = fs::read_to_string(&shim).unwrap();
    fs::write(
        &shim,
        original.replacen(
            "#!/bin/sh\n",
            "#!/bin/sh\ncase \"$*\" in *send-keys*Enter*) exit 1 ;; esac\n",
            1,
        ),
    )
    .unwrap();
    let message = busy_active_arrival(&rig, false).await;
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert!(
        await_within(settled.recv(), WAIT_CAP)
            .await
            .unwrap()
            .as_ref()
            .is_err_and(|error| !effect::surely_unsent(error))
    );
    assert_eq!(active_message_ops(&rig, message), [('❓', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_closed_gate_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_closed_gate_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_104, true).await;
    let message = busy_active_arrival(&rig, false).await;
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_owner_keeps_pending_without_dequeue() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_owner_keeps_pending_without_dequeue"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_112, true).await;
    let message = busy_active_arrival(&rig, false).await;
    assert_eq!(rig.state().await["items"][0]["message_id"], message.get());
    assert!(rig.mailbox.has_active_turn().await.unwrap());
    assert!(events(&rig.renderer).is_empty());
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_transition_handback_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_transition_handback_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_108, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    let _transition = rig
        .shared
        .session_transition_lock(rig.channel)
        .lock_owned()
        .await;
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Err("NotSent(\"session_transition_busy\")".into())
    );
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_restart_handback_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_restart_handback_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_109, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    rig.shared
        .restart
        .reconcile_done
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Err("NotSent(\"restart_recovery\")".into())
    );
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_upload_handback_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_upload_handback_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_110, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    let upload = serde_json::from_value(json!({"bundle_id":uuid::Uuid::new_v4(),
        "identity":{"provider":"claude","channel_id":rig.channel.to_string(),"user_msg_id":message.to_string()},
        "source_count":1})).unwrap();
    rig.mailbox
        .v2_request(move |slot| {
            slot.as_mut().ok_or("owner")?.snapshot.items[0]
                .pending_uploads
                .push(upload);
            Ok(())
        })
        .await
        .unwrap();
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Err("NotSent(\"upload_materialization_failed\")".into())
    );
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_allowed_bot_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_allowed_bot_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_105, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, true).await;
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_active_codex_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_codex_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::build(
        8_113_106,
        true,
        ProviderKind::Codex,
        None,
        Some(false),
        None,
        None,
    )
    .await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let message = busy_active_arrival(&rig, false).await;
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

#[tokio::test(flavor = "current_thread")]
async fn busy_active_herdr_keeps_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_active_herdr_keeps_mailbox_op"
    )) {
        return;
    }
    let _root = active_root();
    let rig = IntakeRig::new(8_113_107, true).await;
    let _host =
        crate::config::session_hosts::force_for_test(Some("mini"), &[(rig.channel.get(), "mini")]);
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    assert!(crate::config::session_hosts::herdr_endpoint(rig.channel.get()).is_some());
    let message = busy_active_arrival(&rig, false).await;
    assert_eq!(active_message_ops(&rig, message), [('📬', true)]);
}

fn pause_next_load(rig: &IntakeRig) -> tokio::net::UnixListener {
    let socket = rig._tmux.root.path().join("input-barrier.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let script = rig._tmux.root.path().join("input-barrier.py");
    fs::write(
        &script,
        format!(
            "import socket\ns = socket.socket(socket.AF_UNIX)\ns.connect({})\ns.sendall(b'R')\ns.recv(1)\n",
            serde_json::to_string(&socket).unwrap()
        ),
    )
    .unwrap();
    let shim = rig._tmux.root.path().join("tmux");
    let original = fs::read_to_string(&shim).unwrap();
    let once = rig._tmux.root.path().join("input-barrier-used");
    let hook = format!(
        "case \"$*\" in *load-buffer*) if [ ! -e '{once}' ]; then : > '{once}'; python3 '{script}'; fi ;; esac\n",
        once = once.display(),
        script = script.display()
    );
    let wrapped = original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{hook}"), 1);
    assert_ne!(wrapped, original);
    fs::write(shim, wrapped).unwrap();
    listener
}

fn reaction_ops(rig: &IntakeRig) -> Vec<(char, bool)> {
    rig.shared
        .turn_view_reconciler
        .ops()
        .into_iter()
        .filter(|op| op.target.channel_id == rig.channel)
        .map(|op| (op.emoji, op.add))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_busy_input_recovers_without_a_pending_view() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "accepted_busy_input_recovers_without_a_pending_view"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_991_104, true).await;
    let before = rig.state().await;
    assert_eq!(before["items"].as_array().unwrap().len(), 1);
    assert_eq!(before["admission_receipts"].as_array().unwrap().len(), 1);
    assert!(reaction_ops(&rig).is_empty());
    assert_eq!(
        view::sweep_orphan_tui_anchor_reactions(&rig.shared, &rig.shared.provider).await,
        0
    );
    let registry = ChannelMailboxRegistry::default();
    let recovered = registry.handle(rig.channel);
    let persistence = discord::queue_dispatch::persistence_context(
        &rig.shared,
        &rig.shared.provider,
        rig.channel,
    );
    let queue = crate::services::turn_orchestrator::v2_queue(&persistence, rig.channel).unwrap();
    let history = crate::services::tui_input::human_frame::history::v2::Registry::at(
        queue.0.with_extension("accepted-history.json"),
    );
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    assert_eq!(
        await_within(
            discord::queue_io::input_owner::recovery::recover(
                &recovered,
                queue,
                history,
                None,
                ("claude", rig.channel.get(), now)
            ),
            WAIT_CAP
        )
        .await,
        Ok(0)
    );
    let after = recovered
        .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
        .await
        .unwrap();
    assert_eq!(after["items"], before["items"]);
    assert_eq!(after["admission_receipts"], before["admission_receipts"]);
    assert!(reaction_ops(&rig).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_handback_has_only_one_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_handback_has_only_one_mailbox_op"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n";
    let rig = IntakeRig::busy_with_screen(7_991_102, modal.into()).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    let result = await_within(settled.recv(), WAIT_CAP).await.unwrap();
    assert_eq!(result, Err("NotSent(\"Modal\")".into()));
    assert!(events(&rig.renderer).is_empty());
    assert_eq!(rig.state().await["items"].as_array().unwrap().len(), 1);
    assert_eq!(reaction_ops(&rig), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_gate_handback_without_stored_route_has_one_mailbox_op() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "parent_gate_handback_without_stored_route_has_one_mailbox_op"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let modal = "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEnter to select · Esc to cancel\n";
    let rig = IntakeRig::busy_with_screen(7_991_107, modal.into()).await;
    let parent = rig.channel.get() + 100;
    rig.mock.unname_channel(rig.channel.get());
    rig.mock.name_channel(parent, "parent-gate");
    rig.mock.add_thread(rig.channel.get(), parent);
    let _gate = discord::router::busy_inject_support::open_gate(parent);
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    let result = await_within(settled.recv(), WAIT_CAP).await.unwrap();
    assert_eq!(
        result,
        Err("NotSent(\"Modal\")".into()),
        "{}",
        rig.trace.text()
    );
    assert!(
        rig.trace
            .text()
            .contains("prior turn genuinely busy per JSONL")
    );
    assert_eq!(reaction_ops(&rig), [('📬', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_unconfirmed_has_only_one_question_op_and_recovers_without_pending() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_unconfirmed_has_only_one_question_op_and_recovers_without_pending"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_991_103, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let shim = rig._tmux.root.path().join("tmux");
    let original = fs::read_to_string(&shim).unwrap();
    fs::write(
        &shim,
        original.replacen(
            "#!/bin/sh\n",
            "#!/bin/sh\ncase \"$*\" in *send-keys*Enter*) exit 1 ;; esac\n",
            1,
        ),
    )
    .unwrap();
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    let result = await_within(settled.recv(), WAIT_CAP).await.unwrap();
    assert!(
        result
            .as_ref()
            .is_err_and(|error| !effect::surely_unsent(error)),
        "{result:?}"
    );
    assert_eq!(reaction_ops(&rig), [('❓', true)]);
    assert_eq!(
        view::sweep_orphan_tui_anchor_reactions(&rig.shared, &rig.shared.provider).await,
        0
    );
    let persistence = discord::queue_dispatch::persistence_context(
        &rig.shared,
        &rig.shared.provider,
        rig.channel,
    );
    let queue = crate::services::turn_orchestrator::v2_queue(&persistence, rig.channel).unwrap();
    let registry = ChannelMailboxRegistry::default();
    let recovered = registry.handle(rig.channel);
    let history_path = queue.0.with_extension("recovered-history.json");
    let history =
        crate::services::tui_input::human_frame::history::v2::Registry::at(history_path.clone());
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64 + 86_400_000;
    assert_eq!(
        await_within(
            discord::queue_io::input_owner::recovery::recover(
                &recovered,
                queue,
                history,
                None,
                ("claude", rig.channel.get(), now)
            ),
            WAIT_CAP
        )
        .await,
        Ok(0)
    );
    let state = recovered
        .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
        .await
        .unwrap();
    assert_eq!(
        state["items"],
        json!([]),
        "uncertain input must not be resent"
    );
    assert_eq!(state["retained_inputs"].as_array().unwrap().len(), 1);
    assert_eq!(state["retained_inputs"][0]["reason"]["kind"], "unconfirmed");
    let projected: Value =
        serde_json::from_str(&fs::read_to_string(history_path).unwrap()).unwrap();
    assert_eq!(projected["passive"].as_array().unwrap().len(), 1);
    assert_eq!(projected["passive"][0]["state"], "retained");
    assert_eq!(
        projected["passive"][0]["registration"]["key"]["identity"],
        state["retained_inputs"][0]["input"]["identity"]
    );
    assert_eq!(reaction_ops(&rig), [('❓', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_crash_snapshot_before_first_key_preserves_the_accepted_original() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "busy_crash_snapshot_before_first_key_preserves_the_accepted_original"
    )) {
        return;
    }
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_991_105, true).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let listener = pause_next_load(&rig);
    let message = rig.state().await["items"][0]["message_id"]
        .as_u64()
        .unwrap();
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    let (mut control, _) = await_within(listener.accept(), WAIT_CAP).await.unwrap();
    assert_eq!(
        await_within(control.read_u8(), WAIT_CAP).await.unwrap(),
        b'R'
    );
    assert!(events(&rig.renderer).is_empty());
    assert!(reaction_ops(&rig).is_empty());
    let persistence = discord::queue_dispatch::persistence_context(
        &rig.shared,
        &rig.shared.provider,
        rig.channel,
    );
    let (path, namespace) =
        crate::services::turn_orchestrator::v2_queue(&persistence, rig.channel).unwrap();
    let snapshot = path.with_extension("crash.json");
    fs::copy(path, &snapshot).unwrap();
    let registry = ChannelMailboxRegistry::default();
    let recovered = registry.handle(rig.channel);
    let history_path = snapshot.with_extension("history.json");
    let history =
        crate::services::tui_input::human_frame::history::v2::Registry::at(history_path.clone());
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64 + 86_400_000;
    assert_eq!(
        await_within(
            discord::queue_io::input_owner::recovery::recover(
                &recovered,
                (snapshot, namespace),
                history,
                None,
                ("claude", rig.channel.get(), now)
            ),
            WAIT_CAP
        )
        .await,
        Ok(0)
    );
    let state = recovered
        .v2_request(|slot| slot.as_ref().ok_or("owner")?.json())
        .await
        .unwrap();
    let _: Value = serde_json::from_str(&fs::read_to_string(history_path).unwrap()).unwrap();
    let queued = state["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["message_id"] == message)
        .count();
    let retained = state["retained_inputs"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["input"]["original"]["message_id"] == message)
        .count();
    assert_eq!(
        queued + retained,
        1,
        "a crash keeps exactly one durable original: {state}"
    );
    assert!(reaction_ops(&rig).is_empty());
    await_within(control.write_all(b"G"), WAIT_CAP)
        .await
        .unwrap();
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Ok(())
    );
    assert_eq!(reaction_ops(&rig), [('➕', true)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gated_idle_start_restores_pending_and_delivers_native_answer() {
    if !creator_child(concat!(
        "services::discord::router::message_handler::provider_dispatch::owner_input::tests::rollback::single_marker::",
        "gated_idle_start_restores_pending_and_delivers_native_answer"
    )) {
        return;
    }
    use notify::Watcher;
    use std::io::Write;
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = IntakeRig::new(7_991_106, false).await;
    let _gate = discord::router::busy_inject_support::open_gate(rig.channel.get());
    let session = rig.shared.core.lock().await.sessions[&rig.channel]
        .session_id
        .clone()
        .unwrap();
    fs::write(rig.renderer.join("native-sid"), session).unwrap();
    fs::write(rig.renderer.join("native-uuid"), "single-idle-user").unwrap();
    fs::create_dir_all(rig.transcript.parent().unwrap()).unwrap();
    fs::write(rig.renderer.join("transcript"), IDLE).unwrap();
    fs::hard_link(rig.renderer.join("transcript"), &rig.transcript).unwrap();
    let listener = pause_next_load(&rig);
    let (user_row, mut recorded) = tokio::sync::mpsc::unbounded_channel();
    let transcript = rig.transcript.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.is_ok_and(|event| event.kind.is_modify())
            && fs::read_to_string(&transcript).is_ok_and(|text| {
                text.lines().any(|line| {
                    serde_json::from_str::<Value>(line).is_ok_and(|row| {
                        row["type"] == "user"
                            && row["message"]["content"]
                                .as_str()
                                .is_some_and(|body| body.contains(PROMPT))
                    })
                })
            })
        {
            let _ = user_row.send(());
        }
    })
    .unwrap();
    watcher
        .watch(&rig.transcript, notify::RecursiveMode::NonRecursive)
        .unwrap();
    let (completed, bridge) = tokio::sync::oneshot::channel();
    *discord::turn_bridge::resume_pin_tests::BRIDGE_COMPLETION_PROBE
        .lock()
        .unwrap() = Some((rig.channel, completed));
    let (drive, mut settled) = rig.drive();
    discord::queue_io::input_owner::drive::wake(drive);
    let (mut control, _) = await_within(listener.accept(), WAIT_CAP).await.unwrap();
    assert_eq!(
        await_within(control.read_u8(), WAIT_CAP).await.unwrap(),
        b'R'
    );
    assert_eq!(reaction_ops(&rig), [('⏳', true)]);
    await_within(control.write_all(b"G"), WAIT_CAP)
        .await
        .unwrap();
    assert_eq!(
        await_within(settled.recv(), WAIT_CAP).await.unwrap(),
        Ok(())
    );
    await_within(recorded.recv(), WAIT_CAP).await.unwrap();
    let mut native = fs::OpenOptions::new()
        .append(true)
        .open(&rig.transcript)
        .unwrap();
    writeln!(native, "{}", json!({"type":"assistant","uuid":"single-idle-answer","apiBlockIndex":0,"message":{"id":"single-idle-answer","role":"assistant","content":[{"type":"text","text":"idle completed"}],"stop_reason":"end_turn"}})).unwrap();
    writeln!(
        native,
        "{}",
        json!({"type":"system","subtype":"turn_duration"})
    )
    .unwrap();
    await_within(bridge, WAIT_CAP).await.unwrap();
    assert!(
        rig.mock
            .written_texts()
            .iter()
            .any(|text| text == "idle completed")
    );
    assert!(
        reaction_ops(&rig)
            .iter()
            .all(|(emoji, _)| ['⏳', '✅'].contains(emoji))
    );
}

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{
    PreparedIncarnation, ResumeStart, execution_context,
};

#[test]
fn resumed_pending_after_boot_reads_only_seed_reply_pg() {
    let fx = Fixture::admitted("restart-resume");
    let launcher = Arc::new(Launcher::default());
    let ports = fx.ports(&launcher);
    let original = Mutex::new(None);
    let (result, _) = fx.turn(&HostedRecord::Legacy, &ports, || {
        let Some(nonce) = fx.start_provider(&launcher, true) else {
            return;
        };
        *original.lock().unwrap() = fx.answer(&nonce).map(|(id, path)| (nonce, id, path));
    });
    assert_eq!(result, Ok(()));
    let (old_nonce, id, path) = original.into_inner().unwrap().unwrap();
    let source = binding_events_since(CHANNEL, 0)
        .unwrap()
        .into_iter()
        .rev()
        .find_map(|event| match event.new {
            BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
            _ => None,
        })
        .unwrap();
    let mut o = ODrive::new();
    let posts = fx.rt.block_on(o.step());
    assert_eq!(posts.iter().filter(|text| text.trim() == "답").count(), 1);
    fx.rig.provider_seen(&context_of(&old_nonce), false);
    let start =
        ResumeStart::capture("restart-command".into(), CHANNEL, &source, o.store()).unwrap();
    assert_eq!(o.captured_through(&source), Some(start.offset));
    let mut context = execution_context("codex", &old_nonce).unwrap();
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    context.execution_nonce = nonce.clone();
    context.launch_mode = "resume".into();
    context.expected_native_session_id = Some(id.clone());
    context.resume_start = Some(start.clone());
    let prepared = PreparedIncarnation::create(context).unwrap();
    append(
        &path,
        &[json!({"type":"event_msg","payload":{"type":"session_resumed"}})],
    );
    assert!(std::fs::metadata(&path).unwrap().len() > start.offset);
    let HostedRecord::Known(mut record) = fx.record() else {
        panic!("the original execution bound its source");
    };
    record.execution_nonce = nonce.clone();
    record.expected.as_mut().unwrap().binding_nonce = nonce.clone();
    record.source_ref.execution_nonce = nonce.clone();
    record.source_ref.initial_source = None;
    record.source_ref.baseline_event_seq = None;
    record.state = HostedState::Pending;
    fx.rt
        .block_on(
            sqlx::query("UPDATE sessions SET hosted_execution=$1 WHERE channel_id=$2")
                .bind(serde_json::to_value(&record).unwrap())
                .bind(CHANNEL.to_string())
                .execute(&fx.pool),
        )
        .unwrap();
    let kept = crate::services::codex_tui::session::read_codex_tui_launch_options_fingerprint(
        fx.logical(),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_launch_options_fingerprint(
        fx.logical(),
        &kept.replacen(&old_nonce, &nonce, 1),
    )
    .unwrap();
    crate::services::tui_prompt_dedupe::reset_state_for_tests();
    crate::services::tui_prompt_dedupe::install_herdr_execution(fx.logical(), &nonce);
    crate::services::discord::stamp_spawn_markers(fx.logical(), Some(&prepared)).unwrap();
    fx.rig.run_provider(&prepared.path, false);
    o.restart();
    let restored = execution_context("codex", &nonce).unwrap();
    assert_eq!(restored.resume_start, Some(start.clone()));
    let (result, messages) = fx.turn(&HostedRecord::Known(record), &ports, || {
        if !wait_for(&fx.finished, "the restored seed", || {
            fx.rig.sends().len() == 4
        }) {
            return;
        }
        let envelope = HookBindingEnvelope::capture_from_env("codex", |key| {
            (key == "AGENTDESK_BINDING_CONTEXT").then(|| prepared.path.as_os_str().to_owned())
        });
        let payload = json!({"session_id":id,"source":"resume","hook_event_name":"SessionStart",
            "cwd":fx.cwd,"transcript_path":path});
        let uri = format!("/hooks/codex/SessionStart?session_id={id}");
        fx.started.store(true, Ordering::SeqCst);
        assert_eq!(
            fx.ingress
                .send_envelope(&uri, &payload, None, Some(&envelope.encode().unwrap()))
                .0,
            202
        );
        append(&path, &turn_lines("restart-seed", "NEW_SEED_REPLY"));
    });
    assert_eq!(
        result,
        Ok(()),
        "Resume must bind instead of waiting for Startup"
    );
    assert_eq!(texts(&messages), ["NEW_SEED_REPLY"]);
    assert_eq!(fx.row(), Some(HostedState::Bound));
    assert_eq!(
        crate::services::codex_tui::session::read_codex_tui_rollout_marker(fx.logical())
            .unwrap()
            .rollout_start_offset,
        Some(start.offset),
        "startup metadata must not move the seed's durable boundary",
    );
    assert_eq!(
        launcher.creates.load(Ordering::SeqCst),
        1,
        "boot reuses Pending"
    );
    let posts = fx.rt.block_on(o.step());
    assert_eq!(
        posts.iter().filter(|text| text.trim() == "답").count(),
        1,
        "historical O output is not posted twice"
    );
    assert_eq!(
        posts
            .iter()
            .filter(|text| text.trim() == "NEW_SEED_REPLY")
            .count(),
        1
    );
}

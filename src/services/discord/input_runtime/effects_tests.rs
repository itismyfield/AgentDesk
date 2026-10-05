#![cfg(any(target_os = "macos", target_os = "linux"))]
use super::*;
use crate::services::discord::inflight::InflightTurnState;
use crate::services::discord::input_transition::Files;
use crate::services::tui_input::ledger::{Ledger, LedgerLease, OPENS};
use crate::services::tui_input::rows::{
    AbandonReason, AttemptEvidence, DoneReason, Entry, HeldReason, Owner, RowState,
};
use crate::services::tui_input::transition::{Host, Move, Outcome, handback};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use serde_json::json;
use std::fs;
use std::io::Write;

const CHANNEL: u64 = 9;

fn sandbox() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn effects(root: &Path, closing: Arc<Closing>, rt: &tokio::runtime::Runtime) -> ProdEffects {
    ProdEffects {
        root: root.to_owned(),
        deps: Deps {
            provider: ProviderKind::Claude,
            channel: CHANNEL,
            tmux_session: "e1-fixture-session-absent".into(),
            token_hash: "token".into(),
            authorized: true,
            active_sources: Vec::new(),
            pool: None,
            closing,
        },
        runtime: rt.handle().clone(),
        notices: Vec::new(),
        actor_requests: 0,
        outbox_for_test: Some(false),
    }
}

fn frozen() -> Arc<Closing> {
    Closing::frozen_for_test(ProviderKind::Claude, CHANNEL)
}

fn files(root: &Path, effects: ProdEffects) -> Files<ProdEffects> {
    let closing = effects.deps.closing.clone();
    Files::frozen(root, ProviderKind::Claude, CHANNEL, closing, effects).unwrap()
}

fn item(key: u64) -> Value {
    json!({"author_id": 7, "message_id": key, "text": format!("input {key}"), "channel_id": CHANNEL})
}

fn save(path: &Path, value: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn state(root: &Path, key: u64) -> RowState {
    let rows = Ledger::open(root, CHANNEL).unwrap().rows().unwrap();
    rows.row(key).unwrap().state
}

#[test]
fn e1_production_effects_move_population_and_request_one_actor() {
    let rt = runtime();
    let root = sandbox();
    let queue = root
        .path()
        .join("discord_pending_queue/claude/token/9.json");
    save(&queue, &json!([item(8)]));
    let marker = queue.with_extension("dispatch");
    save(&marker, &item(12));
    let placeholder = root
        .path()
        .join("discord_queued_placeholders/claude/token/9.json");
    save(
        &placeholder,
        &json!([{"user_message_id": 8, "placeholder_message_id": 108}]),
    );
    let busy = root
        .path()
        .join("discord_busy_followup_retries/claude/9/8.json");
    save(
        &busy,
        &json!({"notice_message_id": 108, "busy_retry_count": 1, "first_busy_retry_at_ms": 100}),
    );
    let mut host = files(root.path(), effects(root.path(), frozen(), &rt));
    let mut lease = LedgerLease::new(root.path(), CHANNEL);
    let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
    assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
    for path in [&queue, &marker, &placeholder, &busy] {
        assert!(!path.exists(), "{} must be retired", path.display());
    }
    assert_eq!(state(root.path(), 8), RowState::Received);
    // A dispatch-only marker has no proof it was never pasted.
    assert_eq!(
        state(root.path(), 12),
        RowState::Held(HeldReason::Ambiguous)
    );
    let effects = host.effects_mut();
    assert_eq!(effects.actor_requests(), 1);
    assert_eq!(
        effects.take_notices(),
        vec![(Some(12), "move_input_requires_attention")]
    );
}

#[test]
fn e1_open_or_unreadable_outbox_never_moves_anything() {
    let rt = runtime();
    // Some(true) is an open intake row; None leaves the probe without Postgres.
    for outbox in [Some(true), None] {
        let root = sandbox();
        let queue = root
            .path()
            .join("discord_pending_queue/claude/token/9.json");
        save(&queue, &json!([item(8)]));
        let original = fs::read(&queue).unwrap();
        let mut effects = effects(root.path(), frozen(), &rt);
        effects.outbox_for_test = outbox;
        let mut host = files(root.path(), effects);
        let mut lease = LedgerLease::new(root.path(), CHANNEL);
        let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
        assert_eq!(
            movement.advance(&mut lease, &mut host),
            Outcome::Legacy,
            "{outbox:?}"
        );
        assert_eq!(fs::read(&queue).unwrap(), original);
        let rows = Ledger::open(root.path(), CHANNEL).unwrap().rows().unwrap();
        assert_eq!(rows.owner(8), Owner::Legacy);
        let effects = host.effects_mut();
        assert_eq!(effects.actor_requests(), 0);
        assert_eq!(
            effects.take_notices(),
            vec![(None, "tui_o:turn_mode_refused")]
        );
    }
}

#[test]
fn e1_turn_row_residue_is_held_with_notice_never_reinjected() {
    let rt = runtime();
    let root = sandbox();
    let row = InflightTurnState::new(
        ProviderKind::Claude,
        CHANNEL,
        None,
        7,
        20,
        0,
        "input".into(),
        None,
        Some("fixture".into()),
        None,
        None,
        0,
    );
    let path = root.path().join("discord_inflight/claude/9.json");
    save(&path, &serde_json::to_value(&row).unwrap());
    let mut host = files(root.path(), effects(root.path(), frozen(), &rt));
    let mut lease = LedgerLease::new(root.path(), CHANNEL);
    let mut movement = Move::prepare(&mut lease, &mut host).unwrap();
    assert_eq!(movement.advance(&mut lease, &mut host), Outcome::Ledger);
    assert!(!path.exists());
    assert_eq!(
        state(root.path(), 20),
        RowState::Held(HeldReason::Ambiguous)
    );
    assert_eq!(
        host.effects_mut().take_notices(),
        vec![(Some(20), "move_input_requires_attention")]
    );
}

#[test]
fn e1_pinned_upload_handback_copies_outside_the_guard_with_one_ledger_handle() {
    let rt = runtime();
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    let pin = ledger
        .pin_blob("8", 0, "sample.txt", b"attachment")
        .unwrap();
    let mut legacy = item(8);
    legacy["pending_uploads"] = json!(["[File uploaded] sample.txt → /gone/sample.txt (10 bytes)"]);
    let mut input = legacy.clone();
    input["legacy_input"] = legacy;
    input["pending_uploads"] = json!(["[File uploaded] sample.txt → pinned (10 bytes)"]);
    input["blob_pins"] = json!([pin.clone()]);
    ledger
        .append_entry(&Entry::Received { key: 8, input }, &[pin])
        .unwrap();
    drop(ledger);
    let closing = frozen();
    closing.begin_handback().unwrap();
    let mut host = files(root.path(), effects(root.path(), closing, &rt));
    let opens = OPENS.with(|opens| opens.get());
    assert_eq!(
        handback(&mut LedgerLease::new(root.path(), CHANNEL), &mut host).unwrap(),
        Outcome::Legacy
    );
    assert_eq!(
        OPENS.with(|opens| opens.get()) - opens,
        1,
        "the handback's own handle is the only one opened"
    );
    assert_eq!(
        state(root.path(), 8),
        RowState::Abandoned(AbandonReason::Handback)
    );
    let queue: Value = serde_json::from_slice(
        &fs::read(
            root.path()
                .join("discord_pending_queue/claude/token/9.json"),
        )
        .unwrap(),
    )
    .unwrap();
    let entry = &queue[0];
    assert!(entry.get("blob_pins").is_none() && entry.get("legacy_input").is_none());
    let upload = entry["pending_uploads"][0].as_str().unwrap();
    let copied = upload
        .split_once(" → ")
        .unwrap()
        .1
        .rsplit_once(" (")
        .unwrap()
        .0;
    assert!(Path::new(copied).starts_with(root.path().join("discord_uploads/9")));
    assert_eq!(fs::read(copied).unwrap(), b"attachment");
}

fn transcript(root: &Path) -> (PathBuf, SourceBinding) {
    let path = root.join("session.jsonl");
    fs::write(&path, "{\"type\":\"summary\"}\n").unwrap();
    let binding = SourceBinding {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        source: source_id_for("session", &path).unwrap(),
    };
    (path, binding)
}

fn attempted(root: &Path, binding: &SourceBinding, eof: u64, prompt: &str, state: RowState) {
    let mut ledger = Ledger::open(root, CHANNEL).unwrap();
    let mut input = item(8);
    input["legacy_input"] = item(8);
    ledger
        .append_entry(&Entry::Received { key: 8, input }, &[])
        .unwrap();
    let attempt = AttemptEvidence {
        binding: binding.clone(),
        execution_nonce: "nonce".into(),
        eof,
        rendered_prompt: prompt.into(),
        source_ids: vec![8],
        record_end: None,
        native_turn_id: None,
    };
    ledger
        .append_entry(
            &Entry::Transition {
                key: 8,
                state,
                attempt: Some(attempt),
            },
            &[],
        )
        .unwrap();
}

#[test]
fn e1_handback_judges_a_ledger_attempt_by_its_own_exact_witness() {
    let rt = runtime();
    let prompt = "[adk:source:8]\ninput 8\n[adk:end]";
    let cases = [RowState::Injecting, RowState::Running]
        .into_iter()
        .flat_map(|state| [(state, true), (state, false)]);
    for (attempt_state, accepted) in cases {
        let root = sandbox();
        let (path, binding) = transcript(root.path());
        let eof = fs::metadata(&path).unwrap().len();
        attempted(root.path(), &binding, eof, prompt, attempt_state);
        if accepted {
            let record = json!({
                "type": "user",
                "uuid": uuid::Uuid::new_v4().to_string(),
                "message": { "role": "user", "content": prompt },
            });
            let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
            writeln!(file, "{record}").unwrap();
        }
        let closing = frozen();
        closing.begin_handback().unwrap();
        let mut host = files(root.path(), effects(root.path(), closing, &rt));
        let outcome = handback(&mut LedgerLease::new(root.path(), CHANNEL), &mut host).unwrap();
        let queue = root
            .path()
            .join("discord_pending_queue/claude/token/9.json");
        assert!(!queue.exists(), "an attempted input is never re-queued");
        if accepted {
            assert_eq!(outcome, Outcome::Legacy, "{attempt_state:?}");
            assert_eq!(
                state(root.path(), 8),
                RowState::Done(DoneReason::HandbackRunning)
            );
        } else {
            assert_eq!(outcome, Outcome::Held, "{attempt_state:?}");
            assert_eq!(state(root.path(), 8), RowState::Held(HeldReason::Ambiguous));
            assert_eq!(
                host.effects_mut().take_notices(),
                vec![(Some(8), "handback_ambiguous")]
            );
        }
    }
}

#[test]
fn e1_every_effect_refuses_an_async_worker_and_runs_under_the_blocking_marker() {
    let rt = runtime();
    let root = sandbox();
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    ledger
        .append_entry(
            &Entry::Received {
                key: 8,
                input: item(8),
            },
            &[],
        )
        .unwrap();
    let row = ledger.rows().unwrap().row(8).unwrap().clone();
    drop(ledger);
    let mut on_scheduler = effects(root.path(), frozen(), &rt);
    let refused = rt.block_on(async {
        let effects = &mut on_scheduler;
        [
            effects.intake_outbox_open().map(drop),
            effects.evidence(8, &item(8)).map(drop),
            effects.reconcile(8, &row).map(drop),
            effects.provider_alive().map(drop),
            effects.materialize_bundle(&json!({})).map(drop),
            effects.enqueue(8, &item(8)).map(drop),
        ]
        .map(|result| result.unwrap_err().to_string())
    });
    assert_eq!(refused, ["input fence: Busy"; 6].map(String::from));
    assert!(!root.path().join("discord_pending_queue").exists());
    let mut unmarked = effects(root.path(), frozen(), &rt);
    let refused = rt
        .block_on(async move {
            tokio::task::spawn_blocking(move || unmarked.evidence(8, &item(8)).unwrap_err()).await
        })
        .unwrap();
    assert_eq!(
        refused.to_string(),
        "input fence: Busy",
        "a blocking-pool thread still sees the runtime"
    );
    let mut marked = effects(root.path(), frozen(), &rt);
    let evidence = rt
        .block_on(async move {
            tokio::task::spawn_blocking(move || fence::blocking(|| marked.evidence(8, &item(8))))
                .await
        })
        .unwrap()
        .unwrap();
    assert!(!evidence.user_record && evidence.composer == Composer::Draft);
}

#[test]
fn e1_production_effects_take_the_runtime_root_and_reject_a_foreign_capability() {
    struct Env(Option<std::ffi::OsString>);
    impl Drop for Env {
        fn drop(&mut self) {
            unsafe {
                match &self.0 {
                    Some(old) => std::env::set_var("AGENTDESK_ROOT_DIR", old),
                    None => std::env::remove_var("AGENTDESK_ROOT_DIR"),
                }
            }
        }
    }
    let _lock = crate::services::turn_orchestrator::test_support::lock_test_env();
    let root = sandbox();
    let _env = Env(std::env::var_os("AGENTDESK_ROOT_DIR"));
    unsafe {
        std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
    }
    let rt = runtime();
    let deps = |closing: Arc<Closing>| Deps {
        provider: ProviderKind::Claude,
        channel: CHANNEL,
        tmux_session: "session".into(),
        token_hash: "token".into(),
        authorized: true,
        active_sources: Vec::new(),
        pool: None,
        closing,
    };
    let built = rt
        .block_on(async { ProdEffects::new(deps(frozen())) })
        .unwrap();
    assert_eq!(built.root(), root.path().join("runtime"));
    assert_eq!(Some(built.root().to_owned()), fence::population_root());
    let foreign = Closing::frozen_for_test(ProviderKind::Claude, CHANNEL + 1);
    assert!(
        rt.block_on(async { ProdEffects::new(deps(foreign)) })
            .is_err()
    );
}

#[test]
fn unbound_key_holds_the_move_before_its_only_legacy_copy_is_retired() {
    use std::os::unix::fs::MetadataExt;
    let (unbound, bound) = (31, 32);
    let rt = runtime();
    let root = sandbox();
    let queue = root
        .path()
        .join("discord_pending_queue/claude/token/9.json");
    save(&queue, &json!([item(unbound), item(bound)]));
    let mut ledger = Ledger::open(root.path(), CHANNEL).unwrap();
    let staged = Entry::Staged {
        key: bound,
        input: item(bound),
        state: RowState::Received,
    };
    ledger.append_entry(&staged, &[]).unwrap();
    let commit = Entry::MoveCommitted {
        first_staged_seq: 1,
        ids: vec![unbound, bound],
    };
    ledger.append_entry(&commit, &[]).unwrap();
    drop(ledger);
    let (bytes, inode) = (
        fs::read(&queue).unwrap(),
        fs::metadata(&queue).unwrap().ino(),
    );
    let mut host = files(root.path(), effects(root.path(), frozen(), &rt));
    let mut lease = LedgerLease::new(root.path(), CHANNEL);
    let collected: Vec<u64> = (host.collect(lease.get().unwrap()).unwrap().iter())
        .map(|input| input.key)
        .collect();
    assert_eq!(
        collected,
        [unbound, bound],
        "no other guard refuses this population"
    );
    if let Ok(mut movement) = Move::prepare(&mut lease, &mut host) {
        movement.advance(&mut lease, &mut host);
    }
    assert_eq!(fs::read(&queue).unwrap(), bytes);
    assert_eq!(fs::metadata(&queue).unwrap().ino(), inode);
    let rows = Ledger::open(root.path(), CHANNEL).unwrap().rows().unwrap();
    assert_eq!(
        rows.unbound().iter().copied().collect::<Vec<_>>(),
        [unbound]
    );
    assert_eq!(
        host.effects_mut().take_notices(),
        vec![(Some(unbound), "move_unbound")]
    );
}

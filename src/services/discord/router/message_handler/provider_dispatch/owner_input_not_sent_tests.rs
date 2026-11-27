//! An owned start that provably sent nothing: the original waits at the front with no notice owed,
//! nothing is resent, and the turn's inflight birth stays the one its bridge captured.
use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnState, load_inflight_state, save_inflight_state,
    save_inflight_state_if_identity_unchanged,
};

#[test]
fn an_owned_start_that_sent_nothing_holds_its_front_and_keeps_its_birth() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let rig = rig("ns-idle", 7_272_571, "normal", view("", false), IDLE);
    // The pane's tmux refuses the load, so no key of the prompt can reach the pane.
    let (shim, fail) = (
        rig.f.root.path().join("tmux"),
        rig.f.root.path().join("ns-fail"),
    );
    let refuse = format!(
        "case \"$*\" in *load-buffer*) [ ! -f '{}' ] || exit 1 ;; esac\n",
        fail.display()
    );
    let script = fs::read_to_string(&shim).unwrap();
    fs::write(&shim, script.replacen("for arg", &(refuse + "for arg"), 1)).unwrap();
    fs::write(&fail, "").unwrap();
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        rig.channel,
        None,
        7,
        1,
        2,
        PROMPT.into(),
        None,
        Some(rig.name.clone()),
        None,
        None,
        0,
    );
    row.turn_nonce = Some("owned-birth".into());
    save_inflight_state(&row).unwrap();

    let (idle, transcript) = (target::Phase::Idle, rig.transcript());
    let result = rig.handoff(rig.granted_front(), ProviderKind::Claude, idle, &transcript);
    assert!(
        matches!(&result, Err(e) if e.starts_with("NotSent")),
        "{result:?}"
    );
    rig.waits_not_sent();
    let front = rig
        .mailbox
        .v2_request(|slot| Ok(slot.as_ref().unwrap().front()));
    let front = rig.runtime.block_on(front).unwrap();
    assert_eq!(front.map(|f| f.1.message_id), Some(rig.original.message_id));
    let after = load_inflight_state(&ProviderKind::Claude, rig.channel).unwrap();
    assert!(
        after.owned_by_turn_nonce(Some("owned-birth")),
        "{:?}",
        after.turn_nonce
    );
    assert_eq!(
        save_inflight_state_if_identity_unchanged(&after, "owned_start_bridge"),
        GuardedSaveOutcome::Saved,
        "the turn's bridge still owns its row"
    );
}

fn assert_wait_site(boundary: &str, route: &str) {
    let owner = include_str!("owner_input.rs")
        .split_whitespace()
        .collect::<String>()
        .replace(",)", ")");
    let (_, after_boundary) = owner.split_once(boundary).expect("owner decision");
    assert!(
        after_boundary.contains(route),
        "the owner decision must preserve its own diagnostic route"
    );
}

#[test]
fn owner_start_wait_site_active_turn() {
    assert_wait_site(
        "letresult=matchhandoff",
        "letactive=matchidle_wait::active_turn(&mailbox,origin,channel,original.1.message_id).await",
    );
}

#[test]
fn owner_start_wait_site_boot_ready() {
    assert_wait_site(
        "asyncfnstart_waits(",
        "native_prepare::boot_ready(shared,channel).await{returnWait::Event(\"boot_ready\");",
    );
}

#[test]
fn owner_start_wait_site_row_or_rest() {
    assert_wait_site(
        "asyncfnstart_waits(",
        "ifrow||rest(provider,id){returnWait::Event(\"row_or_rest\");",
    );
}

#[test]
fn owner_start_wait_site_transcript_busy() {
    assert_wait_site(
        "asyncfnstart_waits(",
        "ifbusy{returnWait::Event(\"transcript_busy\");",
    );
}

#[test]
fn owner_start_wait_site_screen() {
    assert_wait_site(
        "letSome(screen)=screen",
        "returnWait::Watch(\"screen\",Box::new(move||",
    );
}

#[test]
fn owner_start_wait_site_native_wait() {
    assert_wait_site(
        "fnnative_wait(",
        "Screen::Blocked|Screen::Unknown=>Wait::Watch(\"native_wait\",Box::new(read))",
    );
}

#[test]
fn owner_start_wait_site_unnamed_target() {
    assert_wait_site(
        "fnnamed_wait(",
        "Wait::Watch(\"unnamed_target\",Box::new(move||",
    );
}

#[test]
fn owner_start_wait_site_late_screen() {
    assert_wait_site(
        "letprobe=late_probe(",
        "idle_wait::refusal(turn.channel_id,claim.front().1.message_id,\"late_screen\")",
    );
}

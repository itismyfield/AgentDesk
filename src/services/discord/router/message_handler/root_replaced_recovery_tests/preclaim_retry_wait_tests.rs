//! A held root recovery refuses the owner's front on the real idle path before any claim: the
//! front waits on its spent grant, a wake before the due hands nothing, and the due hands it again.
use super::*;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[test]
fn rr_preclaim_held_front_waits_unclaimed_and_is_handed_again_only_at_its_due_pg() {
    let case = PolicyCase::new("rr-preclaim-wait", Some(8), true, false, false);
    let shared = case.case.shared.clone();
    let channel = ChannelId::new(CHANNEL);
    let _wake = WakeCapture::install(channel);
    let _launch = owner::install_root_recovery_launch(
        channel,
        Box::new(|claim, _| {
            claim.defer_before_submit("unexpected root recovery launch");
            Err("unexpected root recovery launch".into())
        }),
    );
    // The drive's spawned passes stay on this thread, which carries the rig's endpoint registry.
    let here = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    here.unwrap().block_on(ingress::ADOPTS.scope(true, async {
        let (_, _http) =
            crate::services::discord::shared_state::test_rest::recording_mock(951, CHANNEL).await;
        let token = shared
            .http
            .cached_bot_token
            .set("root-preclaim-wait".into());
        token.unwrap();
        let queued = queued_input();
        let enqueued = ingress::enqueue(&shared, &ProviderKind::Claude, channel, &queued).await;
        assert!(enqueued.unwrap().enqueued);
        let mailbox = shared.mailbox(channel);
        let (handed, at) = (Arc::new(AtomicUsize::new(0)), shared.clone());
        let counted = handed.clone();
        let idle: Handoff = Arc::new(move |front| {
            counted.fetch_add(1, Ordering::SeqCst);
            Box::pin(owner::start_idle_for_test(at.clone(), channel, front))
        });
        let mut drive = owner::owner_drive(
            (mailbox.clone(), None),
            (ProviderKind::Claude, CHANNEL),
            idle,
            Some(shared.clone()),
        );
        let (pass, mut passes) = tokio::sync::mpsc::unbounded_channel();
        Arc::get_mut(&mut drive).unwrap().settled = Some(Arc::new(move || {
            let _ = pass.send(());
            Box::pin(async {})
        }));
        let state = || {
            mailbox.v2_request(|slot| {
                let owner = slot.as_ref().ok_or("owner absent")?;
                Ok((owner.json()?, owner.claim_number()))
            })
        };
        let wake = || crate::services::discord::queue_io::input_owner::drive::wake(drive.clone());
        wake();
        await_within(passes.recv(), WAIT_CAP).await.unwrap();
        let (refused, claims) = state().await.unwrap();
        let wait = &refused["retry"]["wait"];
        assert_eq!(wait["cause"]["kind"], "root_recovery_held", "{refused}");
        assert_eq!(
            (wait["attempts"].as_u64(), claims),
            (Some(0), 0),
            "no claim"
        );
        assert_eq!(refused["items"].as_array().unwrap().len(), 1, "{refused}");
        wake();
        await_within(passes.recv(), WAIT_CAP).await.unwrap();
        assert_eq!(handed.load(Ordering::SeqCst), 1, "a wake before the due");
        let due = wait["due_at_ms"].as_u64().unwrap();
        let ticked = mailbox.v2_request(move |slot| slot.as_mut().ok_or("owner absent")?.tick(due));
        ticked.await.unwrap();
        wake();
        await_within(passes.recv(), WAIT_CAP).await.unwrap();
        assert_eq!(
            handed.load(Ordering::SeqCst),
            2,
            "the due hands it once more"
        );
        let (again, _) = state().await.unwrap();
        assert_eq!(again["retry"]["wait"]["attempts"], 1, "{again}");
        assert_eq!(again["retained_inputs"], json!([]), "{again}");
    }));
    assert_eq!(case.case.fx.row(), Some(HostedState::Bound));
}

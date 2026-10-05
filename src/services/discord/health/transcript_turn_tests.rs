use std::sync::Arc;
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};

use super::*;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::{
    HealthRegistry, build_health_snapshot, build_public_health_snapshot,
};
use crate::services::discord::inflight::{InflightTurnState, save_inflight_state};
use crate::services::discord::runtime_store;
use crate::services::provider::CancelToken;

const SECS: fn(u64) -> Duration = Duration::from_secs;

/// Projects one channel through the health state machine at `now`, for fixtures elsewhere.
pub(in crate::services::discord) fn project_for_test(
    provider: &str,
    channel_id: u64,
    tokens: &[String],
    now: Instant,
) -> (Vec<String>, serde_json::Value) {
    let (reasons, health) = with_channel(provider, channel_id, |state| {
        state.project(provider, channel_id, tokens, now)
    });
    (reasons, serde_json::to_value(health).unwrap())
}

fn offerable_view() -> TranscriptTurnView {
    TranscriptTurnView {
        o_fact: OFact::Idle,
        writer_ready: true,
        binding_generation: 1,
        counts: InputCounts {
            ready: 1,
            ..InputCounts::default()
        },
        head_key: Some("head-1".into()),
        head_held: false,
        oldest_waiting_age_secs: Some(1),
        hold_reasons: Vec::new(),
        head_offer_veto: None,
        drain_eligible_since: None,
    }
}

/// A supervisor stand-in: runs the clock on each push and publishes the view it produced.
struct Supervisor {
    channel: u64,
    clock: EligibilityClock,
    t0: Instant,
}

impl Supervisor {
    fn new(channel: u64) -> Self {
        Self {
            channel,
            clock: EligibilityClock::default(),
            t0: Instant::now(),
        }
    }

    fn push(&mut self, at: u64, edit: impl FnOnce(&mut TranscriptTurnView)) {
        let mut view = offerable_view();
        edit(&mut view);
        let at = self.t0 + SECS(at);
        view.drain_eligible_since = self.clock.observe(&view, at);
        publish_view("codex", self.channel, view, at);
    }

    fn not_draining_at(&self, at: u64) -> bool {
        let (reasons, _) = project_for_test("codex", self.channel, &[], self.t0 + SECS(at));
        assert!(
            !reasons.iter().any(|reason| reason.contains("reconcile")),
            "{reasons:?}"
        );
        reasons.contains(&format!("tui_o:input_not_draining:{}", self.channel))
    }
}

/// `input_not_draining` needs 300 s of uninterrupted offerable idle time with no veto: every
/// interruption restarts the count, and a re-push of the same state keeps it.
#[test]
fn eligibility_clock_counts_only_uninterrupted_offerable_idle_time() {
    let open = |v: &mut TranscriptTurnView| v.o_fact = OFact::Open;
    let same = |_: &mut TranscriptTurnView| {};
    // (a) Open → Idle starts the clock.
    let mut s = Supervisor::new(6_325_430_001);
    s.push(0, open);
    s.push(10, same);
    assert!(!s.not_draining_at(309) && s.not_draining_at(310));
    // (b) writer not ready, then ready, restarts it.
    let mut s = Supervisor::new(6_325_430_002);
    s.push(0, same);
    s.push(100, |v| v.writer_ready = false);
    s.push(200, same);
    assert!(!s.not_draining_at(499) && s.not_draining_at(500));
    // (c) a hold that clears restarts it.
    let mut s = Supervisor::new(6_325_430_003);
    s.push(0, same);
    s.push(100, |v| v.hold_reasons = vec!["modal".into()]);
    s.push(150, same);
    assert!(!s.not_draining_at(449) && s.not_draining_at(450));
    // (d) a new binding and (e) a new head restart it while still eligible.
    let mut s = Supervisor::new(6_325_430_004);
    s.push(0, same);
    s.push(100, |v| v.binding_generation = 2);
    assert!(!s.not_draining_at(399) && s.not_draining_at(400));
    let mut s = Supervisor::new(6_325_430_005);
    s.push(0, same);
    s.push(100, |v| v.head_key = Some("head-2".into()));
    assert!(!s.not_draining_at(399) && s.not_draining_at(400));
    // (f) re-pushing the same state keeps the start.
    let mut s = Supervisor::new(6_325_430_006);
    for at in [0, 100, 200, 299] {
        s.push(at, same);
    }
    assert!(s.not_draining_at(300));
    // (g) Open for 900 s and (h) a Held head never count.
    let mut s = Supervisor::new(6_325_430_007);
    s.push(0, open);
    assert!(!s.not_draining_at(900));
    let mut s = Supervisor::new(6_325_430_008);
    s.push(0, |v| v.head_held = true);
    assert!(!s.not_draining_at(900));
    // (i) an offerable head vetoed by ThreadActive for 400 s never counts; once the veto
    // clears at t, only t+300 does.
    let mut s = Supervisor::new(6_325_430_009);
    let (t0, vetoed) = (s.t0, SECS(10));
    s.push(0, same);
    s.push(10, |v| {
        v.head_offer_veto = Some((VetoKind::ThreadActive, t0 + vetoed))
    });
    s.push(200, |v| {
        v.head_offer_veto = Some((VetoKind::ThreadActive, t0 + vetoed))
    });
    assert!(!s.not_draining_at(410));
    s.push(410, same);
    assert!(!s.not_draining_at(709) && s.not_draining_at(710));
    // (j) an unknown veto kind still vetoes.
    let mut s = Supervisor::new(6_325_430_010);
    s.push(0, |v| {
        v.head_offer_veto = Some((VetoKind::Other("x".into()), t0));
    });
    assert!(!s.not_draining_at(900));
}

/// A view older than 120 s is stale until the next push; with no push yet, the age counts
/// from the first health poll that saw the channel.
#[test]
fn view_goes_stale_after_120_seconds_without_a_push() {
    let channel = 6_325_430_101;
    let stale = format!("tui_o:input_view_stale:{channel}");
    let t0 = Instant::now();
    let at = |secs| project_for_test("codex", channel, &[], t0 + SECS(secs)).0;
    assert!(!at(0).contains(&stale));
    assert!(!at(120).contains(&stale) && at(121).contains(&stale));
    publish_view("codex", channel, offerable_view(), t0 + SECS(121));
    assert!(!at(121).contains(&stale) && !at(241).contains(&stale));
    assert!(at(242).contains(&stale));
    publish_view("codex", channel, offerable_view(), t0 + SECS(242));
    assert!(!at(242).contains(&stale));
}

fn residue_reasons(state: &mut ChannelState, now: Instant) -> Vec<String> {
    let reasons = state.project("codex", 1, &[], now).0;
    reasons
        .into_iter()
        .filter(|reason| reason.contains("legacy_residue"))
        .collect()
}

/// Residue latches until its observer confirms the absence: an unknown read keeps it, an
/// unknown lasting 600 s degrades on its own, and a stale sweep is not a confirmation.
#[test]
fn residue_changes_only_on_a_confirmed_observation() {
    let residue = vec!["tui_o:legacy_residue:1".to_string()];
    let unknown = "tui_o:legacy_residue_unknown:1".to_string();
    let t0 = Instant::now();
    let mut state = ChannelState::default();
    // One health poll plus a fresh sweep: the row as given, every other degrading kind absent.
    let poll = |state: &mut ChannelState, row, at| {
        state.record(ResidueKind::Row, "", row, at);
        for kind in [
            ResidueKind::MailboxToken,
            ResidueKind::MailboxActiveId,
            ResidueKind::MailboxQueue,
            ResidueKind::AbortMarker,
        ] {
            state.record(kind, "", Presence::Absent, at);
        }
    };
    // (a) a row with an empty mailbox degrades.
    poll(&mut state, Presence::Present(1), t0);
    assert_eq!(residue_reasons(&mut state, t0), residue);
    // (c) a failed stat keeps it, and past 600 s also reports the blind spot.
    poll(
        &mut state,
        Presence::Unknown("row_stat_failed"),
        t0 + SECS(10),
    );
    assert_eq!(residue_reasons(&mut state, t0 + SECS(10)), residue);
    assert_eq!(residue_reasons(&mut state, t0 + SECS(610)), residue);
    let blind = residue_reasons(&mut state, t0 + SECS(611));
    assert!(blind.contains(&residue[0]) && blind.contains(&unknown));
    // (d) a confirmed absence clears both.
    poll(&mut state, Presence::Absent, t0 + SECS(620));
    assert!(residue_reasons(&mut state, t0 + SECS(620)).is_empty());

    // An unknown with no earlier presence raises nothing until 600 s have passed.
    let mut state = ChannelState::default();
    poll(&mut state, Presence::Unknown("row_stat_timeout"), t0);
    assert!(residue_reasons(&mut state, t0).is_empty());
    assert!(residue_reasons(&mut state, t0 + SECS(600)).is_empty());
    assert_eq!(
        residue_reasons(&mut state, t0 + SECS(601)),
        vec![unknown.clone()]
    );

    // (e) a swept absence older than 90 s no longer confirms anything.
    let mut state = ChannelState::default();
    poll(&mut state, Presence::Absent, t0);
    let (_, health) = state.project("codex", 1, &[], t0 + SECS(90));
    assert!(!health.unobserved.contains(&ResidueKind::AbortMarker));
    let (_, health) = state.project("codex", 1, &[], t0 + SECS(91));
    assert!(health.unobserved.contains(&ResidueKind::AbortMarker));
    assert_eq!(residue_reasons(&mut state, t0 + SECS(692)), vec![unknown]);

    // (g) a busy binding alone is cleanup detail, never residue.
    let mut state = ChannelState::default();
    poll(&mut state, Presence::Absent, t0);
    state.record(ResidueKind::BusyRetry, "", Presence::Present(1), t0);
    let (reasons, health) = state.project("codex", 1, &[], t0);
    assert!(reasons.is_empty(), "{reasons:?}");
    assert_eq!(health.legacy_cleanup_pending[&ResidueKind::BusyRetry], 1);
}

/// (b) An active message id alone, in any runtime of the provider, is mailbox residue; with no
/// runtime registered the mailbox is unknown, not empty.
#[test]
fn mailbox_residue_spans_every_runtime_of_the_provider() {
    let channel = ChannelId::new(6_325_430_201);
    let active = ChannelMailboxSnapshot {
        active_user_message_id: Some(MessageId::new(5)),
        ..ChannelMailboxSnapshot::default()
    };
    let runtimes: RuntimeMailboxes = vec![
        ("a".into(), HashMap::new()),
        ("b".into(), HashMap::from([(channel, active)])),
    ];
    assert_eq!(
        mailbox_presence(&runtimes, channel.get()),
        [
            (ResidueKind::MailboxToken, Presence::Absent),
            (ResidueKind::MailboxActiveId, Presence::Present(1)),
            (ResidueKind::MailboxQueue, Presence::Absent),
        ]
    );
    let none = mailbox_presence(&RuntimeMailboxes::new(), channel.get());
    assert!(
        none.iter()
            .all(|(_, presence)| matches!(presence, Presence::Unknown(_)))
    );
}

/// Each store's presence tells a missing store (confirmed absence) from an unreadable one,
/// and counts a corrupt record as residue.
#[test]
fn store_presence_tells_a_missing_store_from_an_unreadable_one() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let (codex, token, channel) = (ProviderKind::Codex, "n4b-token", 6_325_430_301u64);
    let panels = || status_panel_orphan_store::channel_presence(&codex, token, channel);
    let abandons = || abandon_request_store::channel_presence(&codex, token, channel);
    let markers = || tui_direct_abort_marker::channel_presence("codex", channel);
    let busy = || busy_followup_retry_store::channel_presence(&codex, channel);
    for presence in [panels(), abandons(), markers(), busy()] {
        assert_eq!(presence, Presence::Absent);
    }

    let root = runtime_store::runtime_root().unwrap();
    status_panel_orphan_store::enqueue(&codex, token, channel, 11);
    status_panel_orphan_store::enqueue(&codex, token, channel, 12);
    assert_eq!(panels(), Presence::Present(2));
    let abandon_file = root.join(format!(
        "discord_abandon_requests/codex/{token}/{channel}.json"
    ));
    std::fs::create_dir_all(abandon_file.parent().unwrap()).unwrap();
    std::fs::write(&abandon_file, "not json").unwrap();
    assert_eq!(abandons(), Presence::Present(1));
    let marker_dir = runtime_store::tui_direct_abort_marker_root().unwrap();
    std::fs::create_dir_all(&marker_dir).unwrap();
    for name in [
        format!("codex_{channel}_7.json"),
        format!("codex_{channel}9_7.json"),
        format!("claude_{channel}_7.json"),
        format!("codex_{channel}_8.json.lock"),
    ] {
        std::fs::write(marker_dir.join(name), "{").unwrap();
    }
    assert_eq!(markers(), Presence::Present(1));
    let busy_dir = root.join(format!("discord_busy_followup_retries/codex/{channel}"));
    std::fs::create_dir_all(&busy_dir).unwrap();
    std::fs::write(busy_dir.join("5.json"), "{}").unwrap();
    assert_eq!(busy(), Presence::Present(1));

    std::fs::remove_file(&abandon_file).unwrap();
    std::fs::create_dir(&abandon_file).unwrap();
    std::fs::remove_dir_all(&busy_dir).unwrap();
    std::fs::write(&busy_dir, "a file where a directory belongs").unwrap();
    std::fs::remove_dir_all(&marker_dir).unwrap();
    std::fs::write(&marker_dir, "").unwrap();
    for presence in [abandons(), markers(), busy()] {
        assert!(matches!(presence, Presence::Unknown(_)), "{presence:?}");
    }
}

/// The poll stats retired rows read-only: a corrupt row is present, a missing one absent, and
/// a failed stat unknown.
#[tokio::test]
async fn row_observation_tells_a_missing_row_from_a_failed_stat() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let inflight = runtime_store::discord_inflight_root().unwrap();
    std::fs::create_dir_all(inflight.join("codex")).unwrap();
    std::fs::write(inflight.join("codex/6325430401.json"), "corrupt").unwrap();
    std::fs::write(inflight.join("claude"), "a file where a directory belongs").unwrap();
    let channels = [
        ("codex".to_string(), 6_325_430_401),
        ("codex".to_string(), 6_325_430_402),
        ("claude".to_string(), 6_325_430_403),
    ];
    let rows = legacy_supervision::observe_rows(&channels).await;
    assert_eq!(rows[..2], [Presence::Present(1), Presence::Absent]);
    assert!(matches!(rows[2], Presence::Unknown(_)), "{rows:?}");
    assert_eq!(
        std::fs::read_to_string(inflight.join("codex/6325430401.json")).unwrap(),
        "corrupt"
    );
}

fn row(channel: u64) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        1,
        channel * 10,
        channel * 10 + 1,
        "prompt".into(),
        None,
        None,
        None,
        None,
        0,
    )
}

fn reasons_of(json: &serde_json::Value) -> Vec<String> {
    serde_json::from_value(json["degraded_reasons"].clone()).unwrap()
}

fn current_thread() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

/// A retired channel's row or mailbox token degrades both health builds; only the detail
/// build carries the per-channel block, and a Legacy channel's row raises nothing.
#[test]
fn retired_residue_degrades_public_and_detail_health() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    current_thread().block_on(async {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry.register("codex".to_string(), shared.clone()).await;
        let (row_ch, token_ch, legacy_ch) = (6_325_430_501, 6_325_430_502, 6_325_430_503);
        for channel in [row_ch, legacy_ch] {
            save_inflight_state(&row(channel)).unwrap();
        }
        let token = Arc::new(CancelToken::new());
        let msg = MessageId::new(9);
        let started = crate::services::discord::mailbox_try_start_turn(
            &shared,
            ChannelId::new(token_ch),
            token,
            UserId::new(7),
            msg,
        );
        assert!(started.await);
        let _retired = [row_ch, token_ch].map(|c| RetiredForTest::new("codex", c));

        let detail = serde_json::to_value(build_health_snapshot(&registry).await).unwrap();
        let public = serde_json::to_value(build_public_health_snapshot(&registry).await).unwrap();
        for json in [&detail, &public] {
            let reasons = reasons_of(json);
            for channel in [row_ch, token_ch] {
                let reason = format!("tui_o:legacy_residue:{channel}");
                assert!(reasons.contains(&reason), "{reasons:?}");
            }
            assert!(
                !reasons
                    .iter()
                    .any(|r| r.ends_with(&format!(":{legacy_ch}")))
            );
        }
        assert!(public.get("transcript_turns").is_none());
        let channels = detail["transcript_turns"]["channels"].as_array().unwrap();
        let residue_of = |channel: u64| {
            channels
                .iter()
                .find(|entry| entry["channel_id"] == channel)
                .map(|entry| entry["residue"].clone())
        };
        assert_eq!(residue_of(row_ch), Some(serde_json::json!({"row": 1})));
        let token_residue = residue_of(token_ch).unwrap();
        assert_eq!(token_residue["mailbox_token"], 1, "{token_residue}");
        assert_eq!(residue_of(legacy_ch), None);

        let root = runtime_store::discord_inflight_root().unwrap();
        let path = crate::services::discord::inflight::inflight_state_path(
            &root,
            &ProviderKind::Codex,
            row_ch,
        );
        std::fs::remove_file(path).unwrap();
        let detail = serde_json::to_value(build_health_snapshot(&registry).await).unwrap();
        let cleared = format!("tui_o:legacy_residue:{row_ch}");
        assert!(!reasons_of(&detail).contains(&cleared));
    });
}

/// With no retired channel, both health builds and the channel reports carry nothing new;
/// a retired channel adds exactly one report line.
#[test]
fn empty_retired_set_leaves_health_and_reports_unchanged() {
    use crate::services::discord::commands::{
        build_health_report, build_inflight_report, build_status_report,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    current_thread().block_on(async {
        let registry = HealthRegistry::new();
        let shared = crate::services::discord::make_shared_data_for_tests();
        registry.register("codex".to_string(), shared.clone()).await;
        let (legacy, retired) = (6_325_430_601, 6_325_430_602);
        save_inflight_state(&row(legacy)).unwrap();
        shared.mailboxes.handle(ChannelId::new(legacy));

        let detail = serde_json::to_value(build_health_snapshot(&registry).await).unwrap();
        let public = serde_json::to_value(build_public_health_snapshot(&registry).await).unwrap();
        for json in [&detail, &public] {
            assert!(json.get("transcript_turns").is_none(), "{json}");
            let reasons = reasons_of(json);
            let ours = ["legacy_residue", "input_view_stale", "input_not_draining"];
            assert!(
                !reasons
                    .iter()
                    .any(|reason| ours.iter().any(|ours| reason.contains(ours))),
                "{reasons:?}"
            );
        }

        let codex = ProviderKind::Codex;
        let _retired = RetiredForTest::new("codex", retired);
        for channel in [legacy, retired] {
            let channel_id = ChannelId::new(channel);
            let reports = [
                build_health_report(&shared, &codex, channel_id).await,
                build_status_report(&shared, &codex, channel_id).await,
                build_inflight_report(&shared, &codex, channel_id).await,
            ];
            for report in reports {
                let lines = report.matches("- transcript turns: input `ledger`").count();
                assert_eq!(lines, usize::from(channel == retired), "{report}");
            }
        }
    });
}

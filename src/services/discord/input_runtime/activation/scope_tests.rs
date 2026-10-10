use super::scope::{ChannelKind, Refusal, RouteBinding, RoutingSnapshot, ScopeDecision, evaluate};
use crate::services::provider::ProviderKind;
use crate::services::tui_input::ledger::{OPENS, Presence};

const CHANNEL: u64 = 41;

fn binding(provider: ProviderKind) -> RouteBinding {
    RouteBinding {
        agent: "scope-agent".into(),
        channel: Some(CHANNEL),
        original_provider: Some(provider.clone()),
        writer_provider: Some(provider),
    }
}

fn snapshot() -> RoutingSnapshot {
    RoutingSnapshot {
        channel: CHANNEL,
        channel_kind: Some(ChannelKind::NonThread),
        primary: Some(vec![binding(ProviderKind::Claude)]),
        alt: Some(Vec::new()),
        cc: Some(Vec::new()),
        cdx: Some(Vec::new()),
        overrides: Some(Vec::new()),
    }
}

fn decide(snapshot: Option<&RoutingSnapshot>, responsibility: Presence) -> ScopeDecision {
    evaluate(CHANNEL, "claude", snapshot, responsibility)
}

fn assert_refusal(snapshot: Option<&RoutingSnapshot>, reason: Refusal) {
    assert_eq!(
        decide(snapshot, Presence::Absent),
        ScopeDecision::Refused(reason)
    );
    for responsibility in [Presence::Present, Presence::Unreadable] {
        assert_eq!(
            decide(snapshot, responsibility),
            ScopeDecision::Held(reason)
        );
    }
}

#[test]
fn g2_e2_confirmed_primary_non_thread_is_candidate_without_owner_lookup() {
    let snapshot = snapshot();
    for responsibility in [Presence::Absent, Presence::Present, Presence::Unreadable] {
        assert_eq!(
            decide(Some(&snapshot), responsibility),
            ScopeDecision::Candidate
        );
    }
    let mut same_provider = snapshot;
    same_provider
        .alt
        .as_mut()
        .unwrap()
        .push(binding(ProviderKind::Claude));
    let mut unrelated = binding(ProviderKind::Codex);
    unrelated.channel = Some(CHANNEL + 1);
    same_provider.cc.as_mut().unwrap().push(unrelated);
    assert_eq!(
        decide(Some(&same_provider), Presence::Absent),
        ScopeDecision::Candidate
    );
}

#[test]
fn g2_e2_snapshot_lookup_failure_refuses_new_and_holds_existing_responsibility() {
    assert_refusal(None, Refusal::SnapshotUnknown);
    let mut wrong_channel = snapshot();
    wrong_channel.channel += 1;
    assert_refusal(Some(&wrong_channel), Refusal::SnapshotUnknown);
}

#[test]
fn g2_e2_each_missing_routing_field_is_unknown() {
    for field in 0..5 {
        let mut incomplete = snapshot();
        match field {
            0 => incomplete.primary = None,
            1 => incomplete.alt = None,
            2 => incomplete.cc = None,
            3 => incomplete.cdx = None,
            4 => incomplete.overrides = None,
            _ => unreachable!(),
        }
        assert_refusal(Some(&incomplete), Refusal::SnapshotUnknown);
    }
}

#[test]
fn g2_e2_thread_or_channel_kind_unknown_never_passes() {
    let mut snapshot = snapshot();
    snapshot.channel_kind = Some(ChannelKind::Thread);
    assert_refusal(Some(&snapshot), Refusal::Thread);
    snapshot.channel_kind = None;
    assert_refusal(Some(&snapshot), Refusal::ChannelKindUnknown);
}

#[test]
fn g2_e2_other_provider_alt_cc_cdx_and_override_targets_refuse() {
    for field in 0..4 {
        let mut snapshot = snapshot();
        let target = vec![binding(ProviderKind::Codex)];
        match field {
            0 => snapshot.alt = Some(target),
            1 => snapshot.cc = Some(target),
            2 => snapshot.cdx = Some(target),
            3 => snapshot.overrides = Some(target),
            _ => unreachable!(),
        }
        assert_refusal(Some(&snapshot), Refusal::CrossProviderTarget);
    }
}

#[test]
fn g2_e2_primary_must_be_unique_and_match_selected_provider() {
    let mut snapshot = snapshot();
    snapshot.primary = Some(Vec::new());
    assert_refusal(Some(&snapshot), Refusal::PrimaryNotUnique);
    snapshot.primary = Some(vec![
        binding(ProviderKind::Claude),
        binding(ProviderKind::Claude),
    ]);
    assert_refusal(Some(&snapshot), Refusal::PrimaryNotUnique);
    snapshot.primary = Some(vec![binding(ProviderKind::Codex)]);
    assert_refusal(Some(&snapshot), Refusal::PrimaryProviderMismatch);
}

#[test]
fn g2_e2_unknown_agent_or_provider_is_not_same_provider() {
    let cases = [
        RouteBinding {
            channel: None,
            ..binding(ProviderKind::Claude)
        },
        RouteBinding {
            agent: String::new(),
            ..binding(ProviderKind::Claude)
        },
        RouteBinding {
            original_provider: None,
            ..binding(ProviderKind::Claude)
        },
        RouteBinding {
            writer_provider: None,
            ..binding(ProviderKind::Claude)
        },
        binding(ProviderKind::Unsupported("unclassified".into())),
    ];
    for binding in cases {
        for primary in [true, false] {
            let mut snapshot = snapshot();
            if primary {
                snapshot.primary = Some(vec![binding.clone()]);
            } else {
                snapshot.overrides = Some(vec![binding.clone()]);
            }
            assert_refusal(Some(&snapshot), Refusal::BindingUnknown);
        }
    }
}

#[test]
fn g2_e2_codex_primary_and_same_provider_routes_are_supported() {
    let mut snapshot = snapshot();
    for routes in [
        &mut snapshot.primary,
        &mut snapshot.alt,
        &mut snapshot.cc,
        &mut snapshot.cdx,
        &mut snapshot.overrides,
    ] {
        *routes = Some(vec![binding(ProviderKind::Codex)]);
    }
    assert_eq!(
        evaluate(CHANNEL, "codex", Some(&snapshot), Presence::Absent),
        ScopeDecision::Candidate
    );
}

#[test]
fn g2_e2_original_and_effective_writer_must_both_match() {
    for route in [
        RouteBinding {
            original_provider: Some(ProviderKind::Codex),
            ..binding(ProviderKind::Claude)
        },
        RouteBinding {
            writer_provider: Some(ProviderKind::Codex),
            ..binding(ProviderKind::Claude)
        },
    ] {
        for primary in [true, false] {
            let mut snapshot = snapshot();
            let reason = if primary {
                snapshot.primary = Some(vec![route.clone()]);
                Refusal::PrimaryProviderMismatch
            } else {
                snapshot.overrides = Some(vec![route.clone()]);
                Refusal::CrossProviderTarget
            };
            assert_refusal(Some(&snapshot), reason);
        }
    }
}

#[test]
fn g2_e2_unsupported_selection_is_not_a_candidate() {
    let snapshot = snapshot();
    for (channel, provider) in [(0, "claude"), (CHANNEL, "gemini"), (CHANNEL, "")] {
        assert_eq!(
            evaluate(channel, provider, Some(&snapshot), Presence::Absent),
            ScopeDecision::Refused(Refusal::UnsupportedSelection)
        );
    }
}

#[test]
fn g2_e2_refusal_preserves_injected_snapshot_without_opening_wal() {
    let mut snapshot = snapshot();
    snapshot.alt = Some(vec![binding(ProviderKind::Codex)]);
    let before = format!("{snapshot:?}");
    let opens = OPENS.with(|opens| opens.get());
    assert_refusal(Some(&snapshot), Refusal::CrossProviderTarget);
    assert_eq!(OPENS.with(|opens| opens.get()), opens);
    assert_eq!(format!("{snapshot:?}"), before);
}

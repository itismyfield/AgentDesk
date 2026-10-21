use super::*;
use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind,
};
use crate::db::dispatched_sessions::hosted_execution::tests::{TOKEN, owner, record, wire};
use crate::db::dispatched_sessions::hosted_execution::{HostedLookupKey, load_hosted_execution_pg};
use crate::services::discord::turn_presence::activity::tests::BindingRoot;
use crate::services::discord::{inflight, make_shared_data_for_tests};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvidence, BindingTarget, ChannelBindingLog,
};
use poise::serenity_prelude::ChannelId;
use serde_json::json;

const CHANNEL: u64 = 69_180_001;

#[test]
fn s1_hosted_absence_cannot_hide_related_inventory() {
    for scope in [Scope::HerdrFirstCreate, Scope::HerdrRetired] {
        let c = candidates(Some("hosted-first"), &[], vec![], None).unwrap();
        let current = c.current.clone().unwrap();
        let verdict = decide_unbound(
            Ok(c),
            HostScope {
                channel: CHANNEL,
                provider: ProviderKind::Claude,
                name: current.clone(),
                result: scope,
            },
            &Inventory::Sessions(vec![current]),
            &BTreeMap::new(),
        );
        assert_eq!(
            verdict,
            Verdict::Hold("watcher_unbound"),
            "hosted absence bypassed a related live tmux session"
        );
    }
}

#[test]
fn s1_nameless_local_scope_requires_a_complete_empty_inventory() {
    for (registered, names, expected) in [
        (true, vec![], Verdict::Absent(Absence::LocalInventory)),
        (false, vec![], Verdict::Hold("candidate_unresolved")),
        (
            true,
            vec!["other-owned".to_string()],
            Verdict::Hold("candidate_unresolved"),
        ),
    ] {
        let c = candidates(None, &[], vec![], None);
        let scope = HostScope {
            channel: CHANNEL,
            provider: ProviderKind::Claude,
            name: String::new(),
            result: if registered {
                Scope::LocalNameless
            } else {
                Scope::LocalInventory
            },
        };
        let owners = [("other-owned".to_string(), Some(CHANNEL + 1))]
            .into_iter()
            .collect();
        assert_eq!(
            decide_unbound(c, scope, &Inventory::Sessions(names), &owners),
            expected
        );
    }
}
fn fallback(channel: u64, provider: ProviderKind, basis: &str) -> RegisteredChannelBinding {
    RegisteredChannelBinding {
        channel_id: channel,
        owner_provider: provider,
        fallback_name: Some(basis.into()),
    }
}
fn candidates(
    core: Option<&str>,
    fallback: &[RegisteredChannelBinding],
    events: Vec<BindingEvent>,
    row: Option<InflightTurnState>,
) -> Result<Candidates, String> {
    unbound_candidates(
        CHANNEL,
        &ProviderKind::Claude,
        core,
        fallback,
        Ok(events),
        &Ok(row),
    )
}
fn row(channel: u64, name: &str) -> InflightTurnState {
    InflightTurnState::new(
        ProviderKind::Claude,
        channel,
        Some("s0u".into()),
        1,
        0,
        0,
        "fixture".into(),
        None,
        Some(name.into()),
        None,
        None,
        0,
    )
}
fn event(seq: u64, name: &str, committed: i64) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: name.into(),
        execution_nonce: "fixture".into(),
        record: BindingRecord::Bound {
            old: None,
            new: BindingTarget::Pending {
                payload_session_id: "p".into(),
                payload_transcript_path: "/tmp/fixture".into(),
            },
            cause: BindingCause::Startup,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at: chrono::DateTime::from_timestamp(committed, 0).unwrap(),
                reclaims: false,
            },
        },
        committed_at: chrono::DateTime::from_timestamp(committed, 0).unwrap(),
    }
}
fn decide(
    c: Result<Candidates, String>,
    names: &[&str],
    owners: &BTreeMap<String, Option<u64>>,
) -> Verdict {
    let name = c
        .as_ref()
        .ok()
        .and_then(|c| c.current.clone())
        .unwrap_or_else(|| "unresolved".into());
    decide_unbound(
        c,
        HostScope {
            channel: CHANNEL,
            provider: ProviderKind::Claude,
            name,
            result: Scope::LocalInventory,
        },
        &Inventory::Sessions(names.iter().map(|s| (*s).into()).collect()),
        owners,
    )
}

pub(super) fn low_level_unbound_adapter(
    candidates: Result<Candidates, String>,
    _scope: HostScope,
    inventory: &Inventory,
    _owners: &BTreeMap<String, Option<u64>>,
) -> Verdict {
    use crate::services::discord::turn_presence::activity::{self, Activity, Job, Ports, Target};
    use crate::services::provider::session_probe::SessionLiveness;
    struct OldPorts(Vec<String>);
    impl Ports for OldPorts {
        fn binding_seq(&self, _: u64) -> Result<u64, String> {
            Ok(0)
        }
        fn binding_events(&self, _: u64, _: ShadowProvider) -> Result<Vec<BindingEvent>, String> {
            Ok(vec![])
        }
        fn session_present(&self, session: &str) -> bool {
            self.0.iter().any(|name| name == session)
        }
        fn final_ready(&self, _: &str) -> bool {
            false
        }
        fn pane_busy(&self, _: &str) -> bool {
            false
        }
        fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
            SessionLiveness::Alive
        }
        fn spawn(&self, _: Job) -> bool {
            false
        }
    }
    let Ok(candidates) = candidates else {
        return Verdict::Hold("probe_failed");
    };
    let names = match inventory {
        Inventory::Sessions(names) => names.clone(),
        _ => vec![],
    };
    let core = candidates
        .names
        .iter()
        .find(|c| c.origin == Origin::Core)
        .map(|c| c.name.clone());
    let ports: std::sync::Arc<dyn Ports> = std::sync::Arc::new(OldPorts(names));
    let (observed, _) = activity::judge_with_policy(
        &Default::default(),
        &ports,
        ShadowProvider::Claude,
        candidates.channel,
        Target::Unbound(core),
        false,
    );
    match observed.activity {
        Activity::Idle => Verdict::Absent(Absence::LocalInventory),
        _ => Verdict::Hold("watcher_unbound"),
    }
}

#[test]
fn u_unbound_none_requires_complete_adapter() {
    let c = candidates(
        None,
        &[fallback(CHANNEL, ProviderKind::Claude, "s0u-fallback")],
        vec![],
        None,
    );
    let name = ProviderKind::Claude.build_tmux_session_name("s0u-fallback");
    assert_eq!(
        low_level_unbound_adapter(
            c,
            HostScope {
                channel: CHANNEL,
                provider: ProviderKind::Claude,
                name: name.clone(),
                result: Scope::LocalInventory
            },
            &Inventory::Sessions(vec![name]),
            &BTreeMap::new()
        ),
        Verdict::Hold("watcher_unbound")
    );
}

#[test]
fn u_fallback_only_live() {
    let shared = make_shared_data_for_tests();
    assert!(
        shared
            .tmux_watchers
            .channel_binding(&ChannelId::new(CHANNEL))
            .is_none()
    );
    let registered = fallback(CHANNEL, ProviderKind::Claude, "s0u-fallback");
    let name = ProviderKind::Claude.build_tmux_session_name("s0u-fallback");
    let owner = shared
        .tmux_watchers
        .owner_channel_for_tmux_session(&name)
        .map(|id| id.get());
    let owners = [(name.clone(), owner)].into_iter().collect();
    assert_eq!(
        decide(
            candidates(None, &[registered], vec![], None),
            &[&name],
            &owners
        ),
        Verdict::Hold("watcher_unbound")
    );
    for foreign in [
        fallback(CHANNEL + 1, ProviderKind::Claude, "foreign"),
        fallback(CHANNEL, ProviderKind::Codex, "foreign"),
    ] {
        assert!(
            candidates(None, &[foreign], vec![], None)
                .unwrap()
                .names
                .is_empty()
        );
    }
}

#[test]
fn u_restored_owner_only_live() {
    let shared = make_shared_data_for_tests();
    let name = "adk-s0u-restored-only";
    shared
        .tmux_watchers
        .restore_owner_channel_for_tmux_session(name, ChannelId::new(CHANNEL));
    assert!(
        shared
            .tmux_watchers
            .channel_binding(&ChannelId::new(CHANNEL))
            .is_none()
    );
    let owners = [(
        name.into(),
        shared
            .tmux_watchers
            .owner_channel_for_tmux_session(name)
            .map(|id| id.get()),
    )]
    .into_iter()
    .collect();
    assert_eq!(
        decide(candidates(None, &[], vec![], None), &[name], &owners),
        Verdict::Hold("watcher_unbound")
    );
    assert!(
        candidates(None, &[], vec![], None)
            .unwrap()
            .names
            .is_empty()
    );
}

#[test]
fn u_candidate_conflicts() {
    let owners = [("unrelated".into(), Some(CHANNEL + 1))]
        .into_iter()
        .collect();
    assert_eq!(
        decide(
            candidates(Some("current"), &[], vec![], None),
            &["unrelated"],
            &owners
        ),
        Verdict::Absent(Absence::LocalInventory)
    );
    assert_eq!(
        decide(
            candidates(
                Some("core"),
                &[fallback(CHANNEL, ProviderKind::Claude, "fallback")],
                vec![],
                None
            ),
            &[],
            &BTreeMap::new()
        ),
        Verdict::Hold("candidate_conflict")
    );
    let c = candidates(
        Some("current"),
        &[],
        vec![event(1, "older", 100), event(2, "latest", 1)],
        None,
    )
    .unwrap();
    assert!(
        c.names
            .iter()
            .any(|c| c.name == "latest" && c.origin == Origin::Binding(2))
    );
    assert!(!c.names.iter().any(|c| c.name == "older"));
    let current = c.current.clone().unwrap();
    assert_eq!(
        decide(Ok(c), &[&current], &BTreeMap::new()),
        Verdict::Hold("watcher_unbound")
    );
    assert_eq!(
        decide(
            candidates(None, &[], vec![event(1, "gone", 1)], None),
            &[],
            &BTreeMap::new()
        ),
        Verdict::Hold("candidate_unresolved")
    );
    let inflight = candidates(
        Some("current"),
        &[],
        vec![],
        Some(row(CHANNEL, "different")),
    )
    .unwrap();
    assert!(
        inflight
            .names
            .iter()
            .any(|c| c.name == "different" && c.origin == Origin::Inflight)
    );
    assert_eq!(
        decide(Ok(inflight), &[], &BTreeMap::new()),
        Verdict::Hold("candidate_inflight")
    );
    assert_eq!(
        decide(
            candidates(Some("current"), &[], vec![], None),
            &["AgentDesk-claude-unowned"],
            &BTreeMap::new()
        ),
        Verdict::Hold("owner_unresolved")
    );
    assert!(candidates(None, &[], vec![event(2, "a", 1), event(1, "b", 2)], None).is_err());
    let c = candidates(Some("current"), &[], vec![], None).unwrap();
    assert!(candidates(Some(" "), &[], vec![], None).is_err());
    assert!(
        candidates(
            None,
            &[fallback(CHANNEL, ProviderKind::Claude, "")],
            vec![],
            None
        )
        .is_err()
    );
    let current = c.current.clone().unwrap();
    for (channel, provider, name) in [
        (CHANNEL + 1, ProviderKind::Claude, current.clone()),
        (CHANNEL, ProviderKind::Codex, current),
        (CHANNEL, ProviderKind::Claude, "foreign-target".into()),
    ] {
        assert_eq!(
            decide_unbound(
                Ok(c.clone()),
                HostScope {
                    channel,
                    provider,
                    name,
                    result: Scope::LocalInventory
                },
                &Inventory::Sessions(vec![]),
                &BTreeMap::new()
            ),
            Verdict::Hold("scope_mismatch")
        );
    }
}

/// A host session outside AgentDesk's namespace, or one a routine ran in another channel, is not
/// this channel's; the same name still holds once this channel's own evidence names or owns it.
#[test]
fn u_outside_or_elsewhere_sessions_never_hide_own_evidence() {
    let routine = "AgentDesk-claude-routine-brief---counsel";
    let owned = |owner: u64| [(routine.to_string(), Some(owner))].into_iter().collect();
    let current = || candidates(Some("current"), &[], vec![], None);
    let observed = [
        decide(current(), &["claude"], &BTreeMap::new()),
        decide(current(), &["claude", routine], &owned(CHANNEL + 1)),
        decide(current(), &[routine], &BTreeMap::new()),
        decide(
            current(),
            &["claude", "AgentDesk-codex-x"],
            &BTreeMap::new(),
        ),
        decide(current(), &[routine], &owned(CHANNEL)),
        decide(
            candidates(Some("current"), &[], vec![event(1, "claude", 1)], None),
            &["claude"],
            &BTreeMap::new(),
        ),
        decide(
            candidates(Some("current"), &[], vec![], Some(row(CHANNEL, "claude"))),
            &["claude"],
            &BTreeMap::new(),
        ),
    ];
    assert_eq!(
        observed,
        [
            Verdict::Absent(Absence::LocalInventory),
            Verdict::Absent(Absence::LocalInventory),
            Verdict::Hold("owner_unresolved"),
            Verdict::Hold("owner_unresolved"),
            Verdict::Hold("watcher_unbound"),
            Verdict::Hold("watcher_unbound"),
            Verdict::Hold("candidate_inflight"),
        ]
    );
}

#[test]
fn u_absent_logs_and_read_failures() {
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let _log_root = BindingRoot::enter(dir.path());
    let log = ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude);
    assert_eq!(*log.subscribe(CHANNEL).borrow(), 0);
    let read = || observe_candidates(CHANNEL, &ProviderKind::Claude, Some("current"), &[], &log);
    assert_eq!(
        decide(read(), &[], &BTreeMap::new()),
        Verdict::Absent(Absence::LocalInventory)
    );
    let logs = dir
        .path()
        .join(crate::services::tui_prompt_dedupe::binding_events::BINDING_EVENTS_DIR);
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join(format!("{CHANNEL}.log")), "corrupt\n").unwrap();
    assert!(read().is_err());
    assert_eq!(
        decide(read(), &[], &BTreeMap::new()),
        Verdict::Hold("probe_failed")
    );
    assert!(
        unbound_candidates(
            CHANNEL,
            &ProviderKind::Claude,
            None,
            &[],
            Ok(vec![]),
            &Err("denied".into())
        )
        .is_err()
    );
    let mut unknown = row(CHANNEL, "current");
    unknown.runtime_kind_unknown_on_disk = true;
    assert!(candidates(Some("current"), &[], vec![], Some(unknown)).is_err());
    std::fs::write(logs.join(format!("{CHANNEL}.log")), "").unwrap();
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, CHANNEL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "corrupt").unwrap();
    assert!(read().is_err());
}

#[test]
fn u_legacy_inflight_read_only() {
    use std::os::unix::fs::MetadataExt;
    let _runtime = crate::config::TestRuntimeRootGuard::new();
    let dir = tempfile::tempdir().unwrap();
    let _log_root = BindingRoot::enter(dir.path());
    let root = inflight::inflight_runtime_root().unwrap();
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, CHANNEL);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut legacy = serde_json::to_value(row(CHANNEL, "different")).unwrap();
    legacy.as_object_mut().unwrap().remove("finalizer_turn_id");
    let before = serde_json::to_vec(&legacy).unwrap();
    std::fs::write(&path, &before).unwrap();
    let meta = std::fs::metadata(&path).unwrap();
    let entries = std::fs::read_dir(path.parent().unwrap()).unwrap().count();
    let guard = inflight::o_seed_observation::Guard::new(&ProviderKind::Claude, CHANNEL);
    let log = ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude);
    assert_eq!(
        decide(
            observe_candidates(CHANNEL, &ProviderKind::Claude, Some("current"), &[], &log),
            &[],
            &BTreeMap::new()
        ),
        Verdict::Hold("candidate_inflight")
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let after = std::fs::metadata(&path).unwrap();
    assert_eq!(
        (meta.ino(), meta.mtime(), meta.mtime_nsec()),
        (after.ino(), after.mtime(), after.mtime_nsec())
    );
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        entries
    );
    let observed = guard.snapshot();
    assert_eq!(observed.readonly_inflight_load_calls, 1);
    assert_eq!(observed.writable_inflight_load_calls, 0);
    assert_eq!(observed.compatibility_backfill_attempts, 0);
    assert!(observed.events.is_empty());
}

fn config(channel: u64, herdr: bool) -> Config {
    let extra = if herdr {
        format!(
            "session_hosts:\n  herdr:\n    endpoints:\n      fixture: {{execution_node: test-node, socket_path: /adk/herdr.sock, herdr_home: /adk/herdr, herdr_session: agentdesk}}\n    channels: {{'{channel}': fixture}}\n"
        )
    } else {
        String::new()
    };
    serde_yaml::from_str(&format!("server: {{}}\ncluster: {{instance_id: test-node}}\nagents:\n  - id: fixture\n    name: fixture\n    channels:\n      claude: {{id: '{channel}', runtime: tui}}\n{extra}")).unwrap()
}
fn evidence(lookup: &HostedLookup, key: &str, name: &str) -> SessionTargetEvidence {
    SessionTargetEvidence {
        session_key: Some(key.into()),
        session_name: Some(name.into()),
        session_record: session_record_witness(lookup),
        inflight_locator: HostWitness::Absent,
        host_marker: HostWitness::Absent,
        ..SessionTargetEvidence::unread()
    }
}
fn scope(
    config: &Config,
    owner: &HostedOwner,
    key: &str,
    (exact, canonical): (&HostedLookup, &HostedLookup),
    evidence: Result<SessionTargetEvidence, String>,
    marker: HostKindMarker,
    row: &Result<Option<InflightTurnState>, String>,
) -> Scope {
    host_scope(HostReads {
        channel: owner.channel_id.parse().unwrap(),
        provider: &ProviderKind::Claude,
        token_hash: TOKEN,
        name: &owner.logical_key,
        config,
        expected_owner: owner,
        exact_key: key,
        exact,
        canonical,
        legacy_absence: false,
        evidence,
        marker,
        inflight: row,
    })
    .result
}
fn test_owner(channel: u64) -> HostedOwner {
    let mut owner = owner(&channel.to_string());
    owner.logical_key = ProviderKind::Claude.build_tmux_session_name("s0u-host");
    owner
}
async fn lookup(pool: &sqlx::PgPool, key: &str) -> HostedLookup {
    load_hosted_execution_pg(pool, HostedLookupKey::SessionKey(key)).await
}

#[tokio::test]
async fn u_local_row_identity_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = test_owner(CHANNEL);
    let config = config(CHANNEL, false);
    let key =
        adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Claude, &owner.logical_key);
    inflight::seed_session_row_keyed(&pool, &key, CHANNEL, None).await;
    let local = lookup(&pool, &key).await;
    let canonical = load_hosted_execution_pg(
        &pool,
        HostedLookupKey::Canonical {
            provider: "claude",
            identity: CanonicalSessionIdentity {
                kind: SessionIdentityKind::DiscordChannel,
                discord_token_hash: TOKEN,
                channel_id: &CHANNEL.to_string(),
            },
        },
    )
    .await;
    assert_eq!(
        scope(
            &config,
            &owner,
            &key,
            (&local, &canonical),
            Ok(evidence(&local, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::LocalInventory
    );
    assert_eq!(
        scope(
            &config,
            &owner,
            &key,
            (&HostedLookup::Missing, &canonical),
            Ok(evidence(&HostedLookup::Missing, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold("exact_canonical_mismatch")
    );
    let other_key = format!("claude/{TOKEN}/foreign:other");
    inflight::seed_session_row_keyed(&pool, &other_key, CHANNEL + 1, None).await;
    let other = lookup(&pool, &other_key).await;
    assert_eq!(
        scope(
            &config,
            &owner,
            &key,
            (&local, &other),
            Ok(evidence(&local, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold("exact_canonical_mismatch")
    );
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(&key)
        .bind(json!({"schema": 99}))
        .execute(&pool)
        .await
        .unwrap();
    let changed = lookup(&pool, &key).await;
    let (HostedLookup::Found(a), HostedLookup::Found(b)) = (&local, &changed) else {
        panic!("lookups failed");
    };
    assert_eq!(a.session_id(), b.session_id());
    assert_ne!(a, b);
    assert_eq!(
        scope(
            &config,
            &owner,
            &key,
            (&local, &changed),
            Ok(evidence(&local, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold("exact_canonical_mismatch")
    );
    let missing = HostedLookup::Missing;
    let no_row = evidence(&missing, &key, &owner.logical_key);
    assert_eq!(
        scope(
            &config,
            &owner,
            &key,
            (&missing, &missing),
            Ok(no_row.clone()),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::LocalInventory
    );
    assert!(matches!(
        resolve_session_target(
            SessionTargetInput::SessionKey(key.clone()),
            &Evidence(no_row.clone())
        )
        .host,
        TargetHost::Unknown(_)
    ));
    let mut traced = no_row;
    traced.process_registry_hit = true;
    assert!(matches!(
        scope(
            &config,
            &owner,
            &key,
            (&missing, &missing),
            Ok(traced),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
    let mut tmux_trace = evidence(&missing, &key, &owner.logical_key);
    tmux_trace.runtime_kind_marker =
        Some(crate::services::agent_protocol::RuntimeHandoffKind::LegacyTmuxWrapper);
    assert!(matches!(
        scope(
            &config,
            &owner,
            &key,
            (&missing, &missing),
            Ok(tmux_trace),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
    assert!(matches!(
        scope(
            &config,
            &owner,
            &other_key,
            (&local, &local),
            Ok(evidence(&local, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
}

#[tokio::test]
async fn u_host_scope_matrix_pg() {
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let owner = test_owner(CHANNEL);
    let config = config(CHANNEL, true);
    let key =
        adk_session::build_namespaced_session_key(TOKEN, &ProviderKind::Claude, &owner.logical_key);
    let missing = HostedLookup::Missing;
    let first = scope(
        &config,
        &owner,
        &key,
        (&missing, &missing),
        Ok(evidence(&missing, &key, &owner.logical_key)),
        HostKindMarker::Absent,
        &Ok(None),
    );
    assert_eq!(first, Scope::HerdrFirstCreate);
    assert_eq!(
        decide_unbound(
            candidates(Some("s0u-host"), &[], vec![], None),
            HostScope {
                channel: CHANNEL,
                provider: ProviderKind::Claude,
                name: owner.logical_key.clone(),
                result: first
            },
            &Inventory::ProbeFailed("tmux unsupported".into()),
            &BTreeMap::new()
        ),
        Verdict::Absent(Absence::HerdrFirstCreate)
    );
    for state in [
        HostedState::Pending,
        HostedState::Bound,
        HostedState::Retired,
    ] {
        let mut record = record(&owner, "nonce", state);
        if let Some(location) = &mut record.location {
            location.endpoint_config_key = "fixture".into();
        }
        inflight::seed_session_row_keyed(&pool, &key, CHANNEL, Some(wire(&record))).await;
        let found = lookup(&pool, &key).await;
        let result = scope(
            &config,
            &owner,
            &key,
            (&found, &found),
            Ok(evidence(&found, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None),
        );
        if state == HostedState::Retired {
            assert_eq!(result, Scope::HerdrRetired);
            let mut matched = evidence(&found, &key, &owner.logical_key);
            matched.host_marker = marker_witness(HostKindMarker::Known(HostKind::Herdr));
            assert_eq!(
                scope(
                    &config,
                    &owner,
                    &key,
                    (&found, &found),
                    Ok(matched),
                    HostKindMarker::Known(HostKind::Herdr),
                    &Ok(None)
                ),
                Scope::HerdrRetired
            );
            let mut conflicting = evidence(&found, &key, &owner.logical_key);
            conflicting.host_marker = marker_witness(HostKindMarker::Known(HostKind::Process));
            assert!(matches!(
                scope(
                    &config,
                    &owner,
                    &key,
                    (&found, &found),
                    Ok(conflicting),
                    HostKindMarker::Known(HostKind::Process),
                    &Ok(None)
                ),
                Scope::Hold(_)
            ));
            assert_eq!(
                decide_unbound(
                    candidates(Some("s0u-host"), &[], vec![], None),
                    HostScope {
                        channel: CHANNEL,
                        provider: ProviderKind::Claude,
                        name: owner.logical_key.clone(),
                        result
                    },
                    &Inventory::ProbeFailed("tmux failure".into()),
                    &BTreeMap::new()
                ),
                Verdict::Absent(Absence::HerdrRetired)
            );
            for inflight in [
                Ok(Some(row(CHANNEL, &owner.logical_key))),
                Ok(Some(row(CHANNEL, "other"))),
                Err("inflight read failed".into()),
            ] {
                assert!(matches!(
                    scope(
                        &config,
                        &owner,
                        &key,
                        (&found, &found),
                        Ok(evidence(&found, &key, &owner.logical_key)),
                        HostKindMarker::Absent,
                        &inflight
                    ),
                    Scope::Hold(_)
                ));
            }
            let mut foreign = owner.clone();
            foreign.owner_node = "other-node".into();
            assert!(matches!(
                scope(
                    &config,
                    &foreign,
                    &key,
                    (&found, &found),
                    Ok(evidence(&found, &key, &owner.logical_key)),
                    HostKindMarker::Absent,
                    &Ok(None)
                ),
                Scope::Hold(_)
            ));
        } else {
            assert!(matches!(result, Scope::Hold(_)));
        }
        assert!(matches!(
            scope(
                &super::tests::config(CHANNEL, false),
                &owner,
                &key,
                (&found, &found),
                Ok(evidence(&found, &key, &owner.logical_key)),
                HostKindMarker::Absent,
                &Ok(None)
            ),
            Scope::Hold(_)
        ));
    }
    inflight::seed_session_row_keyed(&pool, &key, CHANNEL, None).await;
    let legacy = lookup(&pool, &key).await;
    let local = super::tests::config(CHANNEL, false);
    for locator in [
        HostWitness::ReadFailed("denied".into()),
        HostWitness::Unrecognized("future".into()),
        HostWitness::Known {
            kind: HostKind::Herdr,
            target: Some("pane".into()),
        },
    ] {
        let mut e = evidence(&legacy, &key, &owner.logical_key);
        e.inflight_locator = locator;
        e.host_marker = marker_witness(HostKindMarker::Known(HostKind::Tmux));
        assert!(matches!(
            scope(
                &local,
                &owner,
                &key,
                (&legacy, &legacy),
                Ok(e),
                HostKindMarker::Known(HostKind::Tmux),
                &Ok(None)
            ),
            Scope::Hold(_)
        ));
    }
    for marker in [
        HostKindMarker::Known(HostKind::Process),
        HostKindMarker::Known(HostKind::Herdr),
        HostKindMarker::Unrecognized("future".into()),
        HostKindMarker::ReadFailed("denied".into()),
    ] {
        let mut e = evidence(&legacy, &key, &owner.logical_key);
        e.host_marker = marker_witness(marker.clone());
        assert!(matches!(
            scope(
                &local,
                &owner,
                &key,
                (&legacy, &legacy),
                Ok(e),
                marker,
                &Ok(None)
            ),
            Scope::Hold(_)
        ));
    }
    let mut failed = evidence(&legacy, &key, &owner.logical_key);
    failed.inflight_locator = HostWitness::ReadFailed("actual read error".into());
    assert_eq!(
        scope(
            &local,
            &owner,
            &key,
            (&legacy, &legacy),
            Ok(failed),
            HostKindMarker::Absent,
            &Ok(Some(row(CHANNEL, &owner.logical_key)))
        ),
        Scope::Hold("probe_failed")
    );
    assert_eq!(
        scope(
            &local,
            &owner,
            &key,
            (&legacy, &legacy),
            Err("read error".into()),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold("probe_failed")
    );
    let mut unsupported = local.clone();
    unsupported.agents.clear();
    assert!(matches!(
        scope(
            &unsupported,
            &owner,
            &key,
            (&missing, &missing),
            Ok(evidence(&missing, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
    let process: Config = serde_yaml::from_str(&format!("server: {{}}\nagents:\n  - id: fixture\n    name: fixture\n    channels:\n      claude: {{id: '{CHANNEL}', runtime: pipe, tui_hosting: true}}\n")).unwrap();
    assert!(matches!(
        scope(
            &process,
            &owner,
            &key,
            (&missing, &missing),
            Ok(evidence(&missing, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
    inflight::seed_session_row_keyed(&pool, &key, CHANNEL, Some(json!({"schema": 99}))).await;
    let unknown = lookup(&pool, &key).await;
    assert!(matches!(
        scope(
            &config,
            &owner,
            &key,
            (&unknown, &unknown),
            Ok(evidence(&unknown, &key, &owner.logical_key)),
            HostKindMarker::Absent,
            &Ok(None)
        ),
        Scope::Hold(_)
    ));
}

use std::cell::{Cell, RefCell};

use super::*;
use crate::config::session_hosts::approval_store::FileApprovalStore;
use crate::config::session_hosts::{EndpointMode, HerdrSection};
use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record};

const CHANNEL: u64 = 41;

struct Store {
    approved: Option<BootSessionHosts>,
    unavailable: Option<&'static str>,
    warning: Option<&'static str>,
    fail_save: bool,
    loads: Cell<usize>,
    saved: RefCell<Vec<Projection>>,
}

impl Store {
    fn new(approved: Option<BootSessionHosts>) -> Self {
        Self {
            approved,
            unavailable: None,
            warning: None,
            fail_save: false,
            loads: Cell::new(0),
            saved: RefCell::new(Vec::new()),
        }
    }
}

impl ApprovalStore for Store {
    fn load(&self) -> Loaded {
        self.loads.set(self.loads.get() + 1);
        if let Some(reason) = self.unavailable {
            Loaded::Unavailable(reason)
        } else if let Some(hosts) = &self.approved {
            Loaded::Known(hosts.clone(), self.warning)
        } else {
            Loaded::Missing
        }
    }

    fn save(&self, hosts: &BootSessionHosts) -> Result<(), String> {
        self.saved.borrow_mut().push(Projection::of(hosts));
        if self.fail_save {
            Err("save failed".into())
        } else {
            Ok(())
        }
    }
}

struct Rows {
    available: bool,
    result: RefCell<Result<Vec<MigrationRow>, String>>,
    requested: RefCell<Vec<BTreeSet<u64>>>,
}

impl Rows {
    fn new(rows: Vec<MigrationRow>) -> Self {
        Self {
            available: true,
            result: RefCell::new(Ok(rows)),
            requested: RefCell::new(Vec::new()),
        }
    }
}

impl RowSource for Rows {
    fn available(&self) -> bool {
        self.available
    }

    async fn rows(&self, channels: &BTreeSet<u64>) -> Result<Vec<MigrationRow>, String> {
        self.requested.borrow_mut().push(channels.clone());
        self.result
            .replace(Err("unexpected second row lookup".into()))
    }
}

struct Holds<const INTRODUCED: bool> {
    calls: Cell<usize>,
    result: Result<Vec<u64>, String>,
}

impl<const INTRODUCED: bool> Holds<INTRODUCED> {
    fn new(result: Result<Vec<u64>, String>) -> Self {
        Self {
            calls: Cell::new(0),
            result,
        }
    }
}

impl<const INTRODUCED: bool> HoldSource for Holds<INTRODUCED> {
    const INTRODUCED: bool = INTRODUCED;

    async fn unresolved(&self, _: &BTreeSet<u64>) -> Result<Vec<u64>, String> {
        self.calls.set(self.calls.get() + 1);
        self.result.clone()
    }
}

fn fixture() -> BootSessionHosts {
    let config = SessionHostsConfig {
        herdr: serde_json::from_value(serde_json::json!({
            "endpoints": {"one": {
                "execution_node": "test-node", "socket_path": "/fixture/herdr.sock",
                "herdr_home": "/fixture/herdr", "herdr_session": "agentdesk"
            }},
            "channels": {"41": "one"}
        }))
        .unwrap(),
        ..SessionHostsConfig::default()
    };
    Projection {
        hosts: config,
        local_node: Some("test-node".into()),
    }
    .into_hosts()
    .unwrap()
}

fn remap(mut hosts: BootSessionHosts, socket: &str) -> BootSessionHosts {
    hosts
        .config
        .herdr
        .endpoints
        .get_mut("one")
        .unwrap()
        .socket_path = socket.into();
    hosts.channels.get_mut(&CHANNEL).unwrap().socket_path = socket.into();
    hosts
}

fn located(state: HostedState) -> HostedRecord {
    let mut value = record(&owner(&CHANNEL.to_string()), "migration-execution", state);
    value.location = Some(
        crate::db::dispatched_sessions::hosted_execution::HostedLocation {
            host: "herdr".into(),
            execution_node: "test-node".into(),
            endpoint_config_key: "one".into(),
            socket_addr: "/fixture/herdr.sock".into(),
            named_session: "agentdesk".into(),
            pane_id: "pane-1".into(),
        },
    );
    HostedRecord::Known(value)
}

fn row(record: HostedRecord) -> MigrationRow {
    MigrationRow {
        id: 123,
        channel: CHANNEL,
        record,
    }
}

fn reason(hosts: &BootSessionHosts) -> Option<&str> {
    hosts.migration().map(|diagnostic| diagnostic.reason)
}

#[tokio::test]
async fn migration_absent_snapshot_seeds_current_projection_without_live_queries() {
    let candidate = fixture();
    let store = Store::new(None);
    let rows = Rows::new(vec![row(located(HostedState::Bound))]);
    let holds = Holds::<true>::new(Err("must not read holds on initial boot".into()));
    let selected = select(candidate.clone(), &store, &rows, &holds, false)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert_eq!(&*store.saved.borrow(), &[Projection::of(&candidate)]);
    assert!(rows.requested.borrow().is_empty());
    assert_eq!(holds.calls.get(), 0);
    assert_eq!(reason(&selected), None);
    assert!(!selected.admission_stopped());
}

#[tokio::test]
async fn migration_unchanged_projection_does_not_query_rows_holds_or_save() {
    let candidate = fixture();
    let store = Store::new(Some(candidate.clone()));
    let rows = Rows::new(Vec::new());
    *rows.result.borrow_mut() = Err("must not query unchanged rows".into());
    let holds = Holds::<true>::new(Err("must not query unchanged holds".into()));
    let selected = select(candidate.clone(), &store, &rows, &holds, false)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert_eq!(store.loads.get(), 1);
    assert!(rows.requested.borrow().is_empty());
    assert_eq!(holds.calls.get(), 0);
    assert!(store.saved.borrow().is_empty());
    assert_eq!(reason(&selected), None);
}

#[tokio::test]
async fn migration_new_channel_is_approved_with_zero_reads_of_unintroduced_holds() {
    let old = fixture();
    let mut candidate = old.clone();
    candidate
        .channels
        .insert(42, candidate.channels[&CHANNEL].clone());
    candidate
        .config
        .herdr
        .channels
        .insert("42".into(), "one".into());
    let store = Store::new(Some(old));
    let rows = Rows::new(Vec::new());
    let holds = Holds::<false>::new(Err("hold store does not exist in this slice".into()));
    let selected = select(candidate.clone(), &store, &rows, &holds, false)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert_eq!(&*rows.requested.borrow(), &[BTreeSet::from([42])]);
    assert_eq!(holds.calls.get(), 0);
    assert_eq!(store.saved.borrow().len(), 1);
    assert_eq!(reason(&selected), None);
}

#[tokio::test]
async fn migration_live_row_variants_refuse_moves_and_legacy_or_retired_allow_them() {
    let mut remote = located(HostedState::Bound);
    if let HostedRecord::Known(value) = &mut remote {
        value.location.as_mut().unwrap().execution_node = "another-node".into();
    }
    for (name, value, refuses) in [
        (
            "pending without location",
            HostedRecord::Known(record(&owner("41"), "n", HostedState::Pending)),
            true,
        ),
        ("pending with location", located(HostedState::Pending), true),
        ("bound", located(HostedState::Bound), true),
        ("remote node", remote, true),
        (
            "unknown",
            HostedRecord::Unknown(serde_json::json!({"schema": 999})),
            true,
        ),
        ("retired", located(HostedState::Retired), false),
        ("legacy", HostedRecord::Legacy, false),
    ] {
        let old = fixture();
        let candidate = remap(old.clone(), "/fixture/new.sock");
        let store = Store::new(Some(old.clone()));
        let rows = Rows::new(vec![row(value)]);
        let selected = select(candidate.clone(), &store, &rows, &NoHoldStore, false)
            .await
            .into_hosts();
        let expected = if refuses { &old } else { &candidate };
        assert_eq!(
            Projection::of(&selected),
            Projection::of(expected),
            "{name}"
        );
        assert_eq!(
            reason(&selected),
            refuses.then_some("session_hosts_migration_refused"),
            "{name}"
        );
        assert_eq!(store.saved.borrow().len(), usize::from(!refuses), "{name}");
        if refuses {
            assert_eq!(
                selected.migration().unwrap().blockers,
                ["channel 41 row 123"]
            );
        }
    }
}

#[tokio::test]
async fn migration_compares_recorded_location_to_proposed_mapping_not_old_locator() {
    let candidate = fixture();
    let old = remap(candidate.clone(), "/fixture/stale-baseline.sock");
    let store = Store::new(Some(old));
    let rows = Rows::new(vec![row(located(HostedState::Bound))]);
    let selected = select(candidate.clone(), &store, &rows, &NoHoldStore, false)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert_eq!(reason(&selected), None);
    assert_eq!(store.saved.borrow().len(), 1);
}

#[tokio::test]
async fn migration_endpoint_identity_changes_and_channel_removal_refuse_live_rows() {
    type Edit = (&'static str, fn(&mut BootSessionHosts));
    let edits: [Edit; 8] = [
        ("socket", |h| {
            h.config.herdr.endpoints.get_mut("one").unwrap().socket_path =
                "/fixture/new.sock".into();
            h.channels.get_mut(&CHANNEL).unwrap().socket_path = "/fixture/new.sock".into();
        }),
        ("node", |h| {
            h.config
                .herdr
                .endpoints
                .get_mut("one")
                .unwrap()
                .execution_node = "another-node".into();
            h.channels.get_mut(&CHANNEL).unwrap().execution_node = "another-node".into();
        }),
        ("named session", |h| {
            h.config
                .herdr
                .endpoints
                .get_mut("one")
                .unwrap()
                .herdr_session = "new-session".into();
            h.channels.get_mut(&CHANNEL).unwrap().herdr_session = "new-session".into();
        }),
        ("home/config source", |h| {
            h.config.herdr.endpoints.get_mut("one").unwrap().herdr_home =
                "/fixture/new-home".into();
            h.channels.get_mut(&CHANNEL).unwrap().herdr_home = "/fixture/new-home".into();
        }),
        ("mode", |h| {
            h.config.herdr.endpoints.get_mut("one").unwrap().mode = EndpointMode::UserDefault;
        }),
        ("local node", |h| {
            h.local_node = Some("another-node".into());
        }),
        ("channel removed", |h| {
            h.channels.clear();
            h.config.herdr.channels.clear();
        }),
        ("endpoint removed", |h| {
            h.channels.clear();
            h.config.herdr = HerdrSection::default();
        }),
    ];
    for (name, edit) in edits {
        let old = fixture();
        let mut candidate = old.clone();
        edit(&mut candidate);
        let store = Store::new(Some(old.clone()));
        let rows = Rows::new(vec![row(located(HostedState::Bound))]);
        let selected = select(candidate, &store, &rows, &NoHoldStore, false)
            .await
            .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&old), "{name}");
        assert_eq!(
            reason(&selected),
            Some("session_hosts_migration_refused"),
            "{name}"
        );
        assert!(store.saved.borrow().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn migration_hold_blocks_even_without_rows_and_hold_failure_is_unverified() {
    for (held, expected) in [
        (Ok(vec![CHANNEL]), "session_hosts_migration_refused"),
        (
            Err("hold lookup unavailable".into()),
            "session_hosts_migration_unverified",
        ),
    ] {
        let old = fixture();
        let store = Store::new(Some(old.clone()));
        let rows = Rows::new(Vec::new());
        let holds = Holds::<true>::new(held);
        let selected = select(
            remap(old.clone(), "/fixture/new.sock"),
            &store,
            &rows,
            &holds,
            false,
        )
        .await
        .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&old));
        assert_eq!(reason(&selected), Some(expected));
        assert_eq!(holds.calls.get(), 1);
        assert!(store.saved.borrow().is_empty());
    }
    let old = fixture();
    let candidate = remap(old.clone(), "/fixture/new.sock");
    let store = Store::new(Some(old));
    let holds = Holds::<true>::new(Ok(Vec::new()));
    let selected = select(
        candidate.clone(),
        &store,
        &Rows::new(Vec::new()),
        &holds,
        false,
    )
    .await
    .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert_eq!(reason(&selected), None);
    assert_eq!(holds.calls.get(), 1);
    assert_eq!(store.saved.borrow().len(), 1);
}

#[tokio::test]
async fn migration_row_failure_and_missing_pool_keep_approved_projection() {
    for available in [true, false] {
        let old = fixture();
        let store = Store::new(Some(old.clone()));
        let mut rows = Rows::new(Vec::new());
        rows.available = available;
        *rows.result.borrow_mut() = Err("row lookup unavailable".into());
        let holds = Holds::<true>::new(Err("must not query holds after row failure".into()));
        let selected = select(
            remap(old.clone(), "/fixture/new.sock"),
            &store,
            &rows,
            &holds,
            false,
        )
        .await
        .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&old));
        assert_eq!(
            reason(&selected),
            Some("session_hosts_migration_unverified")
        );
        assert_eq!(rows.requested.borrow().len(), usize::from(available));
        assert_eq!(holds.calls.get(), 0);
        assert!(store.saved.borrow().is_empty());
    }
}

#[tokio::test]
async fn migration_read_only_cli_uses_approved_hosts_and_never_seeds_or_saves() {
    let old = fixture();
    let candidate = remap(old.clone(), "/fixture/new.sock");
    let rows = Rows::new(Vec::new());
    let holds = Holds::<true>::new(Err("CLI does not approve migrations".into()));
    let store = Store::new(Some(old.clone()));
    let selected = select(candidate.clone(), &store, &rows, &holds, true)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&old));
    assert_eq!(reason(&selected), Some("session_hosts_migration_pending"));
    assert!(store.saved.borrow().is_empty());
    let absent = Store::new(None);
    let selected = select(candidate.clone(), &absent, &rows, &holds, true)
        .await
        .into_hosts();
    assert_eq!(Projection::of(&selected), Projection::of(&candidate));
    assert!(absent.saved.borrow().is_empty());
    assert!(rows.requested.borrow().is_empty());
    assert_eq!(holds.calls.get(), 0);
}

#[tokio::test]
async fn migration_save_failure_distinguishes_initial_seed_from_changed_approval() {
    let old = fixture();
    let candidate = remap(old.clone(), "/fixture/new.sock");
    for approved in [None, Some(old.clone())] {
        let mut store = Store::new(approved.clone());
        store.fail_save = true;
        let rows = Rows::new(Vec::new());
        let selected = select(candidate.clone(), &store, &rows, &NoHoldStore, false)
            .await
            .into_hosts();
        assert_eq!(
            Projection::of(&selected),
            Projection::of(approved.as_ref().unwrap_or(&candidate))
        );
        assert_eq!(reason(&selected), Some("approval_snapshot_not_durable"));
        assert!(!selected.admission_stopped());
        assert_eq!(store.saved.borrow().len(), 1);
    }
}

#[tokio::test]
async fn migration_unreadable_snapshots_keep_candidate_channels_and_stop_admission() {
    for cause in [
        "approval_snapshot_corrupt",
        "approval_snapshot_unreadable",
        "approval_snapshot_newer",
    ] {
        let candidate = fixture();
        let mut store = Store::new(None);
        store.unavailable = Some(cause);
        let rows = Rows::new(Vec::new());
        let holds = Holds::<true>::new(Err("must not query without approved baseline".into()));
        let selected = select(candidate.clone(), &store, &rows, &holds, false)
            .await
            .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&candidate));
        assert!(selected.herdr_endpoint(CHANNEL).is_some());
        assert!(selected.admission_stopped());
        assert_eq!(reason(&selected), Some(cause));
        assert!(rows.requested.borrow().is_empty());
        assert_eq!(holds.calls.get(), 0);
        assert!(store.saved.borrow().is_empty());
    }
}

fn snapshot(schema: u64, hosts: &BootSessionHosts) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"schema": schema, "projection": Projection::of(hosts)}))
        .unwrap()
}

fn known(loaded: Loaded) -> (BootSessionHosts, Option<&'static str>) {
    match loaded {
        Loaded::Known(hosts, warning) => (hosts, warning),
        Loaded::Missing => panic!("expected known snapshot, found absent"),
        Loaded::Unavailable(reason) => panic!("expected known snapshot: {reason}"),
    }
}

#[test]
fn migration_file_store_distinguishes_absence_corrupt_and_unreadable() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileApprovalStore(Some(temp.path().join("approval")));
    assert!(matches!(store.load(), Loaded::Missing));
    std::fs::create_dir_all(store.0.as_ref().unwrap()).unwrap();
    let current = store.0.as_ref().unwrap().join("current.json");
    std::fs::write(&current, b"broken json").unwrap();
    assert!(matches!(
        store.load(),
        Loaded::Unavailable("approval_snapshot_corrupt")
    ));
    std::fs::remove_file(&current).unwrap();
    std::fs::create_dir(&current).unwrap();
    assert!(matches!(
        store.load(),
        Loaded::Unavailable("approval_snapshot_unreadable")
    ));
    assert!(matches!(
        FileApprovalStore(None).load(),
        Loaded::Unavailable("approval_snapshot_unreadable")
    ));
}

#[test]
fn migration_file_store_old_schema_decodes_same_hosts_and_new_schema_is_not_corrupt() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileApprovalStore(Some(temp.path().to_owned()));
    let candidate = fixture();
    let path = temp.path().join("current.json");
    let mut old: serde_json::Value = serde_json::from_slice(&snapshot(0, &candidate)).unwrap();
    old["projection"]["local_node"] = serde_json::json!(" test-node ");
    std::fs::write(&path, serde_json::to_vec(&old).unwrap()).unwrap();
    let (decoded, warning) = known(store.load());
    assert_eq!(Projection::of(&decoded), Projection::of(&candidate));
    assert_eq!(warning, None);
    assert_eq!(
        decoded.config().herdr.endpoints["one"].mode,
        EndpointMode::LegacyDedicated
    );
    let before = crate::services::session_host::HerdrRegistry::build(&candidate);
    let after = crate::services::session_host::HerdrRegistry::build(&decoded);
    assert!(!before.is_empty());
    assert_eq!(before.is_empty(), after.is_empty());
    assert_eq!(before.sole_host().is_some(), after.sole_host().is_some());
    let HostedRecord::Known(row) = located(HostedState::Bound) else {
        panic!("fixture must contain a located Bound record");
    };
    assert!(before.view(&row).is_some());
    assert!(after.view(&row).is_some());
    std::fs::write(
        &path,
        br#"{"schema": 999, "projection": "unknown newer layout"}"#,
    )
    .unwrap();
    assert!(matches!(
        store.load(),
        Loaded::Unavailable("approval_snapshot_newer")
    ));
}

#[tokio::test]
async fn migration_file_store_recovers_previous_generation_without_applying_candidate() {
    let temp = tempfile::tempdir().unwrap();
    let store = FileApprovalStore(Some(temp.path().to_owned()));
    let old = fixture();
    std::fs::write(temp.path().join("previous.json"), snapshot(1, &old)).unwrap();
    for (bytes, expected) in [
        (b"corrupt".to_vec(), "approval_snapshot_corrupt"),
        (snapshot(999, &old), "approval_snapshot_newer"),
    ] {
        std::fs::write(temp.path().join("current.json"), &bytes).unwrap();
        let (loaded, warning) = known(store.load());
        assert_eq!(Projection::of(&loaded), Projection::of(&old));
        assert_eq!(warning, Some(expected));
        let rows = Rows::new(Vec::new());
        let selected = select(
            remap(old.clone(), "/fixture/new.sock"),
            &store,
            &rows,
            &NoHoldStore,
            false,
        )
        .await
        .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&old));
        assert_eq!(reason(&selected), Some(expected));
        assert!(!selected.admission_stopped());
        assert!(rows.requested.borrow().is_empty());
        assert_eq!(
            std::fs::read(temp.path().join("current.json")).unwrap(),
            bytes
        );
    }
    use std::os::unix::fs::PermissionsExt;
    let pending = temp.path().join("pending");
    let candidate = remap(old.clone(), "/fixture/new.sock");
    std::fs::write(temp.path().join("current.json"), snapshot(1, &candidate)).unwrap();
    std::fs::write(&pending, b"committed").unwrap();
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0)).unwrap();
    assert!(std::fs::read(&pending).is_err());
    let (loaded, warning) = known(store.load());
    assert_eq!(Projection::of(&loaded), Projection::of(&old));
    assert_eq!(warning, Some("approval_snapshot_not_durable"));
    std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o600)).unwrap();
}

#[test]
fn migration_mode_omitted_and_explicit_legacy_normalize_identically() {
    let omitted = fixture();
    let mut explicit = serde_json::to_value(omitted.config()).unwrap();
    explicit["herdr"]["endpoints"]["one"]["mode"] = serde_json::json!("legacy_dedicated");
    let explicit = Projection {
        hosts: serde_json::from_value(explicit).unwrap(),
        local_node: omitted.local_node.clone(),
    }
    .into_hosts()
    .unwrap();
    assert_eq!(Projection::of(&explicit), Projection::of(&omitted));
    assert_eq!(explicit.channels(), omitted.channels());
    assert!(
        serde_json::to_value(explicit.config()).unwrap()["herdr"]["endpoints"]["one"]
            .get("mode")
            .is_none()
    );
}

#[test]
fn migration_user_default_validator_reports_key_and_supported_action() {
    let config: crate::config::Config = serde_yaml::from_str(
        "server: {}\nsession_hosts:\n  herdr:\n    endpoints:\n      one:\n        mode: user_default\n        execution_node: test-node\n"
    ).unwrap();
    let error = crate::config::validate_config(&config)
        .unwrap_err()
        .to_string();
    for required in [
        "session_hosts.herdr.endpoints.one.mode",
        "user_default",
        "지원 조각 미착지",
        "legacy_dedicated",
        "until support is complete",
    ] {
        assert!(error.contains(required), "missing {required:?}: {error}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn migration_failed_publication_keeps_server_and_read_only_cli_on_old_generation() {
    for cleanup in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let store = FileApprovalStore(Some(temp.path().to_owned()));
        let old = fixture();
        store.save(&old).unwrap();
        let candidate = remap(old.clone(), "/fixture/new.sock");
        if cleanup {
            super::super::approval_store::FAIL_CLEANUP_SYNC.with(|fault| fault.set(true));
        } else {
            super::super::approval_store::FAIL_CURRENT_SYNC.with(|fault| fault.set(true));
        }
        let selected = select(
            candidate.clone(),
            &store,
            &Rows::new(vec![]),
            &NoHoldStore,
            false,
        )
        .await
        .into_hosts();
        assert_eq!(Projection::of(&selected), Projection::of(&old));
        assert_eq!(reason(&selected), Some("approval_snapshot_not_durable"));
        assert!(
            String::from_utf8(std::fs::read(temp.path().join("current.json")).unwrap())
                .unwrap()
                .contains("new.sock"),
            "fault happened after publication"
        );
        let (loaded, warning) = known(store.load());
        assert_eq!(Projection::of(&loaded), Projection::of(&old));
        assert_eq!(warning, Some("approval_snapshot_not_durable"));
        let cli = select(candidate, &store, &Rows::new(vec![]), &NoHoldStore, true)
            .await
            .into_hosts();
        assert_eq!(Projection::of(&cli), Projection::of(&old));
    }
}

#[cfg(unix)]
#[test]
fn migration_dangling_snapshot_entry_is_unreadable_instead_of_a_first_boot() {
    let temp = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("missing"),
        temp.path().join("current.json"),
    )
    .unwrap();
    let store = FileApprovalStore(Some(temp.path().to_owned()));
    assert!(matches!(
        store.load(),
        Loaded::Unavailable("approval_snapshot_unreadable")
    ));
}

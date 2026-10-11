use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::db::session_transcripts::native_channel_clear_record;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::DiscordSession;
use crate::services::discord::input_runtime::clear::{self, Outcome, PG_RETRY_NOTICE, Unresolved};
use crate::services::discord::input_runtime::fence::{self, Gate, Mode};
use crate::services::tui_input::durability_tests::supported::Recording;
use crate::services::tui_input::ledger::Ledger;
use crate::services::tui_input::rows::{AbandonReason, DoneReason, Entry, RowState};
use crate::services::tui_prompt_dedupe::binding_context::BindingContext;
use crate::services::tui_prompt_dedupe::binding_events::{
    BindingCause, CauseSource, HookSignal, Proposal, SourceId,
};
use crate::services::tui_prompt_dedupe::native_clear::InputCutoff;

const CUT: RowState = RowState::Abandoned(AbandonReason::UserClear);

/// Records each provider, selector and notice effect in order.
#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    dead: AtomicBool,
    // A reset under a held population scope would self-deadlock on the channel lock.
    scoped: AtomicBool,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn note(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }
}

impl ClearEffects for Fake {
    fn clear_selector<'a>(&'a self, session_key: &'a str) -> Step<'a, bool> {
        self.note(format!("clear:{session_key}"));
        Box::pin(async { true })
    }
    fn reset_process(&self, tmux: &str) {
        self.note(format!("reset:{tmux}"));
        self.scoped.fetch_or(fence::scope_held(), Ordering::SeqCst);
        self.dead.store(true, Ordering::SeqCst);
    }
    fn alive(&self, _: &str) -> bool {
        !self.dead.load(Ordering::SeqCst)
    }
    fn notice<'a>(&'a self, _: serenity::ChannelId, text: &'a str) -> Step<'a, ()> {
        self.note(format!("notice:{text}"));
        Box::pin(async {})
    }
}

struct Fixture {
    db: Option<crate::dispatch::test_support::DispatchPostgresTestDb>,
    pool: sqlx::PgPool,
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: serenity::ChannelId,
    tmux: String,
    session_key: String,
    nonce: String,
    ledger_root: tempfile::TempDir,
    _binding_root: tempfile::TempDir,
    _host: crate::config::TestEnvVarGuard,
    _root: crate::config::TestEnvVarGuard,
    _root_dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(n: u64, provider: ProviderKind) -> Self {
        let root_dir = tempfile::tempdir().unwrap();
        // A config in the scratch root keeps node identity off any machine-wide config.
        let config = root_dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();
        let data = serde_json::to_string(&root_dir.path().join("data")).unwrap();
        let yaml =
            format!("server: {{}}\ndata:\n  dir: {data}\ncluster: {{instance_id: test-node}}\n");
        std::fs::write(config.join("agentdesk.yaml"), yaml).unwrap();
        let root = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root_dir.path());
        let host = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_INSTANCE_ID",
            "test-node".as_ref(),
        );
        let binding_root = tempfile::tempdir().unwrap();
        binding_events::set_test_root(Some(binding_root.path()));
        let channel_id = serenity::ChannelId::new(6_325_500 + n);
        let channel_name = format!(
            "adk-6325-ledger-clear-{n}-{}",
            uuid::Uuid::new_v4().simple()
        );
        let tmux = provider.build_tmux_session_name(&channel_name);
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "agentdesk_ledger_clear_6325",
            "ledger clear adapter",
        )
        .await;
        let pool = db.connect_and_migrate_with_max_connections(4).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.core.lock().await.sessions.insert(
            channel_id,
            DiscordSession {
                session_id: Some("old".into()),
                memento_context_loaded: true,
                memento_reflected: false,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                remote_profile_name: None,
                channel_id: Some(channel_id.get()),
                channel_name: Some(channel_name),
                category_name: None,
                last_active: tokio::time::Instant::now(),
                worktree: None,
                born_generation: shared.restart.current_generation,
            },
        );
        let build = super::super::super::super::adk_session::build_namespaced_session_key;
        let session_key = build(&shared.token_hash, &provider, &tmux);
        let seed = crate::services::discord::inflight::seed_session_row_keyed;
        seed(&pool, &session_key, channel_id.get(), None).await;
        let context = BindingContext {
            schema: 1,
            provider: provider.as_str().into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.clone(),
            channel_id: Some(channel_id.get()),
            owner_runtime_root: root_dir.path().display().to_string(),
            host: Some("test-node".into()),
            expected_native_session_id: Some("old".into()),
            launch_mode: "fresh".into(),
            provider_root: None,
            first_prompt_digest: None,
            source_policy: None,
        };
        let contexts = root_dir
            .path()
            .join("runtime/binding_contexts")
            .join(provider.as_str());
        std::fs::create_dir_all(&contexts).unwrap();
        let context_file = contexts.join(format!("{}.json", context.execution_nonce));
        std::fs::write(context_file, serde_json::to_vec(&context).unwrap()).unwrap();
        let marker = tmux_common::session_temp_path(&tmux, "spawn_nonce");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(&marker, &context.execution_nonce).unwrap();
        let kind = match provider {
            ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
            _ => RuntimeHandoffKind::ClaudeTui,
        };
        tmux_common::write_tmux_runtime_kind_marker(&tmux, kind).unwrap();
        record_on(
            binding_root.path(),
            channel_id,
            provider.as_str(),
            &tmux,
            "old",
        );
        Self {
            db: Some(db),
            pool,
            shared,
            http: Arc::new(serenity::Http::new("")),
            provider,
            channel_id,
            tmux,
            session_key,
            nonce: context.execution_nonce,
            ledger_root: sandbox(),
            _binding_root: binding_root,
            _host: host,
            _root: root,
            _root_dir: root_dir,
        }
    }

    async fn host(&self, fake: &Arc<Fake>) -> LedgerClear {
        let effects: Arc<dyn ClearEffects> = fake.clone();
        let admit = LedgerClear::admit_with;
        admit(
            &self.http,
            &self.shared,
            &self.provider,
            self.channel_id,
            None,
            effects,
        )
        .await
        .unwrap()
    }

    async fn guard(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.shared
            .acquire_session_transition(self.channel_id)
            .await
            .unwrap()
    }

    fn ledger(&self, keys: &[u64]) -> Ledger {
        let mut ledger = Ledger::open(self.ledger_root.path(), self.channel_id.get()).unwrap();
        for &key in keys {
            let input = serde_json::json!({ "text": format!("input {key}") });
            let entry = Entry::Received { key, input };
            ledger.append_entry(&entry, &[]).unwrap();
        }
        ledger
    }

    fn states(&self) -> BTreeMap<u64, RowState> {
        let ledger = Ledger::open(self.ledger_root.path(), self.channel_id.get()).unwrap();
        let rows = ledger.rows().unwrap();
        (1..=64)
            .filter_map(|key| rows.row(key).map(|row| (key, row.state)))
            .collect()
    }

    async fn record(&self) -> session_transcripts::NativeClearRecord {
        let key = self.channel_id.get().to_string();
        native_channel_clear_record(&self.pool, &key)
            .await
            .unwrap()
            .unwrap()
    }

    fn marker_present(&self) -> bool {
        std::path::Path::new(&tmux_common::session_temp_path(&self.tmux, "spawn_nonce")).exists()
    }

    async fn drop_db(mut self) {
        binding_events::forget_channel_for_tests(self.channel_id.get());
        binding_events::set_test_root(None);
        self.pool.close().await;
        self.db.take().unwrap().drop().await;
    }
}

fn record_on(
    root: &std::path::Path,
    channel_id: serenity::ChannelId,
    provider: &str,
    tmux: &str,
    session: &str,
) {
    let source = SourceId {
        session_id: session.into(),
        path: root.join(format!("{session}.jsonl")),
        dev: 1,
        ino: session.len() as u64,
    };
    let path = source.path.display().to_string();
    let hook = HookSignal::from_payload("session_start", &serde_json::json!({"source":"startup"}));
    let proposal = Proposal {
        channel_id: channel_id.get(),
        provider,
        tmux_session: tmux,
        session_id: Some(session),
        path: &path,
        replaced: None,
        cause: CauseSource::Hook(BindingCause::Startup),
        hook: Some(&hook),
    };
    tmux_common::with_tmux_source_authority(tmux, |_| {
        binding_events::record_verified(&proposal, &source).unwrap();
    });
}

// The ledger refuses symlinked ancestors such as macOS's `/var`, so it lives under the target dir.
fn sandbox() -> tempfile::TempDir {
    let parent = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/i01-tmp");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::tempdir_in(parent).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn health(channel: serenity::ChannelId) -> Option<String> {
    let needle = format!(" channel={} ", channel.get());
    clear::health_reasons()
        .into_iter()
        .find(|r| r.contains(&needle))
}

#[test]
fn the_production_adapter_cuts_resets_and_resolves_for_both_providers_pg() {
    runtime().block_on(async {
        for (n, provider) in [(1, ProviderKind::Claude), (2, ProviderKind::Codex)] {
            let fixture = Fixture::new(n, provider.clone()).await;
            let fake = Arc::new(Fake::default());
            let mut host = fixture.host(&fake).await;
            let mut ledger = fixture.ledger(&[11, 12, 13]);
            let done = RowState::Done(DoneReason::Completed);
            let entry = Entry::Transition {
                key: 13,
                state: done,
                attempt: None,
            };
            ledger.append_entry(&entry, &[]).unwrap();
            let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
            assert_eq!(outcome, Outcome::Cleared, "{provider:?}");
            let record = fixture.record().await;
            assert!(record.resolved && !record.superseded);
            let ticket: ClearTicket = serde_json::from_value(record.ticket.unwrap()).unwrap();
            assert_eq!(ticket.context.provider, provider.as_str());
            assert_eq!(ticket.context.execution_nonce, fixture.nonce);
            let cut = ticket.input.unwrap();
            assert_eq!((cut.affected_keys, cut.ledger_seq), (vec![11, 12], 4));
            assert_eq!(
                fixture.states(),
                BTreeMap::from([(11, CUT), (12, CUT), (13, done)])
            );
            let reset = format!("reset:{}", fixture.tmux);
            let selector = format!("clear:{}", fixture.session_key);
            assert_eq!(fake.calls(), vec![selector, reset]);
            assert!(!fixture.marker_present());
            let data = fixture.shared.core.lock().await;
            let session = data.sessions.get(&fixture.channel_id).unwrap();
            assert!(session.cleared && session.session_id.is_none());
            drop(data);
            fixture.drop_db().await;
        }
    });
}

/// Delegates to the adapter; a commit may lose its reply and may take Postgres down after it.
struct Flaky {
    inner: LedgerClear,
    lose_ack: bool,
    drop_on_commit: bool,
    down: Arc<AtomicBool>,
}

impl Flaky {
    fn new(inner: LedgerClear, lose_ack: bool, drop_on_commit: bool) -> Self {
        let down = Arc::new(AtomicBool::new(false));
        Self {
            inner,
            lose_ack,
            drop_on_commit,
            down,
        }
    }
}

impl ClearHost for Flaky {
    fn identity(&self) -> clear::Identity {
        self.inner.identity()
    }
    fn capture(&mut self) -> Option<ClearTicket> {
        self.inner.capture()
    }
    fn record(
        &mut self,
    ) -> Step<'_, anyhow::Result<Option<session_transcripts::NativeClearRecord>>> {
        match self.down.load(Ordering::SeqCst) {
            true => Box::pin(async { Err(anyhow::anyhow!("postgres dropped")) }),
            false => self.inner.record(),
        }
    }
    fn commit<'a>(
        &'a mut self,
        ticket: &'a serde_json::Value,
    ) -> Step<'a, anyhow::Result<NativeClearGeneration>> {
        Box::pin(async move {
            let committed = self.inner.commit(ticket).await;
            if self.drop_on_commit {
                self.down.store(true, Ordering::SeqCst);
            }
            match self.lose_ack {
                true => Err(anyhow::anyhow!("commit reply lost after {committed:?}")),
                false => committed,
            }
        })
    }
    fn execution(&mut self, ticket: &ClearTicket) -> clear::Execution {
        self.inner.execution(ticket)
    }
    fn reset<'a>(&'a mut self, ticket: &'a ClearTicket) -> Step<'a, bool> {
        self.inner.reset(ticket)
    }
    fn resolve(
        &mut self,
        generation: NativeClearGeneration,
    ) -> Step<'_, anyhow::Result<NativeClearResolve>> {
        self.inner.resolve(generation)
    }
    fn notice<'a>(&'a mut self, text: &'a str) -> Step<'a, ()> {
        self.inner.notice(text)
    }
}

#[test]
fn a_lost_commit_reply_is_read_back_from_postgres_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(3, ProviderKind::Claude).await;
        let fake = Arc::new(Fake::default());
        let mut host = Flaky::new(fixture.host(&fake).await, true, false);
        let mut ledger = fixture.ledger(&[11]);
        let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Cleared);
        let record = fixture.record().await;
        assert!(record.resolved && record.generation == NativeClearGeneration(1));
        assert_eq!(fixture.states(), BTreeMap::from([(11, CUT)]));
        assert_eq!(fake.calls().len(), 2);
        fixture.drop_db().await;
    });
}

#[test]
fn postgres_lost_mid_commit_holds_until_the_stored_ticket_replays_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(4, ProviderKind::Codex).await;
        let fake = Arc::new(Fake::default());
        let mut host = Flaky::new(fixture.host(&fake).await, true, true);
        let mut ledger = fixture.ledger(&[11]);
        let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
        assert!(fake.calls().iter().all(|c| !c.starts_with("reset:")));
        assert_eq!(outcome, Outcome::Held(Unresolved::CommitUncertain));
        assert_eq!(fixture.states(), BTreeMap::from([(11, RowState::Received)]));
        assert!(fake.calls().contains(&format!("notice:{PG_RETRY_NOTICE}")));
        assert!(health(fixture.channel_id).is_some_and(|r| r.contains("CommitUncertain")));
        assert!(!fixture.record().await.resolved);
        host.down.store(false, Ordering::SeqCst);
        let outcome = clear::resume(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Cleared);
        assert!(fixture.record().await.resolved);
        assert_eq!(fixture.states(), BTreeMap::from([(11, CUT)]));
        assert!(health(fixture.channel_id).is_none());
        fixture.drop_db().await;
    });
}

#[test]
fn a_superseded_ticket_holds_until_a_new_clear_cuts_its_inputs_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(5, ProviderKind::Claude).await;
        let fake = Arc::new(Fake::default());
        let mut host = fixture.host(&fake).await;
        let mut ledger = fixture.ledger(&[11]);
        // A clear that stopped right after its ticket committed, then an older-style boundary write.
        let mut ticket = host.capture().unwrap();
        ticket.input = Some(InputCutoff {
            ledger_generation: 0,
            ledger_seq: 1,
            affected_keys: vec![11],
        });
        let value = serde_json::to_value(&ticket).unwrap();
        host.commit(&value).await.unwrap();
        let key = fixture.channel_id.get().to_string();
        let tx = session_transcripts::begin_channel_clear_boundary_tx(&fixture.pool)
            .await
            .unwrap();
        session_transcripts::finish_channel_clear_boundary_tx(tx, &key)
            .await
            .unwrap();
        let outcome = clear::resume(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Held(Unresolved::Superseded));
        assert!(fake.calls().iter().all(|c| !c.starts_with("reset:")));
        assert_eq!(fixture.states(), BTreeMap::from([(11, RowState::Received)]));
        let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Cleared);
        assert_eq!(fixture.states(), BTreeMap::from([(11, CUT)]));
        assert!(fixture.record().await.resolved);
        fixture.drop_db().await;
    });
}

#[test]
fn a_wal_failure_keeps_the_postgres_ticket_unresolved_until_resume_pg() {
    // The fault harness is per thread, so the clear runs on this thread.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let fixture = Fixture::new(6, ProviderKind::Claude).await;
        let fake = Arc::new(Fake::default());
        let mut host = fixture.host(&fake).await;
        let mut ledger = fixture.ledger(&[11, 12]);
        let recording = Recording::start(fixture.ledger_root.path());
        recording.arm(Some(3));
        let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
        drop(recording);
        assert!(!fixture.record().await.resolved);
        assert_eq!(outcome, Outcome::Held(Unresolved::WalUncertain));
        assert!(fake.calls().iter().all(|c| !c.starts_with("reset:")));
        let mut ledger =
            Ledger::open(fixture.ledger_root.path(), fixture.channel_id.get()).unwrap();
        let outcome = clear::resume(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Cleared);
        assert!(fixture.record().await.resolved);
        assert_eq!(fixture.states(), BTreeMap::from([(11, CUT), (12, CUT)]));
        fixture.drop_db().await;
    });
}

// A boot replay builds its host with no host-guard clearance; a reset asks for it only when due.
#[test]
fn a_resume_host_admits_no_reset_until_its_ticket_needs_one_pg() {
    runtime().block_on(async {
        for (n, current) in [(8, false), (9, true)] {
            let fixture = Fixture::new(n, ProviderKind::Claude).await;
            let fake = Arc::new(Fake::default());
            let mut ledger = fixture.ledger(&[11, 12]);
            // A clear that stopped right after its ticket committed.
            let mut ticket = fixture.host(&fake).await.capture().unwrap();
            ticket.input = Some(InputCutoff {
                ledger_generation: 0,
                ledger_seq: 2,
                affected_keys: vec![11, 12],
            });
            let value = serde_json::to_value(&ticket).unwrap();
            fixture.host(&fake).await.commit(&value).await.unwrap();
            let for_resume = LedgerClear::for_resume;
            let (http, shared) = (&fixture.http, &fixture.shared);
            let mut host = for_resume(http, shared, &fixture.provider, fixture.channel_id)
                .await
                .unwrap();
            assert!(host.cleared.is_none() && fake.calls().is_empty());
            assert!(fixture.marker_present() && !fixture.record().await.resolved);
            host.effects = fake.clone();
            if !current {
                let marker = tmux_common::session_temp_path(&fixture.tmux, "spawn_nonce");
                std::fs::write(marker, "a-later-execution").unwrap();
            }
            let outcome = clear::resume(&mut ledger, &mut host, fixture.guard().await).await;
            assert_eq!(outcome, Outcome::Cleared, "current={current}");
            assert_eq!(host.cleared.is_some(), current);
            let resets = fake
                .calls()
                .iter()
                .filter(|c| c.starts_with("reset:"))
                .count();
            assert_eq!(resets, usize::from(current));
            assert_eq!(fixture.states(), BTreeMap::from([(11, CUT), (12, CUT)]));
            assert!(fixture.record().await.resolved);
            fixture.drop_db().await;
        }
    });
}

// A protected, closed gate refuses every Legacy population writer and records it as health.
#[test]
fn a_fenced_channel_clears_without_a_legacy_population_write_pg() {
    runtime().block_on(async {
        let fixture = Fixture::new(7, ProviderKind::Codex).await;
        let channel = fixture.channel_id.get();
        let gate = Gate::protect(fixture.provider.clone(), channel).unwrap();
        let _health = fence::test_health::Clear::new(&gate);
        let _closing = gate.close().unwrap();
        let fake = Arc::new(Fake::default());
        let mut host = fixture.host(&fake).await;
        let mut ledger = fixture.ledger(&[11]);
        // The binding test root is per thread, so the clear runs on this one.
        let outcome = clear::run(&mut ledger, &mut host, fixture.guard().await).await;
        assert_eq!(outcome, Outcome::Cleared);
        assert_eq!(fixture.states(), BTreeMap::from([(11, CUT)]));
        assert!(fake.calls().contains(&format!("reset:{}", fixture.tmux)));
        assert!(!fake.scoped.load(Ordering::SeqCst));
        let needle = format!(" channel={channel} ");
        let fenced = || fence::health_reasons().iter().any(|r| r.contains(&needle));
        assert!(!fenced(), "{:?}", fence::health_reasons());
        assert_eq!(gate.mode(), Mode::Closing);
        let lock = fence::population_root().unwrap().join("discord_inflight");
        let lock = lock
            .join(fixture.provider.as_str())
            .join(format!("{channel}.json.lock"));
        assert!(!lock.exists());
        // The fixture sees a Legacy writer: one refused write is reported.
        assert!(fence::write(&fixture.provider, channel, || Ok(())).is_err());
        assert!(fenced());
        fixture.drop_db().await;
    });
}

#[tokio::test]
async fn without_postgres_the_adapter_refuses_with_the_retry_notice() {
    let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    let http = Arc::new(serenity::Http::new(""));
    let effects: Arc<dyn ClearEffects> = Arc::new(Fake::default());
    let channel = serenity::ChannelId::new(6_325_599);
    let admit = LedgerClear::admit_with;
    let refused = admit(
        &http,
        &shared,
        &ProviderKind::Claude,
        channel,
        None,
        effects,
    )
    .await;
    assert_eq!(refused.err().as_deref(), Some(PG_RETRY_NOTICE));
}

// Only an admitted clear withdraws, before the row clear, and a reset that runs later withdraws again.
#[test]
fn b2b1_ledger_clear_withdraws_at_admission_and_again_when_its_reset_runs_pg() {
    use crate::services::discord::turn_presence::entrypoints::tests::Probe;
    runtime().block_on(async {
        let fixture = Fixture::new(10, ProviderKind::Claude).await;
        let probe = Probe::install(&fixture.shared);
        let fake = Arc::new(Fake::default());
        let channel = fixture.channel_id.get();
        let herdr = crate::config::session_hosts::force_for_test(None, &[(channel, "mac-mini")]);
        let kept = probe.arm(channel);
        let effects: Arc<dyn ClearEffects> = fake.clone();
        let admit = LedgerClear::admit_with;
        let (http, shared) = (&fixture.http, &fixture.shared);
        let refused = admit(
            http,
            shared,
            &fixture.provider,
            fixture.channel_id,
            None,
            effects,
        );
        assert!(
            refused.await.is_err(),
            "the host guard keeps a Herdr channel"
        );
        assert!(
            Probe::current(&kept),
            "a refused admission withdraws nothing"
        );
        drop(herdr);
        let admitted = probe.arm(channel);
        let mut host = fixture.host(&fake).await;
        assert!(
            !Probe::current(&admitted),
            "admission withdrew before the row clear"
        );
        let since = probe.arm(fixture.channel_id.get());
        let ticket = host.capture().unwrap();
        assert!(Probe::current(&since), "capturing a ticket changes nothing");
        assert!(host.reset(&ticket).await);
        assert!(!Probe::current(&since), "the reset body withdrew again");
        fixture.drop_db().await;
    });
}

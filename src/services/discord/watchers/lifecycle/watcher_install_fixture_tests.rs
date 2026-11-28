use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::health::test_support::MockDiscord;
use crate::services::discord::inflight::o_seed_observation::{Guard, Snapshot};
use crate::services::discord::tmux::watcher_install_fixture::{list_sessions, pane};
use crate::services::discord::{SharedData, TmuxWatcherHandle};
use crate::services::provider::ProviderKind;
use crate::services::tmux_common as tc;
use crate::services::tui_prompt_dedupe::{self as binding, TuiRuntimeBinding};
use poise::serenity_prelude::{self as serenity, ChannelId};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub(in crate::services::discord) struct InstallFixture {
    pub shared: Arc<SharedData>,
    pub registry: Arc<HealthRegistry>,
    pub http: Arc<serenity::Http>,
    pub provider: ProviderKind,
    pub channel: ChannelId,
    pub session: String,
    pub output: String,
}

impl InstallFixture {
    pub async fn new(provider: ProviderKind, channel: u64) -> Self {
        Self::new_with_pool(provider, channel, None).await
    }
    pub(in crate::services::discord) async fn new_with_pool(
        provider: ProviderKind,
        channel: u64,
        pool: Option<sqlx::PgPool>,
    ) -> Self {
        let channel = ChannelId::new(channel);
        let registry = Arc::new(HealthRegistry::new());
        let mut shared = crate::services::discord::make_shared_data_for_tests();
        Arc::get_mut(&mut shared).unwrap().health_registry = (&registry).into();
        Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
        Arc::get_mut(&mut shared).unwrap().pg_pool = pool;
        {
            let mut settings = shared.settings.write().await;
            settings.provider = provider.clone();
            settings.allowed_channel_ids = vec![channel.get()];
        }
        registry
            .register(provider.as_str().to_owned(), shared.clone())
            .await;
        let session = provider.build_tmux_session_name(&format!(
            "wfx-{}-{}",
            channel.get(),
            if provider == ProviderKind::Codex {
                "cdx"
            } else {
                "cc"
            }
        ));
        let output = tc::session_temp_path(&session, "jsonl");
        std::fs::create_dir_all(Path::new(&output).parent().unwrap()).unwrap();
        std::fs::write(&output, []).unwrap();
        std::fs::write(tc::session_temp_path(&session, "generation"), "wfx-source").unwrap();
        std::fs::write(
            tc::tmux_owner_path(&session),
            tc::current_tmux_owner_marker(),
        )
        .unwrap();
        tc::write_tmux_channel_binding(&session, channel.get()).unwrap();
        tc::write_tmux_runtime_kind_marker(
            &session,
            match provider {
                ProviderKind::Codex => RuntimeHandoffKind::CodexTui,
                ProviderKind::Claude => RuntimeHandoffKind::ClaudeTui,
                _ => panic!("fixture requires TUI provider"),
            },
        )
        .unwrap();
        Self {
            shared,
            registry,
            http: Arc::new(serenity::Http::new("wfx-fixture")),
            provider,
            channel,
            session,
            output,
        }
    }

    pub fn handle(&self) -> TmuxWatcherHandle {
        let handle = self.shared.tmux_watchers.get(&self.channel).unwrap();
        TmuxWatcherHandle {
            tmux_session_name: handle.tmux_session_name.clone(),
            output_path: handle.output_path.clone(),
            paused: handle.paused.clone(),
            resume_offset: handle.resume_offset.clone(),
            cancel: handle.cancel.clone(),
            pause_epoch: handle.pause_epoch.clone(),
            turn_delivered: handle.turn_delivered.clone(),
            last_heartbeat_ts_ms: handle.last_heartbeat_ts_ms.clone(),
        }
    }
}

pub(in crate::services::discord) async fn fixture(
    provider: ProviderKind,
    channel: u64,
) -> (InstallFixture, MockDiscord) {
    fixture_with_pool(provider, channel, None).await
}

pub(in crate::services::discord) async fn fixture_with_pool(
    provider: ProviderKind,
    channel: u64,
    pool: Option<sqlx::PgPool>,
) -> (InstallFixture, MockDiscord) {
    pane("busy");
    let mut fixture = InstallFixture::new_with_pool(provider.clone(), channel, pool).await;
    let discord = MockDiscord::start().await;
    fixture.http = discord.http.clone();
    fixture
        .registry
        .register_http(provider.as_str().to_owned(), fixture.http.clone())
        .await;
    binding::register_tmux_runtime_binding(
        &fixture.session,
        TuiRuntimeBinding {
            runtime_kind: if provider == ProviderKind::Codex {
                RuntimeHandoffKind::CodexTui
            } else {
                RuntimeHandoffKind::ClaudeTui
            },
            output_path: fixture.output.clone(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some("e2e".into()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&fixture.session, "host_kind"),
        "tmux",
    )
    .unwrap();
    list_sessions(&[&fixture.session]);
    (fixture, discord)
}

pub(in crate::services::discord) fn observe(fixture: &InstallFixture) -> Guard {
    let observed = Guard::new(&fixture.provider, fixture.channel.get());
    observed.register_source(Path::new(&fixture.output));
    observed
}

pub(in crate::services::discord) async fn until(
    observed: &Guard,
    what: &str,
    done: impl Fn(&Snapshot) -> bool,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(12);
    loop {
        let snapshot = observed.snapshot();
        if done(&snapshot) {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "{what}: key={snapshot:?} raw={:?}",
            observed.raw_process_snapshot()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

//! Daemon code reads config through `config::snapshot`; only CLI, boot and file-writing
//! functions read the config file themselves.

use crate::test_support::production_scan::{Owner, scan, unowned_matches};
use regex::Regex;
use std::sync::LazyLock;

const FILE_READ: &str = r"\b(?:load_graceful|load_from_path|load_config_file|load_runtime_config_file)\s*\(|\bconfig::load\s*\(\s*\)";

static FILE_READ_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(FILE_READ).unwrap());

/// Production functions that may read the config file, and why.
const FILE_READERS: &[Owner] = &[
    (
        "src/config.rs",
        "pub fn load_from_path(",
        "the strict loader",
    ),
    (
        "src/config.rs",
        "pub fn load_graceful(",
        "the graceful loader",
    ),
    (
        "src/config.rs",
        "pub fn snapshot(",
        "falls back to the file without a live snapshot",
    ),
    (
        "src/config/config_path.rs",
        "",
        "the resolver and its loaders",
    ),
    (
        "src/config_live_reload.rs",
        "pub fn reload_from_path(",
        "validates an edit before it is swapped in",
    ),
    ("src/bootstrap.rs", "pub(crate) fn initialize(", "boot"),
    (
        "src/services/discord_config_audit.rs",
        "pub(crate) fn load_runtime_config(",
        "boot and CLI loader",
    ),
    (
        "src/cli/dcserver.rs",
        "pub fn handle_dcserver(",
        "boot; body pinned by check_legacy_supervision_census",
    ),
    (
        "src/services/discord/runtime_bootstrap.rs",
        "pub(crate) async fn run_bot(",
        "bot boot",
    ),
    (
        "src/services/discord/runtime_bootstrap/framework_setup.rs",
        "pub(super) async fn run_bot_framework_setup(",
        "bot boot",
    ),
    (
        "src/services/discord/runtime_bootstrap/shared_data.rs",
        "pub(super) fn run_bot_build_shared_data(",
        "bot boot",
    ),
    (
        "src/services/discord/runtime_bootstrap/voice.rs",
        "pub(super) async fn run_bot_spawn_voice_auto_join(",
        "bot boot",
    ),
    ("src/cli/channel_provider.rs", "fn from_path(", "CLI"),
    ("src/cli/client.rs", "pub fn api_base(", "CLI"),
    ("src/cli/client.rs", "fn auth_token(", "CLI"),
    (
        "src/cli/direct.rs",
        "pub(crate) async fn cmd_release_migrate_postgres(",
        "CLI",
    ),
    ("src/cli/direct.rs", "async fn build_app_state(", "CLI"),
    ("src/cli/herdr.rs", "pub(super) fn run(", "CLI"),
    ("src/cli/init.rs", "fn load_init_config_defaults(", "CLI"),
    (
        "src/cli/intake_outbox.rs",
        "pub(crate) async fn cmd_dispatched_audit(",
        "CLI",
    ),
    (
        "src/cli/intake_outbox.rs",
        "pub(crate) async fn cmd_settle(",
        "CLI",
    ),
    ("src/cli/monitoring.rs", "fn auth_token(", "CLI"),
    (
        "src/cli/doctor/orchestrator/managed_herdr_config.rs",
        "fn hosts(",
        "what a restart would install, when no boot snapshot exists",
    ),
    (
        "src/cli/init.rs",
        "fn write_agentdesk_discord_config(",
        "rewrites the file",
    ),
    (
        "src/cli/migrate/apply.rs",
        "pub(super) fn apply_import_plan(",
        "rewrites the file",
    ),
    (
        "src/runtime_layout/config_merge.rs",
        "pub(super) fn merge_role_map_into_agentdesk_yaml(",
        "rewrites the file",
    ),
    (
        "src/services/discord/settings/write.rs",
        "fn save_runtime_bot_settings_checked(",
        "rewrites the file",
    ),
    (
        "src/services/discord/agentdesk_config.rs",
        "pub(crate) fn load_agent_setup_config(",
        "agent setup rewrites the file",
    ),
    (
        "src/server/routes/voice_config.rs",
        "fn load_editable_config(",
        "the voice config API rewrites the file",
    ),
    (
        "src/services/onboarding/mod.rs",
        "fn write_agentdesk_discord_config(",
        "onboarding rewrites the file",
    ),
    (
        "src/services/onboarding/mod.rs",
        "pub(super) fn load_onboarding_config(",
        "onboarding rewrites the file",
    ),
    (
        "src/services/onboarding/mod.rs",
        "fn verify_onboarding_settings_artifacts(",
        "checks the file onboarding wrote",
    ),
    (
        "src/services/onboarding/mod.rs",
        "fn verify_onboarding_pipeline_artifact(",
        "checks the file onboarding wrote",
    ),
    (
        "src/services/discord/router/herdr_consent/return_input.rs",
        "fn rollback_target(",
        "compares the file the next boot reads with this boot's candidate",
    ),
    (
        "src/services/cluster/intake_routing_config.rs",
        "pub(crate) fn effective_intake_routing_config(",
        "reads the snapshot; without one it must tell a broken file from an absent one",
    ),
];

/// Daemon reads that still go to the file. This list only shrinks.
const NOT_YET_SNAPSHOT: &[Owner] = &[
    (
        "src/cli/doctor/orchestrator.rs",
        "pub(crate) fn run_doctor_report(",
        "also runs in the daemon at startup",
    ),
    (
        "src/cli/doctor/orchestrator/health_snapshot.rs",
        "pub(super) fn fetch_health_snapshot(",
        "also runs in the daemon at startup",
    ),
    (
        "src/services/discord/agentdesk_config.rs",
        "fn load_agentdesk_config_with_path(",
        "role bindings and meetings",
    ),
    (
        "src/services/git/repo_resolver.rs",
        "fn load_repo_resolution_config(",
        "repo resolution",
    ),
    (
        "src/services/mcp_config.rs",
        "fn load_runtime_config(",
        "MCP config per provider spawn",
    ),
    (
        "src/services/provider_cli/orchestration.rs",
        "pub fn configured_provider_agents(",
        "provider CLI agents",
    ),
    (
        "src/runtime_layout/mod.rs",
        "fn load_memory_backend_from_yaml(",
        "memory backend per call",
    ),
    (
        "src/services/discord/turn_bridge/completion_guard/completion_postgres.rs",
        "fn with_runtime_postgres_result<",
        "held path",
    ),
    (
        "src/services/discord/turn_bridge/completion_guard/completion_postgres.rs",
        "async fn runtime_max_entry_retries_pg(",
        "held path",
    ),
    (
        "src/services/discord/turn_bridge/recovery_text.rs",
        "fn take_kv_meta_value_runtime_pg(",
        "held path",
    ),
];

#[test]
fn daemon_config_reads_go_through_the_snapshot() {
    let owners: Vec<Owner> = FILE_READERS
        .iter()
        .chain(NOT_YET_SNAPSHOT)
        .copied()
        .collect();
    let (reads, stale) = unowned_matches(&FILE_READ_RE, &owners);
    assert!(
        reads.is_empty(),
        "read config through crate::config::snapshot() instead: {reads:?}"
    );
    assert!(stale.is_empty(), "drop these owners: {stale:?}");
}

#[test]
fn file_read_scan_finds_each_loader_call_but_not_snapshot() {
    let lines = |text: &str| scan("src/x.rs", text, &FILE_READ_RE, &[]).unwrap().lines;
    let text = "fn a() { crate::config::load_graceful(); }\n\
        fn b() { config::load().unwrap(); }\n\
        fn c(p: &Path) { load_from_path(p); }\n\
        fn d() { config_path::load_runtime_config_file(); }\n\
        fn e(p: &Path) { load_config_file(p); }\n\
        fn f() { crate::config::snapshot(); settings::load(); }\n\
        // config::load_graceful() in a comment\n";
    assert_eq!(lines(text), vec![1, 2, 3, 4, 5]);

    // An array type in an owner's signature does not end it; a declaration has no body.
    let owners = [("src/x.rs", "fn digest(", "reason")];
    let owned = "fn digest() -> Result<([u8; 32], String), String> { load_graceful(); }\n\
        fn other() { load_graceful(); }\n";
    let found = scan("src/x.rs", owned, &FILE_READ_RE, &owners).unwrap();
    assert_eq!(
        (found.lines, found.owners_used),
        (vec![2], vec!["fn digest("])
    );
    let declared = "fn digest(seed: [u8; 4]);\n";
    assert!(scan("src/x.rs", declared, &FILE_READ_RE, &owners).is_err());
}

fn live_guard() -> crate::config_live_reload::TestLiveGuard {
    crate::config_live_reload::test_live_guard(
        crate::config::test_env_lock::acquire_shared_test_env_lock(),
    )
}

/// A config file in `dir` whose data dir stays inside it.
fn config_in(dir: &std::path::Path) -> (std::path::PathBuf, crate::config::Config) {
    let mut config = crate::config::Config::default();
    config.data.dir = dir.join("data");
    (dir.join("agentdesk.yaml"), config)
}

#[test]
fn snapshot_reads_the_file_only_until_a_live_config_is_installed() {
    let guard = live_guard();
    guard.clear();
    let dir = tempfile::tempdir().unwrap();
    let (path, mut config) = config_in(dir.path());
    config.server.port = 8711;
    crate::config::save_to_path(&path, &config).unwrap();
    let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    assert_eq!(crate::config::snapshot().server.port, 8711);

    config.server.port = 8712;
    crate::config_live_reload::install(config);
    assert_eq!(crate::config::snapshot().server.port, 8712);
}

#[test]
fn an_edit_the_watcher_rejected_does_not_reach_a_daemon_token_check() {
    let _guard = live_guard();
    let dir = tempfile::tempdir().unwrap();
    let (path, mut config) = config_in(dir.path());
    config.server.auth_token = Some("live-token".to_string());
    crate::config::save_to_path(&path, &config).unwrap();
    crate::config_live_reload::install(crate::config::load_from_path(&path).unwrap());
    let _path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );

    // The edit parses but fails validation, so the watcher keeps the running config.
    config.server.auth_token = Some("edited-token".to_string());
    config.discord.scheduled_message_required_mention_user_ids = vec!["not-a-user-id".into()];
    crate::config::save_to_path(&path, &config).unwrap();
    assert!(matches!(
        crate::config_live_reload::reload_from_path(&path),
        crate::config_live_reload::ReloadOutcome::Rejected { .. }
    ));

    let mut headers = axum::http::HeaderMap::new();
    headers.insert("authorization", "Bearer live-token".parse().unwrap());
    assert!(
        crate::services::kanban_cards::require_explicit_bearer_token(&headers, "test").is_ok(),
        "the running token still authorizes"
    );
}

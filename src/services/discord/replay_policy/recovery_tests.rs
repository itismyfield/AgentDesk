use super::*;
use crate::db::replay_disposition::{
    hold_notice::NoticeRouting,
    write::{self, BeginFrom, CanonicalInput, EffectProjection},
};
use crate::services::{
    discord::{inflight, recovery_engine::o_cut_recorder},
    provider::ProviderKind,
};
use sqlx::PgPool;

type StoredAttempt = (
    i64,
    String,
    Option<String>,
    Option<String>,
    Option<serde_json::Value>,
);

const ROUTING: NoticeRouting<'static> = NoticeRouting {
    bot: "notify",
    session_key: None,
};

fn state(n: u64) -> inflight::InflightTurnState {
    let mut s = inflight::InflightTurnState::new(
        ProviderKind::Codex,
        7_330_600 + n * 10,
        None,
        7,
        7_331_600 + n * 10,
        7_332_600 + n * 10,
        "original recovery input".into(),
        None,
        Some(format!("p5-recovery-{n}")),
        None,
        None,
        0,
    );
    s.full_response = format!("captured local restart {n}");
    s
}

async fn begin(
    pool: &PgPool,
    s: &mut inflight::InflightTurnState,
    target: &str,
    owner: &str,
) -> write::ExactAttempt {
    let input = CanonicalInput {
        request_key: format!("p5-recovery:{}", s.channel_id),
        provider: s.provider.clone(),
        channel: s.channel_id.to_string(),
        sources: vec![s.user_msg_id.to_string()],
        original_text: s.user_text.clone(),
        owner_id: s.request_owner_user_id.to_string(),
        agent_id: String::new(),
        attachments: serde_json::json!([]),
        reply_context: None,
        provenance: "new_input",
    };
    let r = write::register_or_reuse(pool, &input, target)
        .await
        .unwrap();
    let attempt = write::begin(
        pool,
        r.id,
        BeginFrom::Registered {
            nonce: r.episode_nonce.as_deref().unwrap(),
        },
        &EffectProjection {
            effect_target: "provider_start",
            input_hash: "prepared",
            binding: &serde_json::json!({"turn_nonce":s.turn_nonce,"user_msg_id":s.user_msg_id}),
        },
        owner,
    )
    .await
    .unwrap();
    s.replay_receipt_id = Some(r.id);
    attempt.into_parts().0
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

fn native_probe(root: &std::path::Path) {
    let config = root.join("native-fixture.yaml");
    std::fs::write(&config, "server: {}\ndata: {dir: data}\nagents: []\n").unwrap();
    let _config = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &config,
    );
    let cli = root.join("native-codex");
    std::fs::write(&cli,format!("#!/bin/sh\ncase \"$1\" in --version|-v) echo 99.1.0; exit 0;; esac\nprintf 'start\\n' >> '{}'\ncat >/dev/null\nprintf '%s\\n' '{{\"type\":\"turn.completed\",\"usage\":{{}}}}'\n",root.join("native-starts").display())).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _cli = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CODEX_PATH",
        &cli,
    );
    let (tx, rx) = std::sync::mpsc::channel();
    crate::services::codex::execute_command_streaming(
        "probe",
        Some("old-native"),
        root.to_str().unwrap(),
        tx,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        false,
    )
    .unwrap();
    assert!(rx.try_iter().any(|v| matches!(
        v,
        crate::services::agent_protocol::StreamMessage::Done { .. }
    )));
}

#[test]
fn p5_actual_own_restart_and_foreign_orphan_preserve_the_authority_boundary_pg() {
    const SETUP: &str = "ADK_TEST_P5_RESTART_WRITER";
    if let Ok(raw) = std::env::var(SETUP) {
        let (url, root, target): (String, std::path::PathBuf, String) =
            serde_json::from_str(&raw).unwrap();
        let _env = crate::config::set_agentdesk_root_for_test(&root);
        let _lock = crate::cli::dcserver::replay_recovery::acquire_for_tests(&root).unwrap();
        runtime().block_on(async {
            let pool = PgPool::connect(&url).await.unwrap();
            let mut saved = state(0);
            begin(&pool, &mut saved, &target, super::super::incarnation()).await;
            native_probe(&root);
            inflight::save_inflight_state_create_new(&saved).unwrap();
            let mut gap = state(1);
            begin(&pool, &mut gap, &target, super::super::incarnation()).await;
            let mut no_effect = state(2);
            let attempt = begin(&pool, &mut no_effect, &target, super::super::incarnation()).await;
            let mut seen = super::super::live::RawObservation::for_attempt(&attempt);
            for fact in [
                super::super::live::Seen::StdoutEof,
                super::super::live::Seen::Exited { waited: true },
                super::super::live::Seen::StderrDrained,
            ] {
                seen.record(fact);
            }
            write::classify_no_effect(&pool, attempt, &seen.no_effect_evidence().unwrap())
                .await
                .unwrap();
            inflight::save_inflight_state_create_new(&no_effect).unwrap();
            pool.close().await;
        });
        println!("P5_OLD_WRITER_FINISHED_WITH_ONE_SNAPSHOT_AND_ONE_GAP");
        return;
    }
    let _env = crate::config::TestRuntimeRootGuard::new();
    let root = runtime_store::agentdesk_root().unwrap();
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    runtime().block_on(async {
        let db=crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool=db.connect_and_migrate().await;
        let target=crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        let name=concat!(module_path!(),"::p5_actual_own_restart_and_foreign_orphan_preserve_the_authority_boundary_pg").strip_prefix("agentdesk::").unwrap();
        let old=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact",name,"--nocapture"])
            .env(SETUP,serde_json::to_string(&(&db.database_url,&root,&target)).unwrap()).env("AGENTDESK_INSTANCE_ID",&target).output().unwrap();
        assert!(old.status.success(),"{} {}",String::from_utf8_lossy(&old.stdout),String::from_utf8_lossy(&old.stderr));
        assert!(String::from_utf8_lossy(&old.stdout).contains("P5_OLD_WRITER_FINISHED"));
        let before:Vec<StoredAttempt>=sqlx::query_as("SELECT id,replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(before.len(),3);
        // A different root has no snapshot and cannot settle the terminated foreign owner.
        {
            let other=tempfile::tempdir().unwrap();let _other_env=crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", other.path());
            let shared=crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
            let sink=o_cut_recorder::start(state(0).channel_id).await;
            crate::services::discord::recovery_engine::restore_inflight_turns(&sink.http,&shared,&ProviderKind::Codex).await;
            let after:Vec<StoredAttempt>=sqlx::query_as("SELECT id,replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox ORDER BY id").fetch_all(&pool).await.unwrap();
            assert_eq!(after,before);assert!(sink.calls().is_empty());
        }
        let lock=crate::cli::dcserver::replay_recovery::acquire_for_tests(&root).unwrap();
        let shared=crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.restart.replay_restart.set(lock).unwrap();
        let sink=o_cut_recorder::start(state(0).channel_id).await;
        crate::services::discord::recovery_engine::restore_inflight_turns(&sink.http,&shared,&ProviderKind::Codex).await;
        let rows:Vec<StoredAttempt>=sqlx::query_as("SELECT id,replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(rows[0].1,"withheld");assert_eq!(rows[0].2,before[0].2);assert_eq!(rows[0].3,before[0].3);
        assert_eq!(rows[1],before[1],"begin/save gap has no local proof");
        assert_eq!(rows[2].1,"withheld");assert_eq!(rows[2].2,before[2].2);assert_eq!(rows[2].3,before[2].3);
        let notices:i64=sqlx::query_scalar("SELECT count(*) FROM message_outbox WHERE reason_code='stale_replay_withheld'").fetch_one(&pool).await.unwrap();
        assert_eq!(notices,2);
        assert_eq!(std::fs::read_to_string(root.join("native-starts")).unwrap().lines().count(),1,"recovery never invokes the native provider again");
        assert_eq!(shared.tmux_watchers.len(), 0);
        println!("P5_OWN_RESTART held=2 notice=2 gap_unchanged=1 native=1");
        pool.close().await;db.drop().await;
    });
}

#[tokio::test]
async fn p5_db_locked_recovery_rejects_each_unproved_local_identity_pg() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let root = runtime_store::agentdesk_root().unwrap();
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    let lock = crate::cli::dcserver::replay_recovery::acquire_for_tests(&root).unwrap();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let target = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
    for (n, case) in [
        "turn",
        "projection",
        "target",
        "current",
        "null-owner",
        "message",
        "future",
        "no-lock",
        "other-root",
    ]
    .into_iter()
    .enumerate()
    {
        let mut s = state(10 + n as u64);
        let owner = if case == "current" {
            super::super::incarnation()
        } else {
            "previous-process"
        };
        if matches!(case, "projection" | "null-owner") {
            let preserved = serde_json::json!({"old":true,"projection":{"nonce":if case=="projection"{"wrong"}else{"episode"},"binding":{"turn_nonce":s.turn_nonce}}});
            let id: i64=sqlx::query_scalar("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_episode_nonce,replay_owner_incarnation,replay_preserved) VALUES($1,$1,$2,$3,'7',$4,'standard','','codex','unknown',TRUE,'started_unclassified',$5,'episode',$6,$7) RETURNING id")
                .bind(&target).bind(s.channel_id.to_string()).bind(s.user_msg_id.to_string()).bind(&s.user_text).bind(vec![s.user_msg_id.to_string()])
                .bind(if case=="null-owner"{None}else{Some(owner)}).bind(preserved).fetch_one(&pool).await.unwrap();
            s.replay_receipt_id = Some(id);
        } else {
            begin(
                &pool,
                &mut s,
                if case == "target" {
                    "foreign-node"
                } else {
                    &target
                },
                owner,
            )
            .await;
        }
        if case == "turn" {
            s.turn_nonce = Some("wrong-local-turn".into());
        }
        if case == "message" {
            s.user_msg_id += 1;
        }
        if case == "future" {
            s.version = u32::MAX;
        }
        inflight::save_inflight_state_create_new(&s).unwrap();
        let proof = LocalSnapshot::load(if case == "no-lock" { None } else { Some(&lock) }, &s);
        let before:(String,Option<String>,Option<String>,Option<serde_json::Value>)=sqlx::query_as("SELECT replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox WHERE id=$1").bind(s.replay_receipt_id).fetch_one(&pool).await.unwrap();
        let result = if case == "other-root" {
            let other = tempfile::tempdir().unwrap();
            std::fs::create_dir(other.path().join("runtime")).unwrap();
            let other_lock =
                crate::cli::dcserver::replay_recovery::acquire_for_tests(other.path()).unwrap();
            assert!(LocalSnapshot::load(Some(&other_lock), &s).is_none());
            write::recover_hold_notice(
                &pool,
                s.replay_receipt_id.unwrap(),
                &s.provider,
                &s.channel_id.to_string(),
                None,
                None,
                ROUTING,
            )
            .await
        } else {
            write::recover_hold_notice(
                &pool,
                s.replay_receipt_id.unwrap(),
                &s.provider,
                &s.channel_id.to_string(),
                proof.as_ref(),
                None,
                ROUTING,
            )
            .await
        };
        assert!(
            matches!(result, Err(write::WriteError::CasMiss)),
            "{case}: {result:?}"
        );
        let after:(String,Option<String>,Option<String>,Option<serde_json::Value>)=sqlx::query_as("SELECT replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox WHERE id=$1").bind(s.replay_receipt_id).fetch_one(&pool).await.unwrap();
        assert_eq!(after, before, "{case}");
    }
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let sink = o_cut_recorder::start(state(10).channel_id).await;
    let raw: Vec<_> = inflight::load_inflight_states(&ProviderKind::Codex)
        .into_iter()
        .map(|s| {
            let path = inflight::inflight_state_path(
                &inflight::inflight_runtime_root().unwrap(),
                &ProviderKind::Codex,
                s.channel_id,
            );
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
    crate::services::discord::recovery_engine::restore_inflight_turns(
        &sink.http,
        &shared,
        &ProviderKind::Codex,
    )
    .await;
    assert!(
        sink.calls().is_empty(),
        "unproved resume cannot enter captured delivery"
    );
    for (path, bytes) in raw {
        assert_eq!(
            std::fs::read(path).unwrap(),
            bytes,
            "unproved resume cannot save or clean up the original snapshot"
        );
    }
    let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(notices, 0);
    assert!(shared.core.lock().await.sessions.is_empty());
    assert_eq!(shared.tmux_watchers.len(), 0);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn p5_null_unfinished_identity_and_withheld_only_pagination_pg() {
    let _env = crate::config::TestRuntimeRootGuard::new();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for n in 0..72_u64 {
        let disposition = match n {
            70 => "started_unclassified",
            71 => "startup_failed_no_effect",
            _ => "withheld",
        };
        sqlx::query("INSERT INTO intake_outbox(target_instance_id,forwarded_by_instance_id,channel_id,user_msg_id,request_owner_id,user_text,turn_kind,agent_id,provider,status,replay_only,replay_disposition,replay_source_message_ids,replay_preserved) VALUES('old','old',$1,$2,'7','old raw bytes','standard','','codex','unknown',TRUE,$3,$4,'{\"old\":true}'::jsonb)")
            .bind((7_330_900+n).to_string()).bind((7_331_900+n).to_string()).bind(disposition).bind(vec![(7_331_900+n).to_string()]).execute(&pool).await.unwrap();
    }
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let sink = o_cut_recorder::start(7_330_900).await;
    for _ in 0..2 {
        crate::services::discord::recovery_engine::restore_inflight_turns(
            &sink.http,
            &shared,
            &ProviderKind::Codex,
        )
        .await;
    }
    let counts:(i64,i64)=sqlx::query_as("SELECT count(*),count(DISTINCT replay_preserved #>> '{stale_replay_notice,outbox_id}') FROM intake_outbox WHERE replay_disposition='withheld'").fetch_one(&pool).await.unwrap();
    assert_eq!(counts, (70, 70));
    let unproved:i64=sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE replay_disposition IN ('started_unclassified','startup_failed_no_effect') AND replay_owner_incarnation IS NULL AND replay_episode_nonce IS NULL AND replay_preserved='{\"old\":true}'::jsonb").fetch_one(&pool).await.unwrap();
    assert_eq!(unproved, 2);
    assert!(sink.calls().is_empty());
    pool.close().await;
    db.drop().await;
}

#[test]
fn p5_live_no_effect_handoff_survives_local_and_foreign_recovery_pg() {
    const PEER: &str = "ADK_TEST_P5_NO_EFFECT_PEER";
    if let Ok(url) = std::env::var(PEER) {
        let _root = crate::config::TestRuntimeRootGuard::new();
        runtime().block_on(async {
            let pool = PgPool::connect(&url).await.unwrap();
            let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
                pool.clone(),
            ));
            let sink = o_cut_recorder::start(state(30).channel_id).await;
            crate::services::discord::recovery_engine::restore_inflight_turns(
                &sink.http,
                &shared,
                &ProviderKind::Codex,
            )
            .await;
            assert!(sink.calls().is_empty());
            pool.close().await;
        });
        println!("NO_EFFECT_PEER_PRESERVED");
        return;
    }
    let _env = crate::config::TestRuntimeRootGuard::new();
    let root = runtime_store::agentdesk_root().unwrap();
    std::fs::create_dir_all(root.join("runtime")).unwrap();
    let lock = crate::cli::dcserver::replay_recovery::acquire_for_tests(&root).unwrap();
    runtime().block_on(async {
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let target = crate::services::cluster::node_registry::resolve_self_instance_id_without_config();
        let mut s = state(30);
        let attempt = begin(&pool, &mut s, &target, super::super::incarnation()).await;
        inflight::save_inflight_state_create_new(&s).unwrap();
        let mut observation = super::super::live::RawObservation::for_attempt(&attempt);
        for seen in [super::super::live::Seen::StdoutEof, super::super::live::Seen::Exited { waited: true }, super::super::live::Seen::StderrDrained] {
            observation.record(seen);
        }
        let super::super::ReplayDecision::AllowStartupRetry(retry) = super::super::decide_stale(&pool, Some(attempt), &observation, None).await else { panic!("no-effect commit must issue one exact continuation"); };
        let before: (String, String, String, serde_json::Value) = sqlx::query_as("SELECT replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox WHERE id=$1").bind(s.replay_receipt_id).fetch_one(&pool).await.unwrap();
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        shared.restart.replay_restart.set(lock).unwrap();
        let sink = o_cut_recorder::start(s.channel_id).await;
        crate::services::discord::recovery_engine::restore_inflight_turns(&sink.http, &shared, &ProviderKind::Codex).await;
        let name = concat!(module_path!(), "::p5_live_no_effect_handoff_survives_local_and_foreign_recovery_pg").strip_prefix("agentdesk::").unwrap();
        let peer = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", name, "--nocapture"]).env(PEER, &db.database_url).env("AGENTDESK_STALE_REPLAY_V1", "0").output().unwrap();
        assert!(peer.status.success(), "{} {}", String::from_utf8_lossy(&peer.stdout), String::from_utf8_lossy(&peer.stderr));
        assert!(String::from_utf8_lossy(&peer.stdout).contains("NO_EFFECT_PEER_PRESERVED"));
        let after: (String, String, String, serde_json::Value) = sqlx::query_as("SELECT replay_disposition,replay_episode_nonce,replay_owner_incarnation,replay_preserved FROM intake_outbox WHERE id=$1").bind(s.replay_receipt_id).fetch_one(&pool).await.unwrap();
        assert_eq!(after, before);
        assert!(sink.calls().is_empty());
        sqlx::query("INSERT INTO sessions(session_key,channel_id,provider,status,current_replay_receipt_id,replay_episode_nonce,claude_session_id) VALUES('p5-no-effect',$1,'codex','turn_active',$2,$3,'stale')").bind(s.channel_id.to_string()).bind(s.replay_receipt_id).bind(&before.1).execute(&pool).await.unwrap();
        let binding = serde_json::json!({"turn_nonce":s.turn_nonce,"user_msg_id":s.user_msg_id});
        let from = || BeginFrom::NoEffect { previous: retry.previous_nonce(), next: retry.next_nonce(), session_key: "p5-no-effect" };
        let effect = EffectProjection { effect_target: "provider_start", input_hash: "prepared", binding: &binding };
        write::begin(&pool, s.replay_receipt_id.unwrap(), from(), &effect, super::super::incarnation()).await.unwrap();
        native_probe(&root);
        assert!(write::begin(&pool, s.replay_receipt_id.unwrap(), from(), &effect, super::super::incarnation()).await.is_err());
        assert_eq!(std::fs::read_to_string(root.join("native-starts")).unwrap().lines().count(), 1);
        let notices: i64 = sqlx::query_scalar("SELECT count(*) FROM message_outbox").fetch_one(&pool).await.unwrap();
        assert_eq!(notices, 0);
        pool.close().await;
        db.drop().await;
    });
}

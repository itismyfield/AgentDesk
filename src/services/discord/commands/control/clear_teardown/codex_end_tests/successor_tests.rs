use super::*;
use std::os::unix::fs::PermissionsExt;

use crate::services::codex_tui::execution_end::producer::LaunchObservation;
use crate::services::tui_prompt_dedupe::binding_context::SpawnNonceMarker;

#[path = "reader_retirement_tests.rs"]
mod reader_retirement_tests;

fn isolated(test: &str) -> bool {
    const CHILD: &str = "ADK_CODEX_SUCCESSOR_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let _spawn_environment = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test.split_once("::").unwrap().1, "--nocapture"])
        .env(CHILD, "1")
        .env("AGENTDESK_ROOT_DIR", root.path())
        .env("AGENTDESK_CONFIG", root.path().join("missing-config.yaml"))
        .env("AGENTDESK_CODEX_DIRECT_TUI_SOURCE_MODE", "shadow")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        result.status.success()
            && stdout.lines().any(|line| line
                .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
        "{test}: {}\n{stdout}\n{stderr}",
        result.status
    );
    eprintln!("{stdout}\n{stderr}");
    false
}

fn successor(pane: &Pane) -> Option<serde_json::Value> {
    let records = pane.records();
    assert_eq!(records.len(), 1, "one observed old execution");
    records[0]
        .get("successor")
        .filter(|s| !s.is_null())
        .cloned()
}

fn fresh(pane: &Pane, prompt: Option<&str>) -> (BindingContext, PathBuf, String) {
    let mut context = pane.context.clone();
    context.created_at = chrono::Utc::now();
    context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
    context.launch_mode = "fresh".into();
    context.expected_native_session_id = None;
    context.first_prompt_digest =
        prompt.map(|prompt| format!("sha256:{:x}", Sha256::digest(prompt.as_bytes())));
    let session = uuid::Uuid::new_v4().to_string();
    let path = context
        .provider_root
        .as_ref()
        .unwrap()
        .join(format!("rollout-{session}.jsonl"));
    let header = serde_json::json!({"type":"session_meta","payload":{
        "id":session,"source":"cli","cwd":canonical_tmp(),
        "timestamp":context.created_at.to_rfc3339()
    }});
    std::fs::write(&path, format!("{header}\n")).unwrap();
    (context, path, session)
}

fn spawn(pane: &Pane, context: &BindingContext, observation: &mut LaunchObservation) {
    observation.prepare(context);
    PreparedIncarnation::create(context.clone()).unwrap();
    tc::write_tmux_owner_marker(&pane.name).unwrap();
    std::fs::write(
        tc::session_temp_path(&pane.name, "spawn_nonce"),
        &context.execution_nonce,
    )
    .unwrap();
    observation.spawned(context);
}

fn publish(pane: &Pane, path: &Path, session: &str, restored: bool) {
    if restored {
        assert!(dedupe::register_rehydrated_tmux_runtime_binding(
            "codex",
            &pane.name,
            pane.channel,
            binding(path, session)
        ));
    } else {
        dedupe::register_launched_tmux_runtime_binding(&pane.name, binding(path, session));
    }
}

fn cold_pane(dir: &Path) -> Pane {
    let canonical = dir.canonicalize().unwrap();
    let dir = canonical.as_path();
    let id = uuid::Uuid::new_v4();
    let channel = 1_479_671_305_348_000_000 + u64::from(id.as_fields().0);
    let label = format!("a1-{}", id.simple());
    let name = ProviderKind::Codex.build_tmux_session_name(&label);
    let fake = FakeTmux::install(&name);
    fake.become_missing_after_kill();
    let process = FixtureProcess::start(0);
    fake.run_pane(process.pid);
    tc::write_tmux_owner_marker(&name).unwrap();
    let provider_root = dir.canonicalize().unwrap();
    let context = BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: name.clone(),
        channel_id: Some(channel),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: binding_context::stable_host_identity(),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(provider_root),
        first_prompt_digest: None,
        source_policy: Some("shadow".into()),
    };
    let context_path = PreparedIncarnation::create(context.clone()).unwrap().path;
    std::fs::write(
        tc::session_temp_path(&name, "spawn_nonce"),
        &context.execution_nonce,
    )
    .unwrap();
    let source = dir.join(format!("rollout-2026-10-23-{id}.jsonl"));
    let header = serde_json::json!({"type":"session_meta","payload":{
        "id":id.to_string(),"source":"cli","cwd":canonical_tmp(),
        "timestamp":(context.created_at + chrono::Duration::seconds(1)).to_rfc3339()
    }});
    std::fs::write(&source, format!("{header}\n")).unwrap();
    dedupe::register_tmux_channel(&name, channel);
    dedupe::register_launched_tmux_runtime_binding(&name, binding(&source, &id.to_string()));
    assert_eq!(p5::binding_events_since(channel, 0).unwrap().len(), 1);
    crate::services::codex_tui::execution_end::capture::capture(channel, &name)
        .unwrap_or_else(|reason| panic!("valid capture fixture: {reason}"));
    Pane {
        channel,
        label,
        name,
        context,
        context_path,
        source,
        process,
        fake,
    }
}

fn cold_cli(fixture: &Fixture, pane: &Pane) -> Vec<crate::config::TestEnvVarGuard> {
    pane.fake.enable_fixture_launch();
    let cli = fixture.logs.path().join("codex-successor-fixture");
    std::fs::write(&cli, r#"#!/bin/sh
if [ "${1-}" = --version ]; then echo 'codex-cli 0.157.1'; exit 0; fi
if [ "${1-} ${2-}" = 'resume --help' ]; then echo '--dangerously-bypass-hook-trust'; exit 0; fi
python3 - "$@" <<'PY'
import datetime, json, os, pathlib, uuid
context = json.loads(pathlib.Path(os.environ['AGENTDESK_BINDING_CONTEXT']).read_text())
working_dir = CANONICAL_TMP if context.get('expected_native_session_id') else str(pathlib.Path(context['provider_root']).parent)
identity = context.get('expected_native_session_id') or str(uuid.uuid4())
stamp = datetime.datetime.now(datetime.timezone.utc).isoformat()
rows = [
 {'type':'session_meta','timestamp':stamp,'payload':{'id':identity,'timestamp':stamp,'cwd':working_dir,'source':'cli','originator':'codex_cli_rs'}},
 {'type':'response_item','payload':{'type':'message','role':'user','content':'resume prompt'}},
 {'type':'event_msg','payload':{'type':'task_started','turn_id':'successor-turn'}},
 {'type':'response_item','payload':{'type':'message','role':'assistant','content':[{'type':'output_text','text':'successor complete'}]}},
 {'type':'event_msg','payload':{'type':'task_complete','turn_id':'successor-turn','last_agent_message':'successor complete'}},
 {'type':'event_msg','payload':{'type':'composer_ready','turn_id':'successor-turn'}}
]
path = pathlib.Path(context['provider_root']) / ('rollout-' + identity + '.jsonl')
if context.get('expected_native_session_id'):
    path = next(pathlib.Path(context['provider_root']).glob('*' + identity + '.jsonl'))
    with path.open('a') as stream:
        stream.write(''.join(json.dumps(row) + '\n' for row in rows[1:]))
else:
    path.write_text(''.join(json.dumps(row) + '\n' for row in rows))
PY
"#.replace("CANONICAL_TMP", &serde_json::to_string(&canonical_tmp()).unwrap())).unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    vec![
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CODEX_PATH",
            &cli,
        ),
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "CODEX_HOME",
            &fixture.logs.path().canonicalize().unwrap(),
        ),
        crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "AGENTDESK_CODEX_TUI_WARM_FOLLOWUP",
            "0".as_ref(),
        ),
    ]
}

fn cold_launch(fixture: &Fixture, pane: &Pane) -> String {
    let _cli = cold_cli(fixture, pane);
    let successor_process = FixtureProcess::start(0);
    let fake_pid = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .next()
        .unwrap()
        .join("pane_pid");
    let successor_pid = successor_process.pid;
    crate::services::codex::actual_submit_tests::before_create_effect(Box::new(move || {
        std::fs::write(fake_pid, successor_pid.to_string()).unwrap();
    }));
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let (sender, _receiver) = std::sync::mpsc::channel();
    let mut result = None;
    let logs = capture_logs(|| {
        result = Some(crate::services::codex::actual_submit_tests::cold_driver(
            fixture.logs.path(),
            "missing-native-session",
            (&pane.name, pane.channel),
            Arc::new(crate::services::provider::CancelToken::new()),
            sender,
        ));
    });
    let result = result.unwrap();
    assert!(result.is_ok(), "actual absence launch: {result:?}\n{logs}");
    eprintln!("{logs}");
    let nonce = binding_context::observe_spawn_nonce_marker(&pane.name);
    assert!(matches!(&nonce, SpawnNonceMarker::Known(n) if n != &pane.context.execution_nonce));
    let actual = dedupe::runtime_binding_for_tmux_session(&pane.name)
        .expect("actual cold tail must publish a runtime binding");
    assert_ne!(
        actual.output_path,
        pane.source.to_string_lossy(),
        "new source must actually bind"
    );
    logs
}

#[test]
fn explicit_clear_then_actual_absence_fresh_seals_exactly_once() {
    if !isolated(concat!(
        module_path!(),
        "::explicit_clear_then_actual_absence_fresh_seals_exactly_once"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let sessions = fixture.logs.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let pane = cold_pane(&sessions);
    runtime().block_on(async {
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let channel = serenity::ChannelId::new(pane.channel);
        super::super::super::clear_persist_failure_tests::seed_session(&shared, channel).await;
        shared
            .core
            .lock()
            .await
            .sessions
            .get_mut(&channel)
            .unwrap()
            .channel_name = Some(pane.label.clone());
        let report = super::super::super::clear_channel_session_state(
            &Arc::new(serenity::Http::new("")),
            &shared,
            &ProviderKind::Codex,
            channel,
            "!clear",
        )
        .await
        .unwrap();
        assert!(!report.end_unconfirmed);
    });
    assert!(successor(&pane).is_none());
    let logs = cold_launch(&fixture, &pane);
    assert_eq!(
        logs.lines()
            .filter(
                |line| line.contains("WARN") && line.contains("Codex successor Seal not recorded")
            )
            .count(),
        0,
        "successful actual clear/fresh must not report a failed Seal: {logs}"
    );
    assert_eq!(
        logs.lines()
            .filter(|line| line.contains("INFO") && line.contains("Codex successor Seal durable"))
            .count(),
        1,
        "the actual fresh successor must report its durable Seal once: {logs}"
    );
    let seal = successor(&pane).expect("actual !clear followed by actual fresh launch must seal");
    assert_ne!(seal["execution_nonce"], pane.context.execution_nonce);
    assert_eq!(seal["context"]["launch_mode"], "fresh");
    assert!(seal["binding_seq"].as_u64().unwrap() > 1);
    let binding = dedupe::runtime_binding_for_tmux_session(&pane.name).unwrap();
    dedupe::register_launched_tmux_runtime_binding(&pane.name, binding);
    assert_eq!(
        successor(&pane),
        Some(seal),
        "retry preserves the first Seal"
    );
    reader_retirement_tests::actual_end_retires_only_its_old_reader(&pane);
}

#[test]
fn direct_replacement_actual_absence_fresh_seals_exactly_once() {
    if !isolated(concat!(
        module_path!(),
        "::direct_replacement_actual_absence_fresh_seals_exactly_once"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let sessions = fixture.logs.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let pane = cold_pane(&sessions);
    cold_launch(&fixture, &pane);
    assert_eq!(pane.records()[0]["basis"], "direct_replacement");
    assert!(
        successor(&pane).is_some(),
        "actual direct replacement must seal its fresh launch"
    );
    reader_retirement_tests::actual_end_retires_only_its_old_reader(&pane);
}

#[test]
fn direct_replacement_retries_unsynced_end_with_its_original_frozen_identity() {
    use crate::services::tui_o::store::fault::{self, Step};
    use std::io::ErrorKind;

    if !isolated(concat!(
        module_path!(),
        "::direct_replacement_retries_unsynced_end_with_its_original_frozen_identity"
    )) {
        return;
    }
    for evict in [false, true] {
        let fixture = Fixture::new();
        let sessions = fixture.logs.path().join("sessions");
        std::fs::create_dir(&sessions).unwrap();
        let pane = cold_pane(&sessions);
        let root = crate::config::runtime_root().unwrap();
        let fault = fault::plant(
            &root.join(codex_end::DIR_NAME),
            Step::ReplaceDirSync,
            ErrorKind::Other,
            Some(1),
        );
        cold_launch(&fixture, &pane);
        let records = pane.records();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["basis"], "direct_replacement");
        assert!(
            successor(&pane).is_none(),
            "an unsynced End cannot immediately seal"
        );
        let identity_before = serde_json::to_vec(&records[0]).unwrap();
        let ended_at = records[0]["ended_at"].clone();
        drop(fault);
        let end_id = records[0]["end_id"].as_str().unwrap().to_owned();
        if evict {
            let original = codex_end::read(&root, pane.channel).unwrap().remove(0);
            for _ in 0..16 {
                let mut next = original.clone();
                next.end_id = uuid::Uuid::new_v4().simple().to_string();
                assert_eq!(
                    codex_end::append(&root, &next).unwrap(),
                    codex_end::Appended::Durable
                );
            }
            assert!(
                codex_end::read(&root, pane.channel)
                    .unwrap()
                    .iter()
                    .all(|end| end.end_id != end_id)
            );
        }

        let SpawnNonceMarker::Known(nonce) =
            binding_context::observe_spawn_nonce_marker(&pane.name)
        else {
            panic!("the actual fresh launch must publish its nonce");
        };
        tc::with_tmux_source_authority(&pane.name, |authority| {
            crate::services::codex_tui::execution_end::producer::retry_bound_candidate(
                pane.channel,
                authority,
                &nonce,
            );
        });
        let records = pane.records();
        if evict {
            assert!(
                records.iter().all(|end| end["end_id"] != end_id),
                "stale unsynced Candidate must never recreate an evicted End"
            );
            continue;
        }
        assert_eq!(
            records.len(),
            1,
            "retry cannot append a different replacement End"
        );
        assert_eq!(records[0]["basis"], "direct_replacement");
        assert_eq!(records[0]["ended_at"], ended_at);
        assert_eq!(records[0]["successor"]["execution_nonce"], nonce);
        let mut identity_after = records[0].clone();
        identity_after.as_object_mut().unwrap().remove("successor");
        assert_eq!(
            serde_json::to_vec(&identity_after).unwrap(),
            identity_before,
            "retry must preserve every confirmed End byte except the added Seal"
        );
    }
}

#[test]
fn first_live_and_rehydrated_publishers_seal_only_the_observed_fresh() {
    let fixture = Fixture::new();
    for restored in [false, true] {
        let pane = Pane::start(fixture.logs.path());
        pane.end(true);
        let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
        let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
        let (context, path, session) = fresh(&pane, None);
        spawn(&pane, &context, &mut observation);
        assert!(successor(&pane).is_none(), "spawn alone never seals");
        publish(&pane, &path, &session, restored);
        let seal = successor(&pane).expect("the first durable binding must seal");
        assert_eq!(seal["execution_nonce"], context.execution_nonce);
        assert_eq!(seal["source"]["session_id"], session);
        publish(&pane, &path, &session, restored);
        assert_eq!(successor(&pane), Some(seal));
    }
}

#[test]
fn late_first_prompt_retries_on_unchanged_binding_without_retroactive_other_source() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, Some("late first prompt"));
    spawn(&pane, &context, &mut observation);
    let logs = capture_logs(|| publish(&pane, &path, &session, false));
    assert!(
        successor(&pane).is_none(),
        "missing prompt evidence stays pending"
    );
    assert_eq!(
        logs.lines()
            .filter(
                |line| line.contains("WARN") && line.contains("Codex successor Seal not recorded")
            )
            .count(),
        0,
        "pending evidence remains retryable: {logs}"
    );
    let diagnostic = logs
        .lines()
        .find(|line| line.contains("DEBUG") && line.contains("Codex successor Seal not recorded"))
        .unwrap_or_else(|| panic!("missing pending-evidence diagnostic: {logs}"));
    for field in [
        &format!("channel={}", pane.channel),
        "pane=",
        &pane.name,
        "pid=None",
        "pgid=None",
        "attempt_state=\"Bound:PendingEvidence\"",
        "reason=\"fresh source evidence pending\"",
    ] {
        assert!(diagnostic.contains(field), "missing {field}: {diagnostic}");
    }
    let count = p5::records_strict(pane.channel).unwrap().unwrap().len();
    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{}",
        serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":"late first prompt"
        }})
    )
    .unwrap();
    publish(&pane, &path, &session, false);
    assert_eq!(
        p5::records_strict(pane.channel).unwrap().unwrap().len(),
        count
    );
    assert_eq!(
        successor(&pane).unwrap()["execution_nonce"],
        context.execution_nonce
    );
}

/// Codex writes AGENTS.md instructions and its environment as a user record before the prompt.
#[test]
fn fresh_successor_seals_when_agents_md_context_precedes_the_first_prompt() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let prompt = "[User: tester (ID: 1)] first prompt after clear";
    let (context, path, session) = fresh(&pane, Some(prompt));
    let user = |texts: &[&str], kinds: &[&str]| {
        let content: Vec<_> = texts
            .iter()
            .map(|text| serde_json::json!({"type":"input_text","text":text}))
            .collect();
        serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":content,
            "internal_chat_message_metadata_passthrough":{"content_item_kinds":kinds}
        }})
    };
    let agents = "# AGENTS.md instructions\n\n<INSTRUCTIONS>\ndummy rule\n</INSTRUCTIONS>";
    let env = "<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>";
    let kinds = ["agents_md.instructions", "environments.environment_context"];
    let rows = [
        user(&[agents, env], &kinds),
        user(&[prompt], &["user.text"]),
    ];
    let mut body = std::fs::read_to_string(&path).unwrap();
    body.extend(rows.iter().map(|row| format!("{row}\n")));
    std::fs::write(&path, body).unwrap();
    spawn(&pane, &context, &mut observation);
    let logs = capture_logs(|| publish(&pane, &path, &session, false));
    let seal = successor(&pane).unwrap_or_else(|| panic!("the fresh launch must seal: {logs}"));
    assert_eq!(seal["execution_nonce"], context.execution_nonce);
    assert!(logs.contains("Codex successor Seal durable"), "{logs}");
}

#[test]
fn next_begin_warns_once_when_discarding_bound_candidate() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, None);
    spawn(&pane, &context, &mut observation);
    assert!(
        successor(&pane).is_none(),
        "the bound Candidate awaits its first binding"
    );

    let mut next = None;
    let logs = capture_logs(|| {
        next = Some(attempt::begin(Some(pane.channel), Some(&pane.name)));
    });
    let warnings = logs
        .lines()
        .filter(|line| line.contains("WARN") && line.contains("Codex successor Seal not recorded"))
        .collect::<Vec<_>>();
    assert_eq!(
        warnings.len(),
        1,
        "a superseded bound Candidate must warn once: {logs}"
    );
    for field in [
        &format!("channel={}", pane.channel),
        "pane=",
        &pane.name,
        "pid=None",
        "pgid=None",
        "attempt_state=",
        "reason=\"superseded by next attempt\"",
    ] {
        assert!(
            warnings[0].contains(field),
            "missing {field}: {}",
            warnings[0]
        );
    }
    publish(&pane, &path, &session, false);
    assert!(
        successor(&pane).is_none(),
        "the superseded Candidate cannot seal later"
    );
    drop(next);
}

#[test]
fn overlapping_dispatch_and_resume_each_discard_adjacent_ticket() {
    let fixture = Fixture::new();
    for resume in [false, true] {
        let pane = Pane::start(fixture.logs.path());
        pane.end(true);
        let first = attempt::begin(Some(pane.channel), Some(&pane.name));
        let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
        if resume {
            let source = p5::codex::settled::read(pane.channel, &pane.name)
                .unwrap()
                .unwrap()
                .source;
            let mut context = pane.context.clone();
            context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
            context.launch_mode = "resume".into();
            context.expected_native_session_id = Some(source.session_id.clone());
            spawn(&pane, &context, &mut observation);
            let count = p5::records_strict(pane.channel).unwrap().unwrap().len();
            publish(&pane, &source.path, &source.session_id, false);
            assert_eq!(
                p5::records_strict(pane.channel).unwrap().unwrap().len(),
                count
            );
            assert!(successor(&pane).is_none(), "Unchanged resume never seals");
        } else {
            let next = attempt::begin(Some(pane.channel), Some(&pane.name));
            drop(next);
        }
        let (context, path, session) = fresh(&pane, None);
        spawn(&pane, &context, &mut observation);
        publish(&pane, &path, &session, false);
        assert!(
            successor(&pane).is_none(),
            "resume or overlap cannot claim a later fresh"
        );
        drop(first);
    }
}

#[test]
fn restart_before_binding_and_old_unsealed_end_do_not_backfill() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let first = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, None);
    spawn(&pane, &context, &mut observation);
    drop(observation);
    drop(first);
    attempt::reset_for_tests();
    publish(&pane, &path, &session, true);
    assert!(
        successor(&pane).is_none(),
        "restart loses the volatile Candidate before binding"
    );
}

#[test]
fn first_binding_before_context_blocks_a_later_matching_fresh_source() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, None);
    spawn(&pane, &context, &mut observation);
    let header = serde_json::json!({"type":"session_meta","payload":{
        "id":session,"source":"cli","cwd":canonical_tmp(),
        "timestamp":(context.created_at - chrono::Duration::milliseconds(1)).to_rfc3339()
    }});
    std::fs::write(&path, format!("{header}\n")).unwrap();
    publish(&pane, &path, &session, false);
    assert!(successor(&pane).is_none());
    let (_, later_path, later_session) = fresh(&pane, None);
    publish(&pane, &later_path, &later_session, false);
    assert!(
        successor(&pane).is_none(),
        "the failed first binding cannot migrate to a later source"
    );
}

#[test]
fn exact_pending_then_resolved_runtime_publication_seals_the_resolved_sequence() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, alias_path, session) = fresh(&pane, None);
    let path = alias_path.canonicalize().unwrap();
    let header = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    spawn(&pane, &context, &mut observation);
    dedupe::register_launched_tmux_runtime_binding(
        &pane.name,
        TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let rows = p5::records_strict(pane.channel).unwrap().unwrap();
    let pending = rows.last().unwrap();
    assert!(matches!(&pending.new, p5::BindingTarget::Pending {
        payload_session_id, payload_transcript_path
    } if payload_session_id == &session && payload_transcript_path.as_deref() == path.to_str()));
    let pending_seq = pending.seq;
    assert!(successor(&pane).is_none(), "a durable Pending never seals");

    std::fs::write(&path, header).unwrap();
    publish(&pane, &path, &session, false);
    let rows = p5::records_strict(pane.channel).unwrap().unwrap();
    let resolved = rows.last().unwrap();
    assert!(matches!(&resolved.new, p5::BindingTarget::Resolved {
        pending_seq: resolved_pending, source
    } if *resolved_pending == pending_seq && source.path == path && source.session_id == session));
    assert_eq!(rows.len(), 3);
    let seal = successor(&pane).expect("the exact Pending resolution must seal");
    assert_eq!(seal["binding_seq"], resolved.seq);
    assert_eq!(seal["source"]["session_id"], session);
    assert_eq!(seal["execution_nonce"], context.execution_nonce);
}

#[test]
fn done_candidate_can_retry_late_prompt_without_another_binding_publication() {
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    pane.end(true);
    let dispatch = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, Some("prompt after dispatch completion"));
    spawn(&pane, &context, &mut observation);
    publish(&pane, &path, &session, false);
    assert!(successor(&pane).is_none());
    let id = attempt::current(pane.channel, &pane.name).unwrap();
    drop(dispatch);
    tc::with_tmux_source_authority(&pane.name, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::Done)
        );
    });

    use std::io::Write;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap(),
        "{}",
        serde_json::json!({"type":"response_item","payload":{
            "type":"message","role":"user","content":"prompt after dispatch completion"
        }})
    )
    .unwrap();
    let rows_before = p5::records_strict(pane.channel).unwrap().unwrap();
    tc::with_tmux_source_authority(&pane.name, |authority| {
        crate::services::codex_tui::execution_end::producer::retry_bound_candidate(
            pane.channel,
            authority,
            &context.execution_nonce,
        );
    });
    assert_eq!(
        p5::records_strict(pane.channel).unwrap().unwrap(),
        rows_before
    );
    assert_eq!(
        successor(&pane).unwrap()["execution_nonce"],
        context.execution_nonce
    );
}

#[test]
fn generic_pending_resolution_seals_done_candidate_on_idle_sources_follow() {
    if !isolated(concat!(
        module_path!(),
        "::generic_pending_resolution_seals_done_candidate_on_idle_sources_follow"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let pane = Pane::start(fixture.logs.path());
    assert!(!pane.end(true).end_unconfirmed);
    let dispatch = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let prompt = "prompt after generic resolution and dispatch completion";
    let (context, alias_path, session) = fresh(&pane, Some(prompt));
    let path = alias_path.canonicalize().unwrap();
    let header = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    spawn(&pane, &context, &mut observation);
    dedupe::register_launched_tmux_runtime_binding(
        &pane.name,
        TuiRuntimeBinding {
            runtime_kind: CodexTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );
    let pending = p5::records_strict(pane.channel)
        .unwrap()
        .unwrap()
        .pop()
        .unwrap();
    assert!(matches!(&pending.new, p5::BindingTarget::Pending {
        payload_session_id, payload_transcript_path
    } if payload_session_id == &session && payload_transcript_path.as_deref() == path.to_str()));
    assert!(successor(&pane).is_none());

    std::fs::write(&path, header).unwrap();
    // This real generic publisher resolves P5 and updates the binding without the launched
    // publisher's execution-end callback. O's idle poll must finish its retained Candidate.
    dedupe::register_tmux_runtime_binding(&pane.name, binding(&path, &session));
    let rows = p5::records_strict(pane.channel).unwrap().unwrap();
    assert_eq!(rows.len(), 3);
    assert!(matches!(&rows[2].new, p5::BindingTarget::Resolved {
        pending_seq, source
    } if *pending_seq == pending.seq && source.path == path && source.session_id == session));
    assert!(
        successor(&pane).is_none(),
        "generic resolution has no Seal callback"
    );
    let id = attempt::current(pane.channel, &pane.name).unwrap();
    drop(dispatch);
    tc::with_tmux_source_authority(&pane.name, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::Done)
        );
    });
    reader_retirement_tests::idle_follow_seals_generic_resolution(&pane, &context, &path, prompt);
}

#[test]
fn cleanup_reset_then_refused_or_resumed_dispatch_consumes_the_pending_ticket() {
    let fixture = Fixture::new();
    for resumed in [false, true] {
        let pane = Pane::start(fixture.logs.path());
        pane.end(true);
        assert!(attempt::ticket(pane.channel, &pane.name).is_some());
        tc::cleanup_session_temp_files(&pane.name);
        let dispatch = attempt::begin(Some(pane.channel), Some(&pane.name));
        assert!(attempt::ticket(pane.channel, &pane.name).is_none());
        if resumed {
            let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
            let mut context = pane.context.clone();
            context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
            context.launch_mode = "resume".into();
            context.expected_native_session_id = Some(
                p5::codex::settled::read(pane.channel, &pane.name)
                    .unwrap()
                    .unwrap()
                    .source
                    .session_id,
            );
            spawn(&pane, &context, &mut observation);
        }
        drop(dispatch);

        let _later = attempt::begin(Some(pane.channel), Some(&pane.name));
        let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
        let (context, path, session) = fresh(&pane, None);
        spawn(&pane, &context, &mut observation);
        publish(&pane, &path, &session, false);
        assert!(
            successor(&pane).is_none(),
            "reset cannot give an old ticket to a later fresh"
        );
    }
}

#[test]
fn seven_historical_unsealed_ends_stay_unsealed_when_only_the_adjacent_end_seals() {
    use crate::services::tui_o::store::codex_end::{Appended, CodexEnd, EndBasis};
    use std::os::unix::fs::MetadataExt;

    let mut fixture = Fixture::new();
    fixture.logs = tempfile::Builder::new()
        .prefix("a2a7-")
        .tempdir_in(canonical_tmp())
        .unwrap();
    p5::set_test_root(Some(fixture.logs.path()));
    let pane = Pane::start(fixture.logs.path());
    let root = crate::config::runtime_root().unwrap();
    let current_source = p5::codex::settled::read(pane.channel, &pane.name)
        .unwrap()
        .unwrap()
        .source;
    let provider_root = pane
        .context
        .provider_root
        .as_ref()
        .unwrap()
        .canonicalize()
        .unwrap();
    let mut historical_ids = Vec::new();
    for index in 0..7 {
        let mut context = pane.context.clone();
        context.created_at -= chrono::Duration::days(8 - index);
        context.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        context.provider_root = Some(provider_root.clone());
        let session = uuid::Uuid::new_v4().to_string();
        let path = provider_root.join(format!("rollout-{session}.jsonl"));
        let header = serde_json::json!({"type":"session_meta","payload":{
            "id":session,"source":"cli","cwd":canonical_tmp(),
            "timestamp":context.created_at.to_rfc3339()
        }});
        std::fs::write(&path, format!("{header}\n")).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let mut source = current_source.clone();
        source.path = path;
        source.session_id = session;
        source.dev = metadata.dev();
        source.ino = metadata.ino();
        PreparedIncarnation::create(context.clone()).unwrap();
        let end = CodexEnd {
            end_id: uuid::Uuid::new_v4().simple().to_string(),
            channel: pane.channel,
            provider: "codex".into(),
            tmux_session: pane.name.clone(),
            execution_nonce: context.execution_nonce.clone(),
            host: context.host.clone().unwrap(),
            owner_runtime_root: context.owner_runtime_root.clone(),
            provider_root: provider_root.clone(),
            source,
            binding_seq: 1,
            captured_at: context.created_at,
            ended_at: context.created_at + chrono::Duration::seconds(1),
            basis: EndBasis::ExplicitClear,
            context: ContextSnapshot::from(&context),
            successor: None,
        };
        assert_eq!(codex_end::append(&root, &end).unwrap(), Appended::Durable);
        historical_ids.push(end.end_id);
    }
    assert_eq!(codex_end::read(&root, pane.channel).unwrap().len(), 7);
    pane.end(true);
    let records = codex_end::read(&root, pane.channel).unwrap();
    assert_eq!(records.len(), 8);
    let adjacent_id = records.last().unwrap().end_id.clone();
    let _attempt = attempt::begin(Some(pane.channel), Some(&pane.name));
    let mut observation = LaunchObservation::new(Some(pane.channel), &pane.name);
    let (context, path, session) = fresh(&pane, None);
    spawn(&pane, &context, &mut observation);
    publish(&pane, &path, &session, false);

    let records = codex_end::read(&root, pane.channel).unwrap();
    assert_eq!(
        records.len(),
        8,
        "the fixture must retain all seven historical Ends"
    );
    for id in historical_ids {
        let record = records.iter().find(|record| record.end_id == id).unwrap();
        assert!(
            record.successor.is_none(),
            "historical End {id} cannot be backfilled"
        );
    }
    let adjacent = records
        .iter()
        .find(|record| record.end_id == adjacent_id)
        .unwrap();
    assert_eq!(
        adjacent.successor.as_ref().unwrap().execution_nonce,
        context.execution_nonce
    );
}

#[test]
fn actual_warm_kill_fallback_resumes_without_seal_and_preserves_managed_turn() {
    use crate::services::codex_tui::host_input::spy::{SpyGuard, SpyState};
    use crate::services::discord::codex_absence::replacement::Probe;
    use crate::services::tui_o::store::codex_end::{Appended, CodexEnd, EndBasis};
    if !isolated(concat!(
        module_path!(),
        "::actual_warm_kill_fallback_resumes_without_seal_and_preserves_managed_turn"
    )) {
        return;
    }
    let fixture = Fixture::new();
    let sessions = fixture.logs.path().join("sessions");
    std::fs::create_dir(&sessions).unwrap();
    let pane = cold_pane(&sessions);
    let _cli = cold_cli(&fixture, &pane);
    let _warm = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
        "AGENTDESK_CODEX_TUI_WARM_FOLLOWUP",
        "1".as_ref(),
    );
    let source = p5::codex::settled::read(pane.channel, &pane.name)
        .unwrap()
        .unwrap()
        .source;
    let native = source.session_id.clone();
    let options = crate::services::codex::actual_submit_tests::options(&native, &canonical_tmp());
    crate::services::codex_tui::warm_followup::write_codex_tui_launch_options_evidence(
        &pane.name, &options,
    )
    .unwrap();
    std::fs::write(
        tc::session_temp_path(&pane.name, tc::TMUX_RUNTIME_KIND_TEMP_EXT),
        CodexTui.as_str(),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(
        &pane.name,
        &pane.source,
        Some(&native),
    )
    .unwrap();
    let now = chrono::Utc::now();
    let end = CodexEnd {
        end_id: uuid::Uuid::new_v4().simple().to_string(),
        channel: pane.channel,
        provider: "codex".into(),
        tmux_session: pane.name.clone(),
        execution_nonce: pane.context.execution_nonce.clone(),
        host: pane.context.host.clone().unwrap(),
        owner_runtime_root: pane.context.owner_runtime_root.clone(),
        provider_root: pane
            .context
            .provider_root
            .as_ref()
            .unwrap()
            .canonicalize()
            .unwrap(),
        source,
        binding_seq: 1,
        captured_at: now,
        ended_at: now,
        basis: EndBasis::ExplicitClear,
        context: ContextSnapshot::from(&pane.context),
        successor: None,
    };
    let root = crate::config::runtime_root().unwrap();
    assert_eq!(codex_end::append(&root, &end).unwrap(), Appended::Durable);
    tc::with_tmux_source_authority(&pane.name, |authority| {
        let epoch = attempt::generation(pane.channel, authority);
        assert!(attempt::install_ticket(
            pane.channel,
            authority,
            epoch,
            &end.end_id
        ));
    });
    let token = Arc::new(crate::services::provider::CancelToken::new());
    let row_path = crate::services::discord::save_actual_episode_for_tests(
        pane.channel,
        &pane.name,
        30,
        &token,
    );
    let row_before = std::fs::read(&row_path).unwrap();
    let row: serde_json::Value = serde_json::from_slice(&row_before).unwrap();
    assert_eq!(row["request_owner_user_id"], 8);
    assert_eq!(row["turn_nonce"].as_str(), token.turn_nonce());
    let ready = "╭──────────────────────────────────────────────╮\n│ ▌                                            │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let draft = "╭──────────────────────────────────────────────╮\n│ resume prompt ▌                              │\n╰──────────────────────────────────────────────╯\n  Esc to interrupt   Ctrl+J newline   ⏎ send";
    let spy = SpyGuard::install(SpyState {
        captures: [ready, ready, ready, draft]
            .into_iter()
            .map(|pane| Some(pane.to_owned()))
            .collect(),
        pane_pid: Some(pane.process.pid),
        ..SpyState::default()
    });
    let state = std::rc::Rc::clone(&spy.0);
    spy.0.borrow_mut().on_enter = Some(Box::new(move || {
        state.borrow_mut().captures = [Some(draft.to_owned()), Some(draft.to_owned())].into();
    }));
    let successor_process = FixtureProcess::start(0);
    let fake_pid = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .next()
        .unwrap()
        .join("pane_pid");
    let successor_pid = successor_process.pid;
    let observed = std::rc::Rc::new(std::cell::Cell::new(false));
    let did_observe = std::rc::Rc::clone(&observed);
    let state = std::rc::Rc::clone(&spy.0);
    let name = pane.name.clone();
    let channel = pane.channel;
    let before = row_before.clone();
    let row_copy = row_path.clone();
    let guarded_token = Arc::clone(&token);
    crate::services::codex::actual_submit_tests::before_create_effect(Box::new(move || {
        let probe = Probe::new(channel, &name);
        assert!(
            probe.defer_death(),
            "warm replacement must span cold create"
        );
        assert!(probe.cleanup().is_none());
        assert_eq!(std::fs::read(&row_copy).unwrap(), before);
        assert!(
            !guarded_token
                .cancelled
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert!(state.borrow().calls.iter().any(|call| call == "kill_tree"));
        state.borrow_mut().captures = std::iter::repeat_n(Some(ready.to_owned()), 128).collect();
        std::fs::write(&fake_pid, successor_pid.to_string()).unwrap();
        did_observe.set(true);
    }));
    let dispatch = attempt::begin(Some(pane.channel), Some(&pane.name));
    let id = attempt::current(pane.channel, &pane.name).unwrap();
    let (sender, _receiver) = std::sync::mpsc::channel();
    let mut result = None;
    let logs = capture_logs(|| {
        result = Some(crate::services::codex::actual_submit_tests::cold_driver(
            &canonical_tmp(),
            &native,
            (&pane.name, pane.channel),
            Arc::clone(&token),
            sender,
        ));
    });
    let outcome = result.unwrap();
    assert!(outcome.is_ok(), "{outcome:?}\n{logs}");
    assert!(
        observed.get(),
        "actual FallbackAfterPaneKill must reach cold create: {logs}"
    );
    assert!(
        !Probe::new(pane.channel, &pane.name).defer_death(),
        "actual cold finish must complete C guard"
    );
    assert_eq!(std::fs::read(&row_path).unwrap(), row_before);
    assert!(!token.cancelled.load(std::sync::atomic::Ordering::Acquire));
    let SpawnNonceMarker::Known(nonce) = binding_context::observe_spawn_nonce_marker(&pane.name)
    else {
        panic!("actual resume spawn nonce absent: {logs}");
    };
    let context = binding_context::execution_context("codex", &nonce).unwrap();
    assert_eq!(context.launch_mode, "resume", "{logs}");
    assert_eq!(
        context.expected_native_session_id.as_deref(),
        Some(native.as_str())
    );
    tc::with_tmux_source_authority(&pane.name, |authority| {
        assert_eq!(
            attempt::phase_under(&id, authority),
            Some(attempt::Phase::Bound { nonce })
        )
    });
    assert!(attempt::take_ticket(&id).is_none());
    assert_eq!(
        codex_end::read(&root, pane.channel).unwrap().as_slice(),
        std::slice::from_ref(&end)
    );
    assert_eq!(
        spy.calls()
            .iter()
            .filter(|call| call.as_str() == "kill_tree")
            .count(),
        1
    );
    drop(dispatch);
    let _later = attempt::begin(Some(pane.channel), Some(&pane.name));
    assert!(attempt::take_ticket(&attempt::current(pane.channel, &pane.name).unwrap()).is_none());
    assert_eq!(codex_end::read(&root, pane.channel).unwrap(), [end]);
}

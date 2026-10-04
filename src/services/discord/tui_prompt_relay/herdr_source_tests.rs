//! Herdr source attach: launch and restart through the real launch, reconcile and hook receiver.
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use super::*;
use crate::db::dispatched_sessions::hosted_execution::tests::{expected, owner};
use crate::db::dispatched_sessions::hosted_execution::{
    ExpectedExecution, HostedLocation, HostedState,
};
use crate::services::claude_tui::hook_server::observation_ingress::tests::{Ingress, events, uuid};
use crate::services::discord::recovery_engine::host_reconcile::HerdrPaneEvidence;
use crate::services::discord::tmux::execution_identity::herdr_observation::{
    HerdrMismatch, HerdrUnknown,
};
use crate::services::herdr_launch::{
    EvidenceProbe, HerdrCreateOutcome, HerdrCreateRequest, HerdrLaunch, HerdrLaunchCommand,
    HerdrLaunchEndpoint, HerdrLaunchHost, HerdrLaunchOutcome, launch_herdr_session,
    unset_herdr_env_before_exec,
};
use crate::services::session_host::{EvidenceGap, RestoreResume, ServerWitness};
use crate::services::tui_o::shadow::capture::file_identity;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::binding_events::{
    APPEND_FAULT, BindingEvent, forget_channel_for_tests,
};
use crate::services::tui_prompt_dedupe::{
    register_tmux_runtime_binding, reset_state_for_tests, runtime_binding_for_tmux_session,
};
use HerdrExecutionMatch::{Match, Mismatch, Unknown};

const CHANNEL: &str = "1479671301387059300";
const PANE: &str = "pane-7";

/// The launch's Herdr side: E7 off, one created pane, its root and provider processes.
struct Launcher;

impl HerdrLaunchHost for Launcher {
    fn restore_resume(&self, _endpoint: &HerdrLaunchEndpoint) -> RestoreResume {
        RestoreResume::Off {
            witness: ServerWitness::for_test(1),
        }
    }

    fn create(&self, _request: &HerdrCreateRequest) -> HerdrCreateOutcome {
        HerdrCreateOutcome::Created {
            pane_id: PANE.into(),
        }
    }

    fn launch_evidence(&self, probe: &EvidenceProbe) -> Result<ExpectedExecution, EvidenceGap> {
        Ok(expected(&probe.execution_nonce, 100))
    }
}

/// One exact pane as the restart reader sees it; `reads` counts panes it was asked for.
struct Pane {
    endpoint: Option<HerdrEndpointId>,
    reading: HerdrPaneReading,
}

impl HerdrExecutionReader for Pane {
    fn endpoint(&self) -> Option<&HerdrEndpointId> {
        self.endpoint.as_ref()
    }

    fn read_pane(&self, pane_id: &str) -> HerdrPaneReading {
        assert_eq!(pane_id, PANE, "only the stored pane is read");
        self.reading.clone()
    }
}

/// A Claude channel's canonical row, its scratch binding log and transcripts, and the receiver.
struct Herdr {
    ingress: Ingress,
    rt: tokio::runtime::Runtime,
    db: Option<crate::db::auto_queue::test_support::TestPostgresDb>,
    pool: PgPool,
    owner: HostedOwner,
    channel: u64,
    location: Option<HostedLocation>,
    _root: crate::config::TestRuntimeRootGuard,
}

impl Herdr {
    fn new(tag: &str) -> Self {
        // The root guard takes the shared env lock before the receiver takes the dedupe lock.
        let root = crate::config::TestRuntimeRootGuard::new();
        let ingress = Ingress::new();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut owner = owner(CHANNEL);
        owner.logical_key = format!("AgentDesk-claude-p7s-{tag}");
        let (db, pool) = rt.block_on(async {
            let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
            let pool = db.connect_and_migrate().await;
            sqlx::query(
                "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                       discord_token_hash, channel_id, hosted_execution)
                 VALUES ($1, 'claude', 'idle', 'discord_channel', $2, $3, NULL)",
            )
            .bind(format!(
                "claude/{}/test-node:{}",
                owner.discord_token_hash, owner.logical_key
            ))
            .bind(&owner.discord_token_hash)
            .bind(CHANNEL)
            .execute(&pool)
            .await
            .unwrap();
            (db, pool)
        });
        Self {
            ingress,
            rt,
            db: Some(db),
            pool,
            owner,
            channel: CHANNEL.parse().unwrap(),
            location: None,
            _root: root,
        }
    }

    fn logical(&self) -> &str {
        &self.owner.logical_key
    }

    /// Launches `session` through the production Herdr launch, with the Claude TUI's own
    /// settings and script preparation; returns the execution nonce.
    fn launch(&mut self, session: &str) -> String {
        // The launch refuses a cwd that is not an existing directory before Pending.
        let dir = self.ingress.path("cwd").with_extension("");
        std::fs::create_dir_all(&dir).unwrap();
        let prepare = |prepared: &PreparedIncarnation| -> Result<HerdrLaunchCommand, String> {
            let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
                tmux_session_name: prepared.context.tmux_session.clone(),
                working_dir: dir.clone(),
                claude_bin: crate::services::claude_command::ClaudeBinary::from_tmux_wrapper_argv(
                    "/usr/local/bin/claude",
                ),
                agentdesk_exe: PathBuf::from("/usr/local/bin/agentdesk"),
                hook_endpoint: "http://127.0.0.1:1".into(),
                session_id: session.to_owned(),
                system_prompt: None,
                model: None,
                resume: false,
            };
            let files = crate::services::claude_tui::session::prepare_claude_tui_launch(&config)?;
            let script = std::fs::read_to_string(&files.launch_script_path).unwrap();
            assert!(unset_herdr_env_before_exec(&script).is_ok());
            Ok(HerdrLaunchCommand {
                cwd: dir.clone(),
                command: format!("bash {}", files.launch_script_path.display()),
            })
        };
        let endpoint = HerdrLaunchEndpoint {
            execution_node: "test-node".into(),
            config_key: "herdr.default".into(),
            socket_addr: "/adk/herdr.sock".into(),
            herdr_session: "agentdesk".into(),
        };
        let launch = HerdrLaunch {
            endpoint: Some(endpoint),
            owner: self.owner.clone(),
            channel_id: Some(self.channel),
            expected_native_session_id: Some(session.to_owned()),
            resume: false,
        };
        let host: Arc<dyn HerdrLaunchHost> = Arc::new(Launcher);
        let outcome = self
            .rt
            .block_on(launch_herdr_session(&self.pool, launch, prepare, host));
        let Ok(HerdrLaunchOutcome::Launched {
            execution_nonce,
            location,
            evidence: Ok(()),
        }) = outcome
        else {
            panic!("{outcome:?}");
        };
        self.location = Some(location);
        execution_nonce
    }

    /// The launched pane still running execution `nonce`, edited by `change`.
    fn pane(&self, nonce: &str, change: impl FnOnce(&mut HerdrPaneEvidence)) -> Pane {
        let stamps = expected("unused", 100);
        let mut evidence = HerdrPaneEvidence {
            binding_nonce: Some(nonce.to_owned()),
            root: Some(stamps.root),
            provider_process: Some(stamps.provider_process),
            agent_session_id: None,
        };
        change(&mut evidence);
        let location = self.location.as_ref().unwrap();
        Pane {
            endpoint: Some(HerdrEndpointId::of(location)),
            reading: HerdrPaneReading::Present(evidence),
        }
    }

    fn launched(&self, nonce: &str, session: &str, reader: &Pane) -> HerdrSourceAttach {
        let transcript = self.ingress.path(session);
        let (pool, owner, channel) = (&self.pool, &self.owner, self.channel);
        self.rt.block_on(attach_launched_herdr_source(
            pool,
            owner,
            channel,
            nonce,
            session,
            &transcript,
            reader,
        ))
    }

    fn restarted(&self, reader: &Pane) -> HerdrSourceAttach {
        let (pool, owner, channel) = (&self.pool, &self.owner, self.channel);
        self.rt
            .block_on(attach_restarted_herdr_source(pool, owner, channel, reader))
    }

    fn row(&self) -> (Option<HostedState>, Option<Value>) {
        let raw: Option<Value> = self.rt.block_on(async {
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
                .bind(CHANNEL)
                .fetch_one(&self.pool)
                .await
                .unwrap()
        });
        let state = match HostedRecord::decode(raw.as_ref()) {
            HostedRecord::Known(record) => Some(record.state),
            _ => None,
        };
        (state, raw)
    }

    /// Clears the row's hosted execution so the channel can launch again.
    fn retire(&self) {
        self.rt.block_on(async {
            sqlx::query("UPDATE sessions SET hosted_execution = NULL WHERE channel_id = $1")
                .bind(CHANNEL)
                .execute(&self.pool)
                .await
                .unwrap();
        });
    }

    fn log(&self) -> Vec<BindingEvent> {
        events(self.channel)
            .into_iter()
            .filter(|event| event.tmux_session == self.owner.logical_key)
            .collect()
    }

    fn bound(&self) -> Option<TuiRuntimeBinding> {
        runtime_binding_for_tmux_session(self.logical())
    }

    /// What a dcserver restart forgets: the bindings in memory and the cached log writer.
    fn restart(&self) {
        reset_state_for_tests();
        forget_channel_for_tests(self.channel);
    }

    /// A UserPromptSubmit of the launched session naming `next` as its transcript.
    fn switch_hook(&self, launched: &str, next: &str, id: &str) -> u16 {
        let payload = self.ingress.payload(next, None);
        self.ingress
            .claude_hook("UserPromptSubmit", launched, &payload, Some(id))
    }
}

impl Drop for Herdr {
    fn drop(&mut self) {
        let db = self.db.take().unwrap();
        let pool = self.pool.clone();
        self.rt.block_on(async {
            pool.close().await;
            db.drop().await;
        });
    }
}

/// Puts a copy at `path` that exists before the old file goes, so the two inodes differ on any
/// filesystem; a delete and recreate may get the same inode back.
fn replace(path: &std::path::Path) {
    let next = path.with_extension("next");
    std::fs::copy(path, &next).unwrap();
    std::fs::rename(&next, path).unwrap();
}

fn source_of(event: &BindingEvent) -> Option<&SourceId> {
    match &event.new {
        BindingTarget::Source(source) | BindingTarget::Resolved { source, .. } => Some(source),
        _ => None,
    }
}

// Launch attach registers only its own matched Pending execution, logs before it publishes, and
// turns the row Bound only once the log names this execution's source.
#[test]
fn herdr_launch_attach_logs_before_it_publishes_and_binds_only_on_its_own_source_pg() {
    {
        // Fresh transcript: one Source of this execution with the file's own identity, then Bound.
        let herdr = &mut Herdr::new("fresh");
        let a = uuid();
        let nonce = herdr.launch(&a);
        let transcript = herdr.ingress.transcript(&a);
        let reader = herdr.pane(&nonce, |e| e.agent_session_id = Some(a.clone()));
        let attached = herdr.launched(&nonce, &a, &reader);
        let published = HerdrSourceAttach::Published {
            bound: true,
            agent_agrees: Some(true),
        };
        assert_eq!(attached, published);
        assert_eq!(herdr.row().0, Some(HostedState::Bound));
        let log = herdr.log();
        let latest = log.last().unwrap();
        assert_eq!(latest.execution_nonce.as_deref(), Some(nonce.as_str()));
        let meta = std::fs::metadata(&transcript).unwrap();
        let (dev, ino) = file_identity(&meta);
        let source = SourceId {
            session_id: a.clone(),
            path: transcript.clone(),
            dev,
            ino,
        };
        assert_eq!(
            source_of(latest),
            Some(&source),
            "no pane or socket in the SourceId"
        );
        assert_eq!(
            herdr.bound().unwrap().output_path,
            transcript.display().to_string()
        );
        drop(reader);
    }
    {
        // Cold start: the transcript is not written yet, so the log holds a Pending and the row stays
        // Pending even though Herdr reports the launched session.
        let herdr = &mut Herdr::new("cold");
        let a = uuid();
        let nonce = herdr.launch(&a);
        let reader = herdr.pane(&nonce, |e| e.agent_session_id = Some(a.clone()));
        let attached = herdr.launched(&nonce, &a, &reader);
        let waiting = HerdrSourceAttach::Published {
            bound: false,
            agent_agrees: Some(false),
        };
        assert_eq!(attached, waiting);
        assert_eq!(herdr.row().0, Some(HostedState::Pending));
        let latest = herdr.log().pop().unwrap();
        assert!(
            matches!(latest.new, BindingTarget::Pending { .. }),
            "{latest:?}"
        );
    }
    {
        // Resume of a source an older execution already logged: no record of this execution exists,
        // so the row stays Pending instead of reading the old record as this launch's baseline.
        let herdr = &mut Herdr::new("resume");
        let a = uuid();
        let transcript = herdr.ingress.transcript(&a);
        let older = herdr.launch(&a);
        let reader = herdr.pane(&older, |_| {});
        assert!(matches!(
            herdr.launched(&older, &a, &reader),
            HerdrSourceAttach::Published { bound: true, .. }
        ));
        herdr.retire();
        let lines = herdr.log().len();
        let nonce = herdr.launch(&a);
        let reader = herdr.pane(&nonce, |_| {});
        let attached = herdr.launched(&nonce, &a, &reader);
        assert!(
            matches!(attached, HerdrSourceAttach::Published { bound: false, .. }),
            "{attached:?}"
        );
        assert_eq!(herdr.row().0, Some(HostedState::Pending));
        assert_eq!(herdr.log().len(), lines, "the same source logs nothing new");
        assert_eq!(
            herdr.bound().unwrap().output_path,
            transcript.display().to_string()
        );
    }
    {
        // Another launch's nonce or a replaced root: refused before any registration.
        let herdr = &mut Herdr::new("refused");
        let a = uuid();
        let nonce = herdr.launch(&a);
        herdr.ingress.transcript(&a);
        let lines = herdr.log().len();
        for (attach_nonce, change, verdict) in [
            (
                "another-launch",
                (|_: &mut HerdrPaneEvidence| {}) as fn(&mut HerdrPaneEvidence),
                HostReconcile::Pending(Match),
            ),
            (
                nonce.as_str(),
                |e| e.root.as_mut().unwrap().pid += 9,
                HostReconcile::Pending(Mismatch(HerdrMismatch::RootReplaced)),
            ),
        ] {
            let reader = herdr.pane(&nonce, change);
            let attached = herdr.launched(attach_nonce, &a, &reader);
            assert_eq!(attached, HerdrSourceAttach::Refused(verdict));
            assert_eq!(herdr.bound(), None, "{attach_nonce}");
            assert_eq!(herdr.log().len(), lines, "{attach_nonce}");
            assert_eq!(herdr.row().0, Some(HostedState::Pending));
        }
    }
    {
        // A failed append publishes nothing and binds nothing, even with an earlier record of this
        // execution in the log that would pass as its baseline.
        let herdr = &mut Herdr::new("append");
        let (a, old) = (uuid(), uuid());
        let nonce = herdr.launch(&a);
        let old_path = herdr.ingress.transcript(&old);
        crate::services::tui_prompt_dedupe::register_tmux_channel(herdr.logical(), herdr.channel);
        register_tmux_runtime_binding(
            herdr.logical(),
            crate::services::claude_tui::hook_server::observation_ingress::tests::claude(
                &old_path, &old,
            ),
        );
        assert_eq!(
            herdr.log().last().unwrap().execution_nonce.as_deref(),
            Some(nonce.as_str())
        );
        herdr.ingress.transcript(&a);
        let reader = herdr.pane(&nonce, |_| {});
        APPEND_FAULT.with(|fault| fault.set(Some("write")));
        let attached = herdr.launched(&nonce, &a, &reader);
        APPEND_FAULT.with(|fault| fault.set(None));
        assert_eq!(attached, HerdrSourceAttach::NotPublished);
        assert_eq!(herdr.row().0, Some(HostedState::Pending));
        assert_eq!(
            herdr.bound().unwrap().output_path,
            old_path.display().to_string()
        );
    }
}

// After a restart only a matched Bound execution gets its logged source back, and only while the
// path still names that file; every other verdict leaves the row, the log and memory as they are.
#[test]
fn herdr_restart_attach_restores_only_a_matched_execution_on_its_logged_source_pg() {
    {
        let herdr = &mut Herdr::new("restart");
        let a = uuid();
        let nonce = herdr.launch(&a);
        let transcript = herdr.ingress.transcript(&a);
        let reader = herdr.pane(&nonce, |_| {});
        assert!(matches!(
            herdr.launched(&nonce, &a, &reader),
            HerdrSourceAttach::Published { bound: true, .. }
        ));
        let (lines, row) = (herdr.log().len(), herdr.row());
        type Change = fn(&mut HerdrPaneEvidence);
        let refused: [(&str, Change, HostReconcile); 3] = [
            (
                "root",
                |e| e.root.as_mut().unwrap().pid += 9,
                HostReconcile::Herdr(Mismatch(HerdrMismatch::RootReplaced)),
            ),
            (
                "provider",
                |e| e.provider_process.as_mut().unwrap().start = "9".into(),
                HostReconcile::Herdr(Mismatch(HerdrMismatch::ProviderReplaced)),
            ),
            (
                "nonce",
                |e| e.binding_nonce = Some("other".into()),
                HostReconcile::Herdr(Mismatch(HerdrMismatch::OtherNonce)),
            ),
        ];
        for (label, change, verdict) in refused {
            herdr.restart();
            let reader = herdr.pane(&nonce, change);
            assert_eq!(
                herdr.restarted(&reader),
                HerdrSourceAttach::Refused(verdict),
                "{label}"
            );
            assert_eq!((herdr.bound(), herdr.log().len()), (None, lines), "{label}");
            assert_eq!(herdr.row(), row, "{label}");
        }
        for (label, reader, verdict) in [
            (
                "no endpoint",
                Pane {
                    endpoint: None,
                    reading: HerdrPaneReading::Missing,
                },
                HostReconcile::Herdr(Unknown(HerdrUnknown::ProbeFailed)),
            ),
            (
                "pane gone",
                Pane {
                    endpoint: herdr.location.as_ref().map(HerdrEndpointId::of),
                    reading: HerdrPaneReading::Missing,
                },
                HostReconcile::Missing,
            ),
        ] {
            herdr.restart();
            assert_eq!(
                herdr.restarted(&reader),
                HerdrSourceAttach::Refused(verdict),
                "{label}"
            );
            assert_eq!((herdr.bound(), herdr.log().len()), (None, lines), "{label}");
            assert_eq!(herdr.row(), row, "{label}");
        }

        // A match restores the logged source; Herdr's differing agent session is only reported.
        herdr.restart();
        let reader = herdr.pane(&nonce, |e| e.agent_session_id = Some("agent-b".into()));
        let restored = HerdrSourceAttach::Published {
            bound: true,
            agent_agrees: Some(false),
        };
        assert_eq!(herdr.restarted(&reader), restored);
        let binding = herdr.bound().unwrap();
        assert_eq!(binding.session_id.as_deref(), Some(a.as_str()));
        assert_eq!(binding.output_path, transcript.display().to_string());

        // A continuation the hook adopted is the source a later restart restores, even when Herdr
        // still reports the session the pane left.
        let b = uuid();
        herdr.ingress.transcript(&b);
        assert_eq!(herdr.switch_hook(&a, &b, &uuid()), 202);
        assert_eq!(
            herdr.bound().unwrap().session_id.as_deref(),
            Some(b.as_str())
        );
        herdr.restart();
        let reader = herdr.pane(&nonce, |e| e.agent_session_id = Some(a.clone()));
        assert!(matches!(
            herdr.restarted(&reader),
            HerdrSourceAttach::Published { .. }
        ));
        assert_eq!(
            herdr.bound().unwrap().session_id.as_deref(),
            Some(b.as_str())
        );

        // The logged path now names another file: nothing is published, pinned or not.
        replace(&herdr.ingress.path(&b));
        herdr.restart();
        let reader = herdr.pane(&nonce, |_| {});
        assert_eq!(herdr.restarted(&reader), HerdrSourceAttach::NotPublished);
        assert_eq!(herdr.bound(), None);
    }
    {
        let herdr = &mut Herdr::new("moved");
        let a = uuid();
        let nonce = herdr.launch(&a);
        let path_a = herdr.ingress.transcript(&a);
        let reader = herdr.pane(&nonce, |_| {});
        assert!(matches!(
            herdr.launched(&nonce, &a, &reader),
            HerdrSourceAttach::Published { .. }
        ));
        replace(&path_a);
        herdr.restart();
        assert_eq!(herdr.restarted(&reader), HerdrSourceAttach::NotPublished);
        assert_eq!(
            herdr.bound(),
            None,
            "a stat-only record is not followed to another file"
        );
    }
}

// A refused attach withholds the pane at once: a hook switch is refused until a later attach
// matches, and what the bound source wrote meanwhile is still ahead of its cursor after that.
#[test]
fn a_refused_restart_attach_withholds_hook_switches_until_a_match_pg() {
    {
        let herdr = &mut Herdr::new("hold");
        let a = uuid();
        let nonce = herdr.launch(&a);
        let path_a = herdr.ingress.transcript(&a);
        let reader = herdr.pane(&nonce, |_| {});
        assert!(matches!(
            herdr.launched(&nonce, &a, &reader),
            HerdrSourceAttach::Published { .. }
        ));
        let gone = Pane {
            endpoint: herdr.location.as_ref().map(HerdrEndpointId::of),
            reading: HerdrPaneReading::Missing,
        };
        assert_eq!(
            herdr.restarted(&gone),
            HerdrSourceAttach::Refused(HostReconcile::Missing)
        );

        let (b, id) = (uuid(), uuid());
        herdr.ingress.transcript(&b);
        let lines = herdr.log().len();
        assert_eq!(herdr.switch_hook(&a, &b, &id), 425);
        assert_eq!(
            herdr.bound().unwrap().session_id.as_deref(),
            Some(a.as_str())
        );
        assert_eq!(herdr.log().len(), lines, "a withheld switch logs nothing");

        // A prompt written after the refusal is still what the idle relay scans once admitted.
        let cursor = herdr.bound().unwrap().last_offset;
        let prompt = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": "unread prompt"}]}, "sessionId": a});
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path_a)
            .and_then(|mut file| {
                std::io::Write::write_all(&mut file, format!("{prompt}\n").as_bytes())
            })
            .unwrap();
        assert!(matches!(
            herdr.restarted(&reader),
            HerdrSourceAttach::Published { .. }
        ));
        assert_eq!(herdr.bound().unwrap().last_offset, cursor);
        let scan = super::super::scan_claude_idle_transcript_for_prompt(&path_a, cursor);
        assert!(
            matches!(&scan, Ok(super::super::ClaudeIdleTranscriptScan::Prompt { prompt, .. })
                if prompt == "unread prompt"),
            "{scan:?}"
        );

        // A matched attach admits the pane again and the same request is taken.
        assert_eq!(herdr.switch_hook(&a, &b, &id), 202);
        assert_eq!(
            herdr.bound().unwrap().session_id.as_deref(),
            Some(b.as_str())
        );
    }
    {
        // A new execution of the same source is held from its launch, so its refusal leaves no
        // admission of the earlier execution open.
        let herdr = &mut Herdr::new("superseded");
        let a = uuid();
        herdr.ingress.transcript(&a);
        let older = herdr.launch(&a);
        let reader = herdr.pane(&older, |_| {});
        assert!(matches!(
            herdr.launched(&older, &a, &reader),
            HerdrSourceAttach::Published { bound: true, .. }
        ));
        herdr.retire();
        let nonce = herdr.launch(&a);
        let replaced = herdr.pane(&nonce, |e| e.root.as_mut().unwrap().pid += 9);
        let refused = herdr.launched(&nonce, &a, &replaced);
        assert!(
            matches!(refused, HerdrSourceAttach::Refused(_)),
            "{refused:?}"
        );
        let (b, lines) = (uuid(), herdr.log().len());
        herdr.ingress.transcript(&b);
        assert_eq!(herdr.switch_hook(&a, &b, &uuid()), 425);
        let bound = herdr.bound().unwrap().session_id;
        assert_eq!((bound, herdr.log().len()), (Some(a.clone()), lines));
    }
}

// O begins a channel's era only on a binding baseline its log names: a Herdr launch's own source
// is one, and a refused launch leaves none.
#[test]
fn the_o_switch_takes_a_herdr_channel_only_on_the_source_its_launch_logged_pg() {
    use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
    use crate::services::tui_o::writer::binding::BindingLog;
    use crate::services::tui_o::writer::switch::{Excluded, begin_era_checked};
    for admitted in [true, false] {
        let herdr = &mut Herdr::new(if admitted { "o-taken" } else { "o-left" });
        let a = uuid();
        let nonce = herdr.launch(&a);
        let transcript = herdr.ingress.transcript(&a);
        let change: fn(&mut HerdrPaneEvidence) = match admitted {
            true => |_| {},
            false => |e| e.root.as_mut().unwrap().pid += 9,
        };
        let reader = herdr.pane(&nonce, change);
        herdr.launched(&nonce, &a, &reader);
        let (dev, ino) = file_identity(&std::fs::metadata(&transcript).unwrap());
        let source_id = SourceId {
            session_id: a.clone(),
            path: transcript,
            dev,
            ino,
        };
        let init = Initialized {
            channel: herdr.channel,
            sources: vec![InitSource {
                source_id,
                delivery_start: 0,
                prefix_hash: String::new(),
            }],
            initial_anchor: 1,
            build_digest: "b".into(),
            at: chrono::Utc::now(),
        };
        let o_root = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, o_root.path())
            .unwrap()
            .unwrap();
        let channels = [herdr.channel];
        let (era, excluded) =
            begin_era_checked(&store, &channels, chrono::Utc::now(), &BindingLog, |_| {
                Ok(init.clone())
            })
            .unwrap();
        let reason = "no binding event binds a source attached at the switch";
        let left = Excluded {
            channel: herdr.channel,
            reason: reason.into(),
        };
        match admitted {
            true => assert_eq!(
                (era.initial_channels, excluded),
                (channels.to_vec(), vec![])
            ),
            false => assert_eq!((era.initial_channels, excluded), (vec![], vec![left])),
        }
    }
}

/// The gateway side of an O writer host reading the binding log the launch wrote; the posts
/// survive a restart so each result's posts can be counted.
struct LaunchedLog {
    posts: Arc<crate::services::tui_o::writer::host::test_io::Posts>,
    alarms: crate::services::tui_o::writer::host::test_io::Alarms,
}

impl crate::services::tui_o::writer::host::HostIo for LaunchedLog {
    type Port = crate::services::tui_o::writer::host::test_io::Posts;
    type Lease = crate::services::tui_o::writer::host::test_io::AnyLease;
    type Alarms = crate::services::tui_o::writer::host::test_io::Alarms;
    type Bindings = crate::services::tui_o::writer::binding::ChannelBindingLog;

    fn port(&self) -> impl Future<Output = Arc<Self::Port>> + Send {
        std::future::ready(Arc::clone(&self.posts))
    }

    fn lease(&self) -> Self::Lease {
        crate::services::tui_o::writer::host::test_io::AnyLease
    }

    fn alarms(&self) -> Self::Alarms {
        self.alarms.clone()
    }

    fn bindings(
        &self,
        channel: u64,
        provider: crate::services::tui_o::shadow::ShadowProvider,
    ) -> Arc<Self::Bindings> {
        Arc::new(crate::services::tui_o::writer::binding::ChannelBindingLog::new(channel, provider))
    }

    fn activation_facts(
        &self,
        _: u64,
        _: crate::services::tui_o::shadow::ShadowProvider,
    ) -> impl Future<
        Output = Result<crate::services::tui_o::writer::activation::ActivationFacts, String>,
    > + Send {
        std::future::ready(Ok(Default::default()))
    }

    fn local_custody(
        &self,
        _: u64,
        _: crate::services::tui_o::shadow::ShadowProvider,
    ) -> Result<crate::services::tui_o::writer::host::Custody, String> {
        Ok(crate::services::tui_o::writer::host::Custody::Free)
    }

    fn legacy(&self) -> Arc<dyn crate::services::tui_o::writer::adoption::LegacyView> {
        Arc::new(crate::services::tui_o::writer::adoption::NoLegacy)
    }

    fn legacy_busy(&self, _: u64) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }

    fn relaying(&self, _: u64) -> bool {
        false
    }
}

// A Herdr launch on a channel O owns: the writer posts its result, and after a stop with the next
// result unread, the restarted host takes its store back and posts each result once.
#[test]
fn a_herdr_launch_then_a_writer_stop_posts_each_result_once_from_the_store_pg() {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::tui_o::shadow::ShadowProvider;
    use crate::services::tui_o::writer::actor::POLL_INTERVAL;
    use crate::services::tui_o::writer::host::{HostParts, Readiness, start};
    let herdr = &mut Herdr::new("o-resume");
    let (channel, logical) = (herdr.channel, herdr.logical().to_owned());
    // Legacy's pane started an empty session before O took the channel.
    let legacy = uuid();
    let legacy_path = herdr.ingress.path(&legacy);
    std::fs::write(&legacy_path, b"").unwrap();
    dedupe::register_tmux_channel(&logical, channel);
    dedupe::register_launched_tmux_runtime_binding(
        &logical,
        TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: legacy_path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(legacy.clone()),
            last_offset: 0,
            relay_last_offset: None,
        },
    );

    let runtime_root = crate::config::runtime_root().unwrap();
    let _selected =
        crate::services::tui_o::cutover::test_override::force_candidates(&[(channel, ClaudeTui)]);
    let gate = Arc::new(crate::services::tui_o::ownership::OwnershipGate::default());
    gate.acquired();
    let posts = Arc::new(crate::services::tui_o::writer::host::test_io::Posts::default());
    let writer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap();
    let polls = |count: u32| {
        writer.block_on(async { tokio::time::sleep(POLL_INTERVAL * count).await });
    };
    let host = || {
        let io = Arc::new(LaunchedLog {
            posts: Arc::clone(&posts),
            alarms: Default::default(),
        });
        let parts = || HostParts {
            io: Arc::clone(&io),
            runtime_root: Some(runtime_root.clone()),
            gate: Arc::clone(&gate),
            readiness: Arc::new(Readiness::default()),
        };
        let tasks = writer.block_on(async { start(ShadowProvider::Claude, true, parts) });
        (io, tasks)
    };
    let halted = |io: &LaunchedLog| {
        let raised = io.alarms.0.lock().unwrap();
        let halted = raised.iter().filter(|(_, alarm)| {
            matches!(
                alarm,
                crate::services::tui_o::writer::WriterAlarm::Halted { .. }
            )
        });
        halted.cloned().collect::<Vec<_>>()
    };
    let (io, tasks) = host();
    polls(3);
    assert_eq!(halted(&io), []);
    // The writer's readiness is this host's own, so the launch gate is told it accepts work.
    let _launch_gate = crate::services::herdr_launch::force_writer_accepts(Some(true));
    assert!(
        crate::services::herdr_launch::o_writer_ready(channel),
        "O owns the channel on a seeded store: {:?}",
        io.alarms.0.lock().unwrap()
    );

    let a = uuid();
    let transcript = herdr.ingress.transcript(&a);
    let nonce = herdr.launch(&a);
    let reader = herdr.pane(&nonce, |_| {});
    let attached = herdr.launched(&nonce, &a, &reader);
    assert!(
        matches!(attached, HerdrSourceAttach::Published { bound: true, .. }),
        "{attached:?}"
    );
    let result = |id: &str, text: &str| {
        let row = json!({
            "type": "assistant", "uuid": format!("u-{id}"), "apiBlockIndex": 0,
            "message": {"id": id, "content": [{"type": "text", "text": text}]},
        });
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&transcript)
            .unwrap();
        std::io::Write::write_all(&mut file, format!("{row}\n").as_bytes()).unwrap();
    };
    polls(3);
    result("m1", "posted before the stop");
    polls(3);
    assert_eq!(
        posts.to(channel),
        ["posted before the stop"],
        "{:?}",
        io.alarms.0.lock().unwrap()
    );
    // Recorded with no poll in between, so the stopped writer never read it.
    result("m2", "recorded at the stop");
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    polls(2);
    let store = runtime_root.join("o_store").join(channel.to_string());
    let written = std::fs::read(store.join("init")).unwrap();

    let (io, tasks) = host();
    polls(6);
    assert_eq!(halted(&io), []);
    assert_eq!(
        std::fs::read(store.join("init")).unwrap(),
        written,
        "the restart takes the store back rather than starting another"
    );
    assert_eq!(
        posts.to(channel),
        ["posted before the stop", "recorded at the stop"]
    );
    tasks.iter().for_each(tokio::task::JoinHandle::abort);
    polls(2);
}

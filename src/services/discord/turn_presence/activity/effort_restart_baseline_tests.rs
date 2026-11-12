use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::*;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::restart_end::{self, RestartEnd};
use crate::services::tui_prompt_dedupe::binding_events as p5;

pub(super) const LABEL: &str = "fixture-effort-restart";
pub(super) const NONCE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

struct BindingRoot(Option<PathBuf>);
impl BindingRoot {
    fn new(root: &Path) -> Self {
        let saved = p5::test_root();
        p5::set_test_root(Some(root));
        Self(saved)
    }
}
impl Drop for BindingRoot {
    fn drop(&mut self) {
        p5::set_test_root(self.0.as_deref());
    }
}

pub(super) struct Fixture {
    _binding_root: BindingRoot,
    pub(super) root: tempfile::TempDir,
    pub(super) binding: SourceBinding,
    pub(super) key: Key,
}

impl Fixture {
    pub(super) fn new(channel: u64, records: &[Value]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let guard = BindingRoot::new(root.path());
        let path = root.path().join("parent.jsonl");
        std::fs::write(&path, "").unwrap();
        append(&path, records);
        let binding = SourceBinding {
            channel_id: channel,
            provider: ShadowProvider::Claude,
            source: source_id_for("parent", &path).unwrap(),
        };
        super::tests::log_turn_source(&ProviderKind::Claude, channel, LABEL, NONCE, &path);
        let key = Key {
            channel,
            provider: ShadowProvider::Claude,
            session: LABEL.into(),
            seq: 1,
        };
        Self {
            _binding_root: guard,
            root,
            binding,
            key,
        }
    }

    pub(super) fn end(&self, id: &str, open_after: u64, cutoff: u64) -> RestartEnd {
        let bytes = std::fs::read(&self.binding.source.path).unwrap();
        RestartEnd {
            end_id: format!("{}-end", self.key.channel),
            channel: self.key.channel,
            provider: self.key.provider,
            tmux_session: LABEL.into(),
            execution_nonce: NONCE.into(),
            binding_seq: 1,
            source: self.binding.source.clone(),
            cutoff,
            prefix_hash: hex::encode(Sha256::digest(&bytes[..cutoff as usize])),
            open_after,
            native_turn_id: Some(id.into()),
        }
    }

    pub(super) fn store_end(&self, end: &RestartEnd) {
        assert_eq!(
            restart_end::append(self.root.path(), end).unwrap(),
            restart_end::Appended::Durable
        );
    }

    pub(super) fn live(&self) -> LivePorts {
        LivePorts {
            root: Some(self.root.path().to_path_buf()),
        }
    }

    pub(super) fn boot(&self) -> Arc<Mutex<Watch>> {
        let (outcome, grown) = locate(&self.live(), self.key.channel, &self.key);
        let (through, _) = grown.unwrap_or((0, Instant::now()));
        Arc::new(Mutex::new(Watch {
            key: Some(self.key.clone()),
            outcome,
            through,
            grew_at: Instant::now() - STALE_OPEN_AFTER - Duration::from_secs(1),
            ..Watch::default()
        }))
    }

    pub(super) fn ask(&self, watch: &Arc<Mutex<Watch>>) -> Answer {
        let ports: Arc<dyn Ports> = Arc::new(NoEffects(self.key.seq));
        judge(
            watch,
            &ports,
            self.key.provider,
            self.key.channel,
            Target::Bound(LABEL.into()),
        )
    }
}

pub(super) struct NoEffects(pub(super) u64);
impl Ports for NoEffects {
    fn binding_seq(&self, _: u64) -> Result<u64, String> {
        Ok(self.0)
    }
    fn binding_events(&self, _: u64, _: ShadowProvider) -> Result<Vec<BindingEvent>, String> {
        Err("unexpected effect-point binding fold".into())
    }
    fn session_present(&self, _: &str) -> bool {
        true
    }
    fn final_ready(&self, _: &str) -> bool {
        false
    }
    fn claude_busy(&self, _: &str) -> claude_activity::Busy {
        false.into()
    }
    fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
        SessionLiveness::Alive
    }
    fn spawn(&self, _: Job) -> bool {
        panic!("unexpected effect-point worker")
    }
}

pub(super) fn prompt(id: &str) -> Value {
    json!({"type":"user","uuid":id,"message":{"content":"work"}})
}

pub(super) fn native_end() -> Value {
    json!({"type":"system","subtype":"turn_duration"})
}

pub(super) fn append(path: &Path, records: &[Value]) -> u64 {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for record in records {
        writeln!(file, "{record}").unwrap();
    }
    file.metadata().unwrap().len()
}

#[test]
fn same_source_open_without_closer_progresses_after_exact_end() {
    let fixture = Fixture::new(8_109_220_001, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    fixture.store_end(&fixture.end("old", cutoff, cutoff));
    let watch = fixture.boot();
    let (answer, stamp) = fixture.ask(&watch);
    assert_eq!(answer, observed(Activity::Idle, "effort_restart_ended"));
    assert_eq!(
        stamp.unwrap().closed.unwrap_err().reason,
        "effort_restart_ended"
    );
}

#[test]
fn native_closed_boundary_keeps_its_existing_meaning() {
    let fixture = Fixture::new(8_109_220_002, &[prompt("old"), native_end()]);
    let watch = fixture.boot();
    let (answer, stamp) = fixture.ask(&watch);
    assert_eq!(answer, observed(Activity::Idle, "closed"));
    let closed = stamp.unwrap().closed.unwrap();
    assert_eq!(closed.source, fixture.binding.source.path);
    assert!(!closed.aborted);
}

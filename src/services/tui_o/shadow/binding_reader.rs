//! Read-only view of which transcript feeds each shadowed channel.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use super::capture::file_identity;
use super::{BindingChange, ShadowProvider, SourceBinding, SourceId};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::tui_prompt_dedupe::{
    TuiRuntimeBinding, runtime_binding_for_tmux_session_under_source_authority as read_binding,
};

/// A channel the shadow watches and the tmux session that serves it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowTarget {
    pub channel_id: u64,
    pub tmux_session: String,
}

/// The binding fields the shadow needs, copied out of the live runtime binding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindingView {
    pub provider: ShadowProvider,
    pub output_path: String,
    pub session_id: Option<String>,
}

/// `None` means no fresh view this poll, not that the binding went away.
pub trait BindingLookup: Send + Sync {
    fn lookup(&self, tmux_session: &str) -> Option<BindingView>;
}

/// TUI runtimes map to their native transcript, not the relay copy; others map to nothing.
pub fn view_from_binding(binding: &TuiRuntimeBinding) -> Option<BindingView> {
    let provider = match binding.runtime_kind {
        RuntimeHandoffKind::ClaudeTui => ShadowProvider::Claude,
        RuntimeHandoffKind::CodexTui => ShadowProvider::Codex,
        _ => return None,
    };
    let (output_path, session_id) = (binding.output_path.clone(), binding.session_id.clone());
    Some(BindingView {
        provider,
        output_path,
        session_id,
    })
}

/// Production lookup; a busy source lock yields `None` instead of blocking the relay.
pub struct LiveBindingLookup;

impl BindingLookup for LiveBindingLookup {
    fn lookup(&self, tmux_session: &str) -> Option<BindingView> {
        crate::services::tmux_common::try_with_tmux_source_authority(tmux_session, read_binding)
            .flatten()
            .as_ref()
            .and_then(view_from_binding)
    }
}

pub struct BindingReader {
    lookup: Box<dyn BindingLookup>,
    targets: Vec<ShadowTarget>,
    current: HashMap<u64, SourceBinding>,
}

impl BindingReader {
    pub fn new(lookup: Box<dyn BindingLookup>, targets: Vec<ShadowTarget>) -> Self {
        let current = HashMap::new();
        Self {
            lookup,
            targets,
            current,
        }
    }

    pub fn targets(&self) -> &[ShadowTarget] {
        &self.targets
    }

    pub fn current(&self, channel_id: u64) -> Option<&SourceBinding> {
        self.current.get(&channel_id)
    }

    /// Re-reads every target and returns the bindings whose source changed.
    pub fn poll(&mut self) -> Vec<BindingChange> {
        let mut changes = Vec::new();
        for target in &self.targets {
            let Some(view) = self.lookup.lookup(&target.tmux_session) else {
                continue;
            };
            let session_id = view.session_id.unwrap_or_default();
            // A transcript that does not exist yet is retried on the next poll.
            let Ok(source) = source_id_for(&session_id, Path::new(&view.output_path)) else {
                continue;
            };
            let (channel_id, provider) = (target.channel_id, view.provider);
            let new = SourceBinding {
                channel_id,
                provider,
                source,
            };
            if self.current.get(&channel_id) != Some(&new) {
                let old = self.current.insert(channel_id, new.clone());
                let at = chrono::Utc::now();
                changes.push(BindingChange {
                    channel_id,
                    old,
                    new: Some(new),
                    at,
                });
            }
        }
        changes
    }
}

/// Stats `path` without opening it.
pub fn source_id_for(session_id: &str, path: &Path) -> io::Result<SourceId> {
    let (dev, ino) = file_identity(&std::fs::metadata(path)?);
    let (session_id, path) = (session_id.to_string(), path.to_path_buf());
    Ok(SourceId {
        session_id,
        path,
        dev,
        ino,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct FakeLookup(Arc<Mutex<Option<BindingView>>>);

    impl BindingLookup for FakeLookup {
        fn lookup(&self, _tmux_session: &str) -> Option<BindingView> {
            self.0.lock().unwrap().clone()
        }
    }

    #[test]
    fn binding_reader_reports_source_changes_once_and_keeps_the_last_on_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
        std::fs::write(&a, b"").unwrap();
        let slot = Arc::new(Mutex::new(None));
        let target = ShadowTarget {
            channel_id: 42,
            tmux_session: "shadow-test".into(),
        };
        let mut reader = BindingReader::new(Box::new(FakeLookup(slot.clone())), vec![target]);
        let point_at = |path: &Path| {
            let output_path = path.display().to_string();
            let view = BindingView {
                provider: ShadowProvider::Claude,
                output_path,
                session_id: None,
            };
            *slot.lock().unwrap() = Some(view);
        };

        point_at(&b);
        assert!(
            reader.poll().is_empty(),
            "a missing transcript is retried later"
        );
        point_at(&a);
        assert_eq!(reader.poll().len(), 1);
        assert!(reader.poll().is_empty());
        std::fs::write(&b, b"").unwrap();
        point_at(&b);
        let rotated = reader.poll();
        assert_eq!(
            rotated[0].old.as_ref().map(|old| old.source.path.clone()),
            Some(a)
        );
        *slot.lock().unwrap() = None;
        assert!(reader.poll().is_empty());
        assert_eq!(
            reader.current(42).map(|now| now.source.path.clone()),
            Some(b)
        );
    }

    #[test]
    fn binding_view_reads_the_native_transcript_of_tui_runtimes_only() {
        let mut binding = TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::CodexTui,
            output_path: "/r/rollout.jsonl".into(),
            relay_output_path: Some("/r/relay.jsonl".into()),
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        };
        let view = view_from_binding(&binding).unwrap();
        assert_eq!(view.provider, ShadowProvider::Codex);
        assert_eq!(view.output_path, "/r/rollout.jsonl");
        binding.runtime_kind = RuntimeHandoffKind::ProcessBackend;
        assert!(view_from_binding(&binding).is_none());
    }
}

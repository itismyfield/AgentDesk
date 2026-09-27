//! Read-only view of which transcript feeds each shadowed channel.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use super::{BindingChange, ShadowProvider, SourceBinding, SourceId};

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

pub trait BindingLookup: Send + Sync {
    fn lookup(&self, tmux_session: &str) -> Option<BindingView>;
}

/// Production lookup over the in-memory TUI runtime binding; never blocks the relay.
pub struct LiveBindingLookup;

impl BindingLookup for LiveBindingLookup {
    fn lookup(&self, tmux_session: &str) -> Option<BindingView> {
        let _ = tmux_session;
        todo!()
    }
}

pub struct BindingReader {
    lookup: Box<dyn BindingLookup>,
    targets: Vec<ShadowTarget>,
    current: HashMap<u64, SourceBinding>,
}

impl BindingReader {
    pub fn new(lookup: Box<dyn BindingLookup>, targets: Vec<ShadowTarget>) -> Self {
        Self {
            lookup,
            targets,
            current: HashMap::new(),
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
        let _ = &self.lookup;
        todo!()
    }
}

/// Stats `path` without opening it for write; dev/ino are zero where the platform lacks them.
pub fn source_id_for(session_id: &str, path: &Path) -> io::Result<SourceId> {
    let _ = (session_id, path);
    todo!()
}

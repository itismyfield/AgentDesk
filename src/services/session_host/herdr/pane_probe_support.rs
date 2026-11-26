//! The fake OS and launch context the pane probe tests and the herdr socket rig share (KRX-TS): upper-crate
//! rig tests drive the real probe through them once KRX is its own crate.
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};

pub(crate) const SHELL: u32 = 10;
pub(crate) const PROVIDER: u32 = 20;
pub(crate) const CHILD: u32 = 30;

pub(crate) fn at(seconds: u64) -> ProcessStart {
    ProcessStart {
        identity: StartIdentity::Darwin { seconds, micros: 0 },
        wall_clock: UNIX_EPOCH + Duration::from_secs(seconds),
    }
}

/// Parents by pid; starts and environments in order per pid, the last one repeating. A pid
/// with no environments of its own reads `environ`.
pub struct FakeOs {
    pub(crate) parents: HashMap<u32, u32>,
    pub(crate) starts: Mutex<HashMap<u32, VecDeque<ProcessStart>>>,
    pub(crate) environ: Result<Vec<String>, RestoreUnverified>,
    pub(crate) environs: Mutex<HashMap<u32, VecDeque<Vec<String>>>>,
}

impl FakeOs {
    /// Shell 10 runs provider 20, which runs child 30, all started after the launch.
    pub fn launched(environ: Vec<String>) -> Self {
        let starts = [
            (SHELL, at(1_001)),
            (PROVIDER, at(1_002)),
            (CHILD, at(1_003)),
        ];
        Self {
            parents: HashMap::from([(SHELL, 1), (PROVIDER, SHELL), (CHILD, PROVIDER)]),
            starts: Mutex::new(
                starts
                    .into_iter()
                    .map(|(pid, start)| (pid, VecDeque::from([start])))
                    .collect(),
            ),
            environ: Ok(environ),
            environs: Mutex::default(),
        }
    }

    pub(crate) fn starting(self, pid: u32, starts: &[ProcessStart]) -> Self {
        self.starts
            .lock()
            .unwrap()
            .insert(pid, starts.iter().copied().collect());
        self
    }

    pub(crate) fn reading(self, pid: u32, environs: &[Vec<String>]) -> Self {
        self.environs
            .lock()
            .unwrap()
            .insert(pid, environs.iter().cloned().collect());
        self
    }

    /// Another child of the shell, started after the launch, whose environment reads `environ`.
    pub(crate) fn startup_child(mut self, pid: u32, environ: Vec<String>) -> Self {
        self.parents.insert(pid, SHELL);
        self.starting(pid, &[at(1_001)]).reading(pid, &[environ])
    }
}

impl ProcessOs for FakeOs {
    fn parent(&self, pid: u32) -> Result<u32, RestoreUnverified> {
        self.parents
            .get(&pid)
            .copied()
            .ok_or(RestoreUnverified::ProcessUnreadable)
    }

    fn start(&self, pid: u32) -> Result<ProcessStart, RestoreUnverified> {
        let mut starts = self.starts.lock().unwrap();
        let queue = starts
            .get_mut(&pid)
            .ok_or(RestoreUnverified::ProcessUnreadable)?;
        Ok(if queue.len() > 1 {
            queue.pop_front().unwrap()
        } else {
            queue[0]
        })
    }

    fn environ(&self, pid: u32) -> Result<Vec<String>, RestoreUnverified> {
        let mut environs = self.environs.lock().unwrap();
        match environs.get_mut(&pid) {
            Some(queue) if queue.len() > 1 => Ok(queue.pop_front().unwrap()),
            Some(queue) => Ok(queue[0].clone()),
            None => self.environ.clone(),
        }
    }

    fn exec_path(&self, _pid: u32) -> Option<String> {
        Some("/opt/claude/2.1.288".into())
    }
}

/// Launch context `nonce`'s file under the test runtime root, as a launch publishes it.
pub fn context(nonce: &str) -> PathBuf {
    let context = BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: nonce.into(),
        tmux_session: "AgentDesk-claude-probe".into(),
        channel_id: Some(1),
        owner_runtime_root: "test".into(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
        resume_start: None,
    };
    PreparedIncarnation::create(context).unwrap().path
}

pub const NONCE: &str = "0123456789abcdef0123456789abcdef";

pub fn env_naming(path: &Path) -> Vec<String> {
    vec![
        "HOME=/Users/x".into(),
        format!("AGENTDESK_BINDING_CONTEXT={}", path.display()),
    ]
}

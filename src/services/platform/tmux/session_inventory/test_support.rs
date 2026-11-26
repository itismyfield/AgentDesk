//! Per-channel tmux command, namespace and probe-count fixtures the inventory probe reads under tests.

use super::*;
use std::cell::RefCell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

thread_local! { pub(super) static COMMAND: RefCell<Option<(Command, Option<PathBuf>)>> = const { RefCell::new(None) }; }

static CHANNEL_PROBES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<u64, Arc<AtomicUsize>>>,
> = std::sync::LazyLock::new(Default::default);
pub(crate) struct ChannelProbeTrace(u64, Arc<AtomicUsize>);
impl ChannelProbeTrace {
    pub(crate) fn new(channel: u64) -> Self {
        let count = Arc::new(AtomicUsize::new(0));
        assert!(
            CHANNEL_PROBES
                .lock()
                .unwrap()
                .insert(channel, count.clone())
                .is_none()
        );
        Self(channel, count)
    }
    pub(crate) fn calls(&self) -> usize {
        self.1.load(Ordering::SeqCst)
    }
}
impl Drop for ChannelProbeTrace {
    fn drop(&mut self) {
        CHANNEL_PROBES.lock().unwrap().remove(&self.0);
    }
}
pub(super) fn record_channel_probe(channel: u64) {
    if let Some(count) = CHANNEL_PROBES.lock().unwrap().get(&channel) {
        count.fetch_add(1, Ordering::SeqCst);
    }
}

pub(super) fn take_command() -> Option<(Command, Option<PathBuf>)> {
    COMMAND.with(|slot| slot.borrow_mut().take())
}

static NAMESPACES: std::sync::LazyLock<std::sync::Mutex<std::collections::BTreeMap<u64, PathBuf>>> =
    std::sync::LazyLock::new(Default::default);
static CHANNEL_COMMANDS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<u64, PathBuf>>,
> = std::sync::LazyLock::new(Default::default);
pub(crate) struct ChannelCommand(u64, PathBuf);
impl ChannelCommand {
    pub(crate) fn new(channel: u64, executable: PathBuf) -> Self {
        assert!(
            CHANNEL_COMMANDS
                .lock()
                .unwrap()
                .insert(channel, executable.clone())
                .is_none()
        );
        Self(channel, executable)
    }
}
impl Drop for ChannelCommand {
    fn drop(&mut self) {
        assert_eq!(
            CHANNEL_COMMANDS.lock().unwrap().remove(&self.0),
            Some(self.1.clone())
        );
    }
}
pub(crate) struct ChannelNamespace(u64, PathBuf);
impl ChannelNamespace {
    pub(crate) fn new(channel: u64, socket: PathBuf) -> Self {
        assert!(
            NAMESPACES
                .lock()
                .unwrap()
                .insert(channel, socket.clone())
                .is_none()
        );
        Self(channel, socket)
    }
    pub(crate) fn start_command(&self) -> Command {
        namespace_command(&self.1)
    }
}
impl Drop for ChannelNamespace {
    fn drop(&mut self) {
        NAMESPACES.lock().unwrap().remove(&self.0);
    }
}
fn namespace_command(socket: &Path) -> Command {
    let mut command = tmux_command().unwrap();
    command
        .arg("-S")
        .arg(socket)
        .arg("-f")
        .arg("/dev/null")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE");
    command
}
pub(super) fn channel_command(channel: u64) -> Option<(Command, Option<PathBuf>)> {
    if let Some(executable) = CHANNEL_COMMANDS.lock().unwrap().get(&channel).cloned() {
        return Some((Command::new(executable), None));
    }
    let socket = NAMESPACES.lock().unwrap().get(&channel).cloned()?;
    Some((namespace_command(&socket), Some(socket)))
}
pub(crate) fn assert_same_start_context(
    observation: &Observation,
    command: &Command,
    socket: Option<PathBuf>,
) {
    assert!(observation.current());
    let version = Command::new(command.get_program())
        .arg("-V")
        .output()
        .unwrap();
    assert!(version.status.success());
    eprintln!(
        "S1_START_CONTEXT program={} version={} context={:?} skip=0",
        Path::new(command.get_program()).display(),
        String::from_utf8(version.stdout).unwrap().trim(),
        observation.diagnostics()
    );
    assert_eq!(
        observation.context.as_ref(),
        Some(&Namespace::capture(command, socket)),
        "probe and selected start changed binary/uid/namespace"
    );
}
pub(crate) fn production_start_command() -> Command {
    tmux_command().unwrap()
}

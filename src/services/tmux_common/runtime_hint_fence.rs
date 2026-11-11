//! Serializes runtime hint mutations by physical directory entry, across controllers.

#[cfg(unix)]
use std::{
    collections::HashMap,
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, fs::OpenOptionsExt},
    },
    path::PathBuf,
    sync::{LazyLock, Mutex},
    thread::{self, ThreadId},
};

#[cfg(unix)]
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
struct EntryKey(u64, u64, Vec<u8>);

#[cfg(unix)]
static HELD: LazyLock<Mutex<HashMap<EntryKey, OwnerId>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OwnerId {
    Task(tokio::task::Id),
    Thread(ThreadId),
}

#[cfg(unix)]
fn owner_id() -> OwnerId {
    tokio::task::try_id().map_or_else(|| OwnerId::Thread(thread::current().id()), OwnerId::Task)
}

#[cfg(unix)]
fn source_snapshot(
    session: &str,
    nonblocking: bool,
) -> Option<crate::services::tui_prompt_dedupe::RuntimeObservation> {
    use crate::services::tui_prompt_dedupe::{self as dedupe, RuntimeObservation};
    let snapshot = |authority: &super::TmuxSourceAuthority<'_>| {
        let mut observation = dedupe::runtime_observation_under_source_authority(authority);
        if let RuntimeObservation::Present(binding)
        | RuntimeObservation::Expired(binding)
        | RuntimeObservation::Withheld(binding) = &mut observation
        {
            binding.last_offset = 0;
            binding.relay_last_offset = None;
        }
        observation
    };
    if nonblocking {
        super::try_with_tmux_source_authority(session, snapshot)
    } else {
        Some(super::with_tmux_source_authority(session, snapshot))
    }
}

#[cfg(unix)]
#[derive(Debug)]
struct Entry {
    path: PathBuf,
    directory: File,
    key: EntryKey,
}

#[cfg(unix)]
#[derive(PartialEq, Eq)]
struct Witness {
    device: u64,
    inode: u64,
    bytes: Option<Vec<u8>>,
}

#[cfg(unix)]
impl Entry {
    fn open(path: PathBuf) -> Result<Self, String> {
        let parent = path.parent().ok_or("runtime hint has no parent")?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(parent)
            .map_err(|error| format!("open runtime hint parent: {error}"))?;
        let metadata = directory.metadata().map_err(|error| error.to_string())?;
        let basename = path.file_name().ok_or("runtime hint has no basename")?;
        let key = EntryKey(metadata.dev(), metadata.ino(), basename.as_bytes().to_vec());
        let entry = Self {
            path,
            directory,
            key,
        };
        entry.validate()?;
        Ok(entry)
    }

    fn validate(&self) -> Result<(), String> {
        let parent = std::fs::metadata(self.path.parent().ok_or("runtime hint parent missing")?)
            .map_err(|error| format!("recheck runtime hint parent: {error}"))?;
        if (parent.dev(), parent.ino()) != (self.key.0, self.key.1) {
            return Err("runtime hint parent was replaced".into());
        }
        match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) if !metadata.is_file() || metadata.nlink() != 1 => {
                Err("runtime hint entry has an ambiguous alias or file type".into())
            }
            Ok(_) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("inspect runtime hint entry: {error}")),
        }
    }

    fn open_at(&self, name: &[u8], flags: i32) -> Result<Option<File>, String> {
        let name = CString::new(name).map_err(|_| "runtime hint name contains NUL")?;
        // The retained directory and one component keep opening relative to the judged parent.
        let descriptor = unsafe {
            libc::openat(
                self.directory.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
                0o600,
            )
        };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::NotFound {
                return Ok(None);
            }
            return Err(format!("open runtime hint entry: {error}"));
        }
        // openat returned a new descriptor, owned solely by this File.
        Ok(Some(unsafe { File::from_raw_fd(descriptor) }))
    }

    fn lock_name(&self) -> Vec<u8> {
        [b".".as_slice(), &self.key.2, b".agentdesk-hint.lock"].concat()
    }

    fn snapshot(&self, owner: &str) -> Result<Vec<Option<Witness>>, String> {
        [
            "owner",
            "host_kind",
            "spawn_nonce",
            "runtime-kind",
            "generation",
            "codex-tui-rollout.json",
        ]
        .into_iter()
        .map(|extension| {
            let path = self.path.with_extension(extension);
            let name = path
                .file_name()
                .ok_or("runtime hint sibling name missing")?;
            let Some(file) = self.open_at(name.as_bytes(), libc::O_RDONLY)? else {
                return Ok(None);
            };
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !(metadata.is_file() || extension == "host_kind" && metadata.is_dir()) {
                return Err("runtime hint witness is not a regular file".into());
            }
            let mut bytes = Vec::new();
            if metadata.is_file() {
                file.take(4097)
                    .read_to_end(&mut bytes)
                    .map_err(|error| error.to_string())?;
            }
            if bytes.len() > 4096 {
                return Err("runtime hint witness exceeds limit".into());
            }
            if extension == "owner" && bytes != owner.as_bytes() {
                return Err("runtime hint owner mapping is ambiguous".into());
            }
            Ok(Some(Witness {
                device: metadata.dev(),
                inode: metadata.ino(),
                bytes: metadata.is_file().then_some(bytes),
            }))
        })
        .collect()
    }
}

#[cfg(unix)]
pub(crate) struct TmuxPublisherObservation {
    session: String,
    entries: Vec<Entry>,
    witnesses: Vec<Vec<Option<Witness>>>,
    source: crate::services::tui_prompt_dedupe::RuntimeObservation,
}

#[cfg(unix)]
impl TmuxPublisherObservation {
    pub(crate) fn capture(session: &str) -> Result<Self, String> {
        let _ = super::agentdesk_temp_dir();
        Self::capture_current(session)
    }

    fn capture_current(session: &str) -> Result<Self, String> {
        let entries = RuntimeHintFence::paths(session)
            .into_iter()
            .map(Entry::open)
            .collect::<Result<Vec<_>, _>>()?;
        let owner = super::current_tmux_owner_marker();
        let witnesses = entries
            .iter()
            .map(|entry| entry.snapshot(&owner))
            .collect::<Result<Vec<_>, _>>()?;
        let source = source_snapshot(session, false).ok_or("runtime hint source unavailable")?;
        Ok(Self {
            session: session.into(),
            entries,
            witnesses,
            source,
        })
    }

    pub(crate) fn acquire(self) -> Result<RuntimeHintFence, String> {
        self.entries.iter().try_for_each(Entry::validate)?;
        let fence = RuntimeHintFence::acquire_initialized(&self.session, false)?;
        let current = Self::capture_current(&self.session)?;
        if self
            .entries
            .iter()
            .zip(&current.entries)
            .any(|(old, fresh)| old.key != fresh.key)
            || self.witnesses != current.witnesses
            || self.source != current.source
        {
            return Err("tmux publisher authority changed after its initial observation".into());
        }
        fence.validate_tmux_publisher(&self.session)?;
        Ok(fence)
    }
}

#[derive(Debug)]
pub(crate) struct RuntimeHintFence {
    #[cfg(unix)]
    entries: Vec<Entry>,
    #[cfg(unix)]
    locks: Vec<(EntryKey, File)>,
}

impl RuntimeHintFence {
    pub(crate) fn acquire(session: &str) -> Result<Self, String> {
        Self::acquire_initialized(session, true)
    }

    fn acquire_initialized(session: &str, initialize: bool) -> Result<Self, String> {
        #[cfg(unix)]
        Self::reject_reentry(session)?;
        if initialize {
            let _ = super::agentdesk_temp_dir();
        }
        let acquired = match tokio::runtime::Handle::try_current()
            .map(|handle| handle.runtime_flavor())
        {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| Self::acquire_mode(session, false))
            }
            Ok(tokio::runtime::RuntimeFlavor::CurrentThread) => Self::acquire_mode(session, true),
            _ => Self::acquire_mode(session, false),
        }?;
        let fence = acquired
            .ok_or("runtime hint fence busy; asynchronous callers must await acquisition")?;
        fence.validate(session)?;
        Ok(fence)
    }

    pub(crate) async fn acquire_async(session: &str) -> Result<Self, String> {
        #[cfg(unix)]
        Self::reject_reentry(session)?;
        let blocking_session = session.to_owned();
        let fence = tokio::task::spawn_blocking(move || {
            let _ = super::agentdesk_temp_dir();
            Self::acquire_mode(&blocking_session, false)
        })
        .await
        .map_err(|error| format!("runtime hint fence acquisition task: {error}"))??
        .ok_or("runtime hint fence busy")?;
        fence.validate(session)?;
        Ok(fence)
    }

    #[cfg(unix)]
    fn reject_reentry(session: &str) -> Result<(), String> {
        let held = HELD.lock().unwrap_or_else(|error| error.into_inner());
        if held.values().any(|owner| *owner == owner_id()) {
            for entry in Self::paths(session).into_iter().map(Entry::open) {
                if held.get(&entry?.key) == Some(&owner_id()) {
                    return Err("runtime hint fence reentry requires the held guard".into());
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    pub(crate) fn try_acquire(session: &str) -> Result<Option<Self>, String> {
        let _ = super::agentdesk_temp_dir();
        Self::acquire_mode(session, true)
    }

    fn acquire_mode(session: &str, nonblocking: bool) -> Result<Option<Self>, String> {
        #[cfg(unix)]
        {
            let Some(before) = source_snapshot(session, nonblocking) else {
                return Ok(None);
            };
            let fence = Self::acquire_paths(
                Self::paths(session),
                &super::current_tmux_owner_marker(),
                nonblocking,
            )?;
            if fence.is_some() {
                let Some(after) = source_snapshot(session, nonblocking) else {
                    return Ok(None);
                };
                if after != before {
                    return Err("runtime hint source changed while waiting for fence".into());
                }
            }
            Ok(fence)
        }
        #[cfg(not(unix))]
        {
            let _ = (session, nonblocking);
            Err("runtime hint fence is unavailable on this platform".into())
        }
    }

    #[cfg(unix)]
    fn paths(session: &str) -> Vec<PathBuf> {
        let basename = format!(
            "{}.{}",
            super::session_temp_prefix(session),
            super::TMUX_RUNTIME_KIND_TEMP_EXT
        );
        let canonical = super::persistent_sessions_dir().unwrap_or_else(std::env::temp_dir);
        vec![
            canonical.join(&basename),
            std::env::temp_dir().join(basename),
        ]
    }

    #[cfg(unix)]
    fn acquire_paths(
        paths: Vec<PathBuf>,
        owner: &str,
        nonblocking: bool,
    ) -> Result<Option<Self>, String> {
        let entries = paths
            .into_iter()
            .map(Entry::open)
            .collect::<Result<Vec<_>, _>>()?;
        let snapshots = entries
            .iter()
            .map(|entry| entry.snapshot(owner))
            .collect::<Result<Vec<_>, _>>()?;
        #[cfg(all(test, unix))]
        if let Some(hook) = tests::BEFORE_LOCK.with_borrow_mut(Option::take) {
            hook();
        }
        let mut ordered = entries.iter().collect::<Vec<_>>();
        ordered.sort_by(|a, b| a.key.cmp(&b.key));
        ordered.dedup_by(|a, b| a.key == b.key);
        {
            let held = HELD.lock().unwrap_or_else(|error| error.into_inner());
            if ordered
                .iter()
                .any(|entry| held.get(&entry.key) == Some(&owner_id()))
            {
                return if nonblocking {
                    Ok(None)
                } else {
                    Err("runtime hint fence reentry requires the held guard".into())
                };
            }
        }
        let mut fence = Self {
            entries: Vec::new(),
            locks: Vec::new(),
        };
        for entry in ordered {
            let file = entry
                .open_at(&entry.lock_name(), libc::O_RDWR | libc::O_CREAT)?
                .ok_or("runtime hint sidecar disappeared")?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file()
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
            {
                return Err("runtime hint sidecar ownership is ambiguous".into());
            }
            let operation = libc::LOCK_EX | if nonblocking { libc::LOCK_NB } else { 0 };
            loop {
                if unsafe { libc::flock(file.as_raw_fd(), operation) } == 0 {
                    break;
                }
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if nonblocking && error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(None);
                }
                return Err(format!("lock runtime hint fence: {error}"));
            }
            fence.locks.push((entry.key.clone(), file));
        }
        fence.entries = entries;
        fence.validate_entries()?;
        if fence
            .entries
            .iter()
            .map(|entry| entry.snapshot(owner))
            .collect::<Result<Vec<_>, _>>()?
            != snapshots
        {
            return Err("runtime hint authority changed while waiting for fence".into());
        }
        let mut held = HELD.lock().unwrap_or_else(|error| error.into_inner());
        for (key, _) in &fence.locks {
            held.insert(key.clone(), owner_id());
        }
        Ok(Some(fence))
    }

    pub(crate) fn validate(&self, session: &str) -> Result<(), String> {
        #[cfg(unix)]
        {
            let mut keys = Self::paths(session)
                .into_iter()
                .map(Entry::open)
                .map(|entry| entry.map(|entry| entry.key))
                .collect::<Result<Vec<_>, _>>()?;
            keys.sort();
            keys.dedup();
            if keys
                != self
                    .locks
                    .iter()
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>()
            {
                return Err("runtime hint fence does not cover this path set".into());
            }
            self.validate_entries()?;
            let mut held = HELD.lock().unwrap_or_else(|error| error.into_inner());
            for (key, _) in &self.locks {
                held.insert(key.clone(), owner_id());
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            let _ = session;
            Err("runtime hint fence is unavailable on this platform".into())
        }
    }

    #[cfg(unix)]
    pub(crate) fn validate_tmux_publisher(&self, session: &str) -> Result<(), String> {
        self.validate(session)?;
        use super::host_marker::{HostKindMarker, read_host_kind_marker};
        match read_host_kind_marker(session) {
            HostKindMarker::Absent
            | HostKindMarker::Known(crate::services::session_host::HostKind::Tmux) => Ok(()),
            _ => Err("runtime hint publisher is no longer tmux hosted".into()),
        }
    }

    #[cfg(unix)]
    fn validate_entries(&self) -> Result<(), String> {
        for entry in &self.entries {
            entry.validate()?;
            let lock = self
                .locks
                .iter()
                .find(|(key, _)| key == &entry.key)
                .ok_or("runtime hint entry is not fenced")?;
            let opened = lock.1.metadata().map_err(|error| error.to_string())?;
            let current = entry
                .open_at(&entry.lock_name(), libc::O_RDONLY)?
                .ok_or("runtime hint sidecar was removed")?
                .metadata()
                .map_err(|error| error.to_string())?;
            if (opened.dev(), opened.ino()) != (current.dev(), current.ino()) {
                return Err("runtime hint sidecar was replaced".into());
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for RuntimeHintFence {
    fn drop(&mut self) {
        let mut held = HELD.lock().unwrap_or_else(|error| error.into_inner());
        for (key, file) in &self.locks {
            held.remove(key);
            let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        }
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use crate::services::tui_prompt_dedupe::TuiRuntimeBinding;
    use std::{
        io::{BufRead, BufReader, Write},
        path::Path,
        process::{Child, Command, Stdio},
        sync::mpsc,
        time::Duration,
    };

    const WAIT: Duration = Duration::from_secs(10);
    const HELPER: &str = "services::tmux_common::runtime_hint_fence::tests::fence_subprocess_entry";
    const MODE: &str = "ADK_HINT_FENCE_TEST_MODE";
    thread_local! {
        pub(super) static BEFORE_LOCK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
    }

    fn before_lock(hook: impl FnOnce() + 'static) {
        BEFORE_LOCK.with_borrow_mut(|pending| *pending = Some(Box::new(hook)));
    }

    fn fixture() -> (tempfile::TempDir, Vec<PathBuf>) {
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for name in ["canonical", "legacy"] {
            let parent = directory.path().join(name);
            std::fs::create_dir(&parent).unwrap();
            paths.push(parent.join("fixture.runtime-kind"));
        }
        (directory, paths)
    }

    fn start_child(mode: &str, paths: &[PathBuf]) -> (Child, mpsc::Receiver<String>) {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", HELPER, "--nocapture"])
            .env(MODE, mode)
            .env("ADK_HINT_FENCE_TEST_CANONICAL", &paths[0])
            .env("ADK_HINT_FENCE_TEST_LEGACY", &paths[1])
            .env_remove("AGENTDESK_CONFIG")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let line = line.unwrap();
                if let Some(signal) = line.split("FENCE_SIGNAL:").nth(1) {
                    let _ = tx.send(signal.to_string());
                }
            }
        });
        (child, rx)
    }

    fn signal(value: &str) {
        println!("FENCE_SIGNAL:{value}");
        io::stdout().flush().unwrap();
    }

    #[test]
    fn fence_subprocess_entry() {
        let Ok(mode) = std::env::var(MODE) else {
            return;
        };
        let paths = [
            "ADK_HINT_FENCE_TEST_CANONICAL",
            "ADK_HINT_FENCE_TEST_LEGACY",
        ]
        .map(|name| PathBuf::from(std::env::var_os(name).unwrap()));
        if mode == "normal_writer" {
            signal("READY");
            super::super::write_tmux_runtime_kind_marker(
                "fence-normal-writer",
                crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            )
            .unwrap();
            signal("WRITTEN");
            return;
        }
        assert!(
            RuntimeHintFence::acquire_paths(paths.to_vec(), "mine", true)
                .unwrap()
                .is_none()
        );
        signal("CONTENDED");
        let mut go = [0];
        io::stdin().read_exact(&mut go).unwrap();
        let fence = RuntimeHintFence::acquire_paths(paths.to_vec(), "mine", false)
            .unwrap()
            .unwrap();
        fence.validate_entries().unwrap();
        signal("ACQUIRED");
    }

    #[test]
    fn two_processes_with_different_canonical_paths_share_the_legacy_entry_fence() {
        let (directory, paths) = fixture();
        let other = directory.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let alias = directory.path().join("legacy-alias");
        std::os::unix::fs::symlink(paths[1].parent().unwrap(), &alias).unwrap();
        let other_paths = vec![
            other.join("other.runtime-kind"),
            alias.join("fixture.runtime-kind"),
        ];
        let fence = RuntimeHintFence::acquire_paths(paths, "mine", false)
            .unwrap()
            .unwrap();
        let (mut child, rx) = start_child("contender", &other_paths);
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), "CONTENDED");
        drop(fence);
        child.stdin.take().unwrap().write_all(b"g").unwrap();
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), "ACQUIRED");
        assert!(child.wait().unwrap().success());
    }

    #[test]
    fn normal_runtime_kind_writer_waits_for_the_same_physical_fence() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-normal-writer";
        let paths = RuntimeHintFence::paths(session);
        let fence = RuntimeHintFence::acquire(session).unwrap();
        let (mut child, rx) = start_child("normal_writer", &paths);
        assert_eq!(rx.recv_timeout(WAIT).unwrap(), "READY");
        let early = rx.recv_timeout(Duration::from_millis(250));
        let absent_while_held = !paths[0].exists();
        drop(fence);
        let completed = match early.as_ref() {
            Ok(_) => true,
            Err(_) => rx.recv_timeout(WAIT).is_ok_and(|line| line == "WRITTEN"),
        };
        let successful = child.wait().unwrap().success();
        assert!(
            matches!(early, Err(mpsc::RecvTimeoutError::Timeout)),
            "normal writer bypassed the fence: {early:?}"
        );
        assert!(
            absent_while_held,
            "normal writer changed the marker while another controller held its fence"
        );
        assert!(completed && successful);
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"codex_tui");
    }

    #[test]
    fn directory_aliases_and_leaf_recreation_keep_one_entry_lock() {
        let (directory, paths) = fixture();
        let alias = directory.path().join("canonical-alias");
        std::os::unix::fs::symlink(paths[0].parent().unwrap(), &alias).unwrap();
        let aliased = alias.join("fixture.runtime-kind");
        let fence =
            RuntimeHintFence::acquire_paths(vec![paths[0].clone(), aliased.clone()], "mine", false)
                .unwrap()
                .unwrap();
        assert_eq!(fence.locks.len(), 1);
        std::fs::write(&paths[0], "codex_tui").unwrap();
        std::fs::remove_file(&paths[0]).unwrap();
        std::fs::write(&paths[0], "codex_tui").unwrap();
        fence.validate_entries().unwrap();
        assert!(
            RuntimeHintFence::acquire_paths(vec![aliased], "mine", true)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn replacement_parent_and_leaf_symlink_are_rejected_without_mutation() {
        let (directory, paths) = fixture();
        let fence = RuntimeHintFence::acquire_paths(paths.clone(), "mine", false)
            .unwrap()
            .unwrap();
        let parent = paths[0].parent().unwrap();
        std::fs::rename(parent, directory.path().join("old-parent")).unwrap();
        std::fs::create_dir(parent).unwrap();
        std::fs::write(&paths[0], "replacement").unwrap();
        assert!(fence.validate_entries().is_err());
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"replacement");
        drop(fence);
        std::fs::remove_file(&paths[0]).unwrap();
        std::os::unix::fs::symlink(&paths[1], &paths[0]).unwrap();
        assert!(RuntimeHintFence::acquire_paths(paths.clone(), "mine", false).is_err());
        std::fs::remove_file(&paths[0]).unwrap();
        std::fs::write(&paths[1], "hardlinked").unwrap();
        std::fs::hard_link(&paths[1], &paths[0]).unwrap();
        assert!(RuntimeHintFence::acquire_paths(paths.clone(), "mine", false).is_err());
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"hardlinked");
    }

    #[test]
    fn ambiguous_owner_and_replaced_sidecar_are_rejected() {
        let (_directory, paths) = fixture();
        std::fs::write(paths[0].with_extension("owner"), "mine").unwrap();
        std::fs::write(paths[1].with_extension("owner"), "foreign").unwrap();
        assert!(RuntimeHintFence::acquire_paths(paths.clone(), "mine", false).is_err());
        assert_eq!(
            std::fs::read(paths[1].with_extension("owner")).unwrap(),
            b"foreign"
        );
        std::fs::remove_file(paths[1].with_extension("owner")).unwrap();
        let fence = RuntimeHintFence::acquire_paths(paths, "mine", false)
            .unwrap()
            .unwrap();
        let entry = &fence.entries[0];
        let sidecar = entry
            .path
            .parent()
            .unwrap()
            .join(std::ffi::OsStr::from_bytes(&entry.lock_name()));
        std::fs::rename(&sidecar, sidecar.with_extension("previous")).unwrap();
        std::fs::write(sidecar, "replacement").unwrap();
        assert!(fence.validate_entries().is_err());
    }

    #[test]
    fn held_guard_is_required_for_reentry_and_its_exact_path_set() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-reentry";
        let fence = RuntimeHintFence::acquire(session).unwrap();
        assert!(RuntimeHintFence::acquire(session).is_err());
        super::super::with_tmux_source_authority(session, |_| {
            let contention = super::super::source_authority_contention_key_for_tests(|| {
                assert!(RuntimeHintFence::acquire(session).is_err());
            });
            assert!(contention.is_none(), "reentry tried to reacquire Source");
        });
        assert!(RuntimeHintFence::try_acquire(session).unwrap().is_none());
        assert!(fence.validate("different-session").is_err());
        super::super::write_tmux_runtime_kind_marker_under_fence(
            &fence,
            session,
            crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
        )
        .unwrap();
        fence.validate(session).unwrap();
        assert!(
            super::super::write_tmux_runtime_kind_marker_under_fence(
                &fence,
                "different-session",
                crate::services::agent_protocol::RuntimeHandoffKind::CodexTui,
            )
            .is_err()
        );
        assert!(!RuntimeHintFence::paths("different-session")[0].exists());
        use std::os::unix::fs::PermissionsExt;
        let parent = super::super::persistent_sessions_dir().unwrap();
        std::fs::rename(&parent, _root.path().join("old-sessions")).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o500)).unwrap();
        assert!(fence.validate(session).is_err());
        assert_eq!(
            std::fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o500
        );
    }

    fn race_waiting_snapshot(paths: &[PathBuf], mutate: impl FnOnce()) -> String {
        let fence = RuntimeHintFence::acquire_paths(paths.to_vec(), "mine", false)
            .unwrap()
            .unwrap();
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let waiting_paths = paths.to_vec();
        let worker = std::thread::spawn(move || {
            before_lock(move || {
                snapshot_tx.send(()).unwrap();
                continue_rx.recv_timeout(WAIT).unwrap();
            });
            RuntimeHintFence::acquire_paths(waiting_paths, "mine", false).err()
        });
        snapshot_rx.recv_timeout(WAIT).unwrap();
        mutate();
        continue_tx.send(()).unwrap();
        drop(fence);
        crate::test_support::wait::join_within(worker, WAIT).unwrap()
    }

    #[test]
    fn waiter_rejects_a_successor_nonce_published_before_it_obtains_the_fence() {
        let (_directory, paths) = fixture();
        let error = race_waiting_snapshot(&paths, || {
            std::fs::write(paths[0].with_extension("spawn_nonce"), "successor").unwrap();
        });
        assert!(error.contains("authority changed"));
        assert_eq!(
            std::fs::read(paths[0].with_extension("spawn_nonce")).unwrap(),
            b"successor"
        );
    }

    #[test]
    fn waiter_rejects_recreated_runtime_entry_even_when_bytes_are_unchanged() {
        let (_directory, paths) = fixture();
        std::fs::write(&paths[0], "codex_tui").unwrap();
        let error = race_waiting_snapshot(&paths, || {
            std::fs::rename(&paths[0], paths[0].with_extension("previous")).unwrap();
            std::fs::write(&paths[0], "codex_tui").unwrap();
        });
        assert!(error.contains("authority changed"));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"codex_tui");
    }

    #[test]
    fn tmux_publisher_rejects_a_current_herdr_host_and_accepts_only_tmux_or_absence() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-publisher-host";
        let fence = RuntimeHintFence::acquire(session).unwrap();
        fence.validate_tmux_publisher(session).unwrap();
        let host = super::super::session_temp_path(session, "host_kind");
        for value in ["herdr", "process", "unknown"] {
            std::fs::write(&host, value).unwrap();
            assert!(fence.validate_tmux_publisher(session).is_err());
            assert_eq!(std::fs::read(&host).unwrap(), value.as_bytes());
        }
        std::fs::write(&host, "tmux").unwrap();
        fence.validate_tmux_publisher(session).unwrap();
    }

    fn runtime_binding(output: &Path) -> TuiRuntimeBinding {
        TuiRuntimeBinding {
            runtime_kind: crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
            output_path: output.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: None,
            last_offset: 0,
            relay_last_offset: None,
        }
    }

    #[test]
    fn source_only_registration_during_acquire_revokes_the_waiting_publisher() {
        use crate::services::tui_prompt_dedupe as dedupe;
        let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let _state = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let session = format!("fence-source-change-{}", uuid::Uuid::new_v4());
        let replacement = runtime_binding(&root.path().join("replacement.jsonl"));
        let publish_session = session.clone();
        before_lock(move || {
            dedupe::register_tmux_runtime_binding(&publish_session, replacement);
        });
        let error = RuntimeHintFence::acquire(&session).err().unwrap();
        assert!(error.contains("source changed"), "{error}");
        assert!(dedupe::clear_tmux_runtime_binding(&session));
    }

    #[test]
    fn cursor_progress_is_not_a_runtime_source_replacement() {
        use crate::services::tui_prompt_dedupe as dedupe;
        let root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let _state = dedupe::TEST_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let session = format!("fence-source-cursor-{}", uuid::Uuid::new_v4());
        let binding = runtime_binding(&root.path().join("same-source.jsonl"));
        dedupe::register_tmux_runtime_binding(&session, binding.clone());
        let publish_session = session.clone();
        before_lock(move || {
            let mut progressed = binding;
            progressed.last_offset = 91;
            progressed.relay_last_offset = Some(73);
            dedupe::register_tmux_runtime_binding(&publish_session, progressed);
        });
        let fence = RuntimeHintFence::acquire(&session).unwrap();
        fence.validate(&session).unwrap();
        drop(fence);
        assert!(dedupe::clear_tmux_runtime_binding(&session));
    }

    #[test]
    fn try_acquire_never_waits_for_a_busy_source_authority() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-try-source";
        super::super::with_tmux_source_authority(session, |_| {
            assert!(RuntimeHintFence::try_acquire(session).unwrap().is_none());
        });
        RuntimeHintFence::acquire(session)
            .unwrap()
            .validate(session)
            .unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reentry_follows_the_tokio_task_identity_across_yields() {
        let (_directory, paths) = fixture();
        let mut owners = Vec::new();
        for _ in 0..2 {
            let paths = paths.clone();
            let task = tokio::spawn(async move {
                let owner = owner_id();
                assert!(matches!(owner, OwnerId::Task(_)));
                let thread = thread::current().id();
                let fence = RuntimeHintFence::acquire_paths(paths.clone(), "mine", false)
                    .unwrap()
                    .unwrap();
                tokio::task::yield_now().await;
                assert_eq!(owner_id(), owner);
                assert!(RuntimeHintFence::acquire_paths(paths, "mine", false).is_err());
                drop(fence);
                (owner, thread)
            });
            owners.push(
                crate::test_support::wait::await_within(task, WAIT)
                    .await
                    .unwrap(),
            );
        }
        assert_ne!(owners[0].0, owners[1].0);
        assert_eq!(owners[0].1, owners[1].1);
    }

    #[test]
    fn rollout_marker_change_during_acquire_revokes_the_waiting_publisher() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-rollout-change";
        let paths = RuntimeHintFence::paths(session);
        let rollout = paths[0].with_extension("codex-tui-rollout.json");
        before_lock(move || {
            std::fs::write(rollout, br#"{"rollout_path":"replacement"}"#).unwrap();
        });
        let error = RuntimeHintFence::acquire(session).err().unwrap();
        assert!(error.contains("authority changed"), "{error}");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn current_thread_sync_contention_returns_and_async_acquisition_transfers_ownership() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-current-thread-acquire";
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let held = RuntimeHintFence::acquire(session).unwrap();
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
            drop(held);
        });
        ready_rx.recv_timeout(WAIT).unwrap();
        let contender = tokio::spawn(async move { RuntimeHintFence::acquire(session).is_err() });
        let result = crate::test_support::wait::await_within(contender, WAIT).await;
        let _ = release_tx.send(());
        crate::test_support::wait::join_within(holder, WAIT);
        assert!(
            result.unwrap(),
            "sync contention must not block the current-thread executor"
        );
        let successor = tokio::spawn(async move {
            let fence = RuntimeHintFence::acquire_async(session).await.unwrap();
            assert!(RuntimeHintFence::acquire(session).is_err());
            let mut nested = Box::pin(RuntimeHintFence::acquire_async(session));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(
                matches!(
                    std::future::Future::poll(nested.as_mut(), &mut context),
                    std::task::Poll::Ready(Err(_))
                ),
                "async reentry must reject before starting a waiter"
            );
            fence.validate(session).unwrap();
        });
        crate::test_support::wait::await_within(successor, WAIT)
            .await
            .unwrap();
    }

    #[test]
    fn publisher_initial_observation_rejects_a_successor_published_before_acquire() {
        let _root = crate::config::TestRuntimeRootGuard::with_isolated_config();
        let session = "fence-preflight-successor";
        let observation = TmuxPublisherObservation::capture(session).unwrap();
        let successor = RuntimeHintFence::acquire(session).unwrap();
        let nonce = super::super::session_temp_path(session, "spawn_nonce");
        std::fs::write(&nonce, "successor").unwrap();
        drop(successor);
        let error = observation.acquire().unwrap_err();
        assert!(error.contains("initial observation"), "{error}");
        assert_eq!(std::fs::read(nonce).unwrap(), b"successor");
    }
}

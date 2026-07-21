use anyhow::Result;
use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::field;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::writer::MakeWriter;

const DEFAULT_DCSERVER_LOG_MAX_BYTES: u64 = 100 * 1024 * 1024;
const DEFAULT_DCSERVER_LOG_MAX_FILES: usize = 10;

pub(crate) fn tracing_env_filter() -> Result<EnvFilter> {
    let directive = "agentdesk=info"
        .parse()
        .map_err(|error| anyhow::anyhow!("Failed to parse tracing directive: {error}"))?;
    Ok(EnvFilter::from_default_env().add_directive(directive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn tracing_env_filter_with_rust_log_unset() -> EnvFilter {
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        let saved = std::env::var_os("RUST_LOG");
        remove_rust_log_for_test();
        let filter = tracing_env_filter().expect("default tracing filter");
        restore_rust_log_for_test(saved);
        filter
    }

    fn remove_rust_log_for_test() {
        // SAFETY: logging tests serialize this process-wide mutation with ENV_LOCK
        // and restore the previous value before releasing the lock.
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
    }

    fn restore_rust_log_for_test(saved: Option<OsString>) {
        // SAFETY: logging tests serialize this process-wide mutation with ENV_LOCK
        // and restore the previous value before releasing the lock.
        unsafe {
            match saved {
                Some(value) => std::env::set_var("RUST_LOG", value),
                None => std::env::remove_var("RUST_LOG"),
            }
        }
    }

    fn capture_logs_with_default_filter<F>(emit: F) -> String
    where
        F: FnOnce(),
    {
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_env_filter(tracing_env_filter_with_rust_log_unset())
            .with_ansi(false)
            .without_time()
            .with_target(true)
            .with_writer(CapturingWriter {
                buffer: buffer.clone(),
            })
            .finish();

        tracing::subscriber::with_default(subscriber, emit);
        String::from_utf8(buffer.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn trace_context_span_records_turn_correlation_fields() {
        let logs = capture_logs_with_default_filter(|| {
            let payload = serde_json::json!({
                "channel_id": 1473922824350601297_u64,
                "turn_id": "turn-4221",
                "session_key": "session-4221"
            });
            let span = TraceContext::from_payload(&payload).span("mutation-guard");
            span.in_scope(|| tracing::info!("trace context mutation marker"));
        });

        assert!(
            logs.contains("channel_id=Some(\"1473922824350601297\")"),
            "logs={logs}"
        );
        assert!(logs.contains("turn_id=Some(\"turn-4221\")"), "logs={logs}");
        assert!(
            logs.contains("session_key=Some(\"session-4221\")"),
            "logs={logs}"
        );
    }

    #[test]
    fn default_agentdesk_filter_keeps_observability_targets_and_drops_policy_target() {
        let logs = capture_logs_with_default_filter(|| {
            tracing::info!(
                target: crate::api_caller_observability::LOG_TARGET,
                "api caller production filter marker"
            );
            tracing::info!(
                target: crate::engine::ops::TIMEOUT_SHADOW_LOG_TARGET,
                "timeout shadow production filter marker"
            );
            tracing::info!(
                target: "policy",
                "policy production filter marker"
            );
        });

        assert!(
            logs.contains("api caller production filter marker"),
            "logs={logs}"
        );
        assert!(
            logs.contains(crate::api_caller_observability::LOG_TARGET),
            "logs={logs}"
        );
        assert!(
            logs.contains("timeout shadow production filter marker"),
            "logs={logs}"
        );
        assert!(
            logs.contains(crate::engine::ops::TIMEOUT_SHADOW_LOG_TARGET),
            "logs={logs}"
        );
        assert!(
            !logs.contains("policy production filter marker"),
            "logs={logs}"
        );
        assert!(!logs.contains("policy"), "logs={logs}");
    }
}

fn init_tracing_once() -> Result<()> {
    crate::utils::redact::register_common_env_secrets();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_env_filter()?)
        .with_writer(RedactingStdout)
        .try_init()
        .map_err(|error| anyhow::anyhow!("Failed to initialize tracing subscriber: {error}"))?;
    Ok(())
}

pub(crate) fn init_tracing() -> Result<()> {
    static TRACING_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    let init_result =
        TRACING_INIT.get_or_init(|| init_tracing_once().map_err(|error| error.to_string()));
    init_result
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

fn init_dcserver_tracing_once() -> Result<()> {
    crate::utils::redact::register_common_env_secrets();
    let root = crate::config::runtime_root()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve AgentDesk runtime root"))?;
    let log_path = root.join("logs").join("dcserver.stdout.log");
    let writer =
        RotatingLogWriter::new(log_path, dcserver_log_max_bytes(), dcserver_log_max_files())?;

    tracing_subscriber::fmt()
        .with_env_filter(tracing_env_filter()?)
        .with_writer(writer)
        .try_init()
        .map_err(|error| {
            anyhow::anyhow!("Failed to initialize dcserver tracing subscriber: {error}")
        })?;
    Ok(())
}

pub(crate) fn init_dcserver_tracing() -> Result<()> {
    static DCSERVER_TRACING_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

    let init_result = DCSERVER_TRACING_INIT
        .get_or_init(|| init_dcserver_tracing_once().map_err(|error| error.to_string()));
    init_result
        .as_ref()
        .map(|_| ())
        .map_err(|error| anyhow::anyhow!(error.clone()))
}

fn dcserver_log_max_bytes() -> u64 {
    std::env::var("AGENTDESK_DCSERVER_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_DCSERVER_LOG_MAX_BYTES)
}

fn dcserver_log_max_files() -> usize {
    std::env::var("AGENTDESK_DCSERVER_LOG_MAX_FILES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(DEFAULT_DCSERVER_LOG_MAX_FILES)
}

#[derive(Clone)]
struct RotatingLogWriter {
    inner: Arc<Mutex<RotatingLogState>>,
}

struct RotatingLogState {
    path: PathBuf,
    max_bytes: u64,
    max_files: usize,
    file: File,
    size: u64,
}

impl RotatingLogWriter {
    fn new(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if max_files > 0 {
            compact_oversized_current_log(&path, max_bytes, max_files)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        Ok(Self {
            inner: Arc::new(Mutex::new(RotatingLogState {
                path,
                max_bytes,
                max_files,
                file,
                size,
            })),
        })
    }
}

impl<'a> MakeWriter<'a> for RotatingLogWriter {
    type Writer = RotatingLogGuard;

    fn make_writer(&'a self) -> Self::Writer {
        RotatingLogGuard {
            inner: self.inner.clone(),
        }
    }
}

struct RotatingLogGuard {
    inner: Arc<Mutex<RotatingLogState>>,
}

#[derive(Clone, Copy)]
struct RedactingStdout;

struct RedactingIoGuard<W> {
    inner: W,
}

/// Redacts bytes written through tracing stdout and the dcserver rotating log
/// writer. The tmux wrapper also redacts dynamic stderr messages by explicitly
/// calling `redact_log_text`; arbitrary `println!`/`eprintln!` calls are not
/// globally intercepted.
impl<'a> MakeWriter<'a> for RedactingStdout {
    type Writer = RedactingIoGuard<io::Stdout>;

    fn make_writer(&'a self) -> Self::Writer {
        RedactingIoGuard {
            inner: io::stdout(),
        }
    }
}

pub(crate) fn redact_log_text(text: &str) -> String {
    crate::utils::redact::redact_known_secrets(text)
}

fn redacted_log_bytes(buf: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(buf);
    redact_log_text(&text).into_bytes()
}

impl<W: Write> Write for RedactingIoGuard<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let redacted = redacted_log_bytes(buf);
        self.inner.write_all(&redacted)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Write for RotatingLogGuard {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?;
        let redacted = redacted_log_bytes(buf);
        if state.max_files > 0
            && state.max_bytes > 0
            && state.size > 0
            && state.size.saturating_add(redacted.len() as u64) > state.max_bytes
        {
            state.rotate()?;
        }
        state.file.write_all(&redacted)?;
        state.size = state.size.saturating_add(redacted.len() as u64);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut state = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("rotating log writer lock poisoned"))?;
        state.file.flush()
    }
}

impl RotatingLogState {
    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        rotate_log_files(&self.path, self.max_bytes, self.max_files)?;
        self.file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.size = self
            .file
            .metadata()
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        Ok(())
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), index))
}

fn compact_oversized_current_log(path: &Path, max_bytes: u64, max_files: usize) -> io::Result<()> {
    if max_bytes == 0 || max_files == 0 {
        return Ok(());
    }
    let Ok(metadata) = fs::metadata(path) else {
        return Ok(());
    };
    if metadata.len() <= max_bytes {
        return Ok(());
    }
    rotate_log_files(path, max_bytes, max_files)
}

fn rotate_log_files(path: &Path, max_bytes: u64, max_files: usize) -> io::Result<()> {
    if max_files == 0 {
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        return Ok(());
    }

    let oldest = rotated_path(path, max_files);
    match fs::remove_file(&oldest) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for index in (1..max_files).rev() {
        let from = rotated_path(path, index);
        let to = rotated_path(path, index + 1);
        match fs::rename(&from, &to) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }

    if path.exists() {
        copy_tail(path, &rotated_path(path, 1), max_bytes)?;
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    Ok(())
}

fn copy_tail(source: &Path, dest: &Path, max_bytes: u64) -> io::Result<()> {
    let mut input = File::open(source)?;
    let len = input.metadata()?.len();
    let start = len.saturating_sub(max_bytes);
    input.seek(SeekFrom::Start(start))?;
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dest)?;

    let mut remaining = len - start;
    let mut buffer = [0_u8; 64 * 1024];
    while remaining > 0 {
        let limit = buffer.len().min(remaining as usize);
        let read = input.read(&mut buffer[..limit])?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        remaining -= read as u64;
    }
    output.flush()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct TraceContext<'a> {
    pub(crate) dispatch_id: Option<&'a str>,
    pub(crate) card_id: Option<&'a str>,
    pub(crate) agent_id: Option<&'a str>,
    pub(crate) hook_name: Option<&'a str>,
    pub(crate) channel_id: Option<Cow<'a, str>>,
    pub(crate) turn_id: Option<&'a str>,
    pub(crate) session_key: Option<&'a str>,
}

impl<'a> TraceContext<'a> {
    pub(crate) fn from_payload(payload: &'a serde_json::Value) -> Self {
        Self {
            dispatch_id: find_string(payload, &["dispatch_id", "pending_dispatch_id"]),
            card_id: find_string(payload, &["card_id", "kanban_card_id"]),
            agent_id: find_string(
                payload,
                &[
                    "agent_id",
                    "to_agent_id",
                    "assigned_agent_id",
                    "source_agent",
                ],
            ),
            hook_name: None,
            channel_id: find_string_or_u64(payload, &["channel_id", "discord_channel_id"]),
            turn_id: find_string(payload, &["turn_id"]),
            session_key: find_string(payload, &["session_key"]),
        }
    }

    pub(crate) fn with_dispatch_id(mut self, dispatch_id: Option<&'a str>) -> Self {
        self.dispatch_id = dispatch_id.or(self.dispatch_id);
        self
    }

    pub(crate) fn with_card_id(mut self, card_id: Option<&'a str>) -> Self {
        self.card_id = card_id.or(self.card_id);
        self
    }

    pub(crate) fn with_agent_id(mut self, agent_id: Option<&'a str>) -> Self {
        self.agent_id = agent_id.or(self.agent_id);
        self
    }

    pub(crate) fn with_hook_name(mut self, hook_name: Option<&'a str>) -> Self {
        self.hook_name = hook_name.or(self.hook_name);
        self
    }

    pub(crate) fn span(self, name: &'static str) -> tracing::Span {
        tracing::info_span!(
            "trace_context",
            span_name = name,
            dispatch_id = field::debug(self.dispatch_id),
            card_id = field::debug(self.card_id),
            agent_id = field::debug(self.agent_id),
            hook_name = field::debug(self.hook_name),
            channel_id = field::debug(self.channel_id),
            turn_id = field::debug(self.turn_id),
            session_key = field::debug(self.session_key),
        )
    }
}

pub(crate) fn dispatch_span(
    name: &'static str,
    dispatch_id: Option<&str>,
    card_id: Option<&str>,
    agent_id: Option<&str>,
) -> tracing::Span {
    TraceContext::default()
        .with_dispatch_id(dispatch_id)
        .with_card_id(card_id)
        .with_agent_id(agent_id)
        .span(name)
}

pub(crate) fn hook_span(hook_name: &str, payload: &serde_json::Value) -> tracing::Span {
    TraceContext::from_payload(payload)
        .with_hook_name(Some(hook_name))
        .span("policy_hook")
}

fn find_string<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(key).and_then(|v| v.as_str()))
}

fn find_string_or_u64<'a>(value: &'a serde_json::Value, keys: &[&str]) -> Option<Cow<'a, str>> {
    keys.iter().find_map(|key| {
        let value = value.get(key)?;
        value
            .as_str()
            .map(Cow::Borrowed)
            .or_else(|| value.as_u64().map(|number| Cow::Owned(number.to_string())))
    })
}

#[cfg(test)]
mod rotation_tests {
    use super::{RotatingLogWriter, redacted_log_bytes};
    use std::fs;
    use std::io::Write;
    use tracing_subscriber::fmt::writer::MakeWriter;

    #[test]
    fn redacted_log_bytes_masks_registered_plain_secret() {
        crate::utils::redact::register_known_secret("disk-log-secret");

        let redacted = String::from_utf8(redacted_log_bytes(
            b"connection error carried disk-log-secret into Display",
        ))
        .unwrap();

        assert_eq!(redacted, "connection error carried *** into Display");
    }

    #[test]
    fn trace_context_extracts_turn_correlation_fields_from_payload() {
        let payload = serde_json::json!({
            "dispatch_id": "dispatch-4221",
            "discord_channel_id": "channel-4221",
            "turn_id": "turn-4221",
            "session_key": "session-4221"
        });

        let context = super::TraceContext::from_payload(&payload);

        assert_eq!(context.dispatch_id, Some("dispatch-4221"));
        assert_eq!(context.channel_id.as_deref(), Some("channel-4221"));
        assert_eq!(context.turn_id, Some("turn-4221"));
        assert_eq!(context.session_key, Some("session-4221"));
    }

    #[test]
    fn trace_context_prefers_numeric_canonical_channel_id_key() {
        let payload = serde_json::json!({
            "channel_id": 1473922824350601297_u64,
            "discord_channel_id": "legacy-channel"
        });

        let context = super::TraceContext::from_payload(&payload);

        assert_eq!(context.channel_id.as_deref(), Some("1473922824350601297"));
    }

    #[test]
    fn rotating_writer_compacts_oversized_existing_log_on_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dcserver.stdout.log");
        fs::write(&path, b"0123456789abcdefghijklmnopqrstuvwxyz").unwrap();

        let writer = RotatingLogWriter::new(path.clone(), 10, 2).unwrap();
        let mut guard = writer.make_writer();
        guard.write_all(b"new\n").unwrap();
        guard.flush().unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            fs::read_to_string(dir.path().join("dcserver.stdout.log.1")).unwrap(),
            "qrstuvwxyz"
        );
    }

    #[test]
    fn rotating_writer_keeps_bounded_generation_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dcserver.stdout.log");
        let writer = RotatingLogWriter::new(path.clone(), 8, 2).unwrap();

        for index in 0..5 {
            let mut guard = writer.make_writer();
            writeln!(guard, "line-{index}").unwrap();
        }

        assert!(path.exists());
        assert!(dir.path().join("dcserver.stdout.log.1").exists());
        assert!(dir.path().join("dcserver.stdout.log.2").exists());
        assert!(!dir.path().join("dcserver.stdout.log.3").exists());
    }
}

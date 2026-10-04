//! Immutable launch evidence; binding authority remains with the runtime binding.

use crate::services::claude_tui::hook_output_guard::configured_claude_projects_root;
use crate::services::{platform::tmux::SessionPresence, tmux_common as tc};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

pub(crate) const UNSET_CONTEXT: &str = "unset AGENTDESK_BINDING_CONTEXT\n";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct BindingContext {
    pub schema: u32,
    pub provider: String,
    pub created_at: DateTime<Utc>,
    pub execution_nonce: String,
    pub tmux_session: String,
    pub channel_id: Option<u64>,
    pub owner_runtime_root: String,
    pub host: Option<String>,
    pub expected_native_session_id: Option<String>,
    pub launch_mode: String,
    pub provider_root: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) struct PreparedIncarnation {
    pub context: BindingContext,
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ContextPresence {
    Present,
    Absent,
    Unknown,
}
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SpawnNonceMarker {
    Known(String),
    Absent,
    Unreadable,
}

pub(crate) fn stable_host_identity() -> Option<String> {
    crate::config::load_graceful()
        .cluster
        .instance_id
        .into_iter()
        .chain(std::env::var("AGENTDESK_INSTANCE_ID").ok())
        .map(|id| id.trim().to_owned())
        .find(|id| !id.is_empty())
}

fn context_path(provider: &str, nonce: &str) -> io::Result<PathBuf> {
    if !matches!(provider, "claude" | "codex")
        || nonce.len() != 32
        || !nonce.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid context identity",
        ));
    }
    crate::config::runtime_root()
        .map(|root| {
            root.join("runtime/binding_contexts")
                .join(provider)
                .join(format!("{nonce}.json"))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "runtime root unavailable"))
}

pub(crate) fn context_presence(provider: &str, nonce: &str) -> ContextPresence {
    let Ok(path) = context_path(provider, nonce) else {
        return ContextPresence::Unknown;
    };
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => ContextPresence::Present,
        Err(e) if e.kind() == io::ErrorKind::NotFound => ContextPresence::Absent,
        _ => ContextPresence::Unknown,
    }
}

pub(crate) fn observe_spawn_nonce_marker(tmux: &str) -> SpawnNonceMarker {
    for path in [
        tc::session_temp_path(tmux, "spawn_nonce"),
        tc::legacy_tmp_session_path(tmux, "spawn_nonce"),
    ] {
        match fs::read_to_string(path) {
            Ok(s) if !s.trim().is_empty() => return SpawnNonceMarker::Known(s.trim().to_owned()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            _ => return SpawnNonceMarker::Unreadable,
        }
    }
    SpawnNonceMarker::Absent
}

// Hook capture reads this value without consulting mutable markers.
const CONTEXT_ENV: &str = "AGENTDESK_BINDING_CONTEXT";
pub(crate) const BINDING_HEADER: &str = "x-agentdesk-binding-context";
const CONTEXT_LIMIT: u64 = 16 * 1024;
const HEADER_LIMIT: usize = 32 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AbsentReason {
    EnvUnset,
    Unreadable,
    Corrupt,
    NonceMismatch,
    ProviderMismatch,
    SchemaUnsupported,
    LegacyRequest,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum CapturedContext {
    Captured(BindingContext),
    Absent(AbsentReason),
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ObservedHookProcess {
    pub tmux: Option<String>,
    pub tmux_pane: Option<String>,
    pub provider_pid: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HookBindingEnvelope {
    pub context: CapturedContext,
    pub observed: ObservedHookProcess,
}
impl HookBindingEnvelope {
    pub(crate) fn capture(provider: &str) -> Self {
        Self::capture_from_env(provider, |name| std::env::var_os(name))
    }

    pub(crate) fn capture_from_env(
        provider: &str,
        env: impl Fn(&str) -> Option<std::ffi::OsString>,
    ) -> Self {
        let capture = || {
            let path = env(CONTEXT_ENV).ok_or(AbsentReason::EnvUnset)?;
            checked_context(Path::new(&path), provider)
        };
        // Overlong observations are unavailable evidence, keeping the encoded header bounded.
        let observed = |key| {
            env(key)
                .and_then(|s| s.into_string().ok())
                .filter(|s| s.len() <= 256)
        };
        Self {
            context: capture().map_or_else(CapturedContext::Absent, CapturedContext::Captured),
            observed: ObservedHookProcess {
                tmux: observed("TMUX"),
                tmux_pane: observed("TMUX_PANE"),
                provider_pid: observed("CLAUDE_PID"),
            },
        }
    }

    pub(crate) fn legacy_request() -> Self {
        Self {
            context: CapturedContext::Absent(AbsentReason::LegacyRequest),
            observed: ObservedHookProcess::default(),
        }
    }

    pub(crate) fn encode(&self) -> Result<String, String> {
        serde_json::to_vec(self)
            .map(|bytes| URL_SAFE_NO_PAD.encode(bytes))
            .map_err(|e| e.to_string())
    }
}

/// The context file a provider's environment names, as a hook trusts it: readable, named
/// after its own nonce, for `provider`, schema 1.
fn checked_context(path: &Path, provider: &str) -> Result<BindingContext, AbsentReason> {
    let ctx = read_hook_context(path).map_err(|error| match error.kind() {
        io::ErrorKind::InvalidData => AbsentReason::Corrupt,
        _ => AbsentReason::Unreadable,
    })?;
    if path.file_stem().and_then(|s| s.to_str()) != Some(&ctx.execution_nonce) {
        return Err(AbsentReason::NonceMismatch);
    }
    if ctx.provider != provider {
        return Err(AbsentReason::ProviderMismatch);
    }
    if ctx.schema != 1 {
        return Err(AbsentReason::SchemaUnsupported);
    }
    Ok(ctx)
}

/// Execution `nonce`'s own context, read from its own path under the hook's rules.
pub(crate) fn execution_context(provider: &str, nonce: &str) -> Result<BindingContext, String> {
    let own = context_path(provider, nonce).map_err(|error| error.to_string())?;
    let ctx = checked_context(&own, provider).map_err(|reason| format!("{reason:?}"))?;
    if ctx.execution_nonce != nonce {
        return Err(format!("context names execution {}", ctx.execution_nonce));
    }
    Ok(ctx)
}

/// Execution `nonce`'s context when `env_path`, as a provider's environment holds it, is
/// exactly that execution's own context path.
pub(crate) fn context_names_execution(
    provider: &str,
    nonce: &str,
    env_path: &Path,
) -> Result<BindingContext, String> {
    let own = context_path(provider, nonce).map_err(|error| error.to_string())?;
    if env_path != own {
        return Err(format!(
            "{} is not this execution's context",
            env_path.display()
        ));
    }
    execution_context(provider, nonce)
}

#[allow(dead_code)]
pub(crate) fn decode_binding_header(header: &str) -> Result<HookBindingEnvelope, String> {
    if header.len() > HEADER_LIMIT {
        return Err("binding header exceeds size limit".into());
    }
    let bytes = URL_SAFE_NO_PAD.decode(header).map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn durable_directory(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("context directory has no parent"))?;
    durable_directory(parent)?;
    match fs::create_dir(path) {
        Ok(()) => (),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists && path.is_dir() => (),
        Err(e) => return Err(e),
    }
    #[cfg(test)]
    creation_fault("directory")?;
    crate::services::discord::runtime_store::fsync_parent_dir(path)
}

impl PreparedIncarnation {
    pub(crate) fn prepare(
        provider: &str,
        tmux: &str,
        channel_id: Option<u64>,
        expected: Option<&str>,
        resume: bool,
    ) -> Result<Self, String> {
        let root = (provider == "claude")
            .then(configured_claude_projects_root)
            .flatten();
        Self::prepare_at(provider, tmux, channel_id, expected, resume, root)
    }

    /// Records `provider_root` as the source root the launched child will write under.
    pub(crate) fn prepare_at(
        provider: &str,
        tmux: &str,
        channel_id: Option<u64>,
        expected: Option<&str>,
        resume: bool,
        provider_root: Option<PathBuf>,
    ) -> Result<Self, String> {
        let context = BindingContext {
            schema: 1,
            provider: provider.to_owned(),
            created_at: Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: tmux.to_owned(),
            channel_id,
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: stable_host_identity(),
            expected_native_session_id: expected.map(str::to_owned),
            launch_mode: if resume { "resume" } else { "fresh" }.to_owned(),
            provider_root,
        };
        sweep(
            provider,
            Utc::now(),
            32,
            crate::services::platform::tmux::session_presence,
        );
        Self::create(context).map_err(|e| format!("create binding context: {e}"))
    }

    pub(crate) fn create(context: BindingContext) -> io::Result<Self> {
        let path = context_path(&context.provider, &context.execution_nonce)?;
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("context path has no parent"))?;
        durable_directory(parent)?;
        let temp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        tc::with_tmux_source_authority(&context.tmux_session, |_| {
            let result = (|| {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temp)?;
                file.write_all(&serde_json::to_vec(&context)?)?;
                #[cfg(test)]
                creation_fault("file")?;
                file.sync_all()?;
                #[cfg(test)]
                creation_fault("link")?;
                fs::hard_link(&temp, &path)?;
                fs::remove_file(&temp)?;
                #[cfg(test)]
                creation_fault("parent")?;
                crate::services::discord::runtime_store::fsync_parent_dir(&path)
            })();
            let _ = fs::remove_file(&temp);
            result
        })?;
        Ok(Self { context, path })
    }

    pub(crate) fn env_lines(&self) -> String {
        format!(
            "{UNSET_CONTEXT}export AGENTDESK_BINDING_CONTEXT={}\n",
            crate::services::process::shell_escape(&self.path.to_string_lossy())
        )
    }

    pub(crate) fn finish_spawn(&self, result: io::Result<String>) -> Result<(), String> {
        result.map(|_| ()).map_err(|error| {
            crate::services::platform::tmux::kill_session(
                &self.context.tmux_session,
                "binding context publication failed",
            );
            format!("publish binding context: {error}")
        })
    }

    pub(crate) fn validate(&self, tmux: &str) -> io::Result<()> {
        let path = context_path(&self.context.provider, &self.context.execution_nonce)?;
        let valid = context_presence(&self.context.provider, &self.context.execution_nonce)
            == ContextPresence::Present
            && path == self.path
            && read_context(&path).is_ok_and(|ctx| {
                ctx.schema == 1 && ctx.tmux_session == tmux && ctx == self.context
            });
        if valid {
            Ok(())
        } else {
            Err(io::Error::other("ContextNotPublishable"))
        }
    }
}

fn read_context(path: &Path) -> io::Result<BindingContext> {
    serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)
}

/// `fresh` or `resume` as recorded when execution `nonce` was prepared.
pub(crate) fn launch_mode(provider: &str, nonce: &str) -> Option<String> {
    let context = read_context(&context_path(provider, nonce).ok()?).ok()?;
    (context.execution_nonce == nonce).then_some(context.launch_mode)
}

pub(super) fn pane_context(tmux: &str, nonce: &str) -> Option<BindingContext> {
    let ctx = read_hook_context(&context_path("claude", nonce).ok()?).ok()?;
    (ctx.schema == 1
        && ctx.provider == "claude"
        && ctx.tmux_session == tmux
        && ctx.execution_nonce == nonce)
        .then_some(ctx)
}

fn read_hook_context(path: &Path) -> io::Result<BindingContext> {
    let mut bytes = Vec::new();
    fs::File::open(path)?
        .take(CONTEXT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > CONTEXT_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "context exceeds size limit",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

fn context_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|name| {
            name.strip_prefix('.')
                .and_then(|s| s.strip_suffix(".tmp"))
                .is_some_and(|id| id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

fn sweep(
    provider: &str,
    now: DateTime<Utc>,
    budget: usize,
    probe: impl Fn(&str) -> SessionPresence,
) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let Ok(path) = context_path(provider, &"0".repeat(32)) else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json") || context_temp(p))
        .collect();
    paths.sort();
    if paths.is_empty() {
        return;
    }
    let start = NEXT.fetch_add(budget, Ordering::Relaxed) % paths.len();
    for path in paths
        .iter()
        .cycle()
        .skip(start)
        .take(budget.min(paths.len()))
    {
        let Ok(ctx) = read_context(path) else {
            continue;
        };
        if now.signed_duration_since(ctx.created_at) < chrono::Duration::days(7)
            || ctx.provider != provider
            || ctx.schema != 1
            || !context_path(&ctx.provider, &ctx.execution_nonce).is_ok_and(|canonical| {
                canonical == *path || (context_temp(path) && canonical.parent() == path.parent())
            })
        {
            continue;
        }
        let presence = probe(&ctx.tmux_session);
        tc::with_tmux_source_authority(&ctx.tmux_session, |_| {
            let retired = match observe_spawn_nonce_marker(&ctx.tmux_session) {
                SpawnNonceMarker::Known(n) => n != ctx.execution_nonce,
                SpawnNonceMarker::Absent => presence == SessionPresence::Missing,
                SpawnNonceMarker::Unreadable => false,
            };
            if retired {
                let _ = fs::remove_file(path);
            }
        });
    }
}

#[cfg(test)]
thread_local! { pub(crate) static CREATE_FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn creation_fault(step: &str) -> io::Result<()> {
    if CREATE_FAULT.with(|f| f.get() == Some(step)) {
        Err(io::Error::other(format!("injected {step}")))
    } else {
        Ok(())
    }
}

#[cfg(all(test, unix))]
pub(crate) mod tests {
    use super::*;
    use crate::config::TestEnvVarGuard as Guard;
    use crate::services::discord::stamp_spawn_markers;
    use SessionPresence::{Missing, Present, ProbeFailed};
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn fixture() -> (tempfile::TempDir, [Guard; 2]) {
        let root = tempfile::tempdir().unwrap();
        let env = Guard::set_path("AGENTDESK_ROOT_DIR", root.path());
        with_config(root, env)
    }
    /// `fixture` for a caller that already holds the shared env lock, so it can
    /// take that lock before `TEST_LOCK` (the env -> dedupe order).
    pub(crate) fn fixture_after_shared_test_env_lock() -> (tempfile::TempDir, [Guard; 2]) {
        let root = tempfile::tempdir().unwrap();
        let env = Guard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
        with_config(root, env)
    }
    fn with_config(root: tempfile::TempDir, env: Guard) -> (tempfile::TempDir, [Guard; 2]) {
        let config = root.path().join("config.yaml");
        fs::write(&config, "server: {}").unwrap();
        let config_env = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config);
        (root, [config_env, env])
    }
    pub(crate) fn prepared() -> PreparedIncarnation {
        let tmux = format!("binding-{}", uuid::Uuid::new_v4().simple());
        PreparedIncarnation::prepare("claude", &tmux, Some(42), Some("native-id"), false).unwrap()
    }
    pub(crate) fn child_context(lines: &str, expected: Option<&Path>) {
        let inherited = prepared();
        let output = std::process::Command::new("/bin/bash")
            .args(["-c", &format!("{lines}\nexec /usr/bin/env")])
            .env("AGENTDESK_BINDING_CONTEXT", &inherited.path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let env = String::from_utf8(output.stdout).unwrap();
        let value = env
            .lines()
            .find_map(|l| l.strip_prefix("AGENTDESK_BINDING_CONTEXT="));
        assert_eq!(value.map(Path::new), expected);
        if expected.is_none() {
            assert_eq!(
                HookBindingEnvelope::capture_from_env("claude", |key| {
                    env.lines()
                        .find_map(|line| line.strip_prefix(&format!("{key}=")).map(Into::into))
                })
                .context,
                CapturedContext::Absent(AbsentReason::EnvUnset)
            );
        }
    }
    pub(crate) fn fake_tmux(root: &Path) -> Guard {
        let stub = "#!/bin/bash\nprintf '%s\\n' \"$*\" >> \"$AGENTDESK_ROOT_DIR/tmux.calls\"\n";
        fs::write(root.join("tmux"), stub).unwrap();
        fs::set_permissions(root.join("tmux"), fs::Permissions::from_mode(0o700)).unwrap();
        Guard::prepend_path_after_shared_test_env_lock(root)
    }
    pub(crate) fn launch_failures(
        mut launch: impl FnMut(&str) -> Result<(), String>,
        script_ext: &str,
    ) {
        let (root, _env) = fixture();
        let _tmux = fake_tmux(root.path());
        for step in ["directory", "file", "link", "parent"] {
            CREATE_FAULT.with(|f| f.set(Some(step)));
            let result = launch("binding-launch-fault");
            CREATE_FAULT.with(|f| f.set(None));
            assert!(result.unwrap_err().contains(&format!("injected {step}")));
            assert!(!Path::new(&tc::tmux_owner_path("binding-launch-fault")).exists());
            assert!(
                !Path::new(&tc::session_temp_path("binding-launch-fault", script_ext)).exists()
            );
        }
        assert!(!root.path().join("tmux.calls").exists());
    }

    /// Tests that spawn by bare name never take the env lock, so the fake tmux must
    /// shadow `tmux` without hiding the system tools they run while it is installed.
    #[test]
    fn binding_context_fake_tmux_keeps_system_tools_resolvable() {
        let (root, _env) = fixture();
        let _tmux = fake_tmux(root.path());
        let found = std::process::Command::new("sh")
            .args(["-c", "sleep 0 && command -v tmux"])
            .output()
            .expect("sh must resolve from PATH while the fake tmux is installed");
        assert!(found.status.success(), "{found:?}");
        let stub = root.path().join("tmux");
        assert_eq!(
            String::from_utf8_lossy(&found.stdout).trim(),
            stub.to_str().unwrap()
        );
    }

    #[test]
    fn binding_context_t1_create_never_replaces_existing_evidence() {
        let (_root, _env) = fixture();
        let p = prepared();
        let bytes = fs::read(&p.path).unwrap();
        let error = PreparedIncarnation::create(p.context.clone()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&p.path).unwrap(), bytes);
        let mut ctx = p.context;
        ctx.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        let barrier = std::sync::Barrier::new(2);
        let create = || {
            barrier.wait();
            PreparedIncarnation::create(ctx.clone())
        };
        std::thread::scope(|scope| {
            let a = scope.spawn(&create);
            let b = scope.spawn(&create);
            let results = [a.join().unwrap(), b.join().unwrap()];
            assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
            let error = results.iter().find_map(|r| r.as_ref().err()).unwrap();
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        });
    }
    #[test]
    fn binding_context_t3_env_is_the_only_launch_source() {
        let (_root, _env) = fixture();
        let _value = Guard::capture_after_shared_test_env_lock("AGENTDESK_BINDING_CONTEXT");
        unsafe { std::env::remove_var("AGENTDESK_BINDING_CONTEXT") };
        let p = prepared();
        crate::services::discord::stamp_spawn_markers(&p.context.tmux_session, None).unwrap();
        assert!(matches!(
            observe_spawn_nonce_marker(&p.context.tmux_session),
            SpawnNonceMarker::Known(_)
        ));
        assert_eq!(
            HookBindingEnvelope::capture("claude").context,
            CapturedContext::Absent(AbsentReason::EnvUnset)
        );
        crate::services::discord::stamp_spawn_markers(&p.context.tmux_session, Some(&p)).unwrap();
        assert_eq!(
            HookBindingEnvelope::capture("claude").context,
            CapturedContext::Absent(AbsentReason::EnvUnset)
        );
        child_context(&p.env_lines(), Some(&p.path));
        let p = PreparedIncarnation {
            path: p.path.with_file_name("quote' space.json"),
            ..p
        };
        child_context(&p.env_lines(), Some(&p.path));
    }
    #[test]
    fn binding_context_t8_gc_requires_retirement_and_rotates() {
        let (_root, _env) = fixture();
        for marker in ["same", "other", "absent", "unreadable"] {
            for presence in [Present, Missing, ProbeFailed] {
                for age in [1, 8] {
                    let p = prepared();
                    let marker_path = tc::session_temp_path(&p.context.tmux_session, "spawn_nonce");
                    match marker {
                        "same" => fs::write(marker_path, &p.context.execution_nonce).unwrap(),
                        "other" => fs::write(marker_path, "replacement").unwrap(),
                        "unreadable" => fs::create_dir(marker_path).unwrap(),
                        _ => (),
                    }
                    sweep(
                        "claude",
                        Utc::now() + chrono::Duration::days(age),
                        128,
                        |_| presence,
                    );
                    let retired = age == 8
                        && (marker == "other" || (marker == "absent" && presence == Missing));
                    assert_eq!(p.path.exists(), !retired, "{marker} {presence:?} {age}");
                    if p.path.exists() {
                        fs::remove_file(p.path).unwrap();
                    }
                }
            }
        }
        let kept = prepared();
        fs::write(
            tc::session_temp_path(&kept.context.tmux_session, "spawn_nonce"),
            &kept.context.execution_nonce,
        )
        .unwrap();
        let retired = prepared();
        for _ in 0..3 {
            sweep("claude", Utc::now() + chrono::Duration::days(8), 1, |_| {
                SessionPresence::Missing
            });
        }
        assert!(kept.path.exists());
        assert!(!retired.path.exists());
    }
    #[test]
    fn binding_context_capture_errors_and_header_codec_are_bounded() {
        let (_root, _env) = fixture();
        let p = prepared();
        let original = fs::read(&p.path).unwrap();
        let capture = || {
            HookBindingEnvelope::capture_from_env("claude", |key| match key {
                "AGENTDESK_BINDING_CONTEXT" => Some(p.path.clone().into_os_string()),
                "TMUX" => Some("/tmp/socket,12,3".into()),
                "TMUX_PANE" => Some("%7".into()),
                "CLAUDE_PID" => Some("123".into()),
                _ => None,
            })
        };
        let good = capture();
        assert_eq!(good.context, CapturedContext::Captured(p.context.clone()));
        assert_eq!(
            good.observed,
            ObservedHookProcess {
                tmux: Some("/tmp/socket,12,3".into()),
                tmux_pane: Some("%7".into()),
                provider_pid: Some("123".into())
            }
        );
        assert_eq!(
            decode_binding_header(&good.encode().unwrap()).unwrap(),
            good
        );
        for (field, value, reason) in [
            (
                "execution_nonce",
                serde_json::json!("wrong"),
                AbsentReason::NonceMismatch,
            ),
            (
                "provider",
                serde_json::json!("codex"),
                AbsentReason::ProviderMismatch,
            ),
            (
                "schema",
                serde_json::json!(2),
                AbsentReason::SchemaUnsupported,
            ),
        ] {
            let mut ctx: serde_json::Value = serde_json::from_slice(&original).unwrap();
            ctx[field] = value;
            fs::write(&p.path, serde_json::to_vec(&ctx).unwrap()).unwrap();
            assert_eq!(capture().context, CapturedContext::Absent(reason));
        }
        let mut oversized = original.clone();
        oversized.resize(CONTEXT_LIMIT as usize + 1, b' ');
        for bytes in [b"broken".to_vec(), oversized] {
            fs::write(&p.path, bytes).unwrap();
            assert_eq!(
                capture().context,
                CapturedContext::Absent(AbsentReason::Corrupt)
            );
        }
        let mut boundary = original;
        boundary.resize(CONTEXT_LIMIT as usize, b' ');
        fs::write(&p.path, boundary).unwrap();
        assert_eq!(capture(), good);
        fs::remove_file(&p.path).unwrap();
        assert_eq!(
            capture().context,
            CapturedContext::Absent(AbsentReason::Unreadable)
        );
        for invalid in [
            "+///".into(),
            "e30=".into(),
            "e30\n".into(),
            "_x".into(),
            "a".repeat(HEADER_LIMIT + 1),
        ] {
            assert!(decode_binding_header(&invalid).is_err());
        }
        let mut large = p.context;
        large.provider_root = Some(PathBuf::from("x".repeat(15_000)));
        fs::write(&p.path, serde_json::to_vec(&large).unwrap()).unwrap();
        let worst = HookBindingEnvelope::capture_from_env("claude", |key| {
            Some(if key == "AGENTDESK_BINDING_CONTEXT" {
                p.path.clone().into_os_string()
            } else {
                "\0".repeat(256).into()
            })
        });
        assert!(matches!(worst.context, CapturedContext::Captured(_)));
        assert!(worst.encode().unwrap().len() < HEADER_LIMIT);
        assert_eq!(
            decode_binding_header(&worst.encode().unwrap()).unwrap(),
            worst
        );
        assert_eq!(
            HookBindingEnvelope::capture_from_env("claude", |_| Some("x".repeat(257).into()))
                .observed,
            ObservedHookProcess::default()
        );
    }

    #[test]
    fn binding_context_tmp_sweep_preserves_live_writers_and_uncertain_evidence() {
        let (_root, _env) = fixture();
        let p = prepared();
        let temp = p
            .path
            .with_file_name(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        fs::rename(&p.path, &temp).unwrap();
        let now = Utc::now() + chrono::Duration::days(8);
        for presence in [Present, ProbeFailed] {
            sweep("claude", now, 128, |_| presence);
            assert!(temp.exists());
        }
        let marker = tc::session_temp_path(&p.context.tmux_session, "spawn_nonce");
        fs::write(&marker, &p.context.execution_nonce).unwrap();
        sweep("claude", now, 128, |_| Missing);
        assert!(temp.exists());
        fs::remove_file(&marker).unwrap();
        sweep("claude", Utc::now(), 128, |_| Missing);
        assert!(temp.exists());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let worker = tc::with_tmux_source_authority(&p.context.tmux_session, |_| {
                let worker = scope.spawn(|| {
                    sweep("claude", now, 128, |_| {
                        tx.send(()).unwrap();
                        Missing
                    })
                });
                rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(50));
                assert!(temp.exists(), "writer still owns the temporary context");
                worker
            });
            worker.join().unwrap();
        });
        assert!(
            !temp.exists(),
            "retired crash residue is swept after writer releases authority"
        );
        fs::write(&temp, b"partial JSON").unwrap();
        sweep("claude", now, 128, |_| Missing);
        assert!(temp.exists());
    }

    #[test]
    fn binding_context_t12_permission_failure_is_unknown() {
        let (_root, _env) = fixture();
        let p = prepared();
        let parent = p.path.parent().unwrap();
        fs::set_permissions(parent, fs::Permissions::from_mode(0o000)).unwrap();
        let presence = context_presence("claude", &p.context.execution_nonce);
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(presence, ContextPresence::Unknown);
    }
    #[test]
    fn binding_context_t13_host_uses_only_explicit_sources() {
        let (root, _env) = fixture();
        for (config, env, expected) in [
            ("cluster: {instance_id: cfg}", "env", Some("cfg")),
            ("", "env", Some("env")),
            ("cluster: {enabled: true}", "", None),
            ("", "", None),
        ] {
            let yaml = format!("server: {{}}\n{config}");
            fs::write(root.path().join("config.yaml"), yaml).unwrap();
            let _id =
                Guard::set_path_after_shared_test_env_lock("AGENTDESK_INSTANCE_ID", Path::new(env));
            assert_eq!(stable_host_identity().as_deref(), expected);
        }
    }
    #[test]
    fn binding_context_t16_sweep_before_publication_aborts_launch() {
        let (root, _env) = fixture();
        let _tmux = fake_tmux(root.path());
        let p = prepared();
        let tmux = &p.context.tmux_session;
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let launch = scope.spawn(|| {
                barrier.wait();
                barrier.wait();
                p.finish_spawn(stamp_spawn_markers(tmux, Some(&p)))
            });
            barrier.wait();
            sweep("claude", Utc::now() + chrono::Duration::days(8), 32, |_| {
                Missing
            });
            barrier.wait();
            let error = launch.join().unwrap().unwrap_err();
            assert!(error.contains("ContextNotPublishable"));
        });
        assert_eq!(observe_spawn_nonce_marker(tmux), SpawnNonceMarker::Absent);
        assert!(!Path::new(&tc::session_temp_path(tmux, "generation")).exists());
        let calls = fs::read_to_string(root.path().join("tmux.calls")).unwrap();
        assert!(calls.contains(&format!("kill-session -t ={tmux}:")));
    }
}

//! Claude TUI launch-script parsing, the launch transcript and binding a rehydrate derives from
//! it, and the boot read start only the idle relay's boot discovery fixes.

use super::*;
use std::sync::atomic::Ordering::{Acquire, Release};

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ClaudeTuiLaunchInfo {
    pub(super) working_dir: PathBuf,
    pub(super) session_id: String,
    pub(super) binding_context_path: Option<PathBuf>,
}

#[cfg(unix)]
pub(super) fn parse_claude_tui_launch_script(path: &Path) -> Result<ClaudeTuiLaunchInfo, String> {
    let script = std::fs::read_to_string(path)
        .map_err(|error| format!("read Claude TUI launch script {}: {error}", path.display()))?;
    parse_claude_tui_launch_script_content(&script)
        .ok_or_else(|| format!("parse Claude TUI launch script {}", path.display()))
}

/// The launch session of `tmux_session_name` and where its transcript is, whether or not it exists.
#[cfg(unix)]
pub(super) fn claude_tui_launch_transcript(
    tmux_session_name: &str,
    home: Option<&Path>,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    let launch = claude_tui_launch_info(tmux_session_name)?;
    transcript_for_launch(&launch, home)
}

#[cfg(unix)]
fn claude_tui_launch_info(tmux: &str) -> Option<ClaudeTuiLaunchInfo> {
    let launch_script_path = crate::services::tmux_common::resolve_session_temp_path(
        tmux,
        crate::services::tmux_common::CLAUDE_TUI_LAUNCH_SCRIPT_TEMP_EXT,
    )?;
    parse_claude_tui_launch_script(Path::new(&launch_script_path)).ok()
}

#[cfg(unix)]
fn transcript_for_launch(
    launch: &ClaudeTuiLaunchInfo,
    home: Option<&Path>,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    let transcript = crate::services::claude_tui::transcript_tail::claude_transcript_path(
        &launch.working_dir,
        &launch.session_id,
        home,
    )
    .ok()?;
    let session_id = launch.session_id.clone();
    Some(
        crate::services::tui_prompt_dedupe::pending::LaunchTranscript {
            session_id,
            transcript,
        },
    )
}

#[cfg(unix)]
fn claude_rehydrate_home() -> Option<PathBuf> {
    #[cfg(test)]
    {
        super::rehydration::claude_pass_tests::claude_home()
    }
    #[cfg(not(test))]
    {
        None
    }
}

#[cfg(unix)]
pub(super) fn claude_launch_transcript(
    tmux: &str,
) -> Option<crate::services::tui_prompt_dedupe::pending::LaunchTranscript> {
    claude_tui_launch_transcript(tmux, claude_rehydrate_home().as_deref())
}

#[cfg(unix)]
pub(super) fn rehydrated_claude_tui_binding_for_tmux_session(
    tmux: &str,
) -> Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding> {
    claude_launch_observation(tmux).0
}

/// The selector and its immutable context path come from one launch-script read.
#[cfg(unix)]
pub(super) fn claude_launch_observation(tmux: &str) -> ClaudeLaunchObservation {
    launch_observation(tmux, |_| {})
}

/// The rehydrate pass's launch observation; during the boot discovery it also fixes the
/// transcript's boot read start.
#[cfg(unix)]
pub(super) fn claude_rehydrate_launch_observation(tmux: &str) -> ClaudeLaunchObservation {
    let boot = BOOT_DISCOVERY.with_borrow(|boot| boot.as_ref().is_some_and(|b| b.load(Acquire)));
    if boot {
        launch_observation(tmux, observe_boot_read_start)
    } else {
        claude_launch_observation(tmux)
    }
}

#[cfg(unix)]
pub(super) type ClaudeLaunchObservation = (
    Option<crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
    Option<PathBuf>,
);

#[cfg(unix)]
fn launch_observation(tmux: &str, observe: impl FnOnce(&Path)) -> ClaudeLaunchObservation {
    let Some(launch) = claude_tui_launch_info(tmux) else {
        return (None, None);
    };
    let binding = transcript_for_launch(&launch, claude_rehydrate_home().as_deref())
        .filter(|launch| launch.transcript.exists())
        .map(|launch| {
            observe(&launch.transcript);
            claude_tui_rehydrated_binding(&launch.session_id, &launch.transcript)
        });
    (binding, launch.binding_context_path)
}

/// Per transcript, its length when the boot discovery first observed it. Not delivery evidence:
/// output written after it postdates the old process.
#[cfg(unix)]
static BOOT_READ_STARTS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, u64>>,
> = std::sync::LazyLock::new(Default::default);

/// Keeps the first boot length of `transcript`. The length is read under the lock, so a later
/// observation can neither land first nor replace it; a failed read records nothing.
#[cfg(unix)]
fn observe_boot_read_start(transcript: &Path) {
    let key = transcript.display().to_string();
    let mut starts = BOOT_READ_STARTS.lock().unwrap_or_else(|e| e.into_inner());
    if let std::collections::hash_map::Entry::Vacant(slot) = starts.entry(key) {
        if let Ok(metadata) = std::fs::metadata(transcript) {
            #[cfg(test)]
            boot_observation_paused(slot.key(), metadata.len());
            slot.insert(metadata.len());
        }
    }
}

#[cfg(unix)]
thread_local! {
    /// The boot discovery state of the idle relay pass running on this thread, if any.
    static BOOT_DISCOVERY: std::cell::RefCell<Option<Arc<AtomicBool>>> =
        const { std::cell::RefCell::new(None) };
}

/// Ends the boot discovery of the pass on this thread: its tmux listing has completed.
#[cfg(unix)]
pub(super) fn boot_listing_completed() {
    #[cfg(test)]
    injected_panic("listed");
    BOOT_DISCOVERY.with_borrow(|boot| boot.as_ref().map(|b| b.store(false, Release)));
    #[cfg(test)]
    injected_panic("herdr");
}

/// One idle relay rehydrate pass on the blocking pool. It stays the boot discovery until a pass
/// completes its tmux listing, even if that pass panics afterwards.
#[cfg(unix)]
pub(super) async fn run_claude_rehydrate_pass(shared: &Arc<SharedData>, boot: &Arc<AtomicBool>) {
    struct Clear;
    impl Drop for Clear {
        fn drop(&mut self) {
            BOOT_DISCOVERY.set(None);
        }
    }
    let (shared, boot) = (shared.clone(), boot.clone());
    // The pass blocks on `tmux` subprocesses and sleeps between pane probes.
    let rehydrate_result = tokio::task::spawn_blocking(move || {
        BOOT_DISCOVERY.set(Some(boot));
        let _clear = Clear;
        rehydrate_existing_claude_tui_bindings(&shared);
    })
    .await;
    if let Err(error) = rehydrate_result {
        tracing::warn!(
            error = %error,
            "Claude TUI binding rehydrate task panicked or was cancelled"
        );
    }
}

/// The length the boot discovery first observed `transcript_path` at in this process, if any.
#[cfg(unix)]
pub(in crate::services::discord) fn boot_read_start(transcript_path: &str) -> Option<u64> {
    let starts = BOOT_READ_STARTS.lock().unwrap_or_else(|e| e.into_inner());
    starts.get(transcript_path).copied()
}

/// The binding a rehydrate registers for `session_id`, read from the transcript's current end.
#[cfg(unix)]
pub(super) fn claude_tui_rehydrated_binding(
    session_id: &str,
    transcript_path: &Path,
) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        runtime_kind: RuntimeHandoffKind::ClaudeTui,
        output_path: transcript_path.display().to_string(),
        relay_output_path: None,
        input_fifo_path: None,
        session_id: Some(session_id.to_owned()),
        last_offset: claude_tui_rehydrate_start_offset(transcript_path),
        relay_last_offset: None,
    }
}

/// Test-only pause of one boot observation of a chosen transcript, after it reads the length.
#[cfg(all(unix, test))]
type BootObservePause = Box<dyn FnOnce(u64) + Send>;
#[cfg(all(unix, test))]
static BOOT_OBSERVE_PAUSE: std::sync::Mutex<Option<(String, BootObservePause)>> =
    std::sync::Mutex::new(None);

/// Runs `pause` with the length the next boot observation of `transcript` reads, once.
#[cfg(all(unix, test))]
pub(in crate::services::discord) fn pause_next_boot_observation(
    transcript: &str,
    pause: impl FnOnce(u64) + Send + 'static,
) {
    let mut armed = BOOT_OBSERVE_PAUSE.lock().unwrap_or_else(|e| e.into_inner());
    *armed = Some((transcript.to_owned(), Box::new(pause)));
}

#[cfg(all(unix, test))]
fn boot_observation_paused(transcript: &str, len: u64) {
    let mut armed = BOOT_OBSERVE_PAUSE.lock().unwrap_or_else(|e| e.into_inner());
    let pause = armed.take_if(|(at, _)| at == transcript);
    drop(armed);
    if let Some((_, pause)) = pause {
        pause(len);
    }
}

/// The boot discovery's observation of `tmux`, whether it found a transcript.
#[cfg(all(unix, test))]
pub(in crate::services::discord) fn boot_observation_for_tests(tmux: &str) -> bool {
    launch_observation(tmux, observe_boot_read_start)
        .0
        .is_some()
}

#[cfg(all(unix, test))]
pub(in crate::services::discord) async fn run_claude_rehydrate_pass_for_tests(
    shared: &Arc<SharedData>,
    boot: &Arc<AtomicBool>,
) {
    run_claude_rehydrate_pass(shared, boot).await;
}

/// Test-only: the point in `boot_listing_completed` the next pass reaching it panics at.
#[cfg(all(unix, test))]
static PANIC_AT: std::sync::Mutex<Option<&'static str>> = std::sync::Mutex::new(None);

/// Arms `point` (or disarms with `None`) and returns what was armed, `None` once a pass hit it.
#[cfg(all(unix, test))]
pub(in crate::services::discord) fn panic_next_rehydrate_pass_at(
    point: Option<&'static str>,
) -> Option<&'static str> {
    std::mem::replace(
        &mut *PANIC_AT.lock().unwrap_or_else(|e| e.into_inner()),
        point,
    )
}

#[cfg(all(unix, test))]
fn injected_panic(point: &str) {
    let armed = PANIC_AT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_if(|at| *at == point);
    if armed.is_some() {
        panic!("injected rehydrate panic at {point}");
    }
}

/// Whether another thread holds the boot read start map right now.
#[cfg(all(unix, test))]
pub(in crate::services::discord) fn boot_read_starts_held() -> bool {
    matches!(
        BOOT_READ_STARTS.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}

#[cfg(unix)]
fn parse_claude_tui_launch_script_content(script: &str) -> Option<ClaudeTuiLaunchInfo> {
    let mut working_dir: Option<PathBuf> = None;
    let mut session_id: Option<String> = None;
    let mut binding_context_path = None;
    for line in script.lines() {
        let words = shell_words_from_line(line.trim());
        if words.first().is_some_and(|word| word == "export") {
            if let Some(path) = words
                .iter()
                .skip(1)
                .find_map(|word| word.strip_prefix("AGENTDESK_BINDING_CONTEXT="))
            {
                binding_context_path = (!path.is_empty()).then(|| PathBuf::from(path));
            }
            continue;
        }
        if words.first().is_some_and(|word| word == "unset")
            && words
                .iter()
                .skip(1)
                .any(|word| word == "AGENTDESK_BINDING_CONTEXT")
        {
            binding_context_path = None;
            continue;
        }
        if words.first().is_some_and(|word| word == "cd") {
            if let Some(dir) = words.get(1).filter(|value| !value.trim().is_empty()) {
                working_dir = Some(PathBuf::from(dir));
            }
            continue;
        }
        if !words.first().is_some_and(|word| word == "exec") {
            continue;
        }
        for pair in words.windows(2) {
            if matches!(pair[0].as_str(), "--session-id" | "--resume") && !pair[1].trim().is_empty()
            {
                session_id = Some(pair[1].clone());
                break;
            }
        }
    }
    Some(ClaudeTuiLaunchInfo {
        working_dir: working_dir?,
        session_id: session_id?,
        binding_context_path,
    })
}

#[cfg(unix)]
fn shell_words_from_line(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut saw_word = false;
    let mut in_single = false;
    let mut chars = line.chars().peekable();

    while let Some(ch) = chars.next() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current.push(ch);
            }
            saw_word = true;
            continue;
        }

        if ch.is_whitespace() {
            if saw_word {
                words.push(std::mem::take(&mut current));
                saw_word = false;
            }
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                saw_word = true;
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                    saw_word = true;
                }
            }
            _ => {
                current.push(ch);
                saw_word = true;
            }
        }
    }

    if saw_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn parses_claude_tui_launch_script_content() {
        let script = concat!(
            "#!/bin/bash\n",
            "cd '/tmp/project'\\''s dir'\n",
            "exec '/usr/local/bin/claude' '--dangerously-skip-permissions' '--session-id' '01234567-89ab-cdef-0123-456789abcdef' '--settings' '/tmp/settings.json'\n",
        );

        assert_eq!(
            parse_claude_tui_launch_script_content(script),
            Some(ClaudeTuiLaunchInfo {
                working_dir: PathBuf::from("/tmp/project's dir"),
                session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
                binding_context_path: None,
            })
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_launch_selector_keeps_its_context_when_the_live_nonce_changes() {
        use crate::services::tui_prompt_dedupe::binding_context::{
            CapturedContext, HookBindingEnvelope, PreparedIncarnation, SpawnNonceMarker,
            observe_spawn_nonce_marker,
            tests::{fixture, prepared},
        };
        let (_root, _env) = fixture();
        let previous = prepared();
        let script = format!(
            "{}cd '/tmp/project'\\''s dir'\nexec 'claude' '--session-id' 'launch-session'\n",
            previous.env_lines(),
        );
        let parsed = parse_claude_tui_launch_script_content(&script).unwrap();
        assert_eq!(parsed.session_id, "launch-session");
        assert_eq!(parsed.binding_context_path.as_ref(), Some(&previous.path));
        let mut next = previous.context.clone();
        next.execution_nonce = uuid::Uuid::new_v4().simple().to_string();
        let next = PreparedIncarnation::create(next).unwrap();
        let marker = crate::services::tmux_common::session_temp_path(
            &previous.context.tmux_session,
            "spawn_nonce",
        );
        std::fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, &next.context.execution_nonce).unwrap();
        let captured = HookBindingEnvelope::capture_from_env("claude", |key| {
            (key == "AGENTDESK_BINDING_CONTEXT").then(|| {
                parsed
                    .binding_context_path
                    .clone()
                    .unwrap()
                    .into_os_string()
            })
        });
        assert_eq!(
            captured.context,
            CapturedContext::Captured(previous.context.clone())
        );
        assert_eq!(
            observe_spawn_nonce_marker(&previous.context.tmux_session),
            SpawnNonceMarker::Known(next.context.execution_nonce),
        );
    }
}

//! A kept source policy is reused only while every input it was judged from stands still.

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{BindingContext, PreparedIncarnation};
use crate::services::tui_prompt_dedupe::{self as dedupe};
use std::{fs, path::Path};

fn codex_context(root: &Path, policy: Option<&str>) -> BindingContext {
    BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: chrono::Utc::now(),
        execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
        tmux_session: format!("cache-{}", uuid::Uuid::new_v4().simple()),
        channel_id: Some(584_505),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: None,
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(root.canonicalize().unwrap()),
        first_prompt_digest: None,
        source_policy: policy.map(str::to_owned),
        resume_start: None,
    }
}

/// `Some(context)` for a verified pin, `None` for legacy, judged through the cache.
fn judge(tmux: &str) -> Option<String> {
    match tc::with_tmux_source_authority(tmux, super::super::disposition) {
        SourcePolicyState::Legacy => None,
        SourcePolicyState::VerifiedUnavailable => Some("unavailable".into()),
        SourcePolicyState::VerifiedCurrent(pin) => Some(pin.into_context().execution_nonce),
    }
}

fn judged() -> usize {
    JUDGED.with(|count| count.replace(0))
}

fn settled(ahead: bool) {
    AHEAD.with(|clock| {
        clock.set(if ahead {
            Duration::from_secs(3)
        } else {
            Duration::ZERO
        })
    });
}

struct Pane {
    context: BindingContext,
    canonical: PathBuf,
    marker: PathBuf,
}

impl Pane {
    fn new(root: &Path, policy: Option<&str>) -> Self {
        let context = codex_context(root, policy);
        let canonical = PreparedIncarnation::create(context.clone()).unwrap().path;
        let marker = PathBuf::from(tc::session_temp_path(&context.tmux_session, "spawn_nonce"));
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, &context.execution_nonce).unwrap();
        Self {
            context,
            canonical,
            marker,
        }
    }

    fn tmux(&self) -> &str {
        &self.context.tmux_session
    }

    fn nonce(&self) -> Option<String> {
        Some(self.context.execution_nonce.clone())
    }
}

fn locks() -> (
    crate::config::test_env_lock::SharedTestEnvLockGuard,
    std::sync::MutexGuard<'static, ()>,
) {
    let env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    (env, dedupe)
}

#[test]
fn settled_inputs_are_judged_once_and_unsettled_inputs_every_time() {
    let _locks = locks();
    let (root, _guards) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let pane = Pane::new(root.path(), Some("verified"));
    settled(false);
    judged();
    for _ in 0..3 {
        assert_eq!(judge(pane.tmux()), pane.nonce());
    }
    assert_eq!(
        judged(),
        3,
        "a file written within SETTLED is read every time"
    );
    settled(true);
    for _ in 0..3 {
        assert_eq!(judge(pane.tmux()), pane.nonce());
    }
    assert_eq!(judged(), 1, "settled inputs are judged once, then reused");
    settled(false);
}

#[test]
fn a_changed_marker_context_or_binding_log_is_judged_again() {
    let _locks = locks();
    let (root, _guards) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let pane = Pane::new(root.path(), Some("verified"));
    settled(true);
    judged();
    assert_eq!(judge(pane.tmux()), pane.nonce());
    assert_eq!(judge(pane.tmux()), pane.nonce());
    assert_eq!(judged(), 1);

    // The canonical context now says legacy.
    let mut legacy = pane.context.clone();
    legacy.source_policy = None;
    fs::write(&pane.canonical, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(judge(pane.tmux()), None, "the rewritten context is read");
    assert_eq!(judged(), 1);

    // The marker names another verified execution.
    let mut next = codex_context(root.path(), Some("verified"));
    next.tmux_session = pane.context.tmux_session.clone();
    PreparedIncarnation::create(next.clone()).unwrap();
    fs::write(&pane.marker, &next.execution_nonce).unwrap();
    assert_eq!(judge(pane.tmux()), Some(next.execution_nonce.clone()));
    assert_eq!(judged(), 1);
    assert_eq!(judge(pane.tmux()), Some(next.execution_nonce.clone()));
    assert_eq!(judged(), 0);

    // A pane with a channel also watches that channel's binding log.
    binding_events::set_test_root(Some(root.path()));
    dedupe::register_tmux_channel(pane.tmux(), 584_505);
    assert_eq!(judge(pane.tmux()), Some(next.execution_nonce.clone()));
    assert_eq!(judged(), 1, "the new channel is a changed input");
    assert_eq!(judge(pane.tmux()), Some(next.execution_nonce.clone()));
    assert_eq!(judged(), 0);
    let log = binding_events::log_path(584_505).unwrap().unwrap();
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(&log, b"").unwrap();
    assert_eq!(judge(pane.tmux()), Some(next.execution_nonce));
    assert_eq!(judged(), 1, "a created binding log is a changed input");
    binding_events::set_test_root(None);
    settled(false);
}

#[test]
fn a_rewrite_that_restores_mtime_or_an_invalidation_is_judged_again() {
    let _locks = locks();
    let (root, _guards) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    let pane = Pane::new(root.path(), Some("verified"));
    settled(true);
    judged();
    assert_eq!(judge(pane.tmux()), pane.nonce());
    assert_eq!(judged(), 1);

    // Same length, same mtime: only the inode change time witnesses the rewrite.
    let before = Fingerprint::of(&fs::metadata(&pane.canonical).unwrap()).unwrap();
    let mut legacy = pane.context.clone();
    legacy.source_policy = Some("unverify".into());
    let bytes = serde_json::to_vec(&legacy).unwrap();
    assert_eq!(
        bytes.len() as u64,
        before.len,
        "the rewrite keeps the length"
    );
    fs::write(&pane.canonical, &bytes).unwrap();
    fs::File::options()
        .write(true)
        .open(&pane.canonical)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(before.modified))
        .unwrap();
    let after = Fingerprint::of(&fs::metadata(&pane.canonical).unwrap()).unwrap();
    assert_eq!((after.len, after.modified), (before.len, before.modified));
    assert_eq!(judge(pane.tmux()), None, "the hidden rewrite is read");
    assert_eq!(judged(), 1);
    assert_eq!(judge(pane.tmux()), None);
    assert_eq!(judged(), 0);

    // An in-process invalidation of the pane's sources drops the kept answer too.
    tc::with_tmux_source_authority(pane.tmux(), |authority| authority.invalidate());
    assert_eq!(judge(pane.tmux()), None);
    assert_eq!(judged(), 1, "a new source revision is a changed input");
    settled(false);
}

//! One source-policy judgement opens the spawn marker and the canonical context once each.

use super::*;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_context::PreparedIncarnation};
use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
use std::sync::{Arc, atomic::AtomicBool, atomic::Ordering::SeqCst};
use std::{fs, io::Write, path::Path};

fn fifo(path: &Path) {
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

/// Replaces `path` with a FIFO handing `bytes` to each open; the returned call counts the opens.
fn serve(path: &Path, bytes: Vec<u8>) -> impl FnOnce() -> usize {
    let (path, stop) = (path.to_owned(), Arc::new(AtomicBool::new(false)));
    let mut next = path.clone().into_os_string();
    next.push(".next");
    let _ = fs::remove_file(&path);
    fifo(&path);
    let (served, halted) = (path.clone(), stop.clone());
    let server = std::thread::spawn(move || {
        for opens in 0.. {
            let mut pipe = fs::OpenOptions::new().write(true).open(&served).unwrap();
            if halted.load(SeqCst) {
                return opens;
            }
            // A fresh FIFO takes the path before this end closes, so each open is one reader.
            fifo(Path::new(&next));
            fs::rename(&next, &served).unwrap();
            pipe.write_all(&bytes).unwrap();
        }
        unreachable!()
    });
    move || {
        stop.store(true, SeqCst);
        let mut waker = fs::OpenOptions::new();
        let _waker = waker
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)
            .unwrap();
        crate::test_support::wait::join_within(server, crate::test_support::wait::WAIT_CAP)
    }
}

#[test]
fn one_judgement_opens_marker_and_canonical_context_once() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let (root, _guards) = dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    for policy in [Some("verified"), None] {
        let context = BindingContext {
            schema: 1,
            provider: "codex".into(),
            created_at: chrono::Utc::now(),
            execution_nonce: uuid::Uuid::new_v4().simple().to_string(),
            tmux_session: format!("policy-{}", uuid::Uuid::new_v4().simple()),
            channel_id: Some(584_504),
            owner_runtime_root: tc::current_tmux_owner_marker(),
            host: None,
            expected_native_session_id: None,
            launch_mode: "fresh".into(),
            provider_root: Some(root.path().canonicalize().unwrap()),
            first_prompt_digest: None,
            source_policy: policy.map(str::to_owned),
        };
        let canonical = PreparedIncarnation::create(context.clone()).unwrap().path;
        let marker = tc::session_temp_path(&context.tmux_session, "spawn_nonce");
        fs::create_dir_all(Path::new(&marker).parent().unwrap()).unwrap();
        let markers = serve(Path::new(&marker), context.execution_nonce.clone().into());
        let contexts = serve(&canonical, fs::read(&canonical).unwrap());
        let state = match tc::with_tmux_source_authority(&context.tmux_session, disposition) {
            SourcePolicyState::Legacy => None,
            SourcePolicyState::VerifiedUnavailable => Some(None),
            SourcePolicyState::VerifiedCurrent(pin) => Some(Some(pin.into_context())),
        };
        let expected = policy.map(|_| Some(context.clone()));
        assert_eq!(
            (state, markers(), contexts()),
            (expected, 1, 1),
            "{policy:?}"
        );
    }
}

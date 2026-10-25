use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;

type Poster = Arc<dyn Fn(u64, String) -> Result<(), String> + Send + Sync>;

thread_local! {
    static SENDER: RefCell<Option<Poster>> = const { RefCell::new(None) };
}

pub(super) fn sender() -> Option<Poster> {
    SENDER.with(|sender| sender.borrow().clone())
}

struct Restore(Option<Poster>);

impl Drop for Restore {
    fn drop(&mut self) {
        SENDER.with(|sender| *sender.borrow_mut() = self.0.take());
    }
}

impl OperatorNotice {
    /// Test-only: this thread's operator notices post through `send` instead of a provider bot.
    pub(crate) fn send_on_this_thread(
        send: impl Fn(u64, String) -> Result<(), String> + Send + Sync + 'static,
    ) -> impl Drop {
        Restore(SENDER.with(|sender| sender.borrow_mut().replace(Arc::new(send))))
    }

    /// Test-only: a new process, where this fault's owner is gone and only its receipt is left.
    pub(crate) fn restart_for_test(&self) -> bool {
        let mut owners = lock(&OPERATOR);
        let before = owners.len();
        owners.retain(|scope, _| scope.token_hash != self.fault);
        owners.len() < before
    }
}

fn notice(root: &Path, home: &str, channel: u64) -> OperatorNotice {
    OperatorNotice {
        root: Some(root.to_path_buf()),
        ..OperatorNotice::herdr_home("notice-node", &PathBuf::from(home), channel)
    }
}

fn paused() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .start_paused(true)
        .build()
        .unwrap()
}

const RESTART: &str = "herdr_config_restart_required";
const JOB_ENV: &str = "herdr_job_env_mismatch";

/// One logical notice per cause episode: repeated ticks, a restart that restores the receipt and
/// a failed post retried later add none; a changed cause and a recurrence after recovery add one.
#[test]
fn one_operator_notice_per_cause_episode_across_ticks_restarts_and_retries() {
    let root = tempfile::tempdir().unwrap();
    let notice = notice(root.path(), "/managed/notice-home", 41);
    let sent: Arc<Mutex<Vec<(u64, String)>>> = Arc::default();
    let down = Arc::new(AtomicBool::new(true));
    let _send = OperatorNotice::send_on_this_thread({
        let (sent, down) = (sent.clone(), down.clone());
        move |channel, text| {
            if down.load(Ordering::SeqCst) {
                return Err("discord unavailable".into());
            }
            lock(&sent).push((channel, text));
            Ok(())
        }
    });
    let count = || lock(&sent).len();
    paused().block_on(async {
        let advance = tokio::time::advance;
        assert_eq!(notice.observe(Some(RESTART)).await, "armed");
        advance(Duration::from_secs(61)).await;
        assert_eq!(notice.observe(Some(RESTART)).await, "armed");
        assert_eq!(count(), 0, "a failed post is not a sent notice");
        down.store(false, Ordering::SeqCst);
        advance(Duration::from_secs(31)).await;
        assert_eq!(notice.observe(Some(RESTART)).await, "sent");
        for _ in 0..5 {
            advance(Duration::from_secs(30)).await;
            assert_eq!(notice.observe(Some(RESTART)).await, "sent");
        }
        assert_eq!(count(), 1);

        assert!(notice.restart_for_test());
        for _ in 0..3 {
            notice.observe(Some(RESTART)).await;
            advance(Duration::from_secs(61)).await;
        }
        assert_eq!(count(), 1, "a restored receipt is not sent again");

        notice.observe(Some(JOB_ENV)).await;
        advance(Duration::from_secs(61)).await;
        notice.observe(Some(JOB_ENV)).await;
        assert_eq!(count(), 2, "a changed cause is a new episode");
        assert_eq!(notice.observe(None).await, "none");
        notice.observe(Some(JOB_ENV)).await;
        advance(Duration::from_secs(61)).await;
        notice.observe(Some(JOB_ENV)).await;
        assert_eq!(count(), 3, "a recurrence after recovery is a new episode");
    });
    let sent = lock(&sent).clone();
    assert!(sent.iter().all(|(channel, _)| *channel == 41));
    assert!(sent[0].1.starts_with("-# ⚠️ herdr 서버 재시작 필요"));
    assert!(sent[0].1.contains(r"herdr\_config\_restart\_required"));
    assert!(sent[1].1.contains(r"herdr\_job\_env\_mismatch"));
    assert!(
        sent.iter().all(|(_, text)| !text.contains("/managed")),
        "a notice names no path"
    );
}

/// Operator owners stay out of the turn-presence Unknown view even on a confirmed turn channel.
#[test]
fn operator_notices_never_appear_as_turn_unknowns() {
    const CHANNEL: u64 = 7_189_000_043;
    let root = tempfile::tempdir().unwrap();
    let notice = notice(root.path(), "/managed/snapshot-home", CHANNEL);
    let _turns = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let _send = OperatorNotice::send_on_this_thread(|_, _| Ok(()));
    paused().block_on(async {
        notice.observe(Some(RESTART)).await;
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(notice.observe(Some(RESTART)).await, "sent");
        assert!(
            super::super::snapshots()
                .iter()
                .all(|unknown| unknown.channel_id != CHANNEL),
            "an operator fault was listed as a turn Unknown"
        );
    });
}

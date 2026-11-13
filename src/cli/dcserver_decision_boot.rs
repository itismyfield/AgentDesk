use crate::services::decision_gate::{self, SecretString};
use tokio_util::sync::CancellationToken;

static STAGED_KEY: std::sync::Mutex<Option<Option<SecretString>>> = std::sync::Mutex::new(None);

pub(crate) fn stage(captured: Option<SecretString>) {
    *STAGED_KEY
        .lock()
        .expect("decision startup staging poisoned") = Some(captured);
}

pub(crate) async fn initialize_staged() -> Result<(), &'static str> {
    let staged = STAGED_KEY
        .lock()
        .expect("decision startup staging poisoned")
        .take();
    // Shared bootstrap callers outside dcserver never stage a key or start a lookup.
    let Some(captured) = staged else {
        return Ok(());
    };
    if initialize(captured).await {
        Ok(())
    } else {
        Err("dcserver startup cancelled")
    }
}

pub(super) async fn initialize(captured: Option<SecretString>) -> bool {
    let Ok(termination) = StartupTermination::listen() else {
        tracing::info!(outcome = "Off(cancelled)", "Decision gate availability");
        return false;
    };
    let cancel = CancellationToken::new();
    let lookup = decision_gate::initialize(captured, &cancel);
    tokio::pin!(lookup);
    tokio::select! {
        biased;
        _ = termination.wait() => {
            cancel.cancel();
            lookup.await;
            false
        }
        _ = &mut lookup => true,
    }
}

#[cfg(unix)]
static TERMINATED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
struct StartupTermination(libc::sigaction);

#[cfg(unix)]
extern "C" fn mark_terminated(_: libc::c_int) {
    TERMINATED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(unix)]
impl StartupTermination {
    fn listen() -> std::io::Result<Self> {
        TERMINATED.store(false, std::sync::atomic::Ordering::SeqCst);
        // This temporary handler avoids changing Tokio's process-wide signal registry.
        unsafe {
            let mut previous: libc::sigaction = std::mem::zeroed();
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = mark_terminated as *const () as usize;
            libc::sigemptyset(&mut action.sa_mask);
            if libc::sigaction(libc::SIGTERM, &action, &mut previous) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(previous))
        }
    }

    async fn wait(&self) {
        while !TERMINATED.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}

#[cfg(unix)]
impl Drop for StartupTermination {
    fn drop(&mut self) {
        // Restore the prior disposition before replaying a signal, after owned child cleanup.
        unsafe {
            libc::sigaction(libc::SIGTERM, &self.0, std::ptr::null_mut());
            if TERMINATED.swap(false, std::sync::atomic::Ordering::SeqCst) {
                libc::raise(libc::SIGTERM);
            }
        }
    }
}

#[cfg(not(unix))]
struct StartupTermination;

#[cfg(not(unix))]
impl StartupTermination {
    fn listen() -> std::io::Result<Self> {
        Ok(Self)
    }

    async fn wait(&self) {
        std::future::pending::<()>().await;
    }
}

#[cfg(all(test, unix))]
#[path = "dcserver_decision_boot_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "dcserver_decision_stage_tests.rs"]
mod stage_tests;

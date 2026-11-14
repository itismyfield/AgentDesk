//! Child ownership that keeps natural exit cleanup alive after an early return.

use std::io::{self, Read};
use std::ops::{Deref, DerefMut};
use std::process::{Child, Command, ExitStatus, Output};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::thread::ScopedJoinHandle;
use std::time::Duration;

use crate::services::provider::CancelToken;

type Pending = Arc<PendingQueue>;
const POLL: Duration = Duration::from_millis(20);

#[derive(Default)]
struct PendingQueue {
    children: Mutex<Vec<PendingChild>>,
    ready: Condvar,
}

struct PendingChild {
    child: Child,
    cancel: Option<CancelRegistration>,
}

struct CancelRegistration {
    token: Weak<CancelToken>,
    generation: u64,
}

impl PendingChild {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        if let Some(registration) = &self.cancel
            && let Some(token) = registration.token.upgrade()
        {
            return token.try_reap_child(&mut self.child, registration.generation);
        }
        self.child.try_wait()
    }
}

fn pending() -> io::Result<Pending> {
    static PENDING: Mutex<Option<Pending>> = Mutex::new(None);
    let mut pool = PENDING.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(pending) = pool.as_ref() {
        return Ok(pending.clone());
    }
    let pending = Pending::default();
    let children = pending.clone();
    std::thread::Builder::new()
        .name("child-reaper".into())
        .spawn(move || {
            loop {
                let mut current = {
                    let mut queued = children
                        .children
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    while queued.is_empty() {
                        queued = children
                            .ready
                            .wait(queued)
                            .unwrap_or_else(|error| error.into_inner());
                    }
                    std::mem::take(&mut *queued)
                };
                #[cfg(all(test, target_os = "linux"))]
                tests::record_poll();
                current.retain_mut(|child| match child.try_wait() {
                    Ok(Some(_)) => false,
                    Ok(None) => true,
                    Err(error) => {
                        tracing::debug!(pid = child.child.id(), %error, "child reaper will retry wait");
                        true
                    }
                });
                if !current.is_empty() {
                    let mut queued = children
                        .children
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    if queued.is_empty() {
                        *queued = current;
                    } else {
                        queued.append(&mut current);
                    }
                }
                std::thread::sleep(POLL);
            }
        })?;
    *pool = Some(pending.clone());
    Ok(pending)
}

/// Dropping this owner closes its pipes and schedules a wait, without killing the child.
pub(crate) struct ReapingChild {
    child: Option<Child>,
    pending: Pending,
    cancel: Option<CancelRegistration>,
}

pub(crate) trait CommandReapExt {
    fn spawn_reaping(&mut self) -> io::Result<ReapingChild>;
}

impl CommandReapExt for Command {
    fn spawn_reaping(&mut self) -> io::Result<ReapingChild> {
        // Establish the reaper before creating a child, so thread exhaustion cannot orphan it.
        let pending = pending()?;
        Ok(ReapingChild {
            child: Some(self.spawn()?),
            pending,
            cancel: None,
        })
    }
}

impl Deref for ReapingChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        match &self.child {
            Some(child) => child,
            None => unreachable!("child has already been reaped and released"),
        }
    }
}

impl DerefMut for ReapingChild {
    fn deref_mut(&mut self) -> &mut Child {
        match &mut self.child {
            Some(child) => child,
            None => unreachable!("child has already been reaped and released"),
        }
    }
}

impl ReapingChild {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(crate) fn mark_reaped(&mut self) {
        // A raw wait consumed this PID; never poll the numeric PID again after reuse.
        drop(self.child.take());
    }

    pub(crate) fn track_cancel_token(&mut self, token: Option<&Arc<CancelToken>>) {
        self.cancel = token.map(|token| CancelRegistration {
            token: Arc::downgrade(token),
            generation: token.child_reap_generation(),
        });
    }

    pub(crate) fn wait_with_output(mut self) -> io::Result<Output> {
        drop(self.stdin.take());
        let stdout = self.stdout.take();
        let stderr = self.stderr.take();
        // Drain both pipes concurrently and keep ownership through read/wait errors.
        std::thread::scope(|scope| {
            let stdout = stdout.map(|pipe| scope.spawn(|| read_pipe(pipe)));
            let stderr = stderr.map(|pipe| scope.spawn(|| read_pipe(pipe)));
            #[cfg(all(test, target_os = "linux"))]
            tests::output_join_started();
            let stdout = join_pipe(stdout, "stdout");
            let stderr = join_pipe(stderr, "stderr");
            let stdout = stdout?;
            let stderr = stderr?;
            let status = self.wait()?;
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        })
    }
}

fn read_pipe(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe(
    reader: Option<ScopedJoinHandle<'_, io::Result<Vec<u8>>>>,
    name: &str,
) -> io::Result<Vec<u8>> {
    match reader {
        Some(reader) => reader
            .join()
            .map_err(|_| io::Error::other(format!("{name} reader panicked")))?,
        None => Ok(Vec::new()),
    }
}

impl Drop for ReapingChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if self.cancel.is_none() && matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            drop(child.stdin.take());
            drop(child.stdout.take());
            drop(child.stderr.take());
            self.pending
                .children
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(PendingChild {
                    child,
                    cancel: self.cancel.take(),
                });
            self.pending.ready.notify_one();
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "reaping/tests.rs"]
pub(crate) mod tests;

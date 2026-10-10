use std::io;
use std::process::{Command, Output};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

/// `killed`/`reaped` describe cleanup only; the command may already have had an effect.
#[derive(Debug, thiserror::Error)]
pub enum BoundedTmuxError {
    #[error("could not spawn tmux: {0}")]
    Spawn(#[source] io::Error),
    #[error("tmux timed out (killed={killed}, reaped={reaped})")]
    Timeout { killed: bool, reaped: bool },
    #[error("tmux I/O failed (killed={killed}, reaped={reaped}): {source}")]
    Io {
        #[source]
        source: io::Error,
        killed: bool,
        reaped: bool,
    },
    #[error("bounded tmux requires macOS or Linux")]
    UnsupportedPlatform,
}

impl BoundedTmuxError {
    /// Whether this failure can follow execution with an externally visible effect.
    pub fn may_have_effect(&self) -> bool {
        matches!(self, Self::Timeout { .. } | Self::Io { .. })
    }
}

/// Run a caller-selected tmux command for at most five seconds, plus two to reap.
/// Stdin is closed; load-buffer callers must provide a durable file path.
pub async fn run_bounded_tmux(command: &mut Command) -> Result<Output, BoundedTmuxError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        spawn_bounded(&Handle::current(), command)?.wait().await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = command;
        Err(BoundedTmuxError::UnsupportedPlatform)
    }
}

pub struct Pending {
    cleanup: JoinHandle<Result<Output, BoundedTmuxError>>,
    #[cfg(test)]
    start: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Pending {
    pub async fn wait(self) -> Result<Output, BoundedTmuxError> {
        #[cfg(test)]
        if let Some(start) = self.start {
            let _ = start.send(());
        }
        self.cleanup.await.map_err(|error| BoundedTmuxError::Io {
            source: io::Error::other(error),
            killed: false,
            reaped: false,
        })?
    }
}

/// Spawn immediately; the supplied runtime owns cleanup independently of `Pending`.
pub fn spawn_bounded(handle: &Handle, command: &mut Command) -> Result<Pending, BoundedTmuxError> {
    spawn_with_budget(handle, command, Duration::from_secs(5))
}

pub(super) async fn run_with_budget(
    command: &mut Command,
    budget: Duration,
) -> Result<Output, BoundedTmuxError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        spawn_with_budget(&Handle::current(), command, budget)?
            .wait()
            .await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (command, budget);
        Err(BoundedTmuxError::UnsupportedPlatform)
    }
}

pub(super) fn spawn_with_budget(
    handle: &Handle,
    command: &mut Command,
    budget: Duration,
) -> Result<Pending, BoundedTmuxError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn().map_err(BoundedTmuxError::Spawn)?;
        let cleanup = platform::run(child, budget.min(Duration::from_secs(5)));
        #[cfg(test)]
        if super::transition::mutant("cleanup_wait") {
            let (start, started) = tokio::sync::oneshot::channel();
            return Ok(Pending {
                cleanup: handle.spawn(async move {
                    if started.await.is_err() {
                        std::future::pending::<()>().await;
                    }
                    cleanup.await
                }),
                start: Some(start),
            });
        }
        Ok(Pending {
            cleanup: handle.spawn(cleanup),
            #[cfg(test)]
            start: None,
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (handle, command, budget);
        Err(BoundedTmuxError::UnsupportedPlatform)
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod platform {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Child, ExitStatus};
    use std::time::Instant;

    const POLL: Duration = Duration::from_millis(5);
    const OUTPUT_LIMIT: usize = 1024 * 1024;

    struct OwnedChild {
        child: Child,
        owned: bool,
        cleanup_until: Option<Instant>,
    }

    impl OwnedChild {
        fn pid(&self) -> libc::pid_t {
            self.child.id() as libc::pid_t
        }

        fn exited(&mut self) -> io::Result<bool> {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid() as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            if rc == 0 {
                return Ok(unsafe { info.si_pid() } != 0);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.owned = false;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            Err(error)
        }

        fn reap(&mut self) -> io::Result<Option<ExitStatus>> {
            if !self.owned {
                return Ok(None);
            }
            let mut status = 0;
            let rc = unsafe { libc::waitpid(self.pid(), &mut status, libc::WNOHANG) };
            if rc == self.pid() {
                self.owned = false;
                return Ok(Some(ExitStatus::from_raw(status)));
            }
            if rc == 0 {
                return Ok(None);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.owned = false;
            }
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            Err(error)
        }

        fn kill(&self) -> bool {
            self.owned && unsafe { libc::killpg(self.pid(), libc::SIGKILL) } == 0
        }

        fn cleanup_deadline(&mut self) -> Instant {
            *self
                .cleanup_until
                .get_or_insert_with(|| Instant::now() + Duration::from_secs(2))
        }

        async fn terminate(&mut self) -> (bool, bool) {
            #[cfg(test)]
            if super::super::transition::mutant("kill_reap") {
                self.owned = false;
                return (false, false);
            }
            let killed = self.kill();
            let until = self.cleanup_deadline();
            loop {
                match self.reap() {
                    Ok(Some(_)) => return (killed, true),
                    Err(_) => return (killed, false),
                    Ok(None) => {}
                }
                if !self.owned || Instant::now() >= until {
                    return (killed, false);
                }
                tokio::time::sleep(POLL.min(until.saturating_duration_since(Instant::now()))).await;
            }
        }
    }

    impl Drop for OwnedChild {
        fn drop(&mut self) {
            if self.owned {
                self.kill();
                #[cfg(test)]
                if super::super::transition::mutant("drop_reap") {
                    let _ = self.reap();
                    return;
                }
                let until = self.cleanup_deadline();
                // Runtime shutdown can cancel async cleanup before the killed child exits.
                while matches!(self.reap(), Ok(None)) && self.owned && Instant::now() < until {
                    std::thread::sleep(POLL.min(until.saturating_duration_since(Instant::now())));
                }
            }
        }
    }

    fn nonblocking(pipe: &impl AsRawFd) -> io::Result<()> {
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn drain<R: Read>(pipe: &mut Option<R>, output: &mut Vec<u8>) -> io::Result<bool> {
        let Some(pipe) = pipe else { return Ok(true) };
        let mut bytes = [0; 8192];
        // Bound each drain pass so an active writer cannot postpone the deadline.
        for _ in 0..8 {
            match pipe.read(&mut bytes) {
                Ok(0) => return Ok(true),
                Ok(n) => {
                    if output.len() + n > OUTPUT_LIMIT {
                        return Err(io::Error::other("tmux output exceeds 1 MiB per stream"));
                    }
                    output.extend_from_slice(&bytes[..n]);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => return Ok(false),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    pub(super) fn run(
        child: Child,
        budget: Duration,
    ) -> impl std::future::Future<Output = Result<Output, BoundedTmuxError>> {
        let mut owner = OwnedChild {
            child,
            owned: true,
            cleanup_until: None,
        };
        let deadline = Instant::now() + budget;
        async move {
            #[cfg(test)]
            let deadline = if super::super::transition::mutant("deadline_wait") {
                Instant::now() + budget
            } else {
                deadline
            };
            let result = async {
                if let Some(pipe) = &owner.child.stdout {
                    nonblocking(pipe)?;
                }
                if let Some(pipe) = &owner.child.stderr {
                    nonblocking(pipe)?;
                }
                let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
                loop {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                    let out_done = drain(&mut owner.child.stdout, &mut stdout)?;
                    let err_done = drain(&mut owner.child.stderr, &mut stderr)?;
                    if owner.exited()? && out_done && err_done {
                        if let Some(status) = owner.reap()? {
                            return Ok(Some(Output {
                                status,
                                stdout,
                                stderr,
                            }));
                        }
                    }
                    tokio::time::sleep(
                        POLL.min(deadline.saturating_duration_since(Instant::now())),
                    )
                    .await;
                }
            }
            .await;
            let source = match result {
                Ok(Some(output)) => return Ok(output),
                Ok(None) => None,
                Err(source) => Some(source),
            };
            let (killed, reaped) = owner.terminate().await;
            Err(match source {
                None => BoundedTmuxError::Timeout { killed, reaped },
                Some(source) => BoundedTmuxError::Io {
                    source,
                    killed,
                    reaped,
                },
            })
        }
    }
}

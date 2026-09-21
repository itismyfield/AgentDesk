pub(crate) mod stream_queue;

use std::io;
use std::process::{Child, ExitStatus};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::services::provider::{CancelToken, CancelWatchdog, cancel_requested};

pub(crate) const EXIT_POLL: Duration = Duration::from_millis(100);
const EXIT_DRAIN: Duration = Duration::from_secs(1);

pub(crate) struct StreamChild {
    pid: u32,
    #[cfg(unix)]
    owned_group: bool,
    status: Option<ExitStatus>,
    exited_at: Option<Instant>,
    token: Option<Arc<CancelToken>>,
    watchdog: Option<CancelWatchdog>,
}

impl StreamChild {
    pub(crate) fn new(
        child: &Child,
        token: Option<Arc<CancelToken>>,
        watchdog: Option<CancelWatchdog>,
    ) -> Self {
        let pid = child.id();
        #[cfg(all(test, unix))]
        stream_queue::test_delay::register_pid(pid);
        Self {
            pid,
            #[cfg(unix)]
            // The unreaped group leader reserves this PID until cleanup completes.
            owned_group: pid > 1 && unsafe { libc::getpgid(pid as i32) == pid as i32 && libc::getpgrp() != pid as i32 },
            status: None,
            exited_at: None,
            token,
            watchdog,
        }
    }

    fn detach(&mut self) {
        drop(self.watchdog.take());
        if let Some(token) = &self.token {
            token.clear_child_pid_if_matches(self.pid);
        }
    }

    fn observe_exit(&mut self, child: &mut Child) -> io::Result<()> {
        if self.status.is_some() {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // WNOWAIT keeps the dead leader's identity reserved while its group is cleaned.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pid as libc::id_t,
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc != 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    return Ok(());
                }
                return Err(error);
            }
            if unsafe { info.si_pid() } == 0 {
                return Ok(());
            }
            self.detach();
            if self.owned_group {
                unsafe {
                    libc::kill(-(self.pid as i32), libc::SIGTERM);
                }
                std::thread::sleep(Duration::from_millis(200));
                unsafe {
                    libc::kill(-(self.pid as i32), libc::SIGKILL);
                }
            }
            self.status = Some(child.wait()?);
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn WaitForSingleObject(handle: *mut std::ffi::c_void, milliseconds: u32) -> u32;
            }
            match unsafe { WaitForSingleObject(child.as_raw_handle(), 0) } {
                0 => {
                    self.detach();
                    self.status = Some(child.wait()?);
                }
                258 => return Ok(()),
                _ => return Err(io::Error::last_os_error()),
            }
        }
        if self.status.is_some() {
            self.exited_at = Some(Instant::now());
            #[cfg(all(test, unix))]
            stream_queue::test_delay::after_exit();
        }
        Ok(())
    }

    pub(crate) fn observe_and_seal<T>(
        &mut self,
        child: &mut Child,
        output: &stream_queue::Receiver<T>,
    ) -> io::Result<()> {
        self.observe_exit(child)?;
        if self.exited_at.is_some_and(|at| at.elapsed() >= EXIT_DRAIN) {
            output.seal();
        }
        Ok(())
    }

    pub(crate) fn terminate(&mut self, child: &mut Child) {
        if self.observe_exit(child).is_err() {
            return;
        }
        if self.status.is_none() {
            self.detach();
            super::kill_child_tree(child);
            self.status = child.wait().ok();
            self.exited_at = Some(Instant::now());
        }
    }

    pub(crate) fn wait(&mut self, child: &mut Child) -> io::Result<ExitStatus> {
        loop {
            self.observe_exit(child)?;
            if let Some(status) = self.status {
                return Ok(status);
            }
            if cancel_requested(self.token.as_deref()) {
                self.terminate(child);
            }
            std::thread::sleep(EXIT_POLL);
        }
    }
}

pub(crate) fn spawn_reader<T: Send + 'static>(
    read: impl FnOnce() -> T + Send + 'static,
) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(read());
    });
    rx
}

// Called only after child exit/termination; escaped descendants cannot hold up finalization.
pub(crate) fn finish_reader<T: Default>(rx: &mpsc::Receiver<T>) -> T {
    rx.recv_timeout(EXIT_DRAIN).unwrap_or_default()
}

#[cfg(all(test, unix))]
pub(crate) mod test_fixture;

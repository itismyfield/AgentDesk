//! Own dcserver descendants across normal exit, crashes and Task Scheduler stop.
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

// Static storage deliberately outlives Rust shutdown. Windows closes this
// non-inheritable handle even when TerminateProcess skips every destructor.
static RUNTIME_JOB: OnceLock<Result<OwnedHandle, String>> = OnceLock::new();

pub(crate) fn own_runtime_children() -> Result<(), String> {
    RUNTIME_JOB
        .get_or_init(|| create_runtime_job().map_err(|error| error.to_string()))
        .as_ref()
        .map(|_| ())
        .map_err(Clone::clone)
}

fn create_runtime_job() -> io::Result<OwnedHandle> {
    // SAFETY: null security attributes request a non-inheritable handle; the
    // unnamed job is private to this process, so no unrelated job is opened.
    let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateJobObjectW returned a new owned handle. OwnedHandle closes
    // it on every error path; successful ownership is retained in RUNTIME_JOB.
    let job = unsafe { OwnedHandle::from_raw_handle(raw) };
    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    // SAFETY: the structure and byte length match the selected Windows class;
    // the live job handle remains owned throughout both synchronous calls.
    if unsafe {
        SetInformationJobObject(
            job.as_raw_handle(),
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: GetCurrentProcess returns a valid pseudo-handle. Assign before
    // starting any dcserver children, so even fast wrapper grandchildren are
    // covered without a spawn-then-assign race. Nested jobs work on Windows 8+.
    if unsafe { AssignProcessToJobObject(job.as_raw_handle(), GetCurrentProcess()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::os::windows::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn fixture(root: &Path, mode: &str) -> Child {
        let module = module_path!().split_once("::").unwrap().1;
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("{module}::runtime_job_descendant_fixture"),
                "--nocapture",
            ])
            .env("AGENTDESK_TEST_JOB_ROOT", root)
            .env("AGENTDESK_TEST_JOB_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .creation_flags(0x08000000)
            .spawn()
            .unwrap()
    }

    #[test]
    fn runtime_job_descendant_fixture() {
        let Ok(mode) = std::env::var("AGENTDESK_TEST_JOB_MODE") else {
            return;
        };
        let root = std::path::PathBuf::from(std::env::var_os("AGENTDESK_TEST_JOB_ROOT").unwrap());
        if mode == "parent" {
            own_runtime_children().unwrap();
            fixture(&root, "middle").wait().unwrap();
        } else if mode == "middle" {
            fixture(&root, "leaf").wait().unwrap();
        } else {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            std::fs::write(
                root.join("port"),
                listener.local_addr().unwrap().port().to_string(),
            )
            .unwrap();
            std::thread::sleep(Duration::from_secs(20));
            drop(listener);
        }
    }

    #[test]
    fn forced_runtime_exit_terminates_grandchildren_and_releases_ports() {
        struct Cleanup(Child);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let mut parent = Cleanup(fixture(root.path(), "parent"));
        let deadline = Instant::now() + Duration::from_secs(10);
        let port = loop {
            if let Ok(raw) = std::fs::read_to_string(root.path().join("port"))
                && let Ok(port) = raw.parse::<u16>()
            {
                break port;
            }
            assert!(Instant::now() < deadline, "descendant did not start");
            std::thread::sleep(Duration::from_millis(25));
        };
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
        parent.0.kill().unwrap();
        parent.0.wait().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if TcpListener::bind(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "orphaned grandchild retained its port"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

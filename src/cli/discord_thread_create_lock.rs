//! Cross-process lock for idempotent Discord thread creation.
//!
//! Unix uses a protected per-euid directory plus `flock`; after acquiring the
//! lock it pins the pathname back to the opened device/inode. Windows uses a
//! kernel named mutex instead of a pathname: the `Global` object name is keyed
//! by the current token SID, and creation supplies a protected owner-only DACL.
//! This keeps one namespace for service and interactive processes of the same
//! account across sessions, without trusting mutable environment variables.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
#[cfg(unix)]
use std::path::{Path, PathBuf};

pub(crate) struct ThreadCreateFileLock {
    #[cfg(unix)]
    pub(crate) file: fs::File,
    #[cfg(windows)]
    owner: Option<windows::MutexOwner>,
}

impl Drop for ThreadCreateFileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            let _ = self.file.unlock();
        }
        #[cfg(windows)]
        if let Some(owner) = self.owner.take()
            && let Err(err) = owner.release()
        {
            // Drop cannot return an error, but a failed release must be
            // operator-visible: silently continuing would make the next CLI
            // invocation appear to hang behind a leaked mutex.
            eprintln!("ERROR: release thread-create Windows mutex: {err}");
        }
    }
}

#[cfg(unix)]
pub(crate) fn lock_root() -> Result<PathBuf, String> {
    // A fixed OS temp root plus effective UID makes all AgentDesk processes
    // for one Unix account converge even when TMPDIR or AGENTDESK_ROOT_DIR
    // differs. /tmp's sticky bit protects the per-UID directory name.
    let system_temp = fs::canonicalize("/tmp")
        .map_err(|err| format!("resolve canonical system temp directory /tmp: {err}"))?;
    Ok(
        system_temp.join(format!("agentdesk-thread-create-locks-{}", unsafe {
            libc::geteuid()
        })),
    )
}

#[cfg(unix)]
fn verify_secure_lock_directory(root: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(root).map_err(|err| {
        format!(
            "inspect thread-create lock directory {}: {err}",
            root.display()
        )
    })?;
    let expected_uid = unsafe { libc::geteuid() };
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != expected_uid
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(format!(
            "thread-create lock directory {} must be a non-symlink directory owned by uid {expected_uid} with mode 0700",
            root.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn ensure_secure_lock_directory(root: &Path) -> Result<(), String> {
    match fs::DirBuilder::new().mode(0o700).create(root) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(err) => {
            return Err(format!(
                "create thread-create lock directory {}: {err}",
                root.display()
            ));
        }
    }
    verify_secure_lock_directory(root)
}

#[cfg(unix)]
pub(crate) fn lock_path(lock_name: &str) -> Result<PathBuf, String> {
    let root = lock_root()?;
    ensure_secure_lock_directory(&root)?;
    Ok(root.join(format!("{lock_name}.lock")))
}

#[cfg(unix)]
fn verify_locked_path_identity(path: &Path, file: &fs::File) -> Result<(), String> {
    let path_metadata = fs::symlink_metadata(path)
        .map_err(|err| format!("inspect thread-create lock {}: {err}", path.display()))?;
    let file_metadata = file.metadata().map_err(|err| {
        format!(
            "inspect opened thread-create lock {}: {err}",
            path.display()
        )
    })?;
    let expected_uid = unsafe { libc::geteuid() };
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || !file_metadata.is_file()
        || file_metadata.uid() != expected_uid
        || file_metadata.mode() & 0o777 != 0o600
        || path_metadata.dev() != file_metadata.dev()
        || path_metadata.ino() != file_metadata.ino()
    {
        return Err(format!(
            "thread-create lock {} must remain the opened regular file owned by uid {expected_uid} with mode 0600",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn acquire_unix(lock_name: &str) -> Result<ThreadCreateFileLock, String> {
    let path = lock_path(lock_name)?;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|err| format!("open thread-create lock {}: {err}", path.display()))?;
    file.lock()
        .map_err(|err| format!("lock thread-create key {}: {err}", path.display()))?;

    // The 0700, current-euid-owned parent excludes other Unix principals. Pin
    // the post-lock pathname to the opened object so a temp cleaner or a race
    // before flock cannot leave this process locking an unlinked old inode.
    verify_locked_path_identity(&path, &file)?;
    Ok(ThreadCreateFileLock { file })
}

pub(crate) fn acquire(lock_name: &str) -> Result<ThreadCreateFileLock, String> {
    #[cfg(unix)]
    {
        acquire_unix(lock_name)
    }
    #[cfg(windows)]
    {
        windows::acquire(lock_name)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = lock_name;
        Err("thread-create cross-process locking is unsupported on this platform".into())
    }
}

#[cfg(all(test, unix))]
mod unix_identity_tests {
    use std::fs;
    use std::os::unix::fs::OpenOptionsExt as _;

    use super::verify_locked_path_identity;

    #[test]
    fn locked_handle_rejects_replaced_path_inode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lock");
        let displaced = temp.path().join("displaced");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.lock().unwrap();
        fs::rename(&path, &displaced).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();

        assert!(
            verify_locked_path_identity(&path, &file).is_err(),
            "a lock on the displaced inode must not authenticate its replacement"
        );
    }
}

#[cfg(windows)]
pub(crate) mod windows {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::ThreadCreateFileLock;

    pub(super) type Handle = *mut c_void;
    type Dword = u32;
    type Bool = i32;

    const TOKEN_QUERY: Dword = 0x0008;
    const TOKEN_USER_CLASS: Dword = 1;
    const ERROR_INSUFFICIENT_BUFFER: Dword = 122;
    const SDDL_REVISION_1: Dword = 1;
    const SE_KERNEL_OBJECT: Dword = 6;
    const OWNER_SECURITY_INFORMATION: Dword = 0x0000_0001;
    const MUTEX_ALL_ACCESS: Dword = 0x001F_0001;
    const INFINITE: Dword = 0xFFFF_FFFF;
    const WAIT_OBJECT_0: Dword = 0;
    const WAIT_ABANDONED: Dword = 0x0000_0080;
    const WAIT_FAILED: Dword = 0xFFFF_FFFF;
    const RELEASE_ACK_TIMEOUT: Duration = Duration::from_secs(5);
    const OWNER_JOIN_TIMEOUT: Duration = Duration::from_secs(1);

    #[repr(C)]
    struct SidAndAttributes {
        sid: *mut c_void,
        attributes: Dword,
    }

    #[repr(C)]
    struct TokenUser {
        user: SidAndAttributes,
    }

    #[repr(C)]
    struct SecurityAttributes {
        length: Dword,
        security_descriptor: *mut c_void,
        inherit_handle: Bool,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> Handle;
        pub(super) fn CloseHandle(handle: Handle) -> Bool;
        fn GetLastError() -> Dword;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
        fn CreateMutexExW(
            attributes: *const SecurityAttributes,
            name: *const u16,
            flags: Dword,
            desired_access: Dword,
        ) -> Handle;
        fn WaitForSingleObject(handle: Handle, milliseconds: Dword) -> Dword;
        pub(super) fn ReleaseMutex(handle: Handle) -> Bool;
    }

    #[link(name = "advapi32")]
    unsafe extern "system" {
        fn OpenProcessToken(process: Handle, desired_access: Dword, token: *mut Handle) -> Bool;
        fn GetTokenInformation(
            token: Handle,
            information_class: Dword,
            information: *mut c_void,
            information_length: Dword,
            return_length: *mut Dword,
        ) -> Bool;
        fn ConvertSidToStringSidW(sid: *mut c_void, string_sid: *mut *mut u16) -> Bool;
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16,
            revision: Dword,
            descriptor: *mut *mut c_void,
            descriptor_size: *mut Dword,
        ) -> Bool;
        fn GetSecurityInfo(
            handle: Handle,
            object_type: Dword,
            security_info: Dword,
            owner: *mut *mut c_void,
            group: *mut *mut c_void,
            dacl: *mut *mut c_void,
            sacl: *mut *mut c_void,
            descriptor: *mut *mut c_void,
        ) -> Dword;
        fn EqualSid(first: *mut c_void, second: *mut c_void) -> Bool;
    }

    struct OwnedHandle(Handle);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    pub(super) struct MutexOwner {
        release_request: mpsc::Sender<()>,
        release_result: mpsc::Receiver<Result<(), String>>,
        join: Option<thread::JoinHandle<()>>,
    }

    impl MutexOwner {
        pub(super) fn release(mut self) -> Result<(), String> {
            let request_error = self
                .release_request
                .send(())
                .err()
                .map(|err| format!("signal mutex owner thread: {err}"));
            let release_result = self
                .release_result
                .recv_timeout(RELEASE_ACK_TIMEOUT)
                .map_err(|err| format!("await mutex owner release acknowledgement: {err}"));

            let join_result = self
                .join
                .take()
                .map(join_owner_thread_bounded)
                .transpose()
                .map(|_| ());
            if let Some(error) = request_error {
                return Err(error);
            }
            match (release_result, join_result) {
                (Ok(Ok(())), Ok(())) => Ok(()),
                (Ok(Err(release_error)), Ok(())) => Err(release_error),
                (Err(ack_error), Ok(())) => Err(ack_error),
                (Ok(Ok(())), Err(join_error)) => Err(join_error),
                (Ok(Err(release_error)), Err(join_error)) => {
                    Err(format!("{release_error}; {join_error}"))
                }
                (Err(ack_error), Err(join_error)) => Err(format!("{ack_error}; {join_error}")),
            }
        }
    }

    fn join_owner_thread_bounded(handle: thread::JoinHandle<()>) -> Result<(), String> {
        let deadline = Instant::now() + OWNER_JOIN_TIMEOUT;
        while !handle.is_finished() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        if !handle.is_finished() {
            // Dropping the JoinHandle detaches instead of allowing Drop to
            // block forever. The release acknowledgement above was already
            // bounded and is the authoritative ownership result.
            return Err("mutex owner thread did not finish after release acknowledgement".into());
        }
        handle
            .join()
            .map_err(|_| "thread-create mutex owner thread panicked".to_string())
    }

    struct LocalAllocation(*mut c_void);

    impl Drop for LocalAllocation {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(self.0);
                }
            }
        }
    }

    struct CurrentUserSid {
        text: String,
        // GetTokenInformation writes pointer-aligned TOKEN_USER data. A
        // Vec<u8> only promises byte alignment and would make the cast below
        // undefined even though common allocators happen to over-align it.
        token_user: Vec<usize>,
    }

    impl CurrentUserSid {
        fn pointer(&mut self) -> *mut c_void {
            unsafe {
                (*(self.token_user.as_mut_ptr().cast::<TokenUser>()))
                    .user
                    .sid
            }
        }
    }

    fn wide_null(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn win32_error(context: &str) -> String {
        format!("{context}: Windows error {}", unsafe { GetLastError() })
    }

    fn current_user_sid() -> Result<CurrentUserSid, String> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(win32_error(
                "open current process token for thread-create lock",
            ));
        }
        let token = OwnedHandle(token);
        let mut required = 0;
        let first = unsafe {
            GetTokenInformation(token.0, TOKEN_USER_CLASS, ptr::null_mut(), 0, &mut required)
        };
        if first != 0 || required == 0 || unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER {
            return Err(win32_error("size current user SID for thread-create lock"));
        }
        let word = std::mem::size_of::<usize>();
        let mut token_user = vec![0_usize; (required as usize).div_ceil(word)];
        if unsafe {
            GetTokenInformation(
                token.0,
                TOKEN_USER_CLASS,
                token_user.as_mut_ptr().cast(),
                required,
                &mut required,
            )
        } == 0
        {
            return Err(win32_error("read current user SID for thread-create lock"));
        }
        let sid = unsafe { (*(token_user.as_ptr().cast::<TokenUser>())).user.sid };
        if sid.is_null() {
            return Err("current process token returned a null SID".into());
        }
        let mut raw_text = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut raw_text) } == 0 {
            return Err(win32_error(
                "format current user SID for thread-create lock",
            ));
        }
        let allocation = LocalAllocation(raw_text.cast());
        let mut length = 0;
        unsafe {
            while *raw_text.add(length) != 0 {
                length += 1;
            }
        }
        let text = String::from_utf16(unsafe { std::slice::from_raw_parts(raw_text, length) })
            .map_err(|err| format!("decode current user SID for thread-create lock: {err}"))?;
        drop(allocation);
        Ok(CurrentUserSid { text, token_user })
    }

    fn mutex_name(sid: &str, lock_name: &str) -> String {
        format!("Global\\AgentDesk.ThreadCreate.{sid}.{lock_name}")
    }

    #[cfg(test)]
    pub(crate) fn current_mutex_name(lock_name: &str) -> Result<String, String> {
        let sid = current_user_sid()?;
        Ok(mutex_name(&sid.text, lock_name))
    }

    fn owner_only_security_descriptor(sid: &str) -> Result<LocalAllocation, String> {
        // O:<sid> pins ownership. D:P protects inheritance and grants exactly
        // GenericAll to that SID. The explicit medium mandatory label lets an
        // elevated service and a normal interactive token of the same SID
        // converge while excluding low-integrity sandbox processes. A
        // pre-created object owned by another SID is rejected below even if
        // its DACL tries to grant this process access.
        let sddl = wide_null(&format!("O:{sid}D:P(A;;GA;;;{sid})S:(ML;;NW;;;ME)"));
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(win32_error(
                "build owner-only security descriptor for thread-create mutex",
            ));
        }
        Ok(LocalAllocation(descriptor))
    }

    fn verify_mutex_owner(handle: Handle, expected_sid: *mut c_void) -> Result<(), String> {
        let mut owner = ptr::null_mut();
        let mut descriptor = ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                handle,
                SE_KERNEL_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            )
        };
        let descriptor = LocalAllocation(descriptor);
        if status != 0 {
            return Err(format!(
                "inspect thread-create mutex owner: Windows error {status}"
            ));
        }
        if owner.is_null() || unsafe { EqualSid(owner, expected_sid) } == 0 {
            return Err("thread-create mutex is not owned by the current user SID".into());
        }
        drop(descriptor);
        Ok(())
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum MutexWaitAcquisition {
        Normal,
        Abandoned,
    }

    fn classify_wait_status(
        status: Dword,
        last_error: Dword,
    ) -> Result<MutexWaitAcquisition, String> {
        match status {
            WAIT_OBJECT_0 => Ok(MutexWaitAcquisition::Normal),
            WAIT_ABANDONED => Ok(MutexWaitAcquisition::Abandoned),
            WAIT_FAILED => Err(format!(
                "wait for thread-create mutex: Windows error {last_error}"
            )),
            status => Err(format!(
                "wait for thread-create mutex returned unexpected status 0x{status:08x}"
            )),
        }
    }

    fn acquire_on_owner_thread(lock_name: &str) -> Result<Handle, String> {
        let mut sid = current_user_sid()?;
        let descriptor = owner_only_security_descriptor(&sid.text)?;
        let mut attributes = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as Dword,
            security_descriptor: descriptor.0,
            inherit_handle: 0,
        };
        let name = wide_null(&mutex_name(&sid.text, lock_name));
        let handle = unsafe { CreateMutexExW(&mut attributes, name.as_ptr(), 0, MUTEX_ALL_ACCESS) };
        if handle.is_null() {
            return Err(win32_error("create/open SID-scoped thread-create mutex"));
        }
        let owned_handle = OwnedHandle(handle);
        verify_mutex_owner(handle, sid.pointer())?;
        #[cfg(test)]
        if let Some(path) = std::env::var_os("ADK_THREAD_LOCK_WAIT_STARTED") {
            std::fs::write(path, b"waiting").map_err(|err| {
                format!("write thread-create mutex wait-started test marker: {err}")
            })?;
        }
        let wait_status = unsafe { WaitForSingleObject(handle, INFINITE) };
        let last_error = if wait_status == WAIT_FAILED {
            unsafe { GetLastError() }
        } else {
            0
        };
        let _acquisition = classify_wait_status(wait_status, last_error)?;
        std::mem::forget(owned_handle);
        Ok(handle)
    }

    fn release_and_close_on_owner_thread(handle: Handle) -> Result<(), String> {
        let release_error = if unsafe { ReleaseMutex(handle) } == 0 {
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        let close_error = if unsafe { CloseHandle(handle) } == 0 {
            Some(unsafe { GetLastError() })
        } else {
            None
        };
        match (release_error, close_error) {
            (None, None) => Ok(()),
            (Some(release), None) => Err(format!(
                "ReleaseMutex failed on owner thread: Windows error {release}"
            )),
            (None, Some(close)) => Err(format!("CloseHandle failed: Windows error {close}")),
            (Some(release), Some(close)) => Err(format!(
                "ReleaseMutex failed on owner thread: Windows error {release}; CloseHandle failed: Windows error {close}"
            )),
        }
    }

    pub(super) fn acquire(lock_name: &str) -> Result<ThreadCreateFileLock, String> {
        let lock_name = lock_name.to_string();
        let (acquired_tx, acquired_rx) = mpsc::sync_channel(1);
        let (release_request, release_requests) = mpsc::channel();
        let (release_result_tx, release_result) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("adk-thread-create-mutex".into())
            .spawn(move || {
                let handle = match acquire_on_owner_thread(&lock_name) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = acquired_tx.send(Err(error));
                        return;
                    }
                };
                if acquired_tx.send(Ok(())).is_err() {
                    if let Err(err) = release_and_close_on_owner_thread(handle) {
                        eprintln!("ERROR: release cancelled thread-create Windows mutex: {err}");
                    }
                    return;
                }

                // Disconnection is also a release request: if guard
                // construction or an async caller is cancelled, the owner
                // thread must not retain the mutex indefinitely.
                let _ = release_requests.recv();
                let result = release_and_close_on_owner_thread(handle);
                let _ = release_result_tx.send(result);
            })
            .map_err(|err| format!("spawn thread-create mutex owner thread: {err}"))?;

        match acquired_rx.recv() {
            Ok(Ok(())) => Ok(ThreadCreateFileLock {
                owner: Some(MutexOwner {
                    release_request,
                    release_result,
                    join: Some(join),
                }),
            }),
            Ok(Err(error)) => {
                let _ = join_owner_thread_bounded(join);
                Err(error)
            }
            Err(error) => {
                let join_error = join_owner_thread_bounded(join).err();
                Err(match join_error {
                    Some(join_error) => format!(
                        "thread-create mutex owner ended before acquisition acknowledgement: {error}; {join_error}"
                    ),
                    None => format!(
                        "thread-create mutex owner ended before acquisition acknowledgement: {error}"
                    ),
                })
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{
            MutexWaitAcquisition, WAIT_ABANDONED, WAIT_FAILED, WAIT_OBJECT_0, classify_wait_status,
        };

        #[test]
        fn wait_status_accepts_normal_and_abandoned_but_reports_errors() {
            assert_eq!(
                classify_wait_status(WAIT_OBJECT_0, 0).unwrap(),
                MutexWaitAcquisition::Normal
            );
            assert_eq!(
                classify_wait_status(WAIT_ABANDONED, 0).unwrap(),
                MutexWaitAcquisition::Abandoned
            );
            assert!(
                classify_wait_status(WAIT_FAILED, 6)
                    .unwrap_err()
                    .contains("error 6")
            );
            assert!(classify_wait_status(0x102, 0).is_err());
        }
    }
}

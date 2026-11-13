use super::{environment, requested};

#[derive(Debug)]
pub(crate) struct HeldLease {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(unix)]
    run: bool,
}

impl Drop for HeldLease {
    fn drop(&mut self) {
        #[cfg(unix)]
        if self.run
            && let Ok(mut snapshot) = RUN.lock()
        {
            *snapshot = None;
        }
    }
}

fn rejected() -> String {
    "test instance lease admission rejected".into()
}

pub(crate) fn legacy_cleanup_if_unscoped(cleanup: impl FnOnce()) {
    if !requested() {
        cleanup();
    }
}

pub(crate) fn admit_dcserver() -> Result<Option<HeldLease>, String> {
    if !requested() {
        return Ok(None);
    }
    environment::runtime_path()?.ok_or_else(rejected)?;
    #[cfg(unix)]
    {
        admit(super::current()?.ok_or_else(rejected)?).map(Some)
    }
    #[cfg(not(unix))]
    {
        Err(rejected())
    }
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Witness {
    schema_version: u8,
    fd: i32,
    device: u64,
    inode: u64,
    instance_id: String,
    root: std::path::PathBuf,
    nonce: String,
}

#[cfg(unix)]
fn admit(instance: &super::TestInstance) -> Result<HeldLease, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::FileExt;

    let encoded = std::env::var("AGENTDESK_TEST_LEASE").map_err(|_| rejected())?;
    if encoded.len() > 4096 {
        return Err(rejected());
    }
    let witness: Witness = serde_json::from_str(&encoded).map_err(|_| rejected())?;
    if witness.schema_version != 1
        || witness.fd < 3
        || witness.root != instance.root
        || witness.instance_id != instance.instance_id
        || !uuid::Uuid::parse_str(&witness.nonce)
            .is_ok_and(|nonce| !nonce.is_nil() && nonce.hyphenated().to_string() == witness.nonce)
    {
        return Err(rejected());
    }
    let flags = unsafe { libc::fcntl(witness.fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(rejected());
    }
    // Startup takes ownership once and closes it on rejection; it never unlocks the shared OFD.
    let inherited = unsafe { std::fs::File::from_raw_fd(witness.fd) };
    let metadata = inherited.metadata().map_err(|_| rejected())?;
    check_file(&metadata, &witness)?;
    let probe = open_control(&instance.root)?;
    check_file(&probe.metadata().map_err(|_| rejected())?, &witness)?;
    // An independent open must collide, and the inherited OFD must already own that lock.
    if unsafe { libc::flock(probe.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0
        || !matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EWOULDBLOCK)
        )
        || unsafe { libc::flock(inherited.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
    {
        return Err(rejected());
    }
    check_file(
        &open_control(&instance.root)?
            .metadata()
            .map_err(|_| rejected())?,
        &witness,
    )?;
    let mut bytes = [0; 4097];
    let length = inherited.read_at(&mut bytes, 0).map_err(|_| rejected())?;
    if length > 4096
        || serde_json::from_slice::<Witness>(&bytes[..length]).map_err(|_| rejected())? != witness
    {
        return Err(rejected());
    }
    if unsafe {
        libc::fcntl(
            inherited.as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(rejected());
    }
    // Admission runs before worker threads; auxiliary execs receive neither FD nor witness.
    unsafe { std::env::remove_var("AGENTDESK_TEST_LEASE") };
    let run = admit_run(instance, &witness)?;
    unsafe { std::env::remove_var("AGENTDESK_TEST_RUN") };
    Ok(HeldLease {
        _file: inherited,
        run,
    })
}

#[cfg(unix)]
use crate::services::session_host::policy::{file, peer};
#[cfg(unix)]
use std::{os::fd::AsRawFd, os::unix::fs::MetadataExt};
#[cfg(unix)]
static RUN: std::sync::Mutex<Option<RunSnapshot>> = std::sync::Mutex::new(None);
#[cfg(unix)]
static HAD_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RunInput {
    schema: u8,
    head: String,
    worktree: std::path::PathBuf,
    binary: BinaryInput,
    identities: Vec<AppIdentity>,
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BinaryInput {
    fd: i32,
    device: u64,
    inode: u64,
    sha256: String,
}

#[cfg(unix)]
#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct AppIdentity {
    name: String,
    application_id: String,
    device: u64,
    inode: u64,
    size: u64,
    mtime_ns: u64,
    ctime_ns: u64,
}

#[cfg(unix)]
struct RunSnapshot {
    input: RunInput,
    binary: std::fs::File,
    image: file::FileToken,
    nonce: String,
    bound: Option<std::net::SocketAddr>,
}

#[cfg(unix)]
fn admit_run(instance: &super::TestInstance, lease: &Witness) -> Result<bool, String> {
    use std::os::fd::FromRawFd;
    let Some(encoded) = std::env::var_os("AGENTDESK_TEST_RUN") else {
        return Ok(false);
    };
    let encoded = encoded
        .to_str()
        .filter(|value| value.len() <= 8192)
        .ok_or_else(rejected)?;
    let input: RunInput = serde_json::from_str(encoded).map_err(|_| rejected())?;
    if input.schema != 1
        || input.head != instance.head
        || input.worktree != instance.worktree
        || input.binary.fd < 3
        || input.binary.fd == lease.fd
        || input.binary.sha256.len() != 64
        || !input
            .binary
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(rejected());
    }
    let flags = unsafe { libc::fcntl(input.binary.fd, libc::F_GETFD) };
    if flags < 0
        || unsafe { libc::fcntl(input.binary.fd, libc::F_GETFL) } & libc::O_ACCMODE
            != libc::O_RDONLY
    {
        return Err(rejected());
    }
    let binary = unsafe { std::fs::File::from_raw_fd(input.binary.fd) };
    let path = std::env::current_exe().map_err(|_| rejected())?;
    let image = file::image(&path).map_err(|_| rejected())?;
    let snapshot = RunSnapshot {
        input,
        binary,
        image,
        nonce: lease.nonce.clone(),
        bound: None,
    };
    snapshot.verify(instance)?;
    if unsafe {
        libc::fcntl(
            snapshot.binary.as_raw_fd(),
            libc::F_SETFD,
            flags | libc::FD_CLOEXEC,
        )
    } < 0
    {
        return Err(rejected());
    }
    let mut run = RUN.lock().map_err(|_| rejected())?;
    if run.is_some() {
        return Err(rejected());
    }
    *run = Some(snapshot);
    HAD_RUN.store(true, std::sync::atomic::Ordering::Release);
    Ok(true)
}

#[cfg(unix)]
impl RunSnapshot {
    fn verify(&self, instance: &super::TestInstance) -> Result<(), String> {
        instance.verify_run_declaration()?;
        let metadata = self.binary.metadata().map_err(|_| rejected())?;
        if !metadata.is_file()
            || metadata.mode() & 0o022 != 0
            || (metadata.dev(), metadata.ino())
                != (self.input.binary.device, self.input.binary.inode)
            || self.image.dev_ino() != (metadata.dev(), metadata.ino())
            || format!(
                "{:x}",
                sha2::digest::Output::<sha2::Sha256>::from(self.image.digest())
            ) != self.input.binary.sha256
        {
            return Err(rejected());
        }
        // The retained FD pins this inode; the initial image digest and current mutation fence
        // bind its bytes without hashing the executable again on every health request.
        peer::verify_current_image(&self.image).map_err(|_| rejected())?;
        let names = [
            "claude_bot_token",
            "codex_bot_token",
            "announce_bot_token",
            "notify_bot_token",
        ];
        let directory = instance.root.join("credential");
        super::checked_path(&directory)?;
        let metadata = std::fs::symlink_metadata(&directory).map_err(|_| rejected())?;
        let entries = std::fs::read_dir(&directory)
            .map_err(|_| rejected())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<std::collections::BTreeSet<_>>>()
            .map_err(|_| rejected())?;
        let expected: std::collections::BTreeSet<_> = names
            .iter()
            .map(|name| std::ffi::OsString::from(*name))
            .collect();
        if !metadata.is_dir()
            || metadata.mode() & 0o7777 != 0o700
            || metadata.uid() != unsafe { libc::geteuid() }
            || entries != expected
        {
            return Err(rejected());
        }
        if self.input.identities.len() != names.len() {
            return Err(rejected());
        }
        let mut ids = std::collections::BTreeSet::new();
        let mut seen = std::collections::BTreeSet::new();
        let mut files = std::collections::BTreeSet::new();
        for identity in &self.input.identities {
            if !names.contains(&identity.name.as_str())
                || !seen.insert(&identity.name)
                || !identity
                    .application_id
                    .parse::<u64>()
                    .is_ok_and(|id| id != 0 && id.to_string() == identity.application_id)
                || !ids.insert(&identity.application_id)
                || !files.insert((identity.device, identity.inode))
            {
                return Err(rejected());
            }
            let path = instance.root.join("credential").join(&identity.name);
            super::checked_path(&path)?;
            let metadata = std::fs::symlink_metadata(path).map_err(|_| rejected())?;
            if !metadata.is_file()
                || metadata.mode() & 0o7777 != 0o600
                || metadata.uid() != unsafe { libc::geteuid() }
                || metadata.nlink() != 1
                || (metadata.dev(), metadata.ino(), metadata.len())
                    != (identity.device, identity.inode, identity.size)
                || metadata.len() == 0
                || metadata.len() > 4096
                || nanoseconds(metadata.mtime(), metadata.mtime_nsec()) != Some(identity.mtime_ns)
                || nanoseconds(metadata.ctime(), metadata.ctime_nsec()) != Some(identity.ctime_ns)
            {
                return Err(rejected());
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
fn nanoseconds(seconds: i64, nanos: i64) -> Option<u64> {
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(nanos).ok()?)
}

pub(crate) fn has_admitted_run() -> bool {
    #[cfg(unix)]
    {
        RUN.lock().is_ok_and(|snapshot| snapshot.is_some())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub(crate) fn bound_listener(address: std::net::SocketAddr) -> Result<(), String> {
    if !requested() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        let mut run = RUN.lock().map_err(|_| rejected())?;
        if let Some(snapshot) = run.as_mut() {
            snapshot.verify(super::current()?.ok_or_else(rejected)?)?;
            if !address.ip().is_loopback()
                || address.port() == 0
                || snapshot.bound.is_some_and(|bound| bound != address)
            {
                return Err(rejected());
            }
            snapshot.bound = Some(address);
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = address;
        Err(rejected())
    }
}

pub(crate) fn health_witness() -> Result<Option<serde_json::Value>, String> {
    let Some(instance) = super::current()? else {
        return Ok(None);
    };
    #[cfg(unix)]
    {
        let run = RUN.lock().map_err(|_| rejected())?;
        let Some(snapshot) = run.as_ref() else {
            return if HAD_RUN.load(std::sync::atomic::Ordering::Acquire) {
                Err(rejected())
            } else {
                Ok(None)
            };
        };
        snapshot.verify(instance)?;
        let bound = snapshot.bound.ok_or_else(rejected)?;
        Ok(Some(
            serde_json::json!({"schema":1,"instance_id":instance.instance_id,"head":snapshot.input.head,
            "pid":std::process::id(),"binary_sha256":snapshot.input.binary.sha256,"nonce":snapshot.nonce,
            "port":bound.port(),"identities":snapshot.input.identities,"live_ready":false}),
        ))
    }
    #[cfg(not(unix))]
    {
        let _ = instance;
        Err(rejected())
    }
}

pub(crate) fn with_control_routes(app: axum::Router) -> Result<axum::Router, String> {
    if !has_admitted_run() {
        return Ok(app);
    }
    #[cfg(unix)]
    control_epoch()?;
    Ok(app.merge(
        axum::Router::new()
            .route(
                "/_agentdesk/test-control",
                axum::routing::post(control_request),
            )
            .layer(axum::extract::DefaultBodyLimit::max(8192)),
    ))
}

async fn control_request(
    axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match dispatch_control(peer, &body) {
        Ok(value) => axum::Json(value).into_response(),
        Err(_) => axum::http::StatusCode::FORBIDDEN.into_response(),
    }
}

#[cfg(unix)]
#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct ControlRequest {
    schema: u8,
    action: String,
    root: String,
    instance_id: String,
    nonce: String,
    challenge: String,
    proof: String,
}

#[cfg(unix)]
fn dispatch_control(peer: std::net::SocketAddr, body: &[u8]) -> Result<serde_json::Value, String> {
    let request: ControlRequest = serde_json::from_slice(body).map_err(|_| rejected())?;
    let instance = super::current()?.ok_or_else(rejected)?;
    let run = RUN.lock().map_err(|_| rejected())?;
    let snapshot = run.as_ref().ok_or_else(rejected)?;
    if !peer.ip().is_loopback()
        || body.len() > 8192
        || request.schema != 1
        || !matches!(request.action.as_str(), "probe" | "stop")
        || request.root != instance.root.to_string_lossy()
        || request.instance_id != instance.instance_id
        || request.nonce != snapshot.nonce
        || !uuid::Uuid::parse_str(&request.challenge)
            .is_ok_and(|id| !id.is_nil() && id.hyphenated().to_string() == request.challenge)
    {
        return Err(rejected());
    }
    snapshot.verify(instance)?;
    let key = snapshot.control_key(instance)?;
    let mut encoded = serde_json::to_value(&request).map_err(|_| rejected())?;
    encoded
        .as_object_mut()
        .ok_or_else(rejected)?
        .remove("proof");
    let expected = signed_control(&key, b"agentdesk-test-control-v1/request\0", &encoded)?;
    if request.proof.len() != 64
        || !request
            .proof
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        || !crate::utils::auth::constant_time_token_eq(&expected, &request.proof)
    {
        return Err(rejected());
    }
    let bound = snapshot.bound.ok_or_else(rejected)?;
    let witness = serde_json::json!({"schema":1,"instance_id":instance.instance_id,"head":snapshot.input.head,
        "pid":std::process::id(),"binary_sha256":snapshot.input.binary.sha256,"nonce":snapshot.nonce,
        "port":bound.port(),"identities":snapshot.input.identities,"live_ready":false});
    let mut response = serde_json::json!({"schema":1,"action":request.action,"challenge":request.challenge,
        "witness":witness,"control_ready":true,"epoch":control_epoch()?,"accepted":request.action=="stop"});
    response["proof"] =
        signed_control(&key, b"agentdesk-test-control-v1/response\0", &response)?.into();
    if request.action == "stop" {
        write_control_ack(instance, &response)?;
        // The signed receipt precedes the signal; only the external owner can prove exit.
        snapshot.verify(instance)?;
        if unsafe { libc::raise(libc::SIGTERM) } != 0 {
            return Err(rejected());
        }
    }
    Ok(response)
}

#[cfg(not(unix))]
fn dispatch_control(
    _peer: std::net::SocketAddr,
    _body: &[u8],
) -> Result<serde_json::Value, String> {
    Err(rejected())
}

#[cfg(unix)]
impl RunSnapshot {
    fn control_key(&self, instance: &super::TestInstance) -> Result<[u8; 32], String> {
        use std::{io::Read, os::unix::fs::OpenOptionsExt};
        let mut credentials = Vec::new();
        for name in [
            "announce_bot_token",
            "claude_bot_token",
            "codex_bot_token",
            "notify_bot_token",
        ] {
            let identity = self
                .input
                .identities
                .iter()
                .find(|identity| identity.name == name)
                .ok_or_else(rejected)?;
            let path = instance.root.join("credential").join(name);
            super::checked_path(&path)?;
            let file = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
                .open(&path)
                .map_err(|_| rejected())?;
            let matches = |metadata: &std::fs::Metadata| {
                metadata.is_file()
                    && metadata.mode() & 0o7777 == 0o600
                    && metadata.uid() == unsafe { libc::geteuid() }
                    && metadata.nlink() == 1
                    && (metadata.dev(), metadata.ino(), metadata.len())
                        == (identity.device, identity.inode, identity.size)
                    && nanoseconds(metadata.mtime(), metadata.mtime_nsec())
                        == Some(identity.mtime_ns)
                    && nanoseconds(metadata.ctime(), metadata.ctime_nsec())
                        == Some(identity.ctime_ns)
            };
            if !matches(&file.metadata().map_err(|_| rejected())?) {
                return Err(rejected());
            }
            let mut bytes = Vec::new();
            (&file)
                .take(4097)
                .read_to_end(&mut bytes)
                .map_err(|_| rejected())?;
            if bytes.is_empty()
                || bytes.len() > 4096
                || !matches(&file.metadata().map_err(|_| rejected())?)
                || !matches(&std::fs::symlink_metadata(&path).map_err(|_| rejected())?)
            {
                return Err(rejected());
            }
            credentials.push((name, bytes));
        }
        Ok(derive_control_key(
            credentials
                .iter()
                .map(|(name, bytes)| (*name, bytes.as_slice())),
        ))
    }
}

#[cfg(unix)]
fn derive_control_key<'a>(credentials: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> [u8; 32] {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(b"agentdesk-test-control-key-v1\0");
    for (name, bytes) in credentials {
        digest.update((name.len() as u32).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u32).to_be_bytes());
        digest.update(bytes);
    }
    digest.finalize().into()
}

#[cfg(unix)]
fn control_hmac(key: &[u8; 32], bytes: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    let mut inner = [0x36; 64];
    let mut outer = [0x5c; 64];
    for (index, byte) in key.iter().enumerate() {
        inner[index] ^= byte;
        outer[index] ^= byte;
    }
    let mut digest = sha2::Sha256::new();
    digest.update(inner);
    digest.update(bytes);
    let inside = digest.finalize();
    let mut digest = sha2::Sha256::new();
    digest.update(outer);
    digest.update(inside);
    digest.finalize().into()
}

#[cfg(unix)]
fn hex_digest(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(unix)]
fn signed_control(
    key: &[u8; 32],
    domain: &[u8],
    value: &serde_json::Value,
) -> Result<String, String> {
    let mut bytes = domain.to_vec();
    bytes.extend(serde_json::to_vec(value).map_err(|_| rejected())?);
    Ok(hex_digest(control_hmac(key, &bytes)))
}

#[cfg(target_os = "macos")]
fn control_epoch() -> Result<u32, String> {
    let (peer, _other) = std::os::unix::net::UnixStream::pair().map_err(|_| rejected())?;
    let mut raw = [0u32; 8];
    let mut length = std::mem::size_of_val(&raw) as libc::socklen_t;
    // The audit token identifies this exec generation, rather than a reusable PID alone.
    let result = unsafe {
        libc::getsockopt(
            peer.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERTOKEN,
            raw.as_mut_ptr().cast(),
            &mut length,
        )
    };
    if result != 0
        || length as usize != std::mem::size_of_val(&raw)
        || raw[5] != std::process::id()
        || raw[7] == 0
    {
        return Err(rejected());
    }
    Ok(raw[7])
}

#[cfg(all(unix, not(target_os = "macos")))]
fn control_epoch() -> Result<u32, String> {
    Err(rejected())
}

#[cfg(unix)]
fn write_control_ack(
    instance: &super::TestInstance,
    response: &serde_json::Value,
) -> Result<(), String> {
    use std::{io::Write, os::fd::FromRawFd, os::unix::fs::OpenOptionsExt};
    let path = instance.root.join("runtime");
    super::checked_path(&path)?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| rejected())?;
    let metadata = directory.metadata().map_err(|_| rejected())?;
    if metadata.mode() & 0o7777 != 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(rejected());
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"instance-control-ack.json".as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(rejected());
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let original = file.metadata().map_err(|_| rejected())?;
    if original.mode() & 0o7777 != 0o600
        || original.uid() != unsafe { libc::geteuid() }
        || original.nlink() != 1
    {
        return Err(rejected());
    }
    file.write_all(&serde_json::to_vec(response).map_err(|_| rejected())?)
        .map_err(|_| rejected())?;
    file.sync_all().map_err(|_| rejected())?;
    directory.sync_all().map_err(|_| rejected())?;
    let current =
        std::fs::symlink_metadata(instance.root.join("runtime/instance-control-ack.json"))
            .map_err(|_| rejected())?;
    if !current.is_file()
        || (current.dev(), current.ino()) != (original.dev(), original.ino())
        || current.mode() & 0o7777 != 0o600
        || current.uid() != unsafe { libc::geteuid() }
        || current.nlink() != 1
    {
        return Err(rejected());
    }
    Ok(())
}

#[cfg(unix)]
fn check_file(metadata: &std::fs::Metadata, witness: &Witness) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if !metadata.is_file()
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.dev() != witness.device
        || metadata.ino() != witness.inode
    {
        return Err(rejected());
    }
    Ok(())
}

#[cfg(unix)]
fn open_control(root: &std::path::Path) -> Result<std::fs::File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    let parent = root.parent().ok_or_else(rejected)?;
    let mut directory = std::fs::File::open("/").map_err(|_| rejected())?;
    for part in parent.components().skip(1) {
        let std::path::Component::Normal(part) = part else {
            return Err(rejected());
        };
        let part = std::ffi::CString::new(part.as_bytes()).map_err(|_| rejected())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                part.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(rejected());
        }
        directory = unsafe { std::fs::File::from_raw_fd(fd) };
    }
    let metadata = directory.metadata().map_err(|_| rejected())?;
    if metadata.mode() & 0o7777 != 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(rejected());
    }
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".gateway.lease".as_ptr(),
            libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(rejected());
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

#[cfg(all(test, unix))]
#[path = "lifecycle_tests.rs"]
mod tests;

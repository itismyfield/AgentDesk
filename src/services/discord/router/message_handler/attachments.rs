use super::*;
use crate::services::cluster::attachment_transfer::{
    self, store,
    uploads::{PendingUploads, Upload},
};

mod busy_inject;
pub(in crate::services::discord::router) use busy_inject::prepare_busy_inject_uploads;
#[cfg(all(test, unix))]
pub(in crate::services::discord::router) use busy_inject::retain_busy_uploads;

const FILE_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const BUNDLE_DOWNLOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);
const MAX_CONCURRENT_BUNDLE_DOWNLOADS: usize = 4;

pub(super) const DISCORD_ATTACHMENT_HOSTS: &[&str] =
    &["cdn.discordapp.com", "media.discordapp.net"];
pub(super) fn is_allowed_discord_attachment_url(raw_url: &str) -> bool {
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|port| port != 443)
    {
        return false;
    }
    url.host_str()
        .is_some_and(|host| DISCORD_ATTACHMENT_HOSTS.contains(&host))
}

#[cfg(test)]
thread_local! {
    /// Bytes a test serves for an attachment URL in place of the CDN.
    pub(in crate::services::discord::router) static TEST_DOWNLOADS:
        std::cell::RefCell<std::collections::HashMap<String, Vec<u8>>> = Default::default();
}

pub(super) async fn download_discord_attachment(raw_url: &str) -> Result<Vec<u8>, String> {
    #[cfg(test)]
    if let Some(bytes) = TEST_DOWNLOADS.with(|served| served.borrow().get(raw_url).cloned()) {
        return Ok(bytes);
    }
    if !is_allowed_discord_attachment_url(raw_url) {
        return Err("attachment URL host is not allowed".to_string());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(FILE_DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|_| "attachment client initialization failed")?;
    let mut response = client
        .get(raw_url)
        .send()
        .await
        .map_err(|_| "attachment download failed")?
        .error_for_status()
        .map_err(|_| "attachment server returned an error")?;
    if !response.status().is_success()
        || response
            .content_length()
            .is_some_and(|len| len > store::MAX_FILE_BYTES as u64)
    {
        return Err("attachment redirect or file size limit exceeded".into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "attachment body read failed")?
    {
        if bytes.len() + chunk.len() > store::MAX_FILE_BYTES {
            return Err("attachment exceeds 8 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

pub(in crate::services::discord::router) async fn prepare_portable_attachments(
    pool: &sqlx::PgPool,
    identity: attachment_transfer::AttachmentMessageIdentity,
    attachments: &[AttachmentDescriptor],
) -> Result<PendingUploads, String> {
    static DOWNLOADS: std::sync::LazyLock<tokio::sync::Semaphore> =
        std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_BUNDLE_DOWNLOADS));
    let _permit = DOWNLOADS
        .acquire()
        .await
        .map_err(|_| "attachment downloader unavailable")?;
    let entries = tokio::time::timeout(BUNDLE_DOWNLOAD_TIMEOUT, download_all(attachments))
        .await
        .map_err(|_| "attachment bundle download timed out")??;
    let bundle = attachment_transfer::AttachmentBundleV1 {
        version: attachment_transfer::ATTACHMENT_BUNDLE_V1,
        identity: identity.clone(),
        source_attachment_count: attachments.len() as u32,
        entries,
    };
    let validated = attachment_transfer::validate_attachment_bundle_v1(bundle, &identity)
        .map_err(|e| format!("attachment validation failed: {e:?}"))?;
    Ok(vec![Upload::Bundle(store::put(pool, &validated).await?)])
}

async fn download_all(
    attachments: &[AttachmentDescriptor],
) -> Result<Vec<attachment_transfer::AttachmentEntryV1>, String> {
    if attachments.is_empty() || attachments.len() > store::MAX_FILES {
        return Err("attachment count must be between 1 and 10".into());
    }
    let mut entries = Vec::new();
    let mut total = 0;
    for attachment in attachments {
        let bytes = download_discord_attachment(&attachment.url).await?;
        total += bytes.len();
        if total > store::MAX_BUNDLE_BYTES {
            return Err("attachments exceed 24 MiB".into());
        }
        entries.push(attachment_transfer::AttachmentEntryV1 {
            filename: attachment.filename.clone(),
            sha256: attachment_transfer::attachment_sha256_hex(&bytes),
            bytes,
        });
    }
    Ok(entries)
}

/// Side-effect-free attachment metadata captured at Discord intake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::services::discord::router) struct AttachmentDescriptor {
    pub(in crate::services::discord::router) filename: String,
    pub(in crate::services::discord::router) url: String,
}

impl From<&serenity::Attachment> for AttachmentDescriptor {
    fn from(attachment: &serenity::Attachment) -> Self {
        Self {
            filename: attachment.filename.clone(),
            url: attachment.url.clone(),
        }
    }
}

pub(in crate::services::discord::router) fn describe_attachments(
    msg: &serenity::Message,
) -> Vec<AttachmentDescriptor> {
    msg.attachments.iter().map(Into::into).collect()
}

/// Opaque capability for the local attachment materialization boundary.
///
/// The intake coordinator issues this capability only after central admission
/// has granted local execution.
#[derive(Debug)]
pub(in crate::services::discord::router) struct LocalAttachmentPreparationPermit(());

impl LocalAttachmentPreparationPermit {
    pub(in crate::services::discord::router) fn after_local_admission() -> Self {
        Self(())
    }
}

fn persist_attachment(
    save_dir: &Path,
    descriptor: &AttachmentDescriptor,
    bytes: &[u8],
    timestamp_millis: i64,
) -> std::io::Result<(std::path::PathBuf, String)> {
    let safe_name = Path::new(&descriptor.filename)
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
    let stamped_name = format!("{}_{}", timestamp_millis, safe_name.to_string_lossy());
    let dest = save_dir.join(stamped_name);
    fs::write(&dest, bytes)?;
    let upload_record = format!(
        "[File uploaded] {} → {} ({} bytes)",
        descriptor.filename,
        dest.display(),
        bytes.len()
    );
    Ok((dest, upload_record))
}

/// Download and save attachments at the admitted-local side-effect boundary.
pub(in crate::services::discord::router) async fn prepare_admitted_local_attachment(
    http: &Arc<serenity::http::Http>,
    channel_id: serenity::ChannelId,
    attachments: &[AttachmentDescriptor],
    shared: &Arc<SharedData>,
    _permit: &LocalAttachmentPreparationPermit,
) -> Result<PendingUploads, Error> {
    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(http, "Cannot resolve upload directory.")
            .await;
        return Err("cannot resolve upload directory".into());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(http, format!("Failed to prepare upload directory: {}", e))
            .await;
        return Err(e.into());
    }

    // Download every member before writing or returning any upload record.
    let downloaded =
        tokio::time::timeout(BUNDLE_DOWNLOAD_TIMEOUT, download_all(attachments)).await??;
    let mut upload_records = Vec::new();
    for (attachment, entry) in attachments.iter().zip(downloaded) {
        let buf = entry.bytes;
        let file_name = &attachment.filename;

        let file_size = buf.len();
        let ts = chrono::Utc::now().timestamp_millis();
        let (dest, upload_record) = match persist_attachment(&save_dir, attachment, &buf, ts) {
            Ok(saved) => saved,
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(http, format!("Failed to save file: {}", e))
                    .await;
                return Err(e.into());
            }
        };

        let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id.say(http, &msg_text).await;
        debug_assert!(upload_record.starts_with(&format!("[File uploaded] {file_name} → ")));
        upload_records.push(upload_record.into());
    }

    Ok(upload_records)
}

#[cfg(test)]
mod attachment_tests {
    use super::{AttachmentDescriptor, is_allowed_discord_attachment_url, persist_attachment};

    #[test]
    fn discord_attachment_url_guard_allows_discord_cdn_hosts() {
        assert!(is_allowed_discord_attachment_url(
            "https://cdn.discordapp.com/attachments/1/2/file.txt"
        ));
        assert!(is_allowed_discord_attachment_url(
            "https://media.discordapp.net/attachments/1/2/image.png"
        ));
    }

    #[test]
    fn discord_attachment_url_guard_rejects_ssrf_shapes() {
        assert!(!is_allowed_discord_attachment_url(
            "http://cdn.discordapp.com/attachments/1/2/file.txt"
        ));
        assert!(!is_allowed_discord_attachment_url(
            "https://cdn.discordapp.com.evil.test/attachments/1/2/file.txt"
        ));
        assert!(!is_allowed_discord_attachment_url(
            "https://127.0.0.1/attachments/1/2/file.txt"
        ));
        assert!(!is_allowed_discord_attachment_url("not a url"));
    }

    #[test]
    fn attachment_persistence_seam_preserves_path_record_and_bytes() {
        let root = tempfile::tempdir().expect("temporary upload root");
        let descriptor = AttachmentDescriptor {
            filename: "../report.txt".to_string(),
            url: "https://cdn.discordapp.com/attachments/1/2/report.txt".to_string(),
        };

        let (path, record) = persist_attachment(root.path(), &descriptor, b"payload", 1234)
            .expect("persist attachment");

        assert_eq!(path, root.path().join("1234_report.txt"));
        assert_eq!(std::fs::read(&path).expect("read saved file"), b"payload");
        assert_eq!(
            record,
            format!(
                "[File uploaded] ../report.txt → {} (7 bytes)",
                path.display()
            )
        );
    }

    const INTAKE_GATE: &str = include_str!("../intake_gate.rs");
    const HANDLE_EVENT: &str = "pub(in crate::services::discord) async fn handle_event(";
    const PREPARE: &str = "prepare_admitted_live_attachments(";
    const ATTACHMENT_ORDER: [&str; 4] = [
        "let attachments = super::message_handler::describe_attachments(new_message);",
        "super::admit_text_intake(&deps, &submission).await",
        "if deferred_attachment_unbound_check",
        "super::prepare_admitted_live_attachments(",
    ];

    /// Classification 2: descriptor → admission → deferred restore → materialization is
    /// matched as written in `handle_event`, comments excluded; a lexical tripwire.
    fn attachment_order_gaps(source: &str) -> Vec<String> {
        use crate::services::discord::router::source_guard_support::Lexed;
        let lexed = Lexed::new(source);
        let event = lexed.block(HANDLE_EVENT);
        let found = ATTACHMENT_ORDER.map(|step| event.as_ref().and_then(|body| body.find(step)));
        let mut gaps = Vec::new();
        if !found
            .windows(2)
            .all(|pair| matches!(pair, [Some(a), Some(b)] if a < b))
        {
            gaps.push(format!("attachment ownership order broken: {found:?}"));
        }
        if lexed.count(PREPARE) != 1 {
            gaps.push("live attachment materialization has one admitted gate site".into());
        }
        gaps
    }

    #[test]
    fn attachment_materialization_requires_post_admission_opaque_permit() {
        assert_eq!(attachment_order_gaps(INTAKE_GATE), Vec::<String>::new());
    }

    #[test]
    fn attachment_order_guard_rejects_commented_and_reordered_steps() {
        use crate::services::discord::router::source_guard_support::{forge, swap};
        use crate::test_support::CommentForgery;
        for needle in [HANDLE_EVENT].into_iter().chain(ATTACHMENT_ORDER) {
            for forgery in CommentForgery::ALL {
                let forged = forge(INTAKE_GATE, needle, forgery);
                assert!(forged.contains(needle));
                assert!(
                    !attachment_order_gaps(&forged).is_empty(),
                    "{forgery:?} commented `{needle}` still passes"
                );
            }
        }
        let late_admission = swap(INTAKE_GATE, ATTACHMENT_ORDER[1], ATTACHMENT_ORDER[2]);
        assert!(!attachment_order_gaps(&late_admission).is_empty());
        let second = INTAKE_GATE.replacen(
            ATTACHMENT_ORDER[3],
            &format!("// {PREPARE}\n{}", ATTACHMENT_ORDER[3]),
            1,
        );
        assert_eq!(attachment_order_gaps(&second), Vec::<String>::new());
        let coded = format!("{INTAKE_GATE}\nfn forged() {{ {PREPARE}); }}\n");
        assert_eq!(attachment_order_gaps(&coded).len(), 1);
    }
}

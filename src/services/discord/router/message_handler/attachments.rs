use super::*;

pub(super) const DISCORD_ATTACHMENT_HOSTS: &[&str] =
    &["cdn.discordapp.com", "media.discordapp.net"];
pub(super) fn is_allowed_discord_attachment_url(raw_url: &str) -> bool {
    let Ok(url) = Url::parse(raw_url) else {
        return false;
    };
    if url.scheme() != "https" {
        return false;
    }
    url.host_str()
        .is_some_and(|host| DISCORD_ATTACHMENT_HOSTS.contains(&host))
}

pub(super) async fn download_discord_attachment(raw_url: &str) -> Result<Vec<u8>, String> {
    if !is_allowed_discord_attachment_url(raw_url) {
        return Err("attachment URL host is not allowed".to_string());
    }
    let response = reqwest::get(raw_url)
        .await
        .map_err(|error| format!("Download failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("Download failed: {error}"))?;
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| format!("Download failed: {error}"))
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
) -> Result<Vec<String>, Error> {
    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(http, "Cannot resolve upload directory.")
            .await;
        return Ok(Vec::new());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(http, format!("Failed to prepare upload directory: {}", e))
            .await;
        return Ok(Vec::new());
    }

    let mut upload_records = Vec::new();
    for attachment in attachments {
        let file_name = &attachment.filename;

        // Download only from Discord-owned attachment hosts.
        let buf = match download_discord_attachment(&attachment.url).await {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    attachment_url = %attachment.url,
                    "skipping Discord attachment download: {e}"
                );
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(http, format!("Download failed: {e}")).await;
                continue;
            }
        };

        let file_size = buf.len();
        let ts = chrono::Utc::now().timestamp_millis();
        let (dest, upload_record) = match persist_attachment(&save_dir, attachment, &buf, ts) {
            Ok(saved) => saved,
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        };

        let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id.say(http, &msg_text).await;
        debug_assert!(upload_record.starts_with(&format!("[File uploaded] {file_name} → ")));
        upload_records.push(upload_record);
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

    #[test]
    fn attachment_materialization_requires_post_admission_opaque_permit() {
        let source = include_str!("../intake_gate.rs");
        let descriptor = source
            .find("let attachments = super::message_handler::describe_attachments(new_message);")
            .expect("descriptor capture");
        let admission = source
            .find("super::admit_text_intake(&deps, &submission).await")
            .expect("attachment central admission call");
        let deferred_restore = source
            .find("if deferred_attachment_unbound_check")
            .expect("unbound attachment restore guard");
        let prepare = source
            .find("super::prepare_admitted_live_attachments(")
            .expect("post-admission materialization boundary");

        assert!(
            descriptor < admission && admission < deferred_restore && deferred_restore < prepare
        );
        assert_eq!(
            source.matches("prepare_admitted_live_attachments(").count(),
            1,
            "live attachment materialization has one admitted gate site"
        );
    }
}

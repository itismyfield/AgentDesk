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
    filename: String,
    url: String,
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
/// The current intake site issues this capability at its existing point so this
/// preparatory slice preserves ordering. The admission coordinator can move
/// issuance behind local admission without changing the download/save body.
#[derive(Debug)]
pub(in crate::services::discord::router) struct LocalAttachmentPreparationPermit(());

impl LocalAttachmentPreparationPermit {
    pub(in crate::services::discord::router) fn preserving_existing_intake_order() -> Self {
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
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
    attachments: &[AttachmentDescriptor],
    shared: &Arc<SharedData>,
    _permit: &LocalAttachmentPreparationPermit,
) -> Result<Vec<String>, Error> {
    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Cannot resolve upload directory.")
            .await;
        return Ok(Vec::new());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(
                &ctx.http,
                format!("Failed to prepare upload directory: {}", e),
            )
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
                let _ = channel_id
                    .say(&ctx.http, format!("Download failed: {e}"))
                    .await;
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
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        };

        let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id.say(&ctx.http, &msg_text).await;
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
    fn attachment_materialization_requires_opaque_permit_and_keeps_existing_order() {
        let source = include_str!("../intake_gate.rs");
        let restore = source
            .find("auto_restore_session_with_dm_hint(&data.shared, channel_id")
            .expect("existing session restore");
        let descriptor = source
            .find("let attachments = super::message_handler::describe_attachments(new_message);")
            .expect("descriptor capture");
        let permit = source
            .find("let permit = super::message_handler::LocalAttachmentPreparationPermit::preserving_existing_intake_order();")
            .expect("opaque permit issuance");
        let prepare = source
            .find("super::message_handler::prepare_admitted_local_attachment(")
            .expect("materialization boundary");
        let history = source
            .find("record_upload_history(&data.shared, channel_id, &upload_records).await;")
            .expect("existing history update");
        let admission = source
            .find("super::dispatch_text_intake(&deps, submission).await?;")
            .expect("central admission call");

        assert!(restore < descriptor);
        assert!(descriptor < permit && permit < prepare);
        assert!(prepare < history && history < admission);
        assert_eq!(
            source
                .matches("LocalAttachmentPreparationPermit::preserving_existing_intake_order()")
                .count(),
            1,
            "only the compatibility intake site may issue the preparatory permit"
        );
    }
}

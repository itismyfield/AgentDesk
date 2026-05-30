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
/// Handle file uploads from Discord messages
pub(in crate::services::discord::router) async fn handle_file_upload(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
) -> Result<Vec<String>, Error> {
    let channel_id = msg.channel_id;

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
    for attachment in &msg.attachments {
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

        // Save to session path (sanitize filename)
        let safe_name = Path::new(file_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
        let ts = chrono::Utc::now().timestamp_millis();
        let stamped_name = format!("{}_{}", ts, safe_name.to_string_lossy());
        let dest = save_dir.join(stamped_name);
        let file_size = buf.len();

        match fs::write(&dest, &buf) {
            Ok(_) => {
                let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(&ctx.http, &msg_text).await;
            }
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        }

        let upload_record = format!(
            "[File uploaded] {} → {} ({} bytes)",
            file_name,
            dest.display(),
            file_size
        );
        upload_records.push(upload_record);
    }

    Ok(upload_records)
}
#[cfg(test)]
mod attachment_url_tests {
    use super::is_allowed_discord_attachment_url;

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
}

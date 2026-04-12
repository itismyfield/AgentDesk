use std::path::{Path, PathBuf};

use poise::serenity_prelude as serenity;
use serenity::CreateAttachment;

use super::super::{Context, Error, check_auth};
use crate::receipt;
use crate::services::platform;

/// /receipt — Show token usage receipts (one per provider) as PNG images
#[poise::command(slash_command, rename = "receipt")]
pub(in crate::services::discord) async fn cmd_receipt(
    ctx: Context<'_>,
    #[description = "Period: month (30d) or ratelimit (current 7d window)"] period: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] \u{25c0} [{user_name}] /receipt");

    ctx.defer().await?;

    let period_str = period.as_deref().unwrap_or("month");

    // Determine time range
    let now = chrono::Utc::now();
    let (start, label) = match period_str {
        "ratelimit" => {
            let window_start = ctx.data().shared.db.as_ref().and_then(|db| {
                db.lock()
                    .ok()
                    .and_then(|conn| receipt::ratelimit_window_start(&conn))
            });
            (
                window_start.unwrap_or_else(|| now - chrono::Duration::days(7)),
                "Rate Limit Window",
            )
        }
        _ => (now - chrono::Duration::days(30), "Last 30 Days"),
    };

    // Collect data in blocking task (reads many JSONL files)
    let label_owned = label.to_string();
    let data = tokio::task::spawn_blocking(move || receipt::collect(start, now, &label_owned))
        .await
        .map_err(|e| format!("receipt collection failed: {e}"))?;

    if data.models.is_empty() {
        ctx.say("No token usage data found for the selected period.")
            .await?;
        return Ok(());
    }

    // Resolve playwright binary via login-shell PATH (launchd safety)
    let playwright_bin = platform::async_resolve_binary_with_login_shell("playwright")
        .await
        .unwrap_or_else(|| "playwright".into());

    // Build the list of receipts to render:
    // 1. Unified receipt (always)
    // 2. Per-provider receipts (only when multi-provider)
    let per_provider = receipt::split_by_provider(&data);
    let multi_provider = per_provider.len() > 1;
    let mut to_render: Vec<&receipt::ReceiptData> = vec![&data];
    if multi_provider {
        to_render.extend(per_provider.iter());
    }

    let tmp_dir = std::env::temp_dir();
    let unique_id = uuid::Uuid::new_v4();
    let mut temp_files: Vec<PathBuf> = Vec::new();
    let mut attached = 0usize;
    let mut reply = poise::CreateReply::default().content(format!(
        "\u{1f9fe} **Token Receipt** \u{2014} {} ({} ~ {})",
        data.period_label, data.period_start, data.period_end
    ));

    for (i, r) in to_render.iter().enumerate() {
        let label = if i == 0 {
            "combined"
        } else {
            r.providers
                .first()
                .map(|p| p.provider.as_str())
                .unwrap_or("unknown")
        };
        let html = receipt::render_html(r);

        let html_path = tmp_dir.join(format!("adk_receipt_{unique_id}_{i}.html"));
        let png_path = tmp_dir.join(format!("adk_receipt_{unique_id}_{i}.png"));
        std::fs::write(&html_path, &html).map_err(|e| format!("failed to write HTML: {e}"))?;

        let mut cmd = tokio::process::Command::new(&playwright_bin);
        cmd.args([
            "screenshot",
            "--browser",
            "chromium",
            "--full-page",
            "--viewport-size=400,1",
            &format!("file://{}", html_path.display()),
            &png_path.display().to_string(),
        ]);
        if let Some(merged) = platform::merged_runtime_path() {
            cmd.env("PATH", merged);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| format!("playwright failed: {e}"))?;

        temp_files.push(html_path);
        temp_files.push(png_path.clone());

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!("  [{ts}] \u{2716} Playwright error for {label}: {stderr}");
            continue;
        }

        if Path::new(&png_path).exists() {
            let attachment = CreateAttachment::path(&png_path)
                .await
                .map_err(|e| format!("failed to read PNG: {e}"))?;
            reply = reply.attachment(attachment);
            attached += 1;
        }
    }

    if attached == 0 {
        ctx.say(
            "Failed to render receipt images. Check that Playwright and Chromium are installed.",
        )
        .await?;
    } else {
        ctx.send(reply).await?;
    }

    // Cleanup temp files
    for f in &temp_files {
        let _ = std::fs::remove_file(f);
    }

    tracing::info!(
        "  [{ts}] \u{25b6} [{user_name}] Receipt sent ({} providers, total: {})",
        to_render.len(),
        receipt_fmt_cost(data.total)
    );
    Ok(())
}

fn receipt_fmt_cost(c: f64) -> String {
    if c >= 1.0 {
        format!("${:.2}", c)
    } else {
        format!("${:.4}", c)
    }
}

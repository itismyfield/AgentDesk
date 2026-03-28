use std::path::Path;

use poise::serenity_prelude as serenity;
use serenity::CreateAttachment;

use super::super::{Context, Error, check_auth};
use crate::receipt;

/// /receipt — Show token usage receipt as a PNG image
#[poise::command(slash_command, rename = "receipt")]
pub(in crate::services::discord) async fn cmd_receipt(
    ctx: Context<'_>,
    #[description = "Period: month (30d) or ratelimit (current 7d window)"]
    period: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] \u{25c0} [{user_name}] /receipt");

    ctx.defer().await?;

    let period_str = period.as_deref().unwrap_or("month");

    // Determine time range
    let now = chrono::Utc::now();
    let (start, label) = match period_str {
        "ratelimit" => {
            let window_start = ctx.data().shared.db.as_ref().and_then(|db| {
                db.lock().ok().and_then(|conn| receipt::ratelimit_window_start(&conn))
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
    let data = tokio::task::spawn_blocking(move || {
        receipt::collect(start, now, &label_owned)
    })
    .await
    .map_err(|e| format!("receipt collection failed: {e}"))?;

    if data.models.is_empty() {
        ctx.say("No token usage data found for the selected period.").await?;
        return Ok(());
    }

    // Render HTML
    let html = receipt::render_html(&data);

    // Write HTML to temp file (unique per invocation to avoid race conditions)
    let tmp_dir = std::env::temp_dir();
    let unique_id = uuid::Uuid::new_v4();
    let html_path = tmp_dir.join(format!("agentdesk_receipt_{unique_id}.html"));
    let png_path = tmp_dir.join(format!("agentdesk_receipt_{unique_id}.png"));
    std::fs::write(&html_path, &html).map_err(|e| format!("failed to write HTML: {e}"))?;

    // Playwright screenshot
    let output = tokio::process::Command::new("playwright")
        .args([
            "screenshot",
            "--browser", "chromium",
            "--full-page",
            &format!("file://{}", html_path.display()),
            &png_path.display().to_string(),
        ])
        .output()
        .await
        .map_err(|e| format!("playwright failed: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        eprintln!("  [{ts}] \u{2716} Playwright error: {stderr}");
        ctx.say(format!("Failed to render receipt image: {}", stderr.chars().take(200).collect::<String>())).await?;
        return Ok(());
    }

    // Send PNG as attachment
    if !Path::new(&png_path).exists() {
        ctx.say("Receipt image was not generated.").await?;
        return Ok(());
    }

    let attachment = CreateAttachment::path(&png_path).await
        .map_err(|e| format!("failed to read PNG: {e}"))?;

    ctx.send(
        poise::CreateReply::default()
            .content(format!(
                "\u{1f9fe} **Token Receipt** \u{2014} {} ({} ~ {})",
                data.period_label, data.period_start, data.period_end
            ))
            .attachment(attachment),
    )
    .await?;

    // Cleanup temp files
    let _ = std::fs::remove_file(&html_path);
    let _ = std::fs::remove_file(&png_path);

    println!("  [{ts}] \u{25b6} [{user_name}] Receipt sent (total: {})", receipt_fmt_cost(data.total));
    Ok(())
}

fn receipt_fmt_cost(c: f64) -> String {
    if c >= 1.0 { format!("${:.2}", c) } else { format!("${:.4}", c) }
}

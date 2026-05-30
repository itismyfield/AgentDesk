use std::path::{Path, PathBuf};

use poise::serenity_prelude as serenity;
use serenity::CreateAttachment;

use super::super::formatting::send_long_message_ctx;
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
            let window_start = match ctx.data().shared.pg_pool.as_ref() {
                Some(pg_pool) => receipt::ratelimit_window_start_pg(pg_pool).await,
                None => None,
            };
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

/// /usage — Show a text summary of token/rate-limit usage
#[poise::command(slash_command, rename = "usage")]
pub(in crate::services::discord) async fn cmd_usage(
    ctx: Context<'_>,
    #[description = "Period: ratelimit (default) or month"] period: Option<String>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] \u{25c0} [{user_name}] /usage");

    ctx.defer().await?;

    let period_str = period.as_deref().unwrap_or("ratelimit");
    let now = chrono::Utc::now();
    let (start, label) = match period_str {
        "month" => (now - chrono::Duration::days(30), "Last 30 Days"),
        _ => {
            let window_start = match ctx.data().shared.pg_pool.as_ref() {
                Some(pg_pool) => receipt::ratelimit_window_start_pg(pg_pool).await,
                None => None,
            };
            (
                window_start.unwrap_or_else(|| now - chrono::Duration::days(7)),
                "Rate Limit Window",
            )
        }
    };

    let label_owned = label.to_string();
    let data = tokio::task::spawn_blocking(move || receipt::collect(start, now, &label_owned))
        .await
        .map_err(|e| format!("usage collection failed: {e}"))?;

    send_long_message_ctx(ctx, &build_usage_report(&data)).await?;
    Ok(())
}

fn receipt_fmt_cost(c: f64) -> String {
    if c >= 1.0 {
        format!("${:.2}", c)
    } else {
        format!("${:.4}", c)
    }
}

fn usage_fmt_tokens(tokens: u64) -> String {
    let value = tokens as f64;
    if tokens >= 1_000_000 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", value / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn build_usage_report(data: &receipt::ReceiptData) -> String {
    let total_tokens: u64 = data.models.iter().map(|model| model.total_tokens).sum();
    let mut lines = vec![
        format!(
            "**Usage ({})** — {} to {}",
            data.period_label, data.period_start, data.period_end
        ),
        format!(
            "Tokens: {} total across {} message(s) / {} session(s)",
            usage_fmt_tokens(total_tokens),
            data.stats.total_messages,
            data.stats.total_sessions
        ),
        format!(
            "Estimated cost: {} (cache saved {})",
            receipt_fmt_cost(data.total),
            receipt_fmt_cost(data.cache_discount)
        ),
    ];

    if !data.providers.is_empty() {
        lines.push("Providers:".to_string());
        for provider in data.providers.iter().take(3) {
            lines.push(format!(
                "- {}: {} tokens ({:.1}%)",
                provider.provider,
                usage_fmt_tokens(provider.tokens),
                provider.percentage
            ));
        }
    }

    if !data.models.is_empty() {
        lines.push("Top models:".to_string());
        for model in data.models.iter().take(3) {
            lines.push(format!(
                "- {} ({}): {} tokens, {}",
                model.display_name,
                model.provider,
                usage_fmt_tokens(model.total_tokens),
                receipt_fmt_cost(model.cost)
            ));
        }
    }

    lines.push(
        "`/usage` summarizes provider token/rate-limit usage. `/metrics` shows local AgentDesk turn metrics."
            .to_string(),
    );
    lines.join("\n")
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::build_usage_report;
    use crate::receipt::{AgentShare, ModelLineItem, ProviderShare, ReceiptData, ReceiptStats};
    use crate::services::discord::{DISCORD_MSG_LIMIT, formatting::split_message};
    use std::collections::HashMap;

    #[test]
    fn usage_report_describes_usage_and_metrics_distinction() {
        let report = build_usage_report(&ReceiptData {
            period_label: "Rate Limit Window".to_string(),
            period_start: "2026-05-24".to_string(),
            period_end: "2026-05-30".to_string(),
            models: vec![ModelLineItem {
                model: "gpt-5".to_string(),
                display_name: "gpt-5".to_string(),
                input_tokens: 1_200,
                output_tokens: 300,
                cache_read_tokens: 500,
                cache_creation_tokens: 0,
                total_tokens: 2_000,
                cost: 0.015,
                cost_without_cache: 0.020,
                provider: "Codex".to_string(),
            }],
            subtotal: 0.020,
            cache_discount: 0.005,
            total: 0.015,
            stats: ReceiptStats {
                total_messages: 4,
                total_sessions: 2,
                per_provider: HashMap::new(),
                per_provider_agents: HashMap::new(),
            },
            providers: vec![ProviderShare {
                provider: "Codex".to_string(),
                tokens: 2_000,
                percentage: 100.0,
            }],
            agents: vec![AgentShare {
                agent: "codex".to_string(),
                tokens: 2_000,
                cost: 0.015,
                cost_without_cache: 0.020,
                input_tokens: 1_200,
                cache_read_tokens: 500,
                cache_creation_tokens: 0,
                percentage: 100.0,
            }],
        });

        assert!(report.contains("Usage (Rate Limit Window)"));
        assert!(report.contains("Tokens: 2.0K total across 4 message(s) / 2 session(s)"));
        assert!(report.contains("Estimated cost: $0.0150 (cache saved $0.0050)"));
        assert!(report.contains("`/usage` summarizes provider token/rate-limit usage."));
        assert!(report.contains("`/metrics` shows local AgentDesk turn metrics."));
    }

    #[test]
    fn oversized_usage_report_splits_under_discord_limit() {
        let long_suffix = "x".repeat(900);
        let models = (0..3)
            .map(|idx| ModelLineItem {
                model: format!("model-{idx}-{long_suffix}"),
                display_name: format!("display-model-{idx}-{long_suffix}"),
                input_tokens: 1_000,
                output_tokens: 500,
                cache_read_tokens: 250,
                cache_creation_tokens: 0,
                total_tokens: 1_500,
                cost: 0.010,
                cost_without_cache: 0.012,
                provider: format!("Provider-{idx}-{long_suffix}"),
            })
            .collect();
        let providers = (0..3)
            .map(|idx| ProviderShare {
                provider: format!("Provider-{idx}-{long_suffix}"),
                tokens: 1_500,
                percentage: 33.3,
            })
            .collect();

        let report = build_usage_report(&ReceiptData {
            period_label: "Rate Limit Window".to_string(),
            period_start: "2026-05-24".to_string(),
            period_end: "2026-05-30".to_string(),
            models,
            subtotal: 0.036,
            cache_discount: 0.006,
            total: 0.030,
            stats: ReceiptStats {
                total_messages: 12,
                total_sessions: 3,
                per_provider: HashMap::new(),
                per_provider_agents: HashMap::new(),
            },
            providers,
            agents: vec![AgentShare {
                agent: "codex".to_string(),
                tokens: 4_500,
                cost: 0.030,
                cost_without_cache: 0.036,
                input_tokens: 3_000,
                cache_read_tokens: 750,
                cache_creation_tokens: 0,
                percentage: 100.0,
            }],
        });

        assert!(report.len() > DISCORD_MSG_LIMIT);
        let chunks = split_message(&report);
        assert!(chunks.len() > 1);
        assert!(
            chunks.iter().all(|chunk| chunk.len() <= DISCORD_MSG_LIMIT),
            "all chunks must fit Discord's 2000-byte message cap"
        );
    }
}

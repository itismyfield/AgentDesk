use super::super::formatting::send_long_message_reply_ctx;
use super::super::{Context, Error, check_auth};
use crate::receipt;

/// `/usage` window; Discord only accepts the listed choices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, poise::ChoiceParameter)]
enum UsagePeriod {
    /// Current provider rate-limit window (falls back to the last 7 days).
    #[default]
    #[name = "ratelimit"]
    Ratelimit,
    /// Last 30 days.
    #[name = "month"]
    Month,
}

/// /usage — Show a text summary of token/rate-limit usage
#[poise::command(slash_command, rename = "usage")]
pub(in crate::services::discord) async fn cmd_usage(
    ctx: Context<'_>,
    #[description = "Period: ratelimit (default) or month"] period: Option<UsagePeriod>,
) -> Result<(), Error> {
    let user_id = ctx.author().id;
    let user_name = &ctx.author().name;
    if !check_auth(user_id, user_name, &ctx.data().shared, &ctx.data().token).await {
        return Ok(());
    }

    log_command_received!(ctx.channel_id().get(), user_name, "/usage");

    ctx.defer().await?;

    let now = chrono::Utc::now();
    let (start, label) = match period.unwrap_or_default() {
        UsagePeriod::Month => (now - chrono::Duration::days(30), "Last 30 Days"),
        UsagePeriod::Ratelimit => {
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

    send_long_message_reply_ctx(ctx, &build_usage_report(&data)).await?;
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

    lines.push("`/usage` summarizes provider token/rate-limit usage.".to_string());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    /// Discord enforces registered choices, so `/usage period` cannot carry a
    /// value outside the two windows the handler maps.
    #[test]
    fn usage_period_registers_only_the_mapped_choices() {
        let command = super::cmd_usage();
        let period = command
            .parameters
            .iter()
            .find(|parameter| parameter.name == "period")
            .expect("period parameter");
        assert!(!period.required);
        let choices: Vec<&str> = period
            .choices
            .iter()
            .map(|choice| choice.name.as_ref())
            .collect();
        assert_eq!(choices, ["ratelimit", "month"]);
    }
}

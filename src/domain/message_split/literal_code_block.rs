use super::{DISCORD_MSG_LIMIT, byte_index_at_discord_message_units, discord_message_units};

/// Render literal text with closed triple fences and a bounded omission notice.
pub(crate) fn bounded_raw_code_block_notice(header: &str, text: &str) -> String {
    let total_chars = text.chars().count();
    let max_suffix = format!("\n-# … (이하 {total_chars}자)");
    let header_end = byte_index_at_discord_message_units(
        header,
        DISCORD_MSG_LIMIT.saturating_sub(9 + discord_message_units(&max_suffix)),
    );
    let mut output = format!("{}\n```\n", &header[..header_end]);
    let initial = (output.len(), discord_message_units(&output), false);
    let mut units = initial.1;
    let mut outside = false;
    let mut checkpoints = Vec::new();
    let mut chars = text.chars().peekable();
    'source: while let Some(ch) = chars.next() {
        let mut count = 1;
        if ch == '`' {
            while chars.peek() == Some(&'`') {
                chars.next();
                count += 1;
            }
        }
        let escaped_run = ch == '`' && count >= 3;
        for _ in 0..count {
            let boundary = if outside != escaped_run {
                "\n```\n"
            } else {
                ""
            };
            let atom = if escaped_run {
                "\\`".into()
            } else {
                ch.to_string()
            };
            let next_units = units + boundary.len() + discord_message_units(&atom);
            let closing_units = if escaped_run { 9 } else { 4 };
            if next_units + closing_units > DISCORD_MSG_LIMIT {
                break 'source;
            }
            output.push_str(boundary);
            output.push_str(&atom);
            units = next_units;
            outside = escaped_run;
            checkpoints.push((output.len(), units, outside));
        }
    }
    loop {
        let (end, units, outside) = checkpoints.last().copied().unwrap_or(initial);
        let omitted = total_chars - checkpoints.len();
        let suffix = if omitted == 0 {
            String::new()
        } else {
            format!("\n-# … (이하 {omitted}자)")
        };
        // A literal run ends outside; reopen and close an empty final block as well.
        let closing = if outside { "\n```\n\n```" } else { "\n```" };
        if units + closing.len() + discord_message_units(&suffix) <= DISCORD_MSG_LIMIT {
            output.truncate(end);
            output.push_str(closing);
            output.push_str(&suffix);
            return output;
        }
        checkpoints.pop();
    }
}

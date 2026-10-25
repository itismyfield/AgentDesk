use super::*;

pub(super) async fn build_reply_context(
    ctx: &serenity::Context,
    channel_id: serenity::ChannelId,
    new_message: &serenity::Message,
) -> Option<String> {
    let ref_msg = new_message.referenced_message.as_ref()?;
    let attachments = ref_msg
        .attachments
        .iter()
        .map(AttachmentReplyItem::from)
        .collect::<Vec<_>>();
    let preceding = channel_id
        .messages(
            &ctx.http,
            serenity::builder::GetMessages::new()
                .before(ref_msg.id)
                .limit(4),
        )
        .await
        .unwrap_or_default();
    let preceding = preceding
        .iter()
        .map(|m| (m.author.name.as_str(), m.content.as_str()))
        .collect::<Vec<_>>();
    format_reply_context(
        &ref_msg.author.name,
        ref_msg.id.get(),
        &ref_msg.content,
        &attachments,
        &preceding,
    )
}

fn strip_subtext(content: &str) -> String {
    let mut code_fence = None;
    let mut lines = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed.as_bytes().first().copied().unwrap_or_default();
        let width = trimmed.bytes().take_while(|&b| b == marker).count();
        // Subtext inside a fenced code block is literal message content.
        if let Some((open_marker, open_width)) = code_fence {
            if marker == open_marker && width >= open_width && trimmed[width..].trim().is_empty() {
                code_fence = None;
            }
        } else if matches!(marker, b'`' | b'~') && width >= 3 {
            code_fence = Some((marker, width));
        } else if line.starts_with("-# ") {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

fn format_reply_context(
    ref_author: &str,
    ref_message_id: u64,
    content: &str,
    attachments: &[AttachmentReplyItem],
    preceding_newest_first: &[(&str, &str)],
) -> Option<String> {
    let mut entries = Vec::new();
    let mut budget: usize = 1000;
    for (author, content) in preceding_newest_first.iter().take(4).rev() {
        let content = strip_subtext(content);
        if content.is_empty() {
            continue;
        }
        let entry = format!("{}: {}", author, truncate_str(&content, 300));
        if entry.len() > budget {
            break;
        }
        budget -= entry.len();
        entries.push(entry);
    }
    let mut parts = Vec::new();
    if !entries.is_empty() {
        parts.push(format!(
            "[Reply context — earlier messages]\n{}",
            entries.join("\n")
        ));
    }
    let content = strip_subtext(content);
    let target = if !content.is_empty() {
        Some(format!("Content: {}", truncate_str(&content, 500)))
    } else if !attachments.is_empty() {
        Some(format_attachment_reply_content(ref_message_id, attachments))
    } else {
        None
    };
    if let Some(target) = target {
        parts.push(format!(
            "[Reply context — replied-to message]\nAuthor: {ref_author}\n{target}"
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!(
        "{}\n[End of reply context — the user's reply follows]",
        parts.join("\n\n")
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AttachmentReplyItem {
    filename: String,
    size: u32,
    description: Option<String>,
}

impl From<&serenity::Attachment> for AttachmentReplyItem {
    fn from(attachment: &serenity::Attachment) -> Self {
        Self {
            filename: attachment.filename.clone(),
            size: attachment.size,
            description: attachment.description.clone(),
        }
    }
}

fn format_attachment_reply_content(
    ref_message_id: u64,
    attachments: &[AttachmentReplyItem],
) -> String {
    let mut lines = vec![
        format!("Canonical Discord message id: {ref_message_id}"),
        "Content: [message has attachments but no text]".to_string(),
        "Attachments:".to_string(),
    ];
    for (index, attachment) in attachments.iter().take(10).enumerate() {
        let description = attachment.description.as_deref().unwrap_or("").trim();
        let mut line = format!(
            "{}. {} ({} bytes)",
            index + 1,
            attachment.filename,
            attachment.size
        );
        if !description.is_empty() {
            line.push_str(&format!(" — {}", truncate_str(description, 160)));
        }
        lines.push(line);
    }
    if attachments.len() > 10 {
        lines.push(format!("... {} more attachment(s)", attachments.len() - 10));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::router::message_handler::render_input_chunks;

    #[test]
    fn reply_context_removes_subtext_but_keeps_fenced_code() {
        let context = format_reply_context(
            "bot",
            1500,
            "-# ▸ Bash ✓\nanswer\n```text\n-# literal\n~~~\n```\n-# ⏱️ 25초 · 📦 16%",
            &[],
            &[
                ("new", "-# status\nuseful"),
                ("empty", "-# ▸ Bash ✓\n-# timing"),
                ("old", "previous"),
            ],
        )
        .unwrap();
        assert_eq!(
            context,
            "[Reply context — earlier messages]\nold: previous\nnew: useful\n\n[Reply context — replied-to message]\nAuthor: bot\nContent: answer\n```text\n-# literal\n~~~\n```\n[End of reply context — the user's reply follows]"
        );
        let tildes = format_reply_context(
            "bot",
            1,
            "~~~~\n-# literal\n~~~\n-# still literal\n~~~~\n-# status",
            &[],
            &[],
        )
        .unwrap();
        assert!(tildes.contains("Content: ~~~~\n-# literal\n~~~\n-# still literal\n~~~~\n[End"));
    }

    #[test]
    fn reply_context_omits_messages_emptied_by_subtext_removal() {
        assert_eq!(
            format_reply_context("bot", 1, "-# status", &[], &[("empty", "-# timing")]),
            None
        );
        let context =
            format_reply_context("bot", 1, "-# status", &[], &[("old", "answer")]).unwrap();
        assert_eq!(
            context,
            "[Reply context — earlier messages]\nold: answer\n[End of reply context — the user's reply follows]"
        );
    }

    #[test]
    fn reply_context_end_precedes_rendered_user_input() {
        let context = format_reply_context(
            "클로드봇",
            1500,
            "미니도 배포 health 검사를 요청했습니다.",
            &[],
            &[("클로드봇", "-# ▸ Bash ✓\n시험 요청을 받았습니다.")],
        )
        .unwrap();
        let rendered = render_input_chunks(
            &[],
            Some(context),
            "[User: 오부장 (ID: 343742347365974026)]\n애초에 배포를 막는 경고를 확인해 줘.".into(),
        );
        assert_eq!(
            rendered,
            "[Reply context — earlier messages]\n클로드봇: 시험 요청을 받았습니다.\n\n[Reply context — replied-to message]\nAuthor: 클로드봇\nContent: 미니도 배포 health 검사를 요청했습니다.\n[End of reply context — the user's reply follows]\n\n[User: 오부장 (ID: 343742347365974026)]\n애초에 배포를 막는 경고를 확인해 줘."
        );
        assert_eq!(
            crate::services::discord::response_sanitizer::sanitize_hidden_context(&rendered),
            rendered
        );
    }

    #[test]
    fn attachment_reply_context_keeps_canonical_message_id_and_all_files() {
        let attachments = (1..=5)
            .map(|index| AttachmentReplyItem {
                filename: format!("photo-{index}.png"),
                size: 1024 * index,
                description: (index == 3).then_some("middle attachment".to_string()),
            })
            .collect::<Vec<_>>();
        let context = format_reply_context("사용자", 1500, "", &attachments, &[]).unwrap();
        assert!(context.starts_with("[Reply context — replied-to message]\nAuthor: 사용자\n"));
        assert!(context.ends_with("\n[End of reply context — the user's reply follows]"));
        assert!(context.contains("Canonical Discord message id: 1500"));
        assert!(context.contains("photo-1.png"));
        assert!(context.contains("photo-3.png"));
        assert!(context.contains("middle attachment"));
        assert!(context.contains("photo-5.png"));
    }

    #[test]
    fn reply_context_keeps_existing_byte_and_message_budgets() {
        let body = "가".repeat(400);
        let preceding = [
            ("a", body.as_str()),
            ("b", body.as_str()),
            ("c", body.as_str()),
            ("d", body.as_str()),
            ("e", "excluded"),
        ];
        let context = format_reply_context("bot", 1, &body, &[], &preceding).unwrap();
        let (earlier, target) = context.split_once("\n\n").unwrap();
        let entries = earlier.lines().skip(1).collect::<Vec<_>>();
        assert_eq!(entries.len(), 3);
        assert!(entries.iter().map(|entry| entry.len()).sum::<usize>() <= 1000);
        assert!(
            entries
                .iter()
                .all(|entry| entry.split_once(": ").unwrap().1.len() <= 300)
        );
        assert!(!context.contains("excluded"));
        let target = target
            .split_once("Content: ")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap();
        assert_eq!(target.len(), 498);
        let four = format_reply_context(
            "bot",
            1,
            "answer",
            &[],
            &[
                ("a", "one"),
                ("b", "two"),
                ("c", "three"),
                ("d", "four"),
                ("e", "five"),
            ],
        )
        .unwrap();
        assert!(four.starts_with(
            "[Reply context — earlier messages]\nd: four\nc: three\nb: two\na: one\n\n"
        ));
    }
}

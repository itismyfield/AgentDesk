//! Preserve the original bytes; parsed spans identify the only token that may change.

use super::writer_compat;

const KEY: &str = "resume_agents_on_restore";
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EditError {
    Invalid,
    NonBoolean,
    Span,
    WriterIncompatible,
}

pub(crate) fn is_false(raw: &str) -> bool {
    toml_edit::Document::parse(raw).ok().and_then(|document| {
        document
            .get("session")
            .and_then(|s| s.get(KEY))
            .and_then(toml_edit::Item::as_bool)
    }) == Some(false)
}

pub(crate) fn candidate(raw: &str) -> Result<String, EditError> {
    let document = toml_edit::Document::parse(raw).map_err(|_| EditError::Invalid)?;
    let newline = if raw
        .find('\n')
        .is_some_and(|n| n > 0 && raw.as_bytes()[n - 1] == b'\r')
    {
        "\r\n"
    } else {
        "\n"
    };
    let session = document.get("session");
    let mut output = raw.to_owned();
    if let Some(value) = session.and_then(|s| s.get(KEY)) {
        let value = value.as_value().ok_or(EditError::NonBoolean)?;
        let boolean = value.as_bool().ok_or(EditError::NonBoolean)?;
        let span = value.span().ok_or(EditError::Span)?;
        if raw.get(span.clone()) != Some(if boolean { "true" } else { "false" }) {
            return Err(EditError::Span);
        }
        output.replace_range(span, "false");
    } else if let Some(session) = session {
        let table = session.as_table().ok_or(EditError::WriterIncompatible)?;
        if table.is_implicit() || table.is_dotted() {
            return Err(EditError::WriterIncompatible);
        }
        let span = table.span().ok_or(EditError::Span)?;
        let tail = raw.get(span.start..).ok_or(EditError::Span)?;
        let length = tail.find('\n').map_or(tail.len(), |n| n + 1);
        let header = &tail[..length];
        if header.trim() != "[session]" {
            return Err(EditError::WriterIncompatible);
        }
        let prefix = if header.ends_with('\n') { "" } else { newline };
        output.insert_str(
            span.start + length,
            &format!("{prefix}{KEY} = false{newline}"),
        );
    } else {
        if !output.is_empty() && !output.ends_with('\n') {
            output.push_str(newline);
        }
        output.push_str(&format!("[session]{newline}{KEY} = false{newline}"));
    }
    writer_compat::verify(&output)?;
    Ok(output)
}

#[cfg(test)]
#[path = "editor_tests.rs"]
mod tests;

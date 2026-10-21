//! Reproduce the supported writer's section matching before authorizing a file edit.

use super::editor::{EditError, is_false};

// Pinned upstream config/io.rs upsert_section_raw; its line-oriented behavior is intentional.
pub(super) fn upsert(content: &str, section: &str, key: &str, value: &str) -> String {
    let header = format!("[{section}]");
    let assignment = format!("{key} = {value}");
    let lines: Vec<&str> = content.lines().collect();
    let (mut result, mut i, mut found, mut inserted) = (Vec::new(), 0, false, false);
    while i < lines.len() {
        let line = lines[i];
        if line.trim() == header {
            found = true;
            result.push(line.to_string());
            i += 1;
            while i < lines.len() {
                let current = lines[i];
                let trimmed = current.trim();
                if trimmed.starts_with('[') && trimmed.ends_with(']') {
                    if !inserted {
                        result.push(assignment.clone());
                        inserted = true;
                    }
                    break;
                }
                if trimmed.starts_with(&format!("{key} "))
                    || trimmed.starts_with(&format!("{key}="))
                {
                    result.push(assignment.clone());
                    inserted = true;
                } else {
                    result.push(current.to_string());
                }
                i += 1;
            }
            continue;
        }
        result.push(line.to_string());
        i += 1;
    }
    if !found {
        if !result.is_empty() && !result.last().is_some_and(|line| line.trim().is_empty()) {
            result.push(String::new());
        }
        result.push(header);
        result.push(assignment);
    } else if !inserted {
        result.push(assignment);
    }
    result.join("\n") + "\n"
}

pub(super) fn remove_key(content: &str, section: &str, key: &str) -> String {
    let header = format!("[{section}]");
    let mut in_section = false;
    let mut result = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = trimmed == header;
            result.push(line);
            continue;
        }
        if in_section
            && (trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")))
        {
            continue;
        }
        result.push(line);
    }
    result.join("\n") + "\n"
}

pub(super) fn top_bool(content: &str, key: &str, value: bool) -> String {
    let replacement = format!("{key} = {value}");
    let mut lines: Vec<String> = content.lines().map(str::to_owned).collect();
    let mut in_section = false;
    for line in &mut lines {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            in_section = true;
            continue;
        }
        if !in_section
            && (trimmed.starts_with(&format!("{key} ")) || trimmed.starts_with(&format!("{key}=")))
        {
            *line = replacement;
            return lines.join("\n") + "\n";
        }
    }
    if lines.is_empty() {
        format!("{replacement}\n")
    } else {
        format!("{replacement}\n{}\n", lines.join("\n").trim_end())
    }
}

pub(crate) fn verify(candidate: &str) -> Result<(), EditError> {
    if !is_false(candidate) {
        return Err(EditError::Invalid);
    }
    let theme = upsert(candidate, "theme", "name", "\"default\"");
    let toast = upsert(candidate, "ui.toast", "delivery", "\"off\"");
    let replays = [
        upsert(candidate, "session", "resume_agents_on_restore", "false"),
        upsert(&theme, "theme", "auto_switch", "false"),
        upsert(candidate, "ui", "status_indicators", "\"icons\""),
        upsert(candidate, "ui.sound", "enabled", "false"),
        remove_key(&toast, "ui.toast", "enabled"),
        top_bool(candidate, "onboarding", false),
    ];
    if replays.iter().all(|replay| is_false(replay)) {
        Ok(())
    } else {
        Err(EditError::WriterIncompatible)
    }
}

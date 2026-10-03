//! Status rows that anchor compact Codex composers across CLI versions.

pub(super) fn line_is_codex_fast_context_status(line: &str) -> bool {
    let parts: Vec<&str> = line.split('·').map(str::trim).collect();
    let Some(fast_idx) = parts
        .iter()
        .position(|part| matches!(*part, "Fast on" | "Fast off"))
    else {
        return false;
    };
    let context_idx = if fast_idx == 0 {
        if parts.len() != 3 || parts[1].is_empty() {
            return false;
        }
        2
    } else {
        let mut model = parts[0].split_whitespace();
        if !model
            .next()
            .is_some_and(|name| name.to_ascii_lowercase().starts_with("gpt-"))
            || !model.next().is_some_and(|effort| {
                matches!(
                    effort,
                    "default" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
                )
            })
            || model.next().is_some()
        {
            return false;
        }
        fast_idx + 1
    };
    parts
        .get(context_idx)
        .and_then(|value| value.strip_prefix("Context "))
        .and_then(|value| value.strip_suffix("% left"))
        .is_some_and(|percent| !percent.is_empty() && percent.chars().all(|ch| ch.is_ascii_digit()))
        && parts[context_idx + 1..].iter().all(|part| {
            part.strip_suffix(" window").is_some_and(|size| {
                size.chars().next().is_some_and(|ch| ch.is_ascii_digit())
                    && size
                        .chars()
                        .all(|ch| ch.is_ascii_digit() || matches!(ch, '.' | 'K' | 'M'))
            })
        })
}

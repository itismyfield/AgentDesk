//! The same bounded terminal summary is shared by O and Legacy delivery.

pub(crate) mod quota;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Completed,
    Cancelled,
    Failed,
}

pub(crate) fn render(
    outcome: Outcome,
    elapsed_ms: Option<u64>,
    context_pct: Option<u8>,
    quota: Option<quota::Quota>,
) -> String {
    let mut line = match outcome {
        Outcome::Completed if elapsed_ms.is_none() && context_pct.is_none() => "-# ✅ 완료",
        Outcome::Completed if elapsed_ms.is_some() => "-# ⏱️",
        Outcome::Completed => "-# ✅",
        Outcome::Cancelled => "-# ⏹ 중단",
        Outcome::Failed => "-# ⚠️ 오류",
    }
    .to_string();
    if let Some(elapsed_ms) = elapsed_ms {
        line.push_str(if outcome == Outcome::Completed {
            " "
        } else {
            " · "
        });
        line.push_str(&elapsed(elapsed_ms));
    }
    if let Some(percent) = context_pct {
        line.push_str(&format!(" · 📦 {}%", percent.min(100)));
    }
    if outcome == Outcome::Completed
        && elapsed_ms.is_some()
        && let Some(quota) = quota
    {
        quota.append(&mut line);
    }
    line
}

fn elapsed(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds < 60 {
        format!("{seconds}초")
    } else if seconds < 3600 {
        let minutes = seconds / 60;
        match seconds % 60 {
            0 => format!("{minutes}분"),
            remainder => format!("{minutes}분 {remainder}초"),
        }
    } else {
        format!("{}시간 {}분", seconds / 3600, seconds % 3600 / 60)
    }
}

#[cfg(test)]
#[path = "turn_summary_line_tests.rs"]
mod tests;

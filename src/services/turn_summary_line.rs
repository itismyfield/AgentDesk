//! The same bounded terminal summary is shared by O and Legacy delivery.

use chrono::{DateTime, Utc};
use chrono_tz::Asia::Seoul;

pub mod quota;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Default)]
pub struct Model<'a> {
    pub name: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub fast: bool,
}

#[derive(Default)]
pub struct Footer<'a> {
    pub elapsed_ms: Option<u64>,
    pub started: Option<DateTime<Utc>>,
    pub ended: Option<DateTime<Utc>>,
    pub model: Model<'a>,
    pub context_pct: Option<u8>,
    pub quota: Option<quota::Quota>,
}

fn model_name(raw: &str) -> Option<&str> {
    let name = raw.trim();
    (name.len() <= 64
        && name
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" -._/+:@[]".contains(&byte)))
    .then_some(name)
}

pub fn render(outcome: Outcome, footer: Footer<'_>) -> String {
    let name = footer.model.name.and_then(model_name);
    let effort = footer.model.effort.map(str::trim).filter(|effort| {
        !effort.is_empty()
            && effort.len() <= 16
            && effort
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    });
    let modes = [effort, footer.model.fast.then_some("fast")]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
    let mut line = match outcome {
        Outcome::Completed if footer.elapsed_ms.is_some() => "-# ⏱️",
        Outcome::Completed
            if footer.context_pct.is_none() && name.is_none() && modes.is_empty() =>
        {
            "-# ✅ 완료"
        }
        Outcome::Completed => "-# ✅",
        Outcome::Cancelled => "-# ⏹ 중단",
        Outcome::Failed => "-# ⚠️ 오류",
    }
    .to_string();

    if let Some(milliseconds) = footer.elapsed_ms {
        line.push_str(if outcome == Outcome::Completed {
            " "
        } else {
            " · "
        });
        line.push_str(&elapsed(milliseconds));
    }
    if let Some(ended) = footer.ended {
        let ended = ended.with_timezone(&Seoul);
        let date_changed = footer.started.is_some_and(|started| {
            started.with_timezone(&Seoul).date_naive() != ended.date_naive()
        });
        let format = if date_changed { "%m/%d %H:%M" } else { "%H:%M" };
        line.push_str(&format!(" ({})", ended.format(format)));
    }
    if let Some(name) = name {
        line.push_str(&format!(" · {name}"));
    }
    if !modes.is_empty() {
        if name.is_some() {
            line.push_str(&format!(" ({modes})"));
        } else {
            line.push_str(&format!(" · {modes}"));
        }
    }
    if let Some(percent) = footer.context_pct {
        line.push_str(&format!(" · 📦 {}%", percent.min(100)));
    }
    if outcome == Outcome::Completed
        && (footer.elapsed_ms.is_some() || footer.ended.is_some())
        && let Some(quota) = footer.quota
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

#[cfg(test)]
pub(crate) use tests::assert_completed_footer;

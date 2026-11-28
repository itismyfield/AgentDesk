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

#[cfg(any(test, feature = "test-support"))]
pub fn assert_completed_footer(footer: &str) {
    let mut lines = footer.lines();
    let first = lines.next().expect("completed footer");
    let time = r"(?:[01][0-9]|2[0-3]):[0-5][0-9]";
    let day_time = format!(r"(?:0[1-9]|1[0-2])/(?:0[1-9]|[12][0-9]|3[01]) {time}");
    let percent = r"(?:100|[1-9]?[0-9])%";
    let mode = r"[A-Za-z0-9-]{1,16}";
    let name = r"[A-Za-z0-9][A-Za-z0-9 ._+/\-:@\[\]]{0,63}";
    let shape = format!(
        concat!(
            r"\A-# ⏱️ (?:[0-9]+초|[0-9]+분(?: [0-9]+초)?|[0-9]+시간 [0-9]+분)",
            r" \((?:{0}|{1})\)(?: · (?:{2}(?: \({3}(?:, fast)?\))?|{3}, fast))?",
            r"(?: · 📦 {4})?\z"
        ),
        time, day_time, name, mode, percent
    );
    assert!(
        regex::Regex::new(&shape).unwrap().is_match(first),
        "{footer:?}"
    );
    if let Some(quota) = lines.next() {
        let left = r"(?:[1-9][0-9]*d (?:1?[0-9]|2[0-3])h|(?:1?[0-9]|2[0-3])h [1-5]?[0-9]m)";
        let five = format!(r"5h {percent}(?: \({left}, {time}\))?");
        let seven = format!(r"7d {percent}(?: \({left}, {day_time}\))?");
        let fable = format!("Fable {seven}");
        let shape = format!(
            r"\A-# (?:{five}(?: · {seven})?(?: · {fable})?|{seven}(?: · {fable})?|{fable})\z"
        );
        assert!(
            regex::Regex::new(&shape).unwrap().is_match(quota) && quota.ends_with(')'),
            "{footer:?}"
        );
    }
    assert_eq!(lines.next(), None, "{footer:?}");
}

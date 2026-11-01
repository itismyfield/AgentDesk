//! What a same-session relaunch brought up, from the new process's statusline record and its first
//! answer; fast mode is reported as delivered, never as on, as the account and model decide it.

use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::*;

pub(super) struct Confirm {
    pub(super) session: String,
    pub(super) requested_fast: Option<bool>,
    /// When the old process ended, in Unix seconds; an earlier record is the old process's.
    pub(super) at: f64,
    pub(super) model: Option<ModelCheck>,
}

/// Where the new process's first answer shows the model a model change brought up.
pub(super) struct ModelCheck {
    pub(super) requested: Option<String>,
    pub(super) transcript: Option<std::path::PathBuf>,
    /// The transcript's length when the old process ended; what lies before it is the old one's.
    pub(super) offset: u64,
}

pub(super) struct Waits {
    pub(super) record: Duration,
    pub(super) settle: Duration,
    pub(super) poll: Duration,
    pub(super) answer: Duration,
}

const WAITS: Waits = Waits {
    record: Duration::from_secs(90),
    answer: Duration::from_secs(15 * 60),
    // Fast mode is shown on at start and turned off within a second when the account lacks it.
    settle: Duration::from_secs(3),
    poll: Duration::from_secs(1),
};

static TASKS: LazyLock<dashmap::DashMap<serenity::ChannelId, (u64, tokio::task::AbortHandle)>> =
    LazyLock::new(dashmap::DashMap::new);
static NEXT_TASK: AtomicU64 = AtomicU64::new(0);

pub(super) fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

/// Starts the channel's confirmation, replacing any earlier one.
pub(super) fn spawn(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: serenity::ChannelId,
    confirm: Confirm,
) {
    spawn_with(http, shared, channel_id, confirm, WAITS);
}

pub(super) fn spawn_with(
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    channel_id: serenity::ChannelId,
    confirm: Confirm,
    waits: Waits,
) {
    let task = NEXT_TASK.fetch_add(1, Ordering::Relaxed);
    let records = crate::services::claude_tui::hook_bundle::statusline::record_dir();
    let handle = tokio::spawn(async move {
        let current = || {
            let shared = shared.clone();
            async move {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|session| session.session_id.clone())
            }
        };
        let notice = confirm_notice(&confirm, &records, current, &waits).await;
        super::relaunch::say(&http, channel_id, notice).await;
        TASKS.remove_if(&channel_id, |_, (own, _)| *own == task);
    });
    if let Some((_, (_, earlier))) = TASKS.remove(&channel_id) {
        earlier.abort();
    }
    TASKS.insert(channel_id, (task, handle.abort_handle()));
}

#[cfg(test)]
pub(super) fn running(channel_id: serenity::ChannelId) -> bool {
    TASKS
        .get(&channel_id)
        .is_some_and(|task| !task.1.is_finished())
}

pub(super) fn abort(channel_id: serenity::ChannelId) {
    if let Some((_, (_, handle))) = TASKS.remove(&channel_id) {
        handle.abort();
    }
}

struct Record {
    model: Option<String>,
    effort: Option<String>,
    fast_mode: Option<bool>,
}

fn read_record(records: &Path, session: &str, after: f64) -> Option<Record> {
    let path = crate::services::claude_tui::hook_bundle::statusline::record_path(records, session);
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    let written = value["written_at"].as_f64()?;
    if value["session_id"].as_str() != Some(session) || written <= after {
        return None;
    }
    Some(Record {
        model: value["model"]["id"].as_str().map(str::to_string),
        effort: value["effort"]["level"].as_str().map(str::to_string),
        fast_mode: value["fast_mode"].as_bool(),
    })
}

fn short(session: &str) -> &str {
    session.get(..8).unwrap_or(session)
}

fn shown(fast: Option<bool>) -> &'static str {
    match fast {
        Some(true) => "켜짐",
        Some(false) => "꺼짐",
        None => "미상",
    }
}

fn observed(confirm: &Confirm, record: &Record) -> String {
    let fast = match confirm.requested_fast {
        Some(true) => format!(
            "fast 설정 전달됨(켬 요청, 표시 {}). 실제 활성은 계정·모델 조건에 따릅니다",
            shown(record.fast_mode)
        ),
        Some(false) => format!("fast 끔 요청 전달됨(표시 {})", shown(record.fast_mode)),
        None => format!("fast 표시 {}", shown(record.fast_mode)),
    };
    format!(
        "재기동 확인 — 같은 세션 `{}` · 표시 모델 `{}` · effort `{}` · {fast}",
        short(&confirm.session),
        record.model.as_deref().unwrap_or("미상"),
        record.effort.as_deref().unwrap_or("미상"),
    )
}

enum Answer {
    Model(String),
    Failed,
}

/// The first answer of `session` written past `offset`.
fn first_answer(transcript: &Path, offset: u64, session: &str) -> Option<Answer> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(transcript).ok()?;
    file.seek(std::io::SeekFrom::Start(offset)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let tail = String::from_utf8_lossy(&tail);
    let record = tail.lines().find_map(|line| {
        let record: serde_json::Value = serde_json::from_str(line).ok()?;
        (record["type"] == "assistant" && record["sessionId"].as_str() == Some(session))
            .then_some(record)
    })?;
    if record["isApiErrorMessage"].as_bool() == Some(true) {
        return Some(Answer::Failed);
    }
    let model = record["message"]["model"].as_str().unwrap_or("미상");
    Some(Answer::Model(model.to_string()))
}

fn answered(check: &ModelCheck, answer: Option<Answer>) -> String {
    let requested = check.requested.as_deref().unwrap_or("기본값");
    match answer {
        Some(Answer::Model(model)) => format!("응답 모델 `{model}`(요청 `{requested}`)."),
        Some(Answer::Failed) => format!("모델 적용 미확인(응답 오류, 요청 `{requested}`)."),
        None => format!("응답 모델 미확인(요청 `{requested}`)."),
    }
}

/// The notice ending the wait when the channel moved to another session.
async fn switched<F, Fut>(confirm: &Confirm, current: &F) -> Option<String>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let now = current().await.filter(|now| *now != confirm.session)?;
    Some(format!(
        "재기동 뒤 다른 세션 `{}`으로 시작됐습니다(이전 세션 `{}`의 맥락 소실).",
        short(&now),
        short(&confirm.session)
    ))
}

/// Waits for the restarted process's record of `confirm.session` and, for a model change, its first
/// answer; every round first checks for a move to another session, which ends the wait at once.
pub(super) async fn confirm_notice<F, Fut>(
    confirm: &Confirm,
    records: &Path,
    current: F,
    waits: &Waits,
) -> String
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    let deadline = Instant::now() + waits.record;
    let mut seen: Option<(Instant, Record)> = None;
    let record = loop {
        if let Some(notice) = switched(confirm, &current).await {
            return notice;
        }
        if let Some(record) = read_record(records, &confirm.session, confirm.at) {
            let first = seen.as_ref().map_or_else(Instant::now, |(first, _)| *first);
            seen = Some((first, record));
        }
        let settled = seen
            .as_ref()
            .is_some_and(|(first, _)| first.elapsed() >= waits.settle);
        if settled || Instant::now() >= deadline {
            break seen.map(|(_, record)| record);
        }
        tokio::time::sleep(waits.poll).await;
    };
    let status = match record {
        Some(record) => observed(confirm, &record),
        None => format!(
            "재기동 뒤 세션 `{}`의 상태 기록을 확인하지 못했어요. 설정 적용 여부는 아직 확인되지 않았습니다.",
            short(&confirm.session)
        ),
    };
    let Some(check) = &confirm.model else {
        return status;
    };
    let deadline = Instant::now() + waits.answer;
    let answer = loop {
        if let Some(notice) = switched(confirm, &current).await {
            return notice;
        }
        let transcript = check.transcript.as_deref();
        let answer = transcript.and_then(|path| first_answer(path, check.offset, &confirm.session));
        if answer.is_some() || Instant::now() >= deadline {
            break answer;
        }
        tokio::time::sleep(waits.poll).await;
    };
    format!("{status}\n{}", answered(check, answer))
}

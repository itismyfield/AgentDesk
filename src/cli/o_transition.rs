//! `agentdesk o transition`: asks the local server what its writer actor would clean up after a
//! transcript transition. Only Inspect exists; it writes no cleanup and opens no store here.

use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::{Value, json};

#[path = "o_transition/http.rs"]
mod http;

#[derive(Subcommand)]
pub(crate) enum TransitionCommand {
    /// Judge the channel's old transcript sources twice against its execution; writes nothing
    #[command(
        after_help = "Exit: 0 inspected. Busy, InProgress or no answer is no proof of no write"
    )]
    Inspect(InspectArgs),
}

#[derive(Args)]
pub(crate) struct InspectArgs {
    #[arg(long)]
    channel: u64,
    /// Session key of the channel's Bound row
    #[arg(long)]
    session_key: String,
    /// Execution nonce the row must still record
    #[arg(long)]
    execution_nonce: String,
    /// The last hop seq the transition covers
    #[arg(long)]
    cutoff: u64,
    #[arg(long)]
    operator: String,
    #[arg(long)]
    reason: String,
    /// The job ID to repeat; a new UUID otherwise
    #[arg(long)]
    job_id: Option<String>,
    /// The checked end of the old execution: host, pane, first and last hop seq, basis
    #[arg(long, num_args = 5, value_names = ["HOST", "PANE", "FIRST", "LAST", "BASIS"])]
    ended: Option<Vec<String>>,
    /// File the request and the server's answer are written to verbatim
    #[arg(long)]
    manifest: Option<PathBuf>,
}

pub(super) fn run(command: TransitionCommand) -> Result<(), String> {
    let TransitionCommand::Inspect(args) = command;
    let request = request(&args)?;
    let answer = http::post(&crate::cli::client::api_base(), &request, http::READ);
    println!("{}", render(&answer));
    if let Some(path) = &args.manifest {
        let (status, body) = match &answer {
            Ok((status, body)) => (json!(status), body.clone()),
            Err(error) => (Value::Null, json!(format!("{error}"))),
        };
        let manifest = json!({"request": request, "status": status, "answer": body});
        let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
        std::fs::write(path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    match answer {
        Ok((200, _)) => Ok(()),
        _ => Err("o transition inspect: not inspected".into()),
    }
}

fn request(args: &InspectArgs) -> Result<Value, String> {
    let seq = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|_| format!("--ended: {value}"))
    };
    let termination = match args.ended.as_deref() {
        None => Value::Null,
        Some([host, pane, first, last, basis]) => json!({"Confirmed": {
            "host": host, "pane": pane, "first_seq": seq(first)?, "last_seq": seq(last)?,
            "basis": basis,
        }}),
        Some(_) => return Err("--ended takes five values".into()),
    };
    let job_id = (args.job_id.clone()).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    Ok(json!({
        "operation": "inspect", "channel": args.channel, "session_key": args.session_key,
        "execution_nonce": args.execution_nonce, "cutoff": args.cutoff, "job_id": job_id,
        "operator": args.operator, "reason": args.reason, "termination": termination,
    }))
}

const NO_ANSWER: &str =
    "응답을 받지 못했습니다. 접수된 job은 계속 실행될 수 있으니 쓰기가 없었다고 보지 마세요.";

/// What the operator does next for a refusal or an outcome, as the cleanup's runbook says.
const NOTICES: &[(&str, &str)] = &[
    (
        "ForeignRun",
        "대상 actor run이 바뀌었습니다. 새 Inspect가 필요합니다.",
    ),
    ("CodexDeferred", "이 정리는 Claude 대상으로만 지원합니다."),
    ("Busy", "다른 job이 실행 중입니다. 완료 후 다시 확인하세요."),
    (
        "InProgress",
        "같은 job이 실행 중입니다. 이 응답은 완료 영수증이 아닙니다.",
    ),
    (
        "JobConflict",
        "같은 ID의 요청 내용이 다릅니다. 원본 manifest를 확인하세요.",
    ),
    (
        "Held(Stale)",
        "관측 후 상태가 바뀌었습니다. 다시 Inspect하세요.",
    ),
    (
        "Held(TransitionBusy)",
        "전환 권위가 사용 중입니다. 같은 job으로 재확인하세요.",
    ),
    (
        "Held(SourceBusy)",
        "source 권위가 사용 중입니다. 같은 job으로 재확인하세요.",
    ),
    (
        "Held(PriorCleanup)",
        "기존 정리 이력이 있습니다. 재성장·기존 job을 조사하세요.",
    ),
    (
        "Held(Limit)",
        "후보·해시·시간 상한으로 보류했습니다. 같은 job으로 반복하세요.",
    ),
    (
        "Held(NotTried)",
        "앞선 저장 실패로 시도하지 않았습니다. 저장 상태 확인이 먼저입니다.",
    ),
    (
        "Held(Stopping)",
        "actor가 멈추는 중이라 시도하지 않았습니다. 다시 뜬 뒤 같은 job으로 재확인하세요.",
    ),
    (
        "RetiredThenGrew",
        "감사는 저장됐지만 source가 다시 활성화됐습니다. clear를 진행하지 마세요.",
    ),
    (
        "Unknown",
        "저장이 durable했는지 불명입니다. 성공으로 처리하지 마세요.",
    ),
];

fn notice(tag: &str) -> Option<String> {
    if let Some(detail) = tag
        .strip_prefix("Unread(")
        .and_then(|t| t.strip_suffix(')'))
    {
        return Some(format!("상태를 확인할 수 없어 보류했습니다: {detail}"));
    }
    let found = NOTICES.iter().find(|(name, _)| *name == tag);
    found.map(|(_, text)| (*text).to_owned())
}

/// A serialized enum as `Name(Inner)`; an eligible verdict's draft audit is left out.
fn tag(value: &Value) -> String {
    match value {
        Value::String(name) => name.clone(),
        Value::Object(map) if map.len() == 1 => match map.iter().next() {
            Some((name, inner @ (Value::String(_) | Value::Object(_)))) if name != "Eligible" => {
                format!("{name}({})", tag(inner))
            }
            Some((name, _)) => name.clone(),
            None => String::new(),
        },
        other => other.to_string(),
    }
}

fn text(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_owned)
}

/// The answer for the operator, then the answer itself verbatim.
fn render(answer: &Result<(u16, Value), http::PostError>) -> String {
    let (status, body) = match answer {
        Ok(answer) => answer,
        Err(error) => return format!("no answer: {error}\n{NO_ANSWER}"),
    };
    let mut lines = vec![format!("HTTP {status} {}", text(&body["result"]))];
    match body["result"].as_str() {
        Some("reply_lost") => lines.push(NO_ANSWER.into()),
        Some("stale_actor_reply") => lines.push(
            "응답한 actor run이 이제 현재 run이 아니라 읽지 않았습니다. 다시 Inspect하세요.".into(),
        ),
        Some("inspected" | "actor_refused") => reply(&body["reply"], &mut lines),
        _ => lines.push(format!(
            "actor에 보내지 않았습니다: {}",
            text(&body["detail"])
        )),
    }
    lines.push(serde_json::to_string_pretty(body).unwrap_or_default());
    lines.join("\n")
}

fn reply(reply: &Value, lines: &mut Vec<String>) {
    if !reply["refused"].is_null() {
        lines.extend(notice(&tag(&reply["refused"])));
        return;
    }
    match (&reply["execution"]["Ok"], &reply["execution"]["Err"]) {
        (Value::Array(held), _) => lines.push(format!("execution: {} source(s) held", held.len())),
        (_, Value::String(error)) => lines.push(format!("execution: ERROR — {error}")),
        _ => lines.push("execution: unread".into()),
    }
    diagnostics(&reply["diagnostics"], lines);
    let mut outcomes = Vec::new();
    for hop in reply["hops"].as_array().into_iter().flatten() {
        let (judged, outcome) = (&hop["judged"], tag(&hop["outcome"]));
        let (key, seq, verdict) = (
            text(&judged["key"]),
            &judged["hop_seq"],
            tag(&judged["verdict"]),
        );
        lines.push(format!("hop {key} seq {seq}: {verdict} -> {outcome}"));
        if !outcomes.contains(&outcome) {
            outcomes.push(outcome);
        }
    }
    let summary = &reply["summary"];
    let counts = [
        "eligible",
        "already_done",
        "refused",
        "unknown",
        "not_target",
        "unsettled",
    ];
    let counts = counts.map(|name| format!("{name} {}", summary[name]));
    lines.push(format!(
        "summary: {}; unsettled now {}",
        counts.join(", "),
        reply["unsettled"]
    ));
    lines.extend(outcomes.iter().filter_map(|outcome| notice(outcome)));
}

fn diagnostics(diagnostics: &Value, lines: &mut Vec<String>) {
    lines.push(format!("nonce {}", text(&diagnostics["nonce"])));
    let history = &diagnostics["history"]["Ok"];
    if history.is_null() {
        lines.push(format!(
            "history: ERROR — {}",
            text(&diagnostics["history"]["Err"])
        ));
    }
    let pin = |pin: &Value| {
        pin["path"]
            .as_str()
            .map_or("no pin".to_owned(), str::to_owned)
    };
    lines.push(format!(
        "history complete {}, pinned {}",
        history["complete"],
        pin(&history["pinned"])
    ));
    for session in history["sessions"].as_array().into_iter().flatten() {
        let (role, name) = (text(&session[0]), text(&session[1]));
        lines.push(format!("  {role} {name}: {}", pin(&session[2])));
    }
    let mut unpinned = 0;
    for overlap in diagnostics["overlaps"].as_array().into_iter().flatten() {
        let held: Vec<String> = overlap["held_by"]
            .as_array()
            .into_iter()
            .flatten()
            .map(pin)
            .collect();
        let sessions = overlap["unpinned"].as_array().map_or(0, Vec::len);
        unpinned += sessions;
        let key = text(&overlap["key"]);
        lines.push(format!(
            "overlap {key}: held by {held:?}, {sessions} unpinned session(s)"
        ));
    }
    if unpinned > 0 {
        lines.push(format!(
            "Apply 중단: pin 없는 matching session {unpinned}개 — 보호 집합을 확정할 수 없음"
        ));
    }
}

#[cfg(test)]
#[path = "o_transition_tests.rs"]
mod tests;

//! `agentdesk o transition`: asks the local server what its writer actor would clean up after a
//! transcript transition, then has it clean up exactly the scope an Inspect manifest approved.
//! Neither opens a store here.

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
    /// Retire what the manifest's Inspect approved, refused if the scope changed since
    #[command(
        after_help = "Exit: 0 applied. Busy, InProgress or no answer is no proof of no write"
    )]
    Apply(ApplyArgs),
}

#[derive(Args)]
pub(crate) struct ApplyArgs {
    /// Manifest an Inspect with --ended wrote; its request and scope digest are sent as written
    #[arg(long)]
    manifest: PathBuf,
    /// Confirms the scope the manifest lists is the one to retire
    #[arg(long)]
    confirm: bool,
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
    let (request, manifest) = match &command {
        TransitionCommand::Inspect(args) => (request(args)?, args.manifest.as_ref()),
        TransitionCommand::Apply(args) => (apply(args)?, None),
    };
    let answer = http::post(&crate::cli::client::api_base(), &request, http::READ);
    println!("{}", render(&answer));
    if let Some(path) = manifest {
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
        _ => Err(format!("o transition: not {}", request["operation"])),
    }
}

/// The manifest's request verbatim as an Apply carrying its Inspect's scope digest; the manifest
/// itself is never rewritten.
fn apply(args: &ApplyArgs) -> Result<Value, String> {
    if !args.confirm {
        return Err("o transition apply: --confirm is required; nothing was sent".into());
    }
    let path = args.manifest.display();
    let bytes = std::fs::read(&args.manifest).map_err(|error| format!("{path}: {error}"))?;
    let manifest: Value = serde_json::from_slice(&bytes).map_err(|e| format!("{path}: {e}"))?;
    let mut request = manifest["request"].clone();
    if request["operation"] != "inspect" {
        return Err(format!("{path}: not an inspect manifest"));
    }
    let digest = &manifest["answer"]["reply"]["scope"]["Ok"]["digest"];
    let digest =
        (digest.as_str()).ok_or(format!("{path}: no scope digest; inspect with --ended"))?;
    request["operation"] = json!("apply");
    request["scope_digest"] = json!(digest);
    Ok(request)
}

fn request(args: &InspectArgs) -> Result<Value, String> {
    let seq = |value: &str| {
        value
            .parse::<u64>()
            .map_err(|e| format!("--ended {value}: {e}"))
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

const NO_ANSWER: &str = "\
응답을 받지 못했습니다. 접수된 job은 계속 실행될 수 있으니 쓰기가 없었다고 보지 마세요.";

/// What the operator does next after a refusal or an outcome, one `tag: notice` a line.
const NOTICES: &str = "\
ForeignRun: 대상 actor run이 바뀌었습니다. 새 Inspect가 필요합니다.
CodexDeferred: 이 정리는 Claude 대상으로만 지원합니다.
Busy: 다른 job이 실행 중입니다. 완료 후 다시 확인하세요.
InProgress: 같은 job이 실행 중입니다. 이 응답은 완료 영수증이 아닙니다.
JobConflict: 같은 ID의 요청 내용이 다릅니다. 원본 manifest를 확인하세요.
Unread(inspect_scope_changed): Inspect 이후 승인 범위가 바뀌어 쓰지 않았습니다. 같은 job으로 다시 Inspect해 범위를 확인하세요.
Unread: 상태를 확인할 수 없어 보류했습니다.
Held(Stale): 관측 후 상태가 바뀌었습니다. 다시 Inspect하세요.
Held(TransitionBusy): 전환 권위가 사용 중입니다. 같은 job으로 재확인하세요.
Held(SourceBusy): source 권위가 사용 중입니다. 같은 job으로 재확인하세요.
Held(PriorCleanup): 기존 정리 이력이 있습니다. 재성장·기존 job을 조사하세요.
Held(Limit): 후보·해시·시간 상한으로 보류했습니다. 같은 job으로 반복하세요.
Held(NotTried): 앞선 저장 실패로 시도하지 않았습니다. 저장 상태 확인이 먼저입니다.
Held(Stopping): actor가 멈추는 중이라 시도하지 않았습니다. 다시 뜬 뒤 같은 job으로 재확인하세요.
Held(InspectScopeChanged): 실행 중 승인 범위가 바뀌어 남은 source를 쓰지 않았습니다. 앞선 퇴역은 유지됩니다. 다시 Inspect하세요.
RetiredThenGrew: 감사는 저장됐지만 source가 다시 활성화됐습니다. clear를 진행하지 마세요.
Unknown: 저장이 durable했는지 불명입니다. 성공으로 처리하지 마세요.";

fn notice(tag: &str) -> Option<String> {
    let name = if tag.starts_with("Unread(inspect_scope_changed") {
        "Unread(inspect_scope_changed)"
    } else if tag.starts_with("Unread(") {
        "Unread"
    } else {
        tag
    };
    let line = NOTICES
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "));
    line.map(|line| format!("{tag}: {line}"))
}

/// A serialized enum as `Name(Inner)`; an eligible verdict's draft audit is left out.
fn tag(value: &Value) -> String {
    let Some((name, inner)) = value.as_object().and_then(|map| map.iter().next()) else {
        return value
            .as_str()
            .map_or_else(|| value.to_string(), str::to_owned);
    };
    match inner {
        _ if name == "Eligible" => name.clone(),
        Value::String(_) | Value::Object(_) => format!("{name}({})", tag(inner)),
        _ => name.clone(),
    }
}

/// The answer for the operator, then the answer itself verbatim.
fn render(answer: &Result<(u16, Value), http::PostError>) -> String {
    let (status, body) = match answer {
        Ok(answer) => answer,
        Err(error) => return format!("no answer: {error}\n{NO_ANSWER}"),
    };
    let mut lines = vec![format!("HTTP {status} {}", tag(&body["result"]))];
    match body["result"].as_str() {
        Some("reply_lost") => lines.push(NO_ANSWER.into()),
        Some("inspected" | "applied" | "actor_refused") => reply(&body["reply"], &mut lines),
        Some("stale_actor_reply") => {
            lines.push("응답한 run이 현재 run이 아니라 읽지 않았습니다.".into())
        }
        _ => lines.push(format!(
            "actor에 보내지 않았습니다: {}",
            tag(&body["detail"])
        )),
    }
    lines.push(serde_json::to_string_pretty(body).unwrap_or_default());
    lines.join("\n")
}

fn reply(reply: &Value, lines: &mut Vec<String>) {
    let mut tags = vec![tag(&reply["refused"])];
    lines.push(format!("execution {}", tag(&reply["execution"])));
    let diagnostics = &reply["diagnostics"];
    let history = &diagnostics["history"];
    let sessions = history["Ok"]["sessions"].as_array().map_or(0, Vec::len);
    let complete = &history["Ok"]["complete"];
    lines.push(format!(
        "history of nonce {}: complete {complete}, {sessions} session(s) {}",
        diagnostics["nonce"], history["Err"]
    ));
    let mut unpinned = 0;
    for overlap in diagnostics["overlaps"].as_array().into_iter().flatten() {
        unpinned += overlap["unpinned"].as_array().map_or(0, Vec::len);
        lines.push(format!(
            "overlap {}: held by {}, unpinned {}",
            overlap["key"], overlap["held_by"], overlap["unpinned"]
        ));
    }
    if unpinned > 0 {
        lines.push(format!(
            "Apply 중단: pin 없는 matching session {unpinned}개, 보호 집합을 확정할 수 없음"
        ));
    }
    scope(reply, lines);
    for hop in reply["hops"].as_array().into_iter().flatten() {
        let (judged, outcome) = (&hop["judged"], tag(&hop["outcome"]));
        let verdict = tag(&judged["verdict"]);
        lines.push(format!(
            "hop {} seq {}: {verdict} -> {outcome}",
            judged["key"], judged["hop_seq"]
        ));
        tags.push(outcome);
    }
    lines.push(format!(
        "summary {} unsettled now {}",
        reply["summary"], reply["unsettled"]
    ));
    tags.sort();
    tags.dedup();
    lines.extend(tags.iter().filter_map(|tag| notice(tag)));
}

/// The scope the reply approves, a pinned source in it marked as not applicable now.
fn scope(reply: &Value, lines: &mut Vec<String>) {
    let Some(scope) = reply["scope"].get("Ok") else {
        let why = reply["scope"]["Err"]
            .as_str()
            .unwrap_or("최종 snapshot 없음");
        return lines.push(format!("scope 없음({why}): 이 응답으로 Apply할 수 없음"));
    };
    let hops = scope["hops"].as_array().map_or(&[][..], Vec::as_slice);
    lines.push(format!("scope {} hop {}개", scope["digest"], hops.len()));
    let overlaps = reply["diagnostics"]["overlaps"].as_array();
    for hop in hops {
        let pinned = overlaps.into_iter().flatten().any(|overlap| {
            overlap["key"] == hop["key"]
                && overlap["held_by"]
                    .as_array()
                    .is_some_and(|by| !by.is_empty())
        });
        let note = if pinned {
            " (현재 적용할 수 없음: pin이 쥔 source)"
        } else {
            ""
        };
        lines.push(format!(
            "scope seq {} pane {} {} -> {}{note}",
            hop["seq"], hop["pane"], hop["old"]["session_id"], hop["successor"]["session_id"]
        ));
    }
}

#[cfg(test)]
#[path = "o_transition_tests.rs"]
mod tests;

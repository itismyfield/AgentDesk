//! Codex tmux wrapper input plumbing: terminal and external prompt readers.
//!
//! Extracted verbatim from the parent module so the wrapper's prompt intake can
//! grow test coverage without growing the giant file it came from.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

use crate::services::tmux_wrapper::InputMode;
use crate::services::tui_prompt_control::{
    CODEX_LOCAL_CONTROLS, CodexInputClass, classify_codex_input,
};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
const TMUX_PROMPT_B64_CHUNK_PREFIX: &str = "__AGENTDESK_B64_CHUNK__:";

/// Terminal input — only in Fifo mode (interactive tmux session)
pub(super) fn spawn_terminal_input_reader(
    input_mode: InputMode,
    prompt_tx: &mpsc::Sender<CodexPrompt>,
) {
    if input_mode == InputMode::Fifo {
        let prompt_tx = prompt_tx.clone();
        std::thread::spawn(move || {
            loop {
                let reader = open_codex_terminal_input_reader();
                match read_codex_terminal_input_lines(reader, &prompt_tx) {
                    TerminalInputLoopOutcome::RetryReader => {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                    TerminalInputLoopOutcome::Stop => break,
                }
            }
        });
    }
}

/// External input
/// Fifo mode: reads from named FIFO
/// Pipe mode: reads from process stdin (parent writes to child stdin pipe)
pub(super) fn spawn_external_input_reader(
    input_mode: InputMode,
    input_fifo: &str,
    prompt_tx: &mpsc::Sender<CodexPrompt>,
) {
    let prompt_tx = prompt_tx.clone();
    let input_fifo = input_fifo.to_string();
    std::thread::spawn(move || {
        let mut decoder = ExternalPromptDecoder::default();
        let reader: BufReader<Box<dyn std::io::Read + Send>> = match input_mode {
            InputMode::Fifo => {
                let fifo = match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&input_fifo)
                {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("\x1b[90m[input fifo error: {}]\x1b[0m", e);
                        return;
                    }
                };
                BufReader::new(Box::new(fifo))
            }
            InputMode::Pipe => BufReader::new(Box::new(std::io::stdin())),
        };

        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if line.trim().is_empty() {
                continue;
            }
            eprintln!("\x1b[90m[external message received]\x1b[0m");
            match decoder.decode_line(&line) {
                Ok(Some(prompt)) => {
                    if !prompt.trim().is_empty() {
                        let _ = prompt_tx.send(CodexPrompt::external(prompt));
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    eprintln!("\x1b[90m[input decode error: {}]\x1b[0m", err);
                }
            }
        }
    });
}

#[derive(Default)]
struct ExternalPromptDecoder {
    chunked: HashMap<String, ChunkedPrompt>,
}

struct ChunkedPrompt {
    chunks: Vec<Option<String>>,
    received: usize,
}

impl ExternalPromptDecoder {
    fn decode_line(&mut self, line: &str) -> Result<Option<String>, String> {
        if let Some(encoded) = line.strip_prefix(TMUX_PROMPT_B64_PREFIX) {
            return decode_base64_prompt(encoded).map(Some);
        }

        if let Some(chunk) = line.strip_prefix(TMUX_PROMPT_B64_CHUNK_PREFIX) {
            return self.decode_chunk(chunk);
        }

        Ok(Some(line.to_string()))
    }

    fn decode_chunk(&mut self, line: &str) -> Result<Option<String>, String> {
        let mut parts = line.splitn(4, ':');
        let message_id = parts
            .next()
            .filter(|value| !value.is_empty())
            .ok_or("missing chunk message id")?;
        let index = parts
            .next()
            .ok_or("missing chunk index")?
            .parse::<usize>()
            .map_err(|_| "invalid chunk index".to_string())?;
        let total = parts
            .next()
            .ok_or("missing chunk total")?
            .parse::<usize>()
            .map_err(|_| "invalid chunk total".to_string())?;
        let chunk = parts.next().ok_or("missing chunk payload")?;

        if total == 0 || total > 10_000 {
            return Err("invalid chunk total".to_string());
        }
        if index >= total {
            return Err("chunk index out of range".to_string());
        }

        let entry = self
            .chunked
            .entry(message_id.to_string())
            .or_insert_with(|| ChunkedPrompt {
                chunks: vec![None; total],
                received: 0,
            });
        if entry.chunks.len() != total {
            self.chunked.remove(message_id);
            return Err("chunk total changed for message id".to_string());
        }
        if entry.chunks[index].is_some() {
            self.chunked.remove(message_id);
            return Err("duplicate chunk index".to_string());
        }

        entry.chunks[index] = Some(chunk.to_string());
        entry.received += 1;
        if entry.received != total {
            return Ok(None);
        }

        let entry = self
            .chunked
            .remove(message_id)
            .ok_or("completed chunk state missing")?;
        let mut encoded = String::new();
        for chunk in entry.chunks {
            encoded.push_str(&chunk.ok_or("missing completed chunk")?);
        }
        decode_base64_prompt(&encoded).map(Some)
    }
}

fn decode_base64_prompt(encoded: &str) -> Result<String, String> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|e| format!("invalid base64 payload: {}", e))?;
    String::from_utf8(bytes).map_err(|e| format!("invalid utf-8 payload: {}", e))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalInputLoopOutcome {
    RetryReader,
    Stop,
}

fn open_codex_terminal_input_reader() -> Box<dyn BufRead> {
    match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
        Ok(tty) => Box::new(BufReader::new(tty)),
        Err(err) => {
            eprintln!("\x1b[90m[terminal input tty open failed: {}]\x1b[0m", err);
            Box::new(BufReader::new(std::io::stdin()))
        }
    }
}

fn read_codex_terminal_input_lines<R: BufRead>(
    mut reader: R,
    prompt_tx: &mpsc::Sender<CodexPrompt>,
) -> TerminalInputLoopOutcome {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return TerminalInputLoopOutcome::RetryReader,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                super::emit_status("[terminal message received]");
                if prompt_tx.send(CodexPrompt::terminal(trimmed)).is_err() {
                    return TerminalInputLoopOutcome::Stop;
                }
            }
            Err(err) => {
                eprintln!("\x1b[90m[terminal input read error: {}]\x1b[0m", err);
                return TerminalInputLoopOutcome::RetryReader;
            }
        }
    }
}

/// Where a queued prompt came from. Terminal input is a human typing into the
/// tmux pane; external input arrives over the FIFO (or stdin in pipe mode)
/// from the Discord relay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PromptOrigin {
    Terminal,
    External,
}

/// One queued prompt plus the origin that decides whether local controls apply.
pub(super) struct CodexPrompt {
    pub(super) origin: PromptOrigin,
    pub(super) text: String,
}

impl CodexPrompt {
    fn terminal(text: &str) -> Self {
        Self {
            origin: PromptOrigin::Terminal,
            text: text.to_string(),
        }
    }

    fn external(text: String) -> Self {
        Self {
            origin: PromptOrigin::External,
            text,
        }
    }
}

/// #5660: read-only view of this wrapper's launch configuration. These are the
/// same `run()` arguments `run_turn` receives, which is what keeps a reported
/// setting equal to the executed one instead of an invented guess.
#[derive(Clone, Copy)]
pub(super) struct ControlContext<'a> {
    pub(super) codex_model: Option<&'a str>,
    pub(super) reasoning_effort: Option<&'a str>,
}

/// What [`dispatch_prompt`] did with a prompt.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DispatchOutcome {
    RanTurn,
    /// Completed inside the wrapper. `status` is the exact text emitted to
    /// the pane, returned so the wiring is observable through this seam and
    /// not only by calling a renderer directly.
    HandledLocally {
        status: String,
    },
}

/// Reasoning effort a Codex turn actually runs with; today `run_turn` and the
/// `/model` report both compute it here, so their defaults agree by shared use,
/// not by construction. Explicit effort wins; `high` needs a pinned model.
pub(super) fn effective_reasoning_effort<'a>(
    reasoning_effort: Option<&'a str>,
    codex_model: Option<&str>,
) -> Option<&'a str> {
    let default_reasoning_effort = codex_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|_| "high");
    reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(default_reasoning_effort)
}

/// #5660: routes one queued prompt.
///
/// A terminal-origin local control completes here: `run_turn` is not called, no
/// JSONL line is written, and no `ReadyForInput` sentinel is added, so the turn
/// lifecycle never starts. External-origin input keeps today's behaviour on
/// purpose — the Discord intake upstream is already blocked waiting for a
/// terminal signal that only a provider turn produces (#5188), so swallowing it
/// here would wedge the channel. Moving that decision upstream is S3.
pub(super) fn dispatch_prompt<F>(
    prompt: CodexPrompt,
    ctx: ControlContext<'_>,
    run_turn: F,
) -> Result<DispatchOutcome, String>
where
    F: FnOnce(&str) -> Result<(), String>,
{
    let text = prompt.text.trim();
    let status = match (prompt.origin, classify_codex_input(text)) {
        (PromptOrigin::Terminal, CodexInputClass::LocalControl { name, args }) => {
            if name == "/model" {
                render_model_status(ctx, &args)
            } else if args.is_empty() {
                render_controls_notice(None)
            } else {
                let notice = render_controls_notice(None);
                format!("{notice}\n[인수 '{args}' 은 적용되지 않았습니다.]")
            }
        }
        (PromptOrigin::Terminal, CodexInputClass::UnsupportedControl { raw_name }) => {
            render_controls_notice(Some(&raw_name))
        }
        (PromptOrigin::External, class) => {
            if !matches!(class, CodexInputClass::ProviderPrompt) {
                super::emit_status("[external slash control forwarded to provider (#5660 S3)]");
            }
            run_turn(text)?;
            return Ok(DispatchOutcome::RanTurn);
        }
        (PromptOrigin::Terminal, CodexInputClass::ProviderPrompt) => {
            run_turn(text)?;
            return Ok(DispatchOutcome::RanTurn);
        }
    };
    super::emit_status(&status);
    Ok(DispatchOutcome::HandledLocally { status })
}

/// `/model` is a read-only report. Changing the model is out of scope for
/// #5660, so an argument is echoed back with an explicit "not applied" notice
/// rather than being silently ignored or answered with an invented name.
fn render_model_status(ctx: ControlContext<'_>, args: &str) -> String {
    let model = ctx
        .codex_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("(wrapper 미지정 — codex 기본값)");
    let effort =
        effective_reasoning_effort(ctx.reasoning_effort, ctx.codex_model).unwrap_or("(미지정)");
    let mut status = format!("[model: {model}]\n[reasoning effort: {effort}]");
    if !args.is_empty() {
        status.push_str(&format!(
            "\n[모델 변경은 지원하지 않습니다(읽기 전용). 요청한 값 '{args}' 은 적용되지 않았습니다.]"
        ));
    }
    status
}

/// Pane text for a control that produced no provider turn. `refused` carries
/// the raw spelling of a command-shaped token outside the registry, so the user
/// sees what they actually typed rather than a normalized guess.
fn render_controls_notice(refused: Option<&str>) -> String {
    let head = match refused {
        Some(raw_name) => format!("[지원하지 않는 명령입니다: {raw_name}]"),
        None => "[/model 은 현재 설정을 보고만 합니다(읽기 전용).]".to_string(),
    };
    let supported = CODEX_LOCAL_CONTROLS.join(", ");
    format!("{head}\n[지원 명령: {supported}]\n{PATH_RULE_NOTE}")
}

const PATH_RULE_NOTE: &str = "[앞쪽 영숫자 부분이 열거된 루트인 토큰(/tmp, /tmp에, /run-foo)과 \
하위 경로·점 토큰(/data/x, /a.log)은 프롬프트로 전달됩니다. 그 밖의 단일 세그먼트 /이름 은 위 \
지원 명령이 아니면 거부됩니다. 경로였다면 하위 경로를 붙이거나 문장 앞에 단어를 두세요.]";

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn ctx<'a>(model: Option<&'a str>, effort: Option<&'a str>) -> ControlContext<'a> {
        ControlContext {
            codex_model: model,
            reasoning_effort: effort,
        }
    }

    /// Runs one terminal-origin line through the seam and reports what the fake
    /// runner saw, so nothing below can assert a renderer it called directly.
    fn dispatch_terminal(text: &str, ctx: ControlContext<'_>) -> (DispatchOutcome, Vec<String>) {
        let mut ran: Vec<String> = Vec::new();
        let outcome = dispatch_prompt(CodexPrompt::terminal(text), ctx, |prompt| {
            ran.push(prompt.to_string());
            Ok(())
        })
        .expect("dispatch");
        (outcome, ran)
    }

    fn status_of(outcome: &DispatchOutcome) -> &str {
        match outcome {
            DispatchOutcome::HandledLocally { status } => status,
            DispatchOutcome::RanTurn => panic!("expected a locally handled control"),
        }
    }

    #[test]
    fn model_report_carries_the_settings_this_wrapper_was_launched_with() {
        let (outcome_a, ran_a) = dispatch_terminal("/model", ctx(Some("model-A"), Some("low")));
        let (outcome_b, ran_b) = dispatch_terminal("/model", ctx(Some("model-B"), Some("high")));
        let (status_a, status_b) = (status_of(&outcome_a), status_of(&outcome_b));
        assert!(
            status_a.contains("model-A") && status_a.contains("low"),
            "{status_a}"
        );
        assert!(
            status_b.contains("model-B") && status_b.contains("high"),
            "{status_b}"
        );
        assert_ne!(status_a, status_b);
        assert!(ran_a.is_empty() && ran_b.is_empty());
    }

    /// Every row of the display contract, including the one an explicit effort
    /// wins without a pinned model — the `.or(default)` order makes that `low`,
    /// not `(미지정)`.
    #[test]
    fn model_report_covers_every_row_of_the_display_contract() {
        for (model, effort, expect_model, expect_effort) in [
            (Some("m"), None, "m", "high"),
            (None, Some("low"), "(wrapper 미지정", "low"),
            (None, None, "(wrapper 미지정", "(미지정)"),
        ] {
            let outcome = dispatch_terminal("/model", ctx(model, effort)).0;
            let status = status_of(&outcome);
            assert!(status.contains(expect_model), "{status}");
            assert!(status.contains(expect_effort), "{status}");
        }
    }

    #[test]
    fn model_argument_is_echoed_as_not_applied_and_never_changes_the_run_model() {
        let (outcome, ran) = dispatch_terminal("/model gpt-5.6-codex", ctx(Some("model-A"), None));
        let status = status_of(&outcome);
        assert!(status.contains("gpt-5.6-codex"), "{status}");
        assert!(status.contains("적용되지 않았습니다"), "{status}");
        assert!(status.contains("model-A"), "{status}");
        assert!(ran.is_empty());
    }

    #[test]
    fn unsupported_terminal_commands_are_refused_without_reaching_the_provider() {
        let (outcome, ran) = dispatch_terminal("/모델", ctx(Some("model-A"), None));
        assert!(status_of(&outcome).contains("/모델"));
        assert!(ran.is_empty());
        let (help, ran_help) = dispatch_terminal("/help", ctx(Some("model-A"), None));
        assert!(status_of(&help).contains("/model"));
        assert!(ran_help.is_empty());
        let (help_args, _) = dispatch_terminal("/help 이거 어떻게 해", ctx(None, None));
        assert!(status_of(&help_args).contains("'이거 어떻게 해' 은 적용되지 않았습니다"));
    }

    #[test]
    fn external_origin_slash_controls_still_reach_the_provider() {
        let mut ran: Vec<String> = Vec::new();
        let outcome = dispatch_prompt(
            CodexPrompt::external("/model".to_string()),
            ctx(Some("model-A"), None),
            |prompt| {
                ran.push(prompt.to_string());
                Ok(())
            },
        )
        .expect("dispatch");
        assert_eq!(outcome, DispatchOutcome::RanTurn);
        assert_eq!(ran, vec!["/model".to_string()]);
    }

    #[test]
    fn terminal_reader_lines_route_through_dispatch_and_only_prompts_run_a_turn() {
        let (tx, rx) = mpsc::channel::<CodexPrompt>();
        let outcome = read_codex_terminal_input_lines(Cursor::new("/model\nhello\n"), &tx);
        assert_eq!(outcome, TerminalInputLoopOutcome::RetryReader);
        drop(tx);
        let mut ran: Vec<String> = Vec::new();
        for prompt in rx.iter() {
            dispatch_prompt(prompt, ctx(Some("model-A"), Some("low")), |text| {
                ran.push(text.to_string());
                Ok(())
            })
            .expect("dispatch");
        }
        assert_eq!(ran, vec!["hello".to_string()]);
    }

    #[test]
    fn effective_reasoning_effort_matches_what_a_turn_would_run_with() {
        assert_eq!(effective_reasoning_effort(None, Some("m")), Some("high"));
        assert_eq!(
            effective_reasoning_effort(Some("low"), Some("m")),
            Some("low")
        );
        assert_eq!(effective_reasoning_effort(Some("low"), None), Some("low"));
        assert_eq!(effective_reasoning_effort(None, None), None);
    }
}

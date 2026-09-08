//! Codex tmux wrapper input plumbing: terminal and external prompt readers.
//!
//! Extracted verbatim from the parent module so the wrapper's prompt intake can
//! grow test coverage without growing the giant file it came from.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::sync::mpsc;

use crate::services::tmux_wrapper::InputMode;

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
const TMUX_PROMPT_B64_CHUNK_PREFIX: &str = "__AGENTDESK_B64_CHUNK__:";

/// Terminal input — only in Fifo mode (interactive tmux session)
pub(super) fn spawn_terminal_input_reader(input_mode: InputMode, prompt_tx: &mpsc::Sender<String>) {
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
    prompt_tx: &mpsc::Sender<String>,
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
                        let _ = prompt_tx.send(prompt);
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
    prompt_tx: &mpsc::Sender<String>,
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
                if prompt_tx.send(trimmed.to_string()).is_err() {
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

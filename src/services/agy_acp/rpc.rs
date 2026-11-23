//! Line-delimited JSON-RPC 2.0 framing for the `agy acp` stdio pipe.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

/// Longer lines are dropped whole rather than buffered without bound.
pub(super) const MAX_LINE: usize = 16 << 20;

pub(super) const AUTH_REQUIRED: i64 = -32000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RpcError {
    pub code: i64,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum Inbound {
    Response {
        id: u64,
        outcome: Result<Value, RpcError>,
    },
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Non-JSON lines (auth URLs, banners) and unknown shapes are ignored, not relayed.
pub(super) fn parse(line: &[u8]) -> Option<Inbound> {
    let value: Value = serde_json::from_slice(line).ok()?;
    let method = value.get("method").and_then(Value::as_str);
    let params = || value.get("params").cloned().unwrap_or(Value::Null);
    match (value.get("id"), method) {
        (Some(id), Some(method)) => Some(Inbound::Request {
            id: id.clone(),
            method: method.to_owned(),
            params: params(),
        }),
        (None, Some(method)) => Some(Inbound::Notification {
            method: method.to_owned(),
            params: params(),
        }),
        (Some(id), None) => {
            let outcome = match value.get("error") {
                Some(error) => Err(RpcError {
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                None => Ok(value.get("result").cloned().unwrap_or(Value::Null)),
            };
            Some(Inbound::Response {
                id: id.as_u64()?,
                outcome,
            })
        }
        (None, None) => None,
    }
}

/// An auth refusal names the fix and never relays a login URL.
pub(super) fn rpc_message(error: &RpcError, during: &str) -> String {
    if error.code == AUTH_REQUIRED {
        return "agy 인증이 필요합니다. 이 컴퓨터에서 agy에 직접 로그인한 뒤 다시 보내 주세요."
            .into();
    }
    format!(
        "agy acp {during} failed ({}): {}",
        error.code, error.message
    )
}

pub(super) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

pub(super) fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

pub(super) fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

pub(super) fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

pub(super) fn write_line(writer: &mut impl Write, message: &Value) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(message)?;
    line.push(b'\n');
    writer.write_all(&line)?;
    writer.flush()
}

/// Calls `deliver` for each parsed line until EOF, a read error, or `deliver` returns false.
pub(super) fn read_lines(mut reader: impl BufRead, mut deliver: impl FnMut(Inbound) -> bool) {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let (consumed, ended) = match reader.fill_buf() {
            Ok([]) | Err(_) => return,
            Ok(chunk) => match chunk.iter().position(|byte| *byte == b'\n') {
                Some(at) => {
                    if !oversized {
                        line.extend_from_slice(&chunk[..at]);
                    }
                    (at + 1, true)
                }
                None => {
                    if !oversized {
                        line.extend_from_slice(chunk);
                    }
                    (chunk.len(), false)
                }
            },
        };
        reader.consume(consumed);
        if line.len() > MAX_LINE {
            line.clear();
            oversized = true;
        }
        if ended {
            let parsed = (!oversized).then(|| parse(&line)).flatten();
            line.clear();
            oversized = false;
            if let Some(inbound) = parsed
                && !deliver(inbound)
            {
                return;
            }
        }
    }
}

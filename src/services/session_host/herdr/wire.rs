//! Byte framing for the Herdr socket. The schema does not specify stream
//! framing, so it sits behind a trait; line-delimited JSON is an unverified guess.
#![cfg_attr(not(test), allow(dead_code))]

use std::fmt;
use std::io::{self, BufRead, Write};

use super::model::{HerdrCall, HerdrReply};

/// Largest reply frame read before the connection is dropped.
pub(crate) const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameError {
    /// The peer closed before sending any byte of the frame.
    Closed,
    /// The peer closed mid-frame after this many bytes.
    Partial(usize),
    Oversize(usize),
    Timeout,
    Io(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "closed before a reply"),
            Self::Partial(bytes) => write!(f, "closed after a partial frame of {bytes} bytes"),
            Self::Oversize(bytes) => write!(f, "frame over {bytes} bytes"),
            Self::Timeout => write!(f, "timed out waiting for a reply"),
            Self::Io(message) => write!(f, "{message}"),
        }
    }
}

fn io_error(error: io::Error) -> FrameError {
    match error.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => FrameError::Timeout,
        _ => FrameError::Io(error.to_string()),
    }
}

pub(crate) trait HerdrFraming: Send + Sync {
    fn encode(&self, call: &HerdrCall) -> Result<Vec<u8>, String>;
    fn read_frame(&self, reader: &mut dyn BufRead, max_bytes: usize)
    -> Result<Vec<u8>, FrameError>;
}

/// One JSON object per `\n`-terminated line.
pub(crate) struct LineJsonFraming;

impl HerdrFraming for LineJsonFraming {
    fn encode(&self, call: &HerdrCall) -> Result<Vec<u8>, String> {
        let mut bytes = serde_json::to_vec(call).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    fn read_frame(
        &self,
        reader: &mut dyn BufRead,
        max_bytes: usize,
    ) -> Result<Vec<u8>, FrameError> {
        let mut frame = Vec::new();
        loop {
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(io_error(error)),
            };
            if chunk.is_empty() {
                return Err(match frame.len() {
                    0 => FrameError::Closed,
                    bytes => FrameError::Partial(bytes),
                });
            }
            let newline = chunk.iter().position(|byte| *byte == b'\n');
            let take = newline.map_or(chunk.len(), |index| index + 1);
            if frame.len() + take > max_bytes + 1 {
                return Err(FrameError::Oversize(max_bytes));
            }
            frame.extend_from_slice(&chunk[..take]);
            reader.consume(take);
            if newline.is_some() {
                frame.pop();
                return Ok(frame);
            }
        }
    }
}

/// A write that failed after `written` bytes; any byte out means the server may act.
#[derive(Debug)]
pub(crate) struct WriteFailure {
    pub written: usize,
    pub error: io::Error,
}

pub(crate) fn write_frame(writer: &mut dyn Write, frame: &[u8]) -> Result<(), WriteFailure> {
    let mut written = 0;
    while written < frame.len() {
        match writer.write(&frame[written..]) {
            Ok(0) => {
                let error = io::Error::new(io::ErrorKind::WriteZero, "socket accepted 0 bytes");
                return Err(WriteFailure { written, error });
            }
            Ok(bytes) => written += bytes,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(WriteFailure { written, error }),
        }
    }
    writer
        .flush()
        .map_err(|error| WriteFailure { written, error })
}

pub(crate) fn decode_reply(frame: &[u8]) -> Result<HerdrReply, String> {
    serde_json::from_slice(frame).map_err(|error| format!("malformed reply: {error}"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use serde_json::{Map, Value, json};

    use super::*;
    use crate::services::session_host::herdr::contract::capture_request;
    use crate::services::session_host::herdr::model::{
        HERDR_PROTOCOL, HerdrReadSource, HerdrRequest, HerdrResult,
    };

    fn frame(bytes: &[u8], max: usize) -> Result<Vec<u8>, FrameError> {
        LineJsonFraming.read_frame(&mut Cursor::new(bytes.to_vec()), max)
    }

    #[test]
    fn herdr_line_frame_bounds_and_partial_frames() {
        assert_eq!(frame(b"abcd\nnext", 4), Ok(b"abcd".to_vec()));
        assert_eq!(frame(b"abcde\n", 4), Err(FrameError::Oversize(4)));
        assert_eq!(frame(b"abcdefgh", 4), Err(FrameError::Oversize(4)));
        assert_eq!(frame(b"", 4), Err(FrameError::Closed));
        assert_eq!(
            frame(b"{\"id\"", 64),
            Err(FrameError::Partial(5)),
            "a frame cut off by EOF is never a reply"
        );
    }

    struct Choke(usize);

    impl Write for Choke {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            match self.0 {
                0 => Err(io::Error::new(io::ErrorKind::WouldBlock, "full")),
                room => {
                    let taken = room.min(bytes.len());
                    self.0 -= taken;
                    Ok(taken)
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn herdr_write_failure_reports_bytes_already_sent() {
        let failure = write_frame(&mut Choke(3), b"0123456789").unwrap_err();
        assert_eq!(
            failure.written, 3,
            "bytes out before the failure decide NotSent"
        );
        let failure = write_frame(&mut Choke(0), b"0123456789").unwrap_err();
        assert_eq!(failure.written, 0);
        assert!(write_frame(&mut Choke(10), b"0123456789").is_ok());
    }

    fn schema(file: &str) -> Value {
        let path = format!("{}/tests/fixtures/herdr/{file}", env!("CARGO_MANIFEST_DIR"));
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn resolve<'a>(root: &'a Value, node: &'a Value) -> &'a Value {
        match node.get("$ref").and_then(Value::as_str) {
            Some(pointer) => resolve(root, root.pointer(&pointer[1..]).unwrap()),
            None => node,
        }
    }

    /// Smallest value the schema accepts: required fields only, one item per array.
    fn minimal(root: &Value, node: &Value, depth: usize) -> Value {
        let node = resolve(root, node);
        assert!(depth < 12, "schema recursion");
        if let Some(value) = node.get("const") {
            return value.clone();
        }
        if let Some(values) = node.get("enum") {
            return values[0].clone();
        }
        if let Some(first) = ["anyOf", "oneOf"].iter().find_map(|k| node.get(*k)) {
            return minimal(root, &first[0], depth + 1);
        }
        let kind = match &node["type"] {
            Value::Array(kinds) => kinds[0].as_str().unwrap_or("null"),
            kind => kind.as_str().unwrap_or("object"),
        };
        match kind {
            "string" => json!("x"),
            "integer" => node.get("minimum").cloned().unwrap_or(json!(0)),
            "boolean" => json!(false),
            "array" => json!([minimal(root, &node["items"], depth + 1)]),
            "object" => {
                let mut object = Map::new();
                for key in node["required"].as_array().into_iter().flatten() {
                    let key = key.as_str().unwrap();
                    let field = minimal(root, &node["properties"][key], depth + 1);
                    object.insert(key.to_string(), field);
                }
                Value::Object(object)
            }
            _ => Value::Null,
        }
    }

    fn result_branch<'a>(root: &'a Value, tag: &str) -> &'a Value {
        let results = &root["schemas"]["success_response"]["$defs"]["ResponseResult"]["oneOf"];
        let branches = results.as_array().unwrap().iter();
        branches
            .filter(|branch| branch["properties"]["type"]["const"] == tag)
            .next()
            .unwrap_or_else(|| panic!("schema has no result {tag}"))
    }

    /// The requests a pre-0.9.3 server already understood; observation reads only.
    fn observation_requests() -> Vec<HerdrRequest> {
        vec![
            HerdrRequest::Ping {},
            HerdrRequest::SessionSnapshot {},
            HerdrRequest::PaneGet {
                pane_id: "p".into(),
            },
            HerdrRequest::PaneProcessInfo {
                pane_id: "p".into(),
            },
            capture_request("p", -3),
            capture_request("p", 0),
            HerdrRequest::PaneSendText {
                pane_id: "p".into(),
                text: "t".into(),
            },
        ]
    }

    // Pins the vendored schemas: every request we send is valid there, and the
    // smallest schema-valid reply of each result we read still decodes.
    #[test]
    fn herdr_wire_matches_the_vendored_schema() {
        let launch = vec![
            HerdrRequest::PaneSendKeys {
                pane_id: "p".into(),
                keys: vec!["enter".into()],
            },
            HerdrRequest::PaneSendInput {
                pane_id: "p".into(),
                text: "t".into(),
                keys: vec!["enter".into()],
            },
            HerdrRequest::WorkspaceCreate {
                cwd: "/w".into(),
                label: "l".into(),
                focus: false,
            },
        ];
        let stage2 = schema("stage2-herdr-schema.json");
        check_schema(&stage2, observation_requests(), &[]);
        let current = schema("herdr-schema-0.9.3.json");
        let requests = observation_requests().into_iter().chain(launch).collect();
        check_schema(&current, requests, &["workspace_created"]);
    }

    fn check_schema(root: &Value, sent: Vec<HerdrRequest>, results: &[&str]) {
        let root = root.clone();
        assert_eq!(root["protocol"], json!(HERDR_PROTOCOL));
        assert_eq!(root["schema_version"], json!(1));
        let requests = &root["schemas"]["request"]["oneOf"];
        for request in sent {
            let call = serde_json::to_value(HerdrCall {
                id: "i".into(),
                request,
            })
            .unwrap();
            let method = &call["method"];
            let branch = requests.as_array().unwrap().iter();
            let branch = branch
                .filter(|branch| &branch["properties"]["method"]["const"] == method)
                .next()
                .unwrap_or_else(|| panic!("schema has no method {method}"));
            let params = resolve(&root, &branch["properties"]["params"]);
            let sent = call["params"].as_object().unwrap();
            for key in params["required"].as_array().into_iter().flatten() {
                assert!(
                    sent.contains_key(key.as_str().unwrap()),
                    "{method} lacks {key}"
                );
            }
            for (key, value) in sent {
                let field = resolve(&root, &params["properties"][key]);
                assert!(!field.is_null(), "{method} sends unknown param {key}");
                if let Some(allowed) = field.get("enum").and_then(Value::as_array) {
                    assert!(allowed.contains(value), "{method}.{key}={value}");
                }
            }
        }
        let read = [
            "pong",
            "session_snapshot",
            "pane_info",
            "pane_process_info",
            "pane_read",
            "ok",
        ];
        for tag in read.iter().chain(results) {
            let result = minimal(&root, result_branch(&root, tag), 0);
            let reply = json!({"id": "i", "result": result});
            let decoded = decode_reply(reply.to_string().as_bytes())
                .unwrap_or_else(|error| panic!("minimal {tag}: {error}"));
            assert_ne!(
                decoded.body,
                Ok(HerdrResult::Other),
                "{tag} must be modeled"
            );
        }
        let error = minimal(&root, &root["schemas"]["error_response"], 0);
        assert!(
            decode_reply(error.to_string().as_bytes())
                .unwrap()
                .body
                .is_err()
        );
        let sources = &root["schemas"]["request"]["$defs"]["ReadSource"]["enum"];
        for source in [HerdrReadSource::Visible, HerdrReadSource::RecentUnwrapped] {
            let source = serde_json::to_value(source).unwrap();
            assert!(sources.as_array().unwrap().contains(&source));
        }
    }
}

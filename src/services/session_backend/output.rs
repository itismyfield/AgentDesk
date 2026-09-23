//! Bounded output from the file already owned by a native process session.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use super::{ProcessBackend, SessionBackend, SessionHandle, process_sessions};

const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

pub(crate) struct ProcessOutput {
    pub text: String,
    pub alive: bool,
    pub truncated: bool,
}

pub(crate) fn capture_process_output(
    session_name: &str,
    lines: usize,
) -> Option<Result<ProcessOutput, &'static str>> {
    let (file, alive) = {
        let registry = process_sessions();
        let entry = registry.handles.get(session_name)?;
        match &entry.handle {
            SessionHandle::Process { output, .. } => (
                output.clone(),
                ProcessBackend::new().is_alive(&entry.handle),
            ),
            #[cfg(test)]
            SessionHandle::TestProcess { .. } => return Some(Err("process_output_not_bound")),
        }
    };
    // Do not hold the process registry lock during filesystem I/O.
    Some((|| {
        let mut file = file.lock().map_err(|_| "process_output_lock_failed")?;
        let (text, truncated) = tail(&mut file, lines).map_err(|_| "process_output_read_failed")?;
        Ok(ProcessOutput {
            text,
            alive,
            truncated,
        })
    })())
}

fn tail(file: &mut File, lines: usize) -> std::io::Result<(String, bool)> {
    let size = file.metadata()?.len();
    let start = size.saturating_sub(MAX_OUTPUT_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity((size - start) as usize);
    file.take(size - start).read_to_end(&mut bytes)?;
    // A byte-limited capture begins inside a line (possibly inside UTF-8).
    // Discard that partial record before decoding the complete suffix.
    let bytes = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map(|end| &bytes[end + 1..])
            .unwrap_or(&[])
    } else {
        &bytes
    };
    let text = String::from_utf8_lossy(bytes);
    let all: Vec<_> = text.lines().collect();
    let offset = all.len().saturating_sub(lines.clamp(1, 2000));
    Ok((all[offset..].join("\n"), start > 0 || offset > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn process_output_is_bounded_and_keeps_complete_utf8_lines() {
        let mut file = tempfile::tempfile().unwrap();
        write!(file, "{}\n첫째\n둘째\n셋째\n", "가".repeat(100_000)).unwrap();
        assert_eq!(tail(&mut file, 2).unwrap(), ("둘째\n셋째".into(), true));
        let (text, truncated) = tail(&mut file, 2000).unwrap();
        assert_eq!(text, "첫째\n둘째\n셋째");
        assert!(truncated);
        file.set_len(0).unwrap();
        assert_eq!(tail(&mut file, 80).unwrap(), (String::new(), false));
    }

    #[test]
    fn process_output_uses_bound_file_and_stops_with_native_session() {
        use std::process::{Command, Stdio};
        use std::sync::{Arc, Mutex};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("output.jsonl");
        std::fs::write(&path, "{\"text\":\"original\"}\n").unwrap();
        let file = File::open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("old.jsonl")).unwrap();
        std::fs::write(&path, "replacement must not be read").unwrap();
        #[cfg(windows)]
        let mut command = {
            let mut c = Command::new("cmd.exe");
            c.args(["/d", "/q", "/c", "more"]);
            c
        };
        #[cfg(unix)]
        let mut command = Command::new("cat");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let name = format!("output-fixture-{}", uuid::Uuid::new_v4());
        let handle = SessionHandle::Process {
            pid: child.id(),
            child_stdin: Arc::new(Mutex::new(child.stdin.take())),
            child: Arc::new(Mutex::new(Some(child))),
            output: Arc::new(Mutex::new(file)),
        };
        super::super::insert_process_session(&name, handle);
        let snapshot = capture_process_output(&name, 10).unwrap().unwrap();
        let stopped = super::super::terminate_process_session(&name);
        assert_eq!(snapshot.text, "{\"text\":\"original\"}");
        assert!(snapshot.alive);
        assert!(!snapshot.truncated);
        assert!(stopped);
        assert!(capture_process_output(&name, 10).is_none());
    }
}

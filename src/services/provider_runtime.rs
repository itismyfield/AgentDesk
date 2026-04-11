use std::io::{BufRead, BufReader, Read};
use std::sync::mpsc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LineStreamEvent {
    Line(String),
    ReadError(String),
    Eof,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SharedAllowedToolKind {
    Bash,
    Read,
    Edit,
    Write,
    Glob,
    Grep,
    Task,
    WebFetch,
    WebSearch,
    Skill,
    AskUserQuestion,
    PlanMode,
}

pub(crate) fn spawn_line_stream_reader<R>(
    reader: R,
    provider_label: &'static str,
) -> mpsc::Receiver<LineStreamEvent>
where
    R: Read + Send + 'static,
{
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(line) => {
                    if tx.send(LineStreamEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(LineStreamEvent::ReadError(format!(
                        "Failed reading {provider_label} output: {error}"
                    )));
                    return;
                }
            }
        }
        let _ = tx.send(LineStreamEvent::Eof);
    });
    rx
}

pub(crate) fn resolve_shared_allowed_tool_compat(tool: &str) -> Option<SharedAllowedToolKind> {
    match tool.trim() {
        "Bash" => Some(SharedAllowedToolKind::Bash),
        "Read" => Some(SharedAllowedToolKind::Read),
        "Edit" | "NotebookEdit" => Some(SharedAllowedToolKind::Edit),
        "Write" => Some(SharedAllowedToolKind::Write),
        "Glob" => Some(SharedAllowedToolKind::Glob),
        "Grep" => Some(SharedAllowedToolKind::Grep),
        "Task" | "TaskCreate" | "TaskGet" | "TaskUpdate" | "TaskList" | "TaskOutput"
        | "TaskStop" => Some(SharedAllowedToolKind::Task),
        "WebFetch" => Some(SharedAllowedToolKind::WebFetch),
        "WebSearch" => Some(SharedAllowedToolKind::WebSearch),
        "Skill" => Some(SharedAllowedToolKind::Skill),
        "AskUserQuestion" => Some(SharedAllowedToolKind::AskUserQuestion),
        "EnterPlanMode" | "ExitPlanMode" => Some(SharedAllowedToolKind::PlanMode),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LineStreamEvent, SharedAllowedToolKind, resolve_shared_allowed_tool_compat,
        spawn_line_stream_reader,
    };
    use std::io::Cursor;

    #[test]
    fn spawn_line_stream_reader_emits_lines_then_eof() {
        let rx = spawn_line_stream_reader(Cursor::new("first\nsecond\n"), "TestProvider");

        assert_eq!(
            rx.recv().unwrap(),
            LineStreamEvent::Line("first".to_string())
        );
        assert_eq!(
            rx.recv().unwrap(),
            LineStreamEvent::Line("second".to_string())
        );
        assert_eq!(rx.recv().unwrap(), LineStreamEvent::Eof);
    }

    #[test]
    fn resolve_shared_allowed_tool_compat_collapses_aliases() {
        assert_eq!(
            resolve_shared_allowed_tool_compat("TaskOutput"),
            Some(SharedAllowedToolKind::Task)
        );
        assert_eq!(
            resolve_shared_allowed_tool_compat("TaskCreate"),
            Some(SharedAllowedToolKind::Task)
        );
        assert_eq!(
            resolve_shared_allowed_tool_compat("NotebookEdit"),
            Some(SharedAllowedToolKind::Edit)
        );
        assert_eq!(
            resolve_shared_allowed_tool_compat("EnterPlanMode"),
            Some(SharedAllowedToolKind::PlanMode)
        );
        assert_eq!(
            resolve_shared_allowed_tool_compat("AskUserQuestion"),
            Some(SharedAllowedToolKind::AskUserQuestion)
        );
        assert_eq!(resolve_shared_allowed_tool_compat("Unknown"), None);
    }
}

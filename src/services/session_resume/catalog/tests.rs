use super::*;
use filetime::{FileTime, set_file_mtime};
use std::io::{Cursor, Write};

pub(in crate::services::session_resume) struct Fixture {
    pub(in crate::services::session_resume) root: tempfile::TempDir,
    pub(in crate::services::session_resume) home: PathBuf,
    pub(in crate::services::session_resume) current: PathBuf,
    pub(in crate::services::session_resume) sibling: PathBuf,
}
impl Fixture {
    pub(in crate::services::session_resume) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let current = root.path().join("claude-adk-cc-20260101-000001");
        let sibling = root.path().join("claude-adk-cc-20260101-000002");
        fs::create_dir_all(&current).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        Self {
            root,
            home,
            current,
            sibling,
        }
    }
    pub(in crate::services::session_resume) fn transcript(
        &self,
        cwd: &Path,
        id: &str,
        contents: &str,
        mtime: i64,
    ) -> PathBuf {
        let dir = claude_project_dir_candidates_for_cwd(cwd, Some(&self.home))
            .unwrap()
            .remove(0);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, contents).unwrap();
        set_file_mtime(&path, FileTime::from_unix_time(mtime, 0)).unwrap();
        path
    }
    pub(in crate::services::session_resume) fn choices(
        &self,
        current: Option<&str>,
        live: &HashSet<String>,
        partial: &str,
    ) -> Vec<ResumeChoice> {
        choices(
            self.current.to_str().unwrap(),
            current,
            live,
            Some(&self.home),
            partial,
        )
    }
}
pub(in crate::services::session_resume) const PRIOR: &str = "22222222-2222-2222-2222-222222222222";
const OTHER: &str = "33333333-3333-3333-3333-333333333333";

#[test]
fn choices_show_sibling_title_time_and_resolve_selected_cwd_without_recorded_cwd() {
    let f = Fixture::new();
    f.transcript(
        &f.sibling,
        PRIOR,
        "{\"type\":\"ai-title\",\"aiTitle\":\"이전 작업\"}\n",
        2000,
    );
    let choices = f.choices(None, &HashSet::new(), "이전");
    assert_eq!(choices.len(), 1);
    assert_eq!(choices[0].session_id, PRIOR);
    assert!(choices[0].label.contains("이전 작업 · "));
    assert!(
        choices[0]
            .label
            .contains("일 전 · claude-adk-cc-20260101-000002")
    );
    let resolved = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        None,
        &choices[0].session_id,
        Some(&f.home),
    )
    .unwrap();
    assert_eq!(resolved, f.sibling.to_string_lossy());
}

#[test]
fn choices_exclude_current_live_bound_and_other_agent_and_sdk() {
    let f = Fixture::new();
    let other_agent = f.root.path().join("claude-adk-dash-cc-20260101-000001");
    fs::create_dir_all(&other_agent).unwrap();
    f.transcript(&f.current, PRIOR, "{}\n", 1000);
    f.transcript(&f.sibling, OTHER, "{}\n", 2000);
    f.transcript(
        &other_agent,
        "44444444-4444-4444-4444-444444444444",
        "{}\n",
        3000,
    );
    f.transcript(
        &f.current,
        "55555555-5555-5555-5555-555555555555",
        "{\"entrypoint\":\"sdk-cli\"}\n",
        4000,
    );
    assert!(
        f.choices(Some(PRIOR), &HashSet::from([OTHER.to_string()]), "")
            .is_empty()
    );
}

#[test]
fn explicit_wrong_cwd_returns_422_and_actual_location_missing_transcript_refuses() {
    let f = Fixture::new();
    f.transcript(&f.sibling, PRIOR, "{}\n", 1000);
    let error = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        Some(f.current.to_str().unwrap()),
        PRIOR,
        Some(&f.home),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = error.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains(f.sibling.to_str().unwrap())
    );
    let missing = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        None,
        OTHER,
        Some(&f.home),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = missing.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("No transcript"));
    assert!(body["error"].as_str().unwrap().contains("config home"));
    let invalid = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        None,
        "bad-id",
        Some(&f.home),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = invalid.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Invalid Claude session_id UUID")
    );
}

#[test]
fn forks_choose_newest_mtime_explicit_cwd_wins_and_choices_deduplicate() {
    let f = Fixture::new();
    f.transcript(&f.current, PRIOR, "{}\n", 1000);
    f.transcript(&f.sibling, PRIOR, "{}\n", 2000);
    let current = f.current.to_str().unwrap();
    assert_eq!(
        resolve_cwd(Some(current), None, PRIOR, Some(&f.home)).unwrap(),
        f.sibling.to_string_lossy()
    );
    assert_eq!(
        resolve_cwd(Some(current), Some(current), PRIOR, Some(&f.home)).unwrap(),
        current
    );
    assert_eq!(f.choices(None, &HashSet::new(), "").len(), 1);
}

#[test]
fn preview_caps_reads_at_128k_with_an_oversized_record_and_uses_latest_title() {
    struct Meter {
        cursor: Cursor<Vec<u8>>,
        read: usize,
    }
    impl Read for Meter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.cursor.read(buf)?;
            self.read += n;
            Ok(n)
        }
    }
    impl Seek for Meter {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.cursor.seek(pos)
        }
    }
    let mut bytes =
        b"{\"entrypoint\":\"cli\"}\n{\"type\":\"ai-title\",\"aiTitle\":\"old\"}\n".to_vec();
    bytes.resize(8 * WINDOW, b'x');
    bytes.extend_from_slice(b"\n{\"type\":\"ai-title\",\"aiTitle\":\"new\"}\n");
    let mut meter = Meter {
        cursor: Cursor::new(bytes),
        read: 0,
    };
    let preview = read_preview(&mut meter).unwrap();
    assert_eq!(preview.title, "new");
    assert!(preview.tui);
    assert_eq!(meter.read, 2 * WINDOW);
}

#[test]
fn sparse_207mb_autocomplete_finishes_under_three_seconds_and_cache_invalidates() {
    let f = Fixture::new();
    let path = f.transcript(&f.sibling, PRIOR, "{\"entrypoint\":\"cli\"}\n", 1000);
    let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_len(207 * 1024 * 1024).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    writeln!(
        file,
        "\n{{\"type\":\"ai-title\",\"aiTitle\":\"대용량 작업\"}}"
    )
    .unwrap();
    let start = Instant::now();
    let first = f.choices(None, &HashSet::new(), "");
    let elapsed = start.elapsed();
    assert_eq!(first.len(), 1);
    assert!(first[0].label.contains("대용량 작업"));
    assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
    let cached = Instant::now();
    assert_eq!(f.choices(None, &HashSet::new(), "").len(), 1);
    eprintln!(
        "207MiB autocomplete cold={elapsed:?} cached={:?} read_cap={} bytes/file",
        cached.elapsed(),
        2 * WINDOW
    );
    writeln!(file, "{{\"type\":\"ai-title\",\"aiTitle\":\"changed\"}}").unwrap();
    assert!(
        f.choices(None, &HashSet::new(), "")[0]
            .label
            .contains("changed")
    );
}

#[test]
fn prompt_fallback_supports_text_blocks_and_unicode_choice_limits() {
    let f = Fixture::new();
    f.transcript(&f.current, PRIOR, &serde_json::json!({"type":"user", "message":{"content":[{"type":"text","text":"한글".repeat(100)}]}}).to_string(), 1000);
    let list = f.choices(None, &HashSet::new(), "");
    assert_eq!(list.len(), 1);
    assert!(list[0].label.starts_with(&"한글".repeat(20)));
    assert!(list[0].label.chars().count() <= 100);
    for i in 1..31 {
        f.transcript(
            &f.current,
            &uuid::Uuid::from_u128(i).to_string(),
            "{}\n",
            i as i64,
        );
    }
    assert_eq!(f.choices(None, &HashSet::new(), "").len(), 25);
}

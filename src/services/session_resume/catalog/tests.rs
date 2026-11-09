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
        self.transcript_in(&self.home, cwd, id, contents, mtime)
    }
    fn transcript_in(
        &self,
        home: &Path,
        cwd: &Path,
        id: &str,
        contents: &str,
        mtime: i64,
    ) -> PathBuf {
        let dir = claude_project_dir_candidates_for_cwd(cwd, Some(home))
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
            &[],
            Ok(None),
            partial,
            Instant::now() + Duration::from_secs(2),
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
        &[],
        Ok(None),
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
        &[],
        Ok(None),
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
        &[],
        Ok(None),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = missing.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("세션 33333333 트랜스크립트를 찾지 못함(검색 범위 2개)"),
        "{error}"
    );
    assert!(error.contains("config home"), "{error}");
    let invalid = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        None,
        "bad-id",
        Some(&f.home),
        &[],
        Ok(None),
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
    f.transcript(
        &f.current,
        PRIOR,
        "{\"type\":\"ai-title\",\"aiTitle\":\"older fork\"}\n",
        1000,
    );
    f.transcript(
        &f.sibling,
        PRIOR,
        "{\"type\":\"ai-title\",\"aiTitle\":\"newer fork\"}\n",
        2000,
    );
    let current = f.current.to_str().unwrap();
    assert_eq!(
        resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &[], Ok(None)).unwrap(),
        f.sibling.to_string_lossy()
    );
    assert_eq!(
        resolve_cwd(
            Some(current),
            Some(current),
            PRIOR,
            Some(&f.home),
            &[],
            Ok(None)
        )
        .unwrap(),
        current
    );
    assert_eq!(f.choices(None, &HashSet::new(), "").len(), 1);
    assert!(f.choices(None, &HashSet::new(), "older fork").is_empty());
    assert_eq!(f.choices(None, &HashSet::new(), "newer fork").len(), 1);
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
            let snapshot = self.cursor.seek(pos)?;
            if matches!(pos, SeekFrom::End(0)) {
                let grown_len = self.cursor.get_ref().len() + 2 * WINDOW;
                self.cursor.get_mut().resize(grown_len, b'x');
            }
            Ok(snapshot)
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
    assert_eq!(meter.read, 128 * 1024);
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
    assert_eq!(f.choices(None, &HashSet::new(), "").len(), 31);
}

#[test]
fn short_discovery_deadline_preserves_partial_results() {
    let f = Fixture::new();
    for n in 1..31 {
        f.transcript(
            &f.current,
            &uuid::Uuid::from_u128(n).to_string(),
            "{}\n",
            n as i64,
        );
    }
    let _delay = delay_previews_for_tests(f.home.clone(), Duration::from_millis(30));
    let start = Instant::now();
    let result = choices(
        f.current.to_str().unwrap(),
        None,
        &HashSet::new(),
        Some(&f.home),
        &[],
        Ok(None),
        "",
        start + Duration::from_millis(120),
    );
    assert!(
        !result.is_empty() && result.len() < 25,
        "partial result count={}",
        result.len()
    );
    assert!(start.elapsed() < Duration::from_millis(500));
}

#[test]
fn sibling_deadline_is_checked_before_filtering_unreadable_entries() {
    let visited = std::cell::Cell::new(0);
    let entries = (0..1000).map(|_| {
        visited.set(visited.get() + 1);
        Err(std::io::Error::other("unreadable sibling"))
    });
    let found =
        scoped_worktree_entries(Path::new("/work/current"), entries, Some(Instant::now())).count();
    assert_eq!(found, 0);
    assert!(
        visited.get() <= 1,
        "expired discovery scanned {} entries before checking its deadline",
        visited.get()
    );
}

#[test]
fn transcript_only_in_another_home_is_listed_then_copied_once_into_the_same_cwd_key() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let title = "{\"type\":\"ai-title\",\"aiTitle\":\"스왑 작업\"}\n";
    let source = f.transcript_in(&other, &f.sibling, PRIOR, title, 2000);
    let current = f.current.to_str().unwrap();
    let homes = [other.clone()];
    let listed = choices(
        current,
        None,
        &HashSet::new(),
        Some(&f.home),
        &homes,
        Ok(None),
        "스왑",
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0]
            .label
            .ends_with("claude-adk-cc-20260101-000002 · home 1-acct")
    );
    // Without declared homes only the active home is searched.
    assert!(resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &[], Ok(None)).is_err());
    assert_eq!(
        resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &homes, Ok(None)).unwrap(),
        f.sibling.to_string_lossy()
    );
    let copy = claude_transcript_path(&f.sibling, PRIOR, Some(&f.home)).unwrap();
    assert_eq!(fs::read_to_string(&copy).unwrap(), title);
    assert_eq!(fs::read_to_string(&source).unwrap(), title);
    assert!(
        !claude_transcript_path(&f.current, PRIOR, Some(&f.home))
            .unwrap()
            .exists()
    );
    // Once the active home holds a copy, a changed source is never copied again; a newer one refuses.
    fs::write(&source, "{}\n").unwrap();
    let error = resolve_cwd(
        Some(current),
        Some(f.sibling.to_str().unwrap()),
        PRIOR,
        Some(&f.home),
        &homes,
        Ok(None),
    )
    .unwrap_err();
    assert!(format!("{error:?}").contains("bytes"), "{error:?}");
    assert_eq!(fs::read_to_string(&copy).unwrap(), title);
}

#[test]
fn transcript_in_two_other_homes_is_refused_without_choosing_one() {
    let f = Fixture::new();
    let homes = [f.root.path().join("1-acct"), f.root.path().join("2-acct")];
    f.transcript_in(&homes[0], &f.current, PRIOR, "{}\n", 1000);
    f.transcript_in(&homes[1], &f.sibling, PRIOR, "{}\n", 2000);
    let error = resolve_cwd(
        Some(f.current.to_str().unwrap()),
        None,
        PRIOR,
        Some(&f.home),
        &homes,
        Ok(None),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = error.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("Claude config home 2곳")
    );
    assert!(!f.home.exists());
}

#[test]
fn failed_copy_refuses_and_leaves_no_partial_transcript_in_the_active_home() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    f.transcript_in(&other, &f.current, PRIOR, "{}\n", 1000);
    // A directory occupying the target name makes the final publish fail at the resume boundary.
    let target = claude_transcript_path(&f.current, PRIOR, Some(&f.home)).unwrap();
    fs::create_dir_all(&target).unwrap();
    let current = f.current.to_str().unwrap();
    let error = resolve_cwd(
        Some(current),
        None,
        PRIOR,
        Some(&f.home),
        std::slice::from_ref(&other),
        Ok(None),
    )
    .unwrap_err();
    let (status, axum::Json(body)) = error.into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("복사하지 못함"));
    let entries = |dir: &Path| fs::read_dir(dir).map_or(0, |dir| dir.count());
    assert_eq!(entries(target.parent().unwrap()), 1);
    // A read that fails mid-copy leaves neither the target nor a temp file behind.
    struct Broken;
    impl Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk gone"))
        }
    }
    let fresh = f
        .home
        .join("projects")
        .join("fresh")
        .join(format!("{PRIOR}.jsonl"));
    let input = Cursor::new(b"{\"type\":\"user\"}\n".to_vec()).chain(Broken);
    assert!(copy_without_partial(input, SystemTime::UNIX_EPOCH, &fresh).is_err());
    assert_eq!(entries(fresh.parent().unwrap()), 0);
}

#[test]
fn copy_is_refused_before_writing_when_the_launch_home_differs_or_a_candidate_is_unreadable() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let source = f.transcript_in(&other, &f.sibling, PRIOR, "{}\n", 1000);
    let profile = f.root.path().join("profile");
    // A home path the OS cannot stat stands in for an unreadable candidate.
    let both = [other.clone(), f.root.path().join("bad\0home")];
    let mismatch = format!(
        "복사 대상 {}와 다음 실행 Claude home {}",
        f.home.display(),
        profile.display()
    );
    type Case<'a> = (&'a [PathBuf], Result<Option<&'a Path>, &'a str>, &'a str);
    let cases: [Case; 3] = [
        (std::slice::from_ref(&other), Ok(Some(&profile)), &mismatch),
        (
            std::slice::from_ref(&other),
            Err("claude auth fallback chooses the account at launch"),
            "다음 실행 Claude home을 확정하지 못함",
        ),
        (&both, Ok(None), "확인하지 못해 복사하지 않음"),
    ];
    for (homes, launch, expected) in cases {
        let current = f.current.to_str().unwrap();
        let error = resolve_cwd(Some(current), None, PRIOR, Some(&f.home), homes, launch);
        let (status, axum::Json(body)) = error.unwrap_err().into_response();
        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert!(body["error"].as_str().unwrap().contains(expected), "{body}");
        assert!(
            !f.home.exists(),
            "{expected}: nothing is written to the active home"
        );
        assert_eq!(fs::read_to_string(&source).unwrap(), "{}\n");
    }
}

#[test]
fn an_id_the_active_home_holds_is_listed_and_resumed_from_the_active_home_only() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let title = |name: &str| format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{name}\"}}\n");
    f.transcript(&f.current, PRIOR, &title("활성"), 1000);
    f.transcript_in(&other, &f.sibling, PRIOR, &title("다른 home"), 2000);
    let homes = [other.clone()];
    let current = f.current.to_str().unwrap();
    let listed = choices(
        current,
        None,
        &HashSet::new(),
        Some(&f.home),
        &homes,
        Ok(None),
        "",
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].label.starts_with("활성 · "),
        "{}",
        listed[0].label
    );
    assert!(!listed[0].label.contains(" · home "), "{}", listed[0].label);
    let resolved = resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &homes, Ok(None));
    assert_eq!(resolved.unwrap(), current);
    assert!(
        !claude_transcript_path(&f.sibling, PRIOR, Some(&f.home))
            .unwrap()
            .exists()
    );
}

#[test]
fn an_older_active_copy_of_another_size_is_refused_without_overwriting() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let active = f.transcript(&f.current, PRIOR, "{}\n", 1000);
    f.transcript_in(&other, &f.current, PRIOR, "{}\n{\"type\":\"user\"}\n", 2000);
    let current = f.current.to_str().unwrap();
    let homes = [other.clone()];
    let error = resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &homes, Ok(None));
    let (status, axum::Json(body)) = error.unwrap_err().into_response();
    assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
    let error = body["error"].as_str().unwrap();
    assert!(
        error.contains("3 bytes") && error.contains("19 bytes"),
        "{error}"
    );
    assert_eq!(fs::read_to_string(&active).unwrap(), "{}\n");
    // The active copy continued after the adoption is newer, so it resumes.
    set_file_mtime(&active, FileTime::from_unix_time(3000, 0)).unwrap();
    let resumed = resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &homes, Ok(None));
    assert_eq!(resumed.unwrap(), current);
}

#[test]
fn subagent_files_are_copied_with_the_transcript_or_not_at_all() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let source = f.transcript_in(&other, &f.current, PRIOR, "{}\n", 1000);
    let subagents = |transcript: &Path| transcript.with_extension("").join("subagents");
    fs::create_dir_all(subagents(&source)).unwrap();
    let files = [
        ("agent-a.jsonl", "{\"type\":\"user\"}\n"),
        ("agent-a.meta.json", "{}"),
    ];
    for (name, body) in files {
        fs::write(subagents(&source).join(name), body).unwrap();
    }
    let snapshot = |root: &Path| {
        let mut all = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                match entry.file_type().unwrap().is_dir() {
                    true => stack.push(entry.path()),
                    false => all.push((entry.path(), fs::read(entry.path()).unwrap())),
                }
            }
        }
        all.sort();
        all
    };
    let before = snapshot(&other);
    let current = f.current.to_str().unwrap();
    let homes = [other.clone()];
    let resume = || resolve_cwd(Some(current), None, PRIOR, Some(&f.home), &homes, Ok(None));
    // A non-file entry or a failed transcript publish refuses and leaves no partial copy.
    let nested = subagents(&source).join("nested");
    fs::create_dir(&nested).unwrap();
    assert!(resume().is_err());
    assert_eq!(snapshot(&f.home), Vec::new());
    fs::remove_dir(&nested).unwrap();
    let target = claude_transcript_path(&f.current, PRIOR, Some(&f.home)).unwrap();
    fs::create_dir_all(&target).unwrap();
    assert!(resume().is_err());
    assert!(!target.with_extension("").exists());
    fs::remove_dir(&target).unwrap();
    assert_eq!(resume().unwrap(), current);
    for (name, body) in files {
        assert_eq!(
            fs::read_to_string(subagents(&target).join(name)).unwrap(),
            body
        );
    }
    assert_eq!(snapshot(&other), before);
}

#[test]
fn unusable_transcript_homes_warn_once_each_at_load_and_leave_the_list_alone() {
    let f = Fixture::new();
    let other = f.root.path().join("1-acct");
    let title = "{\"type\":\"ai-title\",\"aiTitle\":\"선언 home\"}\n";
    f.transcript_in(&other, &f.sibling, PRIOR, title, 1000);
    let config = f.root.path().join("agentdesk.yaml");
    let missing = f.root.path().join("no-such-home");
    let yaml = format!(
        "server: {{}}\nproviders:\n  claude:\n    transcript_homes: [\".claude-b\", \"{}\", \"{}\"]\n",
        missing.display(),
        other.display()
    );
    fs::write(&config, yaml).unwrap();
    #[derive(Clone, Default)]
    struct Logs(std::sync::Arc<Mutex<Vec<u8>>>);
    impl Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let loaded = tracing::subscriber::with_default(subscriber, || {
        crate::config::load_from_path(&config).unwrap()
    });
    let logs = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    let warned = |entry: &str| {
        logs.lines()
            .filter(|line| line.contains("WARN") && line.contains("transcript_homes"))
            .filter(|line| line.contains(entry))
            .count()
    };
    assert_eq!(warned(".claude-b"), 1, "{logs}");
    assert_eq!(warned(missing.to_str().unwrap()), 1, "{logs}");
    assert_eq!(warned(other.to_str().unwrap()), 0, "{logs}");
    let listed = choices(
        f.current.to_str().unwrap(),
        None,
        &HashSet::new(),
        Some(&f.home),
        &declared_homes(&loaded),
        Ok(None),
        "",
        Instant::now() + Duration::from_secs(2),
    );
    assert_eq!(listed.len(), 1);
    assert!(
        listed[0].label.ends_with(" · home 1-acct"),
        "{}",
        listed[0].label
    );
}

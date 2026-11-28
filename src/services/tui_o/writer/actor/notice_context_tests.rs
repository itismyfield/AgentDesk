use super::*;
use history::notice_owner::{Batch, Decision, context_observation_tests as io};

struct Fixture {
    actor: Host,
    resumed: PreparedIncarnation,
    paths: Vec<PathBuf>,
    frame: String,
    origin: Box<crate::services::tui_o::writer::input_notices::NoticeOrigin>,
    case: Case,
}

impl Fixture {
    async fn new(rows: usize) -> Self {
        Self::with_paths(rows, 1).await
    }

    async fn with_paths(rows: usize, paths: usize) -> Self {
        let case = Case::new("context-observations");
        launch(&case, false, "native");
        let mut actor = case.open();
        case.run(&mut actor).await;
        let resumed = launch(&case, true, "native");
        let mut launches = vec![resumed.context.clone()];
        for _ in 1..paths {
            launches.push(
                PreparedIncarnation::prepare_at(
                    "claude",
                    &case.logical,
                    Some(CHANNEL),
                    Some("native"),
                    true,
                    None,
                )
                .unwrap()
                .context,
            );
        }
        let paths = launches
            .iter()
            .map(|context| {
                crate::services::tui_prompt_dedupe::binding_context::context_path(
                    "claude",
                    &context.execution_nonce,
                )
                .unwrap()
            })
            .collect();
        let frame = case.frame();
        register_current(&case, &frame);
        let path = crate::config::runtime_root()
            .unwrap()
            .join("runtime/injected_frames.json");
        let mut history: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let seed = history["live"][0].clone();
        let mut registrations = vec![seed.clone()];
        for n in 1..rows {
            let mut row = seed.clone();
            let hash = format!("{n:064x}");
            row["id"] = format!("context-{n}").into();
            for key in ["hash", "body_hash", "class_hash"] {
                row[key] = hash.clone().into();
                row["registration"]["frame"][key] = hash.clone().into();
            }
            let key = &mut row["registration"]["key"];
            key["attempt"]["nonce"] = format!("context-{n}").into();
            key["identity"]["primary"] = (10_000 + n).into();
            key["identity"]["sources"][0]["message_id"] = (10_000 + n).into();
            let launch = &launches[n % launches.len()];
            row["registration"]["key"]["attempt"]["target"]["execution_nonce"] =
                launch.execution_nonce.clone().into();
            row["registration"]["source"]["target"] =
                row["registration"]["key"]["attempt"]["target"].clone();
            registrations.push(row);
        }
        history["live"] = registrations.into();
        std::fs::write(&path, serde_json::to_vec(&history).unwrap()).unwrap();
        filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(60),
            ),
        )
        .unwrap();
        case.input("context-owned", &frame);
        actor
            .refresh_for_test(ShadowProvider::Claude)
            .await
            .unwrap();
        actor.read_sources_for_test();
        let origin = actor
            .writer_mut_for_test()
            .store()
            .ledger()
            .input_notices()
            .evidence
            .values()
            .find(|e| e.unit_key.as_ref() == Some(&input_key("context-owned")))
            .unwrap()
            .origin
            .clone()
            .unwrap();
        Self {
            case,
            actor,
            resumed,
            paths,
            frame,
            origin,
        }
    }

    fn decision(&self, batch: &Batch) -> Decision {
        batch.decision_for_test(&self.origin, "context-owned", &self.frame)
    }
}

#[tokio::test(start_paused = true)]
async fn notice_owner_context_io_tracks_unique_paths_instead_of_registration_rows() {
    let mut measurements = Vec::new();
    for (rows, paths) in [(1, 1), (64, 1), (1024, 1), (1024, 4)] {
        let mut fixture = Fixture::with_paths(rows, paths).await;
        for path in &fixture.paths {
            io::take(path);
        }
        let began = std::time::Instant::now();
        let batch = Batch::prepare(fixture.actor.writer_mut_for_test().store());
        let elapsed = began.elapsed();
        let counts = fixture.paths.iter().map(|path| io::take(path)).fold(
            io::Counts::default(),
            |mut sum, next| {
                sum.reads += next.reads;
                sum.stats += next.stats;
                sum
            },
        );
        assert_eq!(fixture.decision(&batch), Decision::Owned);
        println!(
            "CONTEXT_REFRESH rows={rows} paths={paths} reads={} stats={} elapsed_us={}",
            counts.reads,
            counts.stats,
            elapsed.as_micros()
        );
        measurements.push((rows, paths, counts));
        drop(batch);
        drop(fixture);
    }
    for (rows, paths, counts) in measurements {
        assert_eq!(
            counts.reads, paths,
            "{rows} registrations share one real context load"
        );
        assert!(
            counts.stats <= 3 * paths,
            "{rows} registrations: {counts:?}"
        );
    }
}

fn change_context(path: &Path, variant: &str) {
    let before = std::fs::read(path).unwrap();
    let modified =
        filetime::FileTime::from_last_modification_time(&std::fs::metadata(path).unwrap());
    match variant {
        "remove" => std::fs::remove_file(path).unwrap(),
        "replace" => {
            let replacement = path.with_extension("replacement");
            std::fs::write(&replacement, before).unwrap();
            filetime::set_file_mtime(&replacement, modified).unwrap();
            std::fs::rename(replacement, path).unwrap();
        }
        "rewrite" => {
            let mut value: Value = serde_json::from_slice(&before).unwrap();
            value["expected_native_session_id"] = "alien!".into();
            let after = serde_json::to_vec(&value).unwrap();
            assert_eq!(before.len(), after.len());
            std::fs::write(path, after).unwrap();
            filetime::set_file_mtime(path, modified).unwrap();
        }
        _ => panic!("unknown context change"),
    }
}

#[tokio::test(start_paused = true)]
async fn notice_owner_rejects_context_changes_during_read_and_before_batch_confirmation() {
    for stage in [io::Stage::AfterRead, io::Stage::BeforeValidate] {
        for variant in ["replace", "rewrite", "remove"] {
            let mut fixture = Fixture::new(64).await;
            let original = std::fs::read(&fixture.resumed.path).unwrap();
            let hook = io::mutate_once(stage, move |path| change_context(path, variant));
            let batch = Batch::prepare(fixture.actor.writer_mut_for_test().store());
            assert!(
                hook.fired(),
                "{stage:?}/{variant}: actual refresh reached the boundary"
            );
            assert!(
                matches!(fixture.decision(&batch), Decision::Pending(_)),
                "{stage:?}/{variant}"
            );
            drop(hook);
            std::fs::write(&fixture.resumed.path, original).unwrap();
            fixture.case.run(&mut fixture.actor).await;
            fixture.case.excluded(&mut fixture.actor, "context-owned");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn notice_owner_context_read_failures_are_shared_without_surviving_the_refresh() {
    let mut fixture = Fixture::new(64).await;
    let original = std::fs::read(&fixture.resumed.path).unwrap();
    for variant in ["corrupt", "nonce", "provider", "schema", "missing"] {
        if variant == "missing" {
            std::fs::remove_file(&fixture.resumed.path).unwrap();
        } else {
            let mut value: Value = serde_json::from_slice(&original).unwrap();
            let bytes = match variant {
                "corrupt" => b"{".to_vec(),
                "nonce" => {
                    value["execution_nonce"] = "f".repeat(32).into();
                    serde_json::to_vec(&value).unwrap()
                }
                "provider" => {
                    value["provider"] = "codex".into();
                    serde_json::to_vec(&value).unwrap()
                }
                "schema" => {
                    value["schema"] = 2.into();
                    serde_json::to_vec(&value).unwrap()
                }
                _ => panic!("unknown context failure"),
            };
            std::fs::write(&fixture.resumed.path, bytes).unwrap();
        }
        io::take(&fixture.resumed.path);
        let refused = Batch::prepare(fixture.actor.writer_mut_for_test().store());
        assert_eq!(io::take(&fixture.resumed.path).reads, 1, "{variant}");
        assert_eq!(
            fixture.decision(&refused),
            Decision::Pending("execution_context_unavailable"),
            "{variant}"
        );
        std::fs::write(&fixture.resumed.path, &original).unwrap();
        io::take(&fixture.resumed.path);
        let restored = Batch::prepare(fixture.actor.writer_mut_for_test().store());
        assert_eq!(
            io::take(&fixture.resumed.path).reads,
            1,
            "{variant}: new refresh reads again"
        );
        assert_eq!(fixture.decision(&restored), Decision::Owned, "{variant}");
    }
}

#[tokio::test(start_paused = true)]
async fn notice_owner_context_sharing_is_local_to_each_refresh() {
    let mut fixture = Fixture::new(1024).await;
    let mut elapsed = std::time::Duration::ZERO;
    let mut reads = 0;
    for _ in 0..10 {
        io::take(&fixture.resumed.path);
        let began = std::time::Instant::now();
        let batch = Batch::prepare(fixture.actor.writer_mut_for_test().store());
        elapsed += began.elapsed();
        reads += io::take(&fixture.resumed.path).reads;
        assert_eq!(fixture.decision(&batch), Decision::Owned);
    }
    println!(
        "CONTEXT_BENCH rows=1024 paths=1 refreshes=10 reads={reads} mean_us={}",
        elapsed.as_micros() / 10
    );
    assert_eq!(reads, 10, "each refresh loads once; no process-wide cache");
}

#[tokio::test(start_paused = true)]
async fn notice_owner_changed_context_only_withholds_its_own_registered_frames() {
    let mut fixture = Fixture::with_paths(64, 2).await;
    let other = fixture.paths[1].clone();
    let hook = io::mutate_once(io::Stage::BeforeValidate, move |_| {
        std::fs::remove_file(other).unwrap();
    });
    let batch = Batch::prepare(fixture.actor.writer_mut_for_test().store());
    assert!(hook.fired());
    assert_eq!(fixture.decision(&batch), Decision::Owned);
}

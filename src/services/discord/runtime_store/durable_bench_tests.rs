//! Latency measurement of durable publication (run on demand, never in CI):
//! `ADK_BENCH_OUT=<json> ADK_BENCH_BASE=<sha> ADK_BENCH_HEAD=<sha> ADK_BENCH_MACHINE=<text>
//! cargo test --lib durable_publication_latency_bench -- --ignored --nocapture --test-threads=1`.
//! Baseline and candidate alternate in one binary; the baseline skips every directory flush
//! through the test probe, which is how the code behaved before durable publication.
//! `ADK_BENCH_SCALE=quick` shrinks the sample counts and can never yield PASS;
//! `ADK_BENCH_INJECT=eio` fails every candidate parent flush, which must not PASS either.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use serde_json::{Value, json};

use super::super::durable_publish::probe::{Event, Plan, Step, Watch};
use super::super::durable_publish::{FlushStatus, publish_durable};
use super::super::{PARENT_DIR_FSYNC_FLUSHES, process_generation};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{
    ChannelMailboxHandle, ChannelMailboxRegistry, Intervention, InterventionMode,
    QueuePersistenceContext,
};

const P50_GATE_MS: f64 = 10.0;
/// Re-published chains still inside the witness, and the age past which a chain was evicted.
const STEADY: usize = 200;
const EVICTED_AFTER: usize = 256;
const P99_GATE_MS: f64 = 25.0;

#[derive(Clone, Copy)]
struct Scale {
    full: bool,
    primitive_warmup: usize,
    primitive: usize,
    cold: usize,
    enqueue_warmup: usize,
    enqueue: usize,
}

impl Scale {
    fn from_env() -> Self {
        if std::env::var("ADK_BENCH_SCALE").as_deref() == Ok("quick") {
            Self {
                full: false,
                primitive_warmup: 5,
                primitive: 40,
                cold: 20,
                enqueue_warmup: 5,
                enqueue: 40,
            }
        } else {
            Self {
                full: true,
                primitive_warmup: 200,
                primitive: 10_000,
                cold: 1_000,
                enqueue_warmup: 100,
                enqueue: 2_000,
            }
        }
    }
}

/// Per-arm tally: latencies, expected publications, and the directory flushes the probe saw.
#[derive(Default)]
struct Arm {
    ms: Vec<f64>,
    publications: usize,
    writes: usize,
    parent_ok: usize,
    parent_failed: usize,
    ancestors_ok: usize,
    ancestors_failed: usize,
    segments: Vec<[f64; 4]>,
}

impl Arm {
    fn absorb(&mut self, events: &[Event], elapsed: Duration, publications: usize) {
        self.ms.push(elapsed.as_secs_f64() * 1e3);
        self.publications += publications;
        for event in events {
            match (event.step, event.ok) {
                (Step::Write, _) => self.writes += 1,
                (Step::ParentSync | Step::ParentOpen, true) => self.parent_ok += 1,
                (Step::ParentSync | Step::ParentOpen, false) => self.parent_failed += 1,
                (Step::Ancestors, true) => self.ancestors_ok += 1,
                (Step::Ancestors, false) => self.ancestors_failed += 1,
                _ => {}
            }
        }
        if let Some(segments) = segments(events) {
            self.segments.push(segments);
        }
    }

    fn summary(&self) -> Value {
        let column = |i: usize| self.segments.iter().map(|s| s[i]).collect::<Vec<_>>();
        json!({
            "samples": self.ms.len(),
            "p50_ms": pct(&self.ms, 50.0),
            "p99_ms": pct(&self.ms, 99.0),
            "publications": self.publications,
            "writes": self.writes,
            "parent_sync_ok": self.parent_ok,
            "parent_sync_failed": self.parent_failed,
            "ancestor_sync_ok": self.ancestors_ok,
            "ancestor_sync_failed": self.ancestors_failed,
            "segments_p50_ms": {
                "file_write_and_sync": pct(&column(0), 50.0),
                "rename": pct(&column(1), 50.0),
                "parent": pct(&column(2), 50.0),
                "ancestors": pct(&column(3), 50.0),
            },
            "segments_p99_ms": {
                "file_write_and_sync": pct(&column(0), 99.0),
                "rename": pct(&column(1), 99.0),
                "parent": pct(&column(2), 99.0),
                "ancestors": pct(&column(3), 99.0),
            },
        })
    }
}

/// write→rename, rename, rename→parent flush, parent→last ancestor flush, for one publication.
fn segments(events: &[Event]) -> Option<[f64; 4]> {
    let at = |step: Step| events.iter().find(|e| e.step == step).map(|e| e.at);
    let (write, rename, published) = (at(Step::Write)?, at(Step::Rename)?, at(Step::Published)?);
    let parent = at(Step::ParentSync).unwrap_or(published);
    let last = events
        .iter()
        .filter(|e| e.step == Step::Ancestors)
        .map(|e| e.at)
        .max()
        .unwrap_or(parent);
    let ms = |from: Instant, to: Instant| to.saturating_duration_since(from).as_secs_f64() * 1e3;
    Some([
        ms(write, rename),
        ms(rename, published),
        ms(published, parent),
        ms(parent, last),
    ])
}

fn pct(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// One comparison: both arms, paired increases, and the fail-closed verdict.
struct Pair {
    name: String,
    baseline: Arm,
    candidate: Arm,
    /// Directory flushes each candidate publication must show (parent + expected ancestors).
    parent_per_publication: usize,
    gated: bool,
}

impl Pair {
    fn new(name: impl Into<String>, gated: bool) -> Self {
        Self {
            name: name.into(),
            baseline: Arm::default(),
            candidate: Arm::default(),
            parent_per_publication: 1,
            gated,
        }
    }

    /// Reasons this comparison cannot pass; empty means the counts prove every flush ran.
    fn invalid(&self, expected_samples: usize) -> Vec<String> {
        let mut reasons = Vec::new();
        let (b, c) = (&self.baseline, &self.candidate);
        if !PARENT_DIR_FSYNC_FLUSHES {
            reasons.push("platform has no directory flush".into());
        }
        if b.ms.len() < expected_samples || c.ms.len() < expected_samples {
            reasons.push(format!(
                "samples {}/{} < {expected_samples}",
                b.ms.len(),
                c.ms.len()
            ));
        }
        if c.parent_failed + c.ancestors_failed + b.parent_failed + b.ancestors_failed > 0 {
            reasons.push(format!(
                "sync failures candidate={} baseline={}",
                c.parent_failed + c.ancestors_failed,
                b.parent_failed + b.ancestors_failed
            ));
        }
        if c.parent_ok != c.publications * self.parent_per_publication {
            reasons.push(format!(
                "candidate parent syncs {} != publications {}",
                c.parent_ok, c.publications
            ));
        }
        if c.writes != c.publications || b.writes != b.publications {
            reasons.push(format!(
                "writes {}/{} != publications {}/{}",
                b.writes, c.writes, b.publications, c.publications
            ));
        }
        if b.parent_ok + b.ancestors_ok > 0 {
            reasons.push("baseline flushed a directory".into());
        }
        reasons
    }

    fn report(&self, expected_samples: usize) -> (Value, &'static str) {
        let paired: Vec<f64> = self
            .baseline
            .ms
            .iter()
            .zip(&self.candidate.ms)
            .map(|(b, c)| c - b)
            .collect();
        let reasons = self.invalid(expected_samples);
        let p50 = pct(&self.candidate.ms, 50.0).zip(pct(&self.baseline.ms, 50.0));
        let p99 = pct(&self.candidate.ms, 99.0).zip(pct(&self.baseline.ms, 99.0));
        let (p50_delta, p99_delta) = (p50.map(|(c, b)| c - b), p99.map(|(c, b)| c - b));
        let verdict = if !reasons.is_empty() {
            "INVALID"
        } else if !self.gated {
            "MEASURED"
        } else if p50_delta.is_some_and(|d| d <= P50_GATE_MS)
            && p99_delta.is_some_and(|d| d <= P99_GATE_MS)
        {
            "PASS"
        } else {
            "FAIL"
        };
        let value = json!({
            "name": self.name,
            "baseline": self.baseline.summary(),
            "candidate": self.candidate.summary(),
            "p50_increase_ms": p50_delta,
            "p99_increase_ms": p99_delta,
            "paired_increase_p50_ms": pct(&paired, 50.0),
            "paired_increase_p99_ms": pct(&paired, 99.0),
            "invalid_reasons": reasons,
            "verdict": verdict,
        });
        (value, verdict)
    }
}

fn plan(bypass: bool) -> Plan {
    let eio = std::env::var("ADK_BENCH_INJECT").as_deref() == Ok("eio");
    Plan {
        fail: if eio && !bypass {
            vec![Step::ParentSync]
        } else {
            Vec::new()
        },
        bypass,
        skip_bytes: true,
        ..Plan::default()
    }
}

/// Runs `op` once under the arm's probe plan and folds its events into `arm`.
fn timed<T>(
    scope: &Path,
    bypass: bool,
    arm: Option<&mut Arm>,
    publications: usize,
    op: impl FnOnce() -> T,
) -> T {
    let watch = Watch::new(scope, plan(bypass));
    let started = Instant::now();
    let out = op();
    let elapsed = started.elapsed();
    if let Some(arm) = arm {
        arm.absorb(&watch.drain(), elapsed, publications);
    }
    out
}

fn primitive(root: &Path, scale: Scale, size: usize) -> Pair {
    let mut pair = Pair::new(format!("primitive_{}k", size / 1024), false);
    let data = "x".repeat(size);
    let target = root.join("primitive").join(format!("{size}.json"));
    publish_durable(root, &target, &data).unwrap();
    for i in 0..scale.primitive_warmup + scale.primitive {
        let measured = i >= scale.primitive_warmup;
        for bypass in [i % 2 == 0, i % 2 == 1] {
            let arm = if bypass {
                &mut pair.baseline
            } else {
                &mut pair.candidate
            };
            let arm = measured.then_some(arm);
            let published = timed(root, bypass, arm, 1, || {
                publish_durable(root, &target, &data)
            });
            assert!(published.is_ok());
        }
    }
    pair
}

/// Fresh provider/token chains: each publication creates three directories above its file.
fn cold(root: &Path, scale: Scale) -> Vec<Pair> {
    let mut fresh = Pair::new("cold_new_chain", false);
    let mut precreated = Pair::new("cold_externally_precreated", false);
    let mut evicted = Pair::new("cold_after_witness_eviction", false);
    let mut steady = Pair::new("steady_witness_hit", false);
    let data = "x".repeat(1024);
    let chain = |arm: &str, i: usize| {
        root.join("cold")
            .join(arm)
            .join(format!("p{i}"))
            .join("t")
            .join("1.json")
    };
    for i in 0..scale.cold {
        for bypass in [i % 2 == 0, i % 2 == 1] {
            let label = if bypass { "b" } else { "c" };
            let target = chain(label, i);
            let arm = if bypass {
                &mut fresh.baseline
            } else {
                &mut fresh.candidate
            };
            timed(root, bypass, Some(arm), 1, || {
                publish_durable(root, &target, &data)
            })
            .unwrap();
            let target = chain(&format!("pre-{label}"), i);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let arm = if bypass {
                &mut precreated.baseline
            } else {
                &mut precreated.candidate
            };
            timed(root, bypass, Some(arm), 1, || {
                publish_durable(root, &target, &data)
            })
            .unwrap();
        }
    }
    // The witness keeps the newest 512 chains (two per candidate sample above): re-publishing the
    // newest 200 hits it; the oldest, re-published afterwards, were evicted and flush again.
    let hits = (scale.cold.saturating_sub(STEADY)..scale.cold).rev();
    for i in hits.chain(0..scale.cold.saturating_sub(EVICTED_AFTER)) {
        let pair = if i + STEADY >= scale.cold {
            &mut steady
        } else {
            &mut evicted
        };
        for bypass in [i % 2 == 0, i % 2 == 1] {
            let target = chain(&format!("pre-{}", if bypass { "b" } else { "c" }), i);
            let arm = if bypass {
                &mut pair.baseline
            } else {
                &mut pair.candidate
            };
            timed(root, bypass, Some(arm), 1, || {
                publish_durable(root, &target, &data)
            })
            .unwrap();
        }
    }
    vec![fresh, precreated, evicted, steady]
}

/// Two threads publish into the same fresh chain at once; every publication must still flush.
fn concurrent(root: &Path, scale: Scale) -> Pair {
    let mut pair = Pair::new("cold_concurrent_creation", false);
    let data = "x".repeat(1024);
    for i in 0..scale.cold / 2 {
        for bypass in [i % 2 == 0, i % 2 == 1] {
            let dir = root
                .join("race")
                .join(if bypass { "b" } else { "c" })
                .join(format!("p{i}"))
                .join("t");
            let watch = Watch::new(&dir, plan(bypass));
            let barrier = std::sync::Barrier::new(2);
            let elapsed: Vec<Duration> = std::thread::scope(|scope| {
                let handles: Vec<_> = (0..2)
                    .map(|n| {
                        let (dir, data, barrier) = (&dir, &data, &barrier);
                        scope.spawn(move || {
                            barrier.wait();
                            let started = Instant::now();
                            let published =
                                publish_durable(root, &dir.join(format!("{n}.json")), data);
                            assert!(published.is_ok());
                            started.elapsed()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let arm = if bypass {
                &mut pair.baseline
            } else {
                &mut pair.candidate
            };
            let events = watch.drain();
            arm.absorb(&events, elapsed[0], 1);
            arm.absorb(&[], elapsed[1], 1);
        }
    }
    pair
}

fn intervention(id: u64, author: u64, mergeable: bool) -> Intervention {
    Intervention {
        author_id: UserId::new(author),
        author_is_bot: false,
        message_id: MessageId::new(id),
        queued_generation: process_generation(),
        source_message_ids: vec![MessageId::new(id)],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: format!("bench message {id}"),
        mode: InterventionMode::Soft,
        created_at: Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: mergeable,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

struct Mailbox {
    handle: ChannelMailboxHandle,
    persistence: QueuePersistenceContext,
    next_id: u64,
    next_author: u64,
}

impl Mailbox {
    async fn new(registry: &ChannelMailboxRegistry, channel: u64, token: &str, len: usize) -> Self {
        let persistence = QueuePersistenceContext::new(&ProviderKind::Codex, token, None);
        let mut mailbox = Self {
            handle: registry.handle(ChannelId::new(channel)),
            persistence,
            next_id: channel * 1_000_000,
            next_author: 10,
        };
        for _ in 0..len {
            mailbox.push(false).await;
        }
        mailbox
    }

    fn fresh(&mut self, mergeable: bool) -> Intervention {
        self.next_id += 1;
        self.next_author += 1;
        intervention(self.next_id, self.next_author, mergeable)
    }

    async fn push(&mut self, mergeable: bool) {
        let item = self.fresh(mergeable);
        let result = self.handle.enqueue(item, self.persistence.clone()).await;
        assert!(result.enqueued && !result.merged);
    }

    async fn take(&self) {
        assert!(
            self.handle
                .take_next_soft(self.persistence.clone())
                .await
                .intervention
                .is_some()
        );
    }
}

/// The real mailbox enqueue→reply at a fixed queue length: accepted, merged into the tail, and
/// refused as already queued (a no-op that must write nothing).
fn enqueue(root: &Path, rt: &tokio::runtime::Runtime, scale: Scale, len: usize) -> Vec<Pair> {
    let mut accepted = Pair::new(format!("enqueue_accepted_len{len}"), true);
    let mut merged = Pair::new(format!("enqueue_merged_len{len}"), true);
    let mut refused = Pair::new(format!("enqueue_refused_len{len}"), false);
    let registry = ChannelMailboxRegistry::default();
    rt.block_on(async {
        let mut boxes = Vec::new();
        for (n, kind) in ["accept", "merge", "refuse"].into_iter().enumerate() {
            let channel = 7_190_100 + len as u64 * 10 + n as u64;
            let mut mailbox = Mailbox::new(
                &registry,
                channel,
                &format!("bench-{kind}"),
                len.saturating_sub(1),
            )
            .await;
            mailbox.push(kind == "merge").await;
            boxes.push(mailbox);
        }
        let [accept, merge, refuse] = &mut boxes[..] else {
            unreachable!()
        };
        for i in 0..scale.enqueue_warmup + scale.enqueue {
            let measured = i >= scale.enqueue_warmup;
            for bypass in [i % 2 == 0, i % 2 == 1] {
                let item = accept.fresh(false);
                let arm = measured.then_some(if bypass {
                    &mut accepted.baseline
                } else {
                    &mut accepted.candidate
                });
                let result = timed_async(
                    root,
                    bypass,
                    arm,
                    1,
                    accept.handle.enqueue(item, accept.persistence.clone()),
                )
                .await;
                assert!(result.enqueued && !result.merged && result.persistence_error.is_none());
                accept.take().await;

                // Merge into the mergeable tail, then rotate so the queue length stays fixed.
                let author = UserId::new(merge.next_author);
                let mut item = merge.fresh(true);
                item.author_id = author;
                let arm = measured.then_some(if bypass {
                    &mut merged.baseline
                } else {
                    &mut merged.candidate
                });
                let result = timed_async(
                    root,
                    bypass,
                    arm,
                    1,
                    merge.handle.enqueue(item, merge.persistence.clone()),
                )
                .await;
                assert!(
                    result.enqueued && result.merged,
                    "merge sample did not merge"
                );
                merge.take().await;
                merge.push(true).await;

                let queued = refuse.handle.snapshot().await.intervention_queue[0].message_id;
                let mut item = refuse.fresh(false);
                item.message_id = queued;
                item.source_message_ids = vec![queued];
                let arm = measured.then_some(if bypass {
                    &mut refused.baseline
                } else {
                    &mut refused.candidate
                });
                let result = timed_async(
                    root,
                    bypass,
                    arm,
                    0,
                    refuse.handle.enqueue(item, refuse.persistence.clone()),
                )
                .await;
                assert!(
                    !result.enqueued && result.refusal_reason.is_some(),
                    "refusal sample was accepted"
                );
            }
        }
    });
    vec![accepted, merged, refused]
}

/// Cold enqueue: every sample opens a brand-new token directory under the queue store.
fn cold_enqueue(root: &Path, rt: &tokio::runtime::Runtime, scale: Scale) -> Pair {
    let mut pair = Pair::new("enqueue_cold_new_token", true);
    let registry = ChannelMailboxRegistry::default();
    rt.block_on(async {
        for i in 0..scale.cold {
            for bypass in [i % 2 == 0, i % 2 == 1] {
                let token = format!("cold-{}-{i}", if bypass { "b" } else { "c" });
                let persistence = QueuePersistenceContext::new(&ProviderKind::Codex, &token, None);
                let handle = registry.handle(ChannelId::new(7_190_900 + (i as u64 % 50)));
                let item = intervention(8_000_000 + i as u64 * 2 + bypass as u64, 99, false);
                let arm = if bypass {
                    &mut pair.baseline
                } else {
                    &mut pair.candidate
                };
                let result = timed_async(
                    root,
                    bypass,
                    Some(arm),
                    1,
                    handle.enqueue(item, persistence.clone()),
                )
                .await;
                assert!(result.enqueued);
                assert!(
                    handle
                        .take_next_soft(persistence)
                        .await
                        .intervention
                        .is_some()
                );
            }
        }
    });
    pair
}

async fn timed_async<T>(
    scope: &Path,
    bypass: bool,
    arm: Option<&mut Arm>,
    publications: usize,
    op: impl std::future::Future<Output = T>,
) -> T {
    let watch = Watch::new(scope, plan(bypass));
    let started = Instant::now();
    let out = op.await;
    let elapsed = started.elapsed();
    if let Some(arm) = arm {
        arm.absorb(&watch.drain(), elapsed, publications);
    }
    out
}

fn env_or_missing(key: &str) -> Value {
    std::env::var(key).map(Value::String).unwrap_or(Value::Null)
}

#[test]
#[ignore = "latency measurement; run on demand"]
fn durable_publication_latency_bench() {
    let scale = Scale::from_env();
    let lock = crate::config::shared_test_env_lock()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let previous = std::env::var("AGENTDESK_ROOT_DIR").ok();
    unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };
    let root: PathBuf = temp.path().join("runtime");
    std::fs::create_dir_all(&root).unwrap();
    assert!(matches!(
        publish_durable(&root, &root.join("probe.json"), "x")
            .unwrap()
            .flush_status,
        FlushStatus::Flushed | FlushStatus::UnflushedPlatform
    ));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut results = Vec::new();
    let mut verdicts = Vec::new();
    let mut add = |pair: Pair, expected: usize| {
        let (value, verdict) = pair.report(expected);
        eprintln!("{}", serde_json::to_string(&value).unwrap());
        verdicts.push((pair.name.clone(), verdict));
        results.push(value);
    };
    for len in [1, 8, 64] {
        for pair in enqueue(&root, &rt, scale, len) {
            add(pair, scale.enqueue);
        }
    }
    add(cold_enqueue(&root, &rt, scale), scale.cold);
    for size in [1024, 16 * 1024, 256 * 1024] {
        add(primitive(&root, scale, size), scale.primitive);
    }
    for pair in cold(&root, scale) {
        let expected = match pair.name.as_str() {
            "steady_witness_hit" => STEADY.min(scale.cold),
            "cold_after_witness_eviction" => scale.cold.saturating_sub(EVICTED_AFTER),
            _ => scale.cold,
        };
        add(pair, expected);
    }
    add(concurrent(&root, scale), scale.cold / 2);

    let failed = verdicts
        .iter()
        .any(|(_, v)| matches!(*v, "FAIL" | "INVALID"));
    let missing_meta = ["ADK_BENCH_BASE", "ADK_BENCH_HEAD", "ADK_BENCH_MACHINE"]
        .iter()
        .any(|key| std::env::var(key).is_err());
    let overall = if failed {
        "FAIL"
    } else if !scale.full || missing_meta {
        "INVALID"
    } else {
        "PASS"
    };
    let artifact = json!({
        "overall": overall,
        "scale": if scale.full { "full" } else { "quick" },
        "inject": env_or_missing("ADK_BENCH_INJECT"),
        "base": env_or_missing("ADK_BENCH_BASE"),
        "head": env_or_missing("ADK_BENCH_HEAD"),
        "machine": env_or_missing("ADK_BENCH_MACHINE"),
        "parent_dir_fsync_flushes": PARENT_DIR_FSYNC_FLUSHES,
        "gates_ms": {"p50_increase": P50_GATE_MS, "p99_increase": P99_GATE_MS},
        "results": results,
    });
    if let Ok(out) = std::env::var("ADK_BENCH_OUT") {
        std::fs::write(out, serde_json::to_string_pretty(&artifact).unwrap()).unwrap();
    }
    eprintln!("BENCH_OVERALL {overall}");
    match previous {
        Some(previous) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", previous) },
        None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
    }
    drop(lock);
}

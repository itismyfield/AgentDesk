const test = require("node:test");
const assert = require("node:assert/strict");
const { loadRoutine } = require("./support/routine-harness");

const ROUTINE_PATH = "routines/monitoring/automation-candidate-recommender.js";

const BASE_NOW = new Date("2026-05-02T10:00:00Z");

function makeObs(signature, evidenceRef, opts = {}) {
  return {
    signature,
    evidence_ref: evidenceRef || `ref:${signature}`,
    category: opts.category || "routine-candidate",
    summary: `obs for ${signature}`,
    occurrences: opts.occurrences || 1,
    weight: opts.weight || 1,
    timestamp: opts.timestamp || BASE_NOW.toISOString(),
  };
}

function nextMs(base, deltaMs) {
  return new Date(base.getTime() + deltaMs);
}

function parseSummary(result) {
  const s = result.result && result.result.scoring_summary;
  if (!s) return {};
  const out = {};
  for (const part of s.split(",")) {
    const [k, v] = part.trim().split("=");
    out[k] = isNaN(Number(v)) ? v : Number(v);
  }
  return out;
}

// Returns a checkpoint pre-filled so that all observations are already in seen_evidence
function buildFullyDedupedCheckpoint(obs, nowStr) {
  const seen_evidence = {};
  for (const o of obs) {
    const key = o.evidence_ref || `${o.source || ""}|${o.category || ""}|${o.signature || ""}`;
    seen_evidence[key] = nowStr;
  }
  return {
    version: 1,
    cursors: {},
    candidates: {},
    suppressions: {},
    seen_evidence,
    recommendations: [],
    last_tick_at: null,
    ema_scored: 0,
    saturation_ticks: 0,
    fast_fail_ticks: 0,
    reopt_count: 0,
    diversity_mode_ticks_remaining: 0,
    last_reopt_at: null,
    stats: {
      ticks: 0,
      observations_seen: 0,
      agent_escalations: 0,
      recommendations_today: 0,
      recommendation_day: null,
      category_scored: {},
    },
  };
}

test("fast-fail: 2 consecutive all-dedup ticks trigger reopt", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeObs("dup-sig", "ref:dup-sig")];

  // Tick 1: score once (establishes seen_evidence)
  const t1Now = BASE_NOW;
  const r1 = tick({ now: t1Now, checkpoint: null, observations: obs, automationInventory: [] });
  assert.equal(parseSummary(r1).scored, 1, "tick1 should score 1");

  // Tick 2: all dedup (fast_fail_ticks becomes 1)
  const t2Now = nextMs(t1Now, 60_000);
  const r2 = tick({ now: t2Now, checkpoint: r1.checkpoint, observations: obs, automationInventory: [] });
  assert.equal(parseSummary(r2).deduped, 1, "tick2 all deduped");
  assert.equal(parseSummary(r2).fast_fail_ticks, 1, "fast_fail_ticks=1 after tick2");
  assert.equal(parseSummary(r2).reopt_count, 0, "no reopt yet");

  // Tick 3: all dedup (fast_fail_ticks becomes 2 → reopt triggered)
  const t3Now = nextMs(t2Now, 60_000);
  const r3 = tick({ now: t3Now, checkpoint: r2.checkpoint, observations: obs, automationInventory: [] });
  const s3 = parseSummary(r3);
  assert.equal(s3.reopt_count, 1, "reopt_count=1 after fast-fail trigger");
  assert.ok(s3.reopt_triggered === "fast_fail" || r3.result.scoring_summary.includes("reopt_triggered=fast_fail"),
    "reopt reason should be fast_fail");
});

test("EMA tier: 5 consecutive low-ema ticks trigger reopt", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  // Use unique evidence_refs per tick to avoid dedup, but scored=1 each time.
  // ema_scored = 0.9^5 * 0 + 0.1 each tick, quickly < 0.3 so saturation_ticks should accumulate.
  // After 5 ticks of scored=1 (ema stays low since 0.1 base), saturation may or may not trigger.
  // Instead: use zero obs ticks to guarantee scored=0, ema_scored stays at 0.

  const t1Now = BASE_NOW;
  let cp = null;
  for (let i = 1; i <= 5; i++) {
    const now = nextMs(t1Now, i * 60_000);
    // No observations → scored=0, ema_scored stays near 0
    const r = tick({ now, checkpoint: cp, observations: [], automationInventory: [] });
    cp = r.checkpoint;
    const s = parseSummary(r);
    if (i < 5) {
      assert.equal(s.reopt_count, 0, `no reopt before tick 5, got at tick ${i}`);
    } else {
      assert.equal(s.reopt_count, 1, "reopt_count=1 after 5 EMA-saturation ticks");
      assert.ok(r.result.scoring_summary.includes("reopt_triggered=ema_saturation"),
        "reopt reason should be ema_saturation");
    }
  }
});

test("EMA calculation: scored=1 produces ema≈0.1 from zero", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const obs = [makeObs("ema-sig", "ref:ema-sig-unique1")];

  const r = tick({ now: BASE_NOW, checkpoint: null, observations: obs, automationInventory: [] });
  const s = parseSummary(r);
  assert.equal(s.scored, 1, "should score 1");
  // ema_scored = 0.9 * 0 + 0.1 * 1 = 0.1
  assert.ok(Math.abs(s.ema_scored - 0.1) < 0.001, `ema_scored should be ~0.1, got ${s.ema_scored}`);
});

test("partial seen_evidence reset removes old entries that were not just refreshed", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  const recentNow = BASE_NOW;
  // Both entries are within 25h TTL; the matching one is refreshed by the dedup hit.
  const staleTs = nextMs(recentNow, -13 * 3600_000).toISOString();  // 13h ago
  const recentTs = nextMs(recentNow, -1 * 3600_000).toISOString();   // 1h ago

  const baseCheckpoint = {
    version: 1,
    cursors: {},
    candidates: {},
    suppressions: {},
    seen_evidence: {
      "old-unmatched-key": staleTs,
      "recent-key": recentTs,
    },
    recommendations: [],
    last_tick_at: null,
    ema_scored: 0,
    saturation_ticks: 0,
    fast_fail_ticks: 1,  // one more all-dedup tick will reach FAST_FAIL_TICKS=2
    reopt_count: 0,
    diversity_mode_ticks_remaining: 0,
    last_reopt_at: null,
    stats: { ticks: 0, observations_seen: 0, agent_escalations: 0, recommendations_today: 0, recommendation_day: null, category_scored: {} },
  };

  // Provide one obs whose evidence_ref matches the recent key → all obs are deduped,
  // while old-unmatched-key stays old enough for partial reset to remove.
  const obs = [
    { signature: "sig-recent", evidence_ref: "recent-key", category: "routine-candidate", summary: "r", occurrences: 1, weight: 1 },
  ];

  const r = tick({ now: recentNow, checkpoint: baseCheckpoint, observations: obs, automationInventory: [] });
  const cpAfter = r.checkpoint;
  assert.equal(parseSummary(r).reopt_count, 1, "reopt should have triggered (fast_fail_ticks reached 2)");
  assert.ok(!cpAfter.seen_evidence["old-unmatched-key"], "old unmatched key (13h old) should be removed after partial reset");
  assert.ok(cpAfter.seen_evidence["recent-key"], "recent key refreshed by dedup should be kept");
});

test("diversity mode activates after reopt and decrements each tick", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  // Trigger reopt by feeding 5 empty ticks (EMA tier)
  let cp = null;
  let reoptTick = null;
  for (let i = 1; i <= 5; i++) {
    const now = nextMs(BASE_NOW, i * 60_000);
    const r = tick({ now, checkpoint: cp, observations: [], automationInventory: [] });
    cp = r.checkpoint;
    if (i === 5) reoptTick = r;
  }
  assert.equal(parseSummary(reoptTick).reopt_count, 1, "reopt should have triggered");
  // After reopt diversity_mode_ticks_remaining should be 10
  assert.equal(cp.diversity_mode_ticks_remaining, 10, "diversity mode should be 10 after reopt");

  // One more tick should decrement it
  const r2 = tick({ now: nextMs(BASE_NOW, 6 * 60_000), checkpoint: cp, observations: [], automationInventory: [] });
  assert.equal(r2.checkpoint.diversity_mode_ticks_remaining, 9, "diversity_mode_ticks_remaining should decrement");
});

test("saturation counters reset after reopt", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  // 5 empty ticks → EMA saturation → reopt
  let cp = null;
  for (let i = 1; i <= 5; i++) {
    const r = tick({ now: nextMs(BASE_NOW, i * 60_000), checkpoint: cp, observations: [], automationInventory: [] });
    cp = r.checkpoint;
  }
  assert.equal(cp.saturation_ticks, 0, "saturation_ticks reset after reopt");
  assert.equal(cp.fast_fail_ticks, 0, "fast_fail_ticks reset after reopt");
});

test("scoring_summary includes ema_scored, saturation_ticks, reopt_count fields", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);
  const r = tick({ now: BASE_NOW, checkpoint: null, observations: [], automationInventory: [] });
  const s = r.result.scoring_summary;
  assert.ok(s.includes("ema_scored="), "scoring_summary should include ema_scored");
  assert.ok(s.includes("saturation_ticks="), "scoring_summary should include saturation_ticks");
  assert.ok(s.includes("reopt_count="), "scoring_summary should include reopt_count");
});

test("candidate_dispatched obs suppresses re-recommendation (REQ-P1-004)", () => {
  const { tick } = loadRoutine(ROUTINE_PATH);

  // First tick: score a candidate until it is eligible for escalation
  // Build a checkpoint with a high-score candidate
  const sig = "dispatched-sig";
  const baseCheckpoint = {
    version: 1,
    cursors: {},
    candidates: {
      [sig]: {
        category: "routine-candidate",
        state: "observing",
        score: 95,
        evidence_count: 10,
        first_seen_at: BASE_NOW.toISOString(),
        last_seen_at: BASE_NOW.toISOString(),
        examples: [],
        last_recommended_at: null,
        last_recommendation_hash: null,
        cooldown_until: null,
        automation_ref: null,
        has_error_evidence: false,
      },
    },
    suppressions: {},
    seen_evidence: {},
    recommendations: [],
    last_tick_at: null,
    ema_scored: 0, saturation_ticks: 0, fast_fail_ticks: 0, reopt_count: 0,
    diversity_mode_ticks_remaining: 0, last_reopt_at: null,
    stats: { ticks: 0, observations_seen: 0, agent_escalations: 0, recommendations_today: 0, recommendation_day: null, category_scored: {}, category_scored_history: [] },
  };

  // Without dispatched obs: should escalate
  const r1 = tick({ now: BASE_NOW, checkpoint: baseCheckpoint, observations: [], automationInventory: [] });
  assert.equal(r1.action, "agent", "candidate above threshold should escalate");

  // With candidate_dispatched obs: same candidate should be suppressed
  const dispatchedObs = [{
    evidence_ref: `kv_meta:routine_observation:candidate_dispatched:${sig}`,
    summary: "dispatched",
  }];
  const r2 = tick({ now: new Date(BASE_NOW.getTime() + 60_000), checkpoint: baseCheckpoint, observations: dispatchedObs, automationInventory: [] });
  assert.equal(r2.action, "complete", "dispatched obs should suppress candidate and prevent escalation");
});

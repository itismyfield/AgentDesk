// Automation Candidate Recommender
// 매 tick마다 bounded observations를 checkpoint에 누적하고,
// 강한 근거(score >= 80)에서만 agent proposal을 생성한다.
// P0: read-only, no auto-implementation, proposal-only.

const SCORE_THRESHOLD = 80;
const DAILY_CAP = 3;
const COOLDOWN_HOURS = 6;
const MAX_EXAMPLES_PER_CANDIDATE = 3;
const PROMPT_CAP_BYTES = 12288;
const CHECKPOINT_CAP_BYTES = 65536;
const CHECKPOINT_VERSION = 1;
const CANDIDATE_TTL_DAYS = 30;

// seen_evidence dedup: prevents the same evidence from incrementing score/evidence_count
// across multiple ticks.  TTL covers the 24h observation query window with margin.
const SEEN_EVIDENCE_TTL_MS = 25 * 3600 * 1000;   // 25 h
const SEEN_EVIDENCE_MAX_ENTRIES = 500;             // LRU cap

// Saturation detection & re-optimization (P0-E)
// EMA formula mirrors autoresearch train.py ema_beta=0.9 (karpathy/autoresearch).
// Fast-fail tier: analogous to autoresearch "if isnan(loss): exit(1)".
// Re-optimization (partial reset + diversity mode) is original design.
const EMA_BETA = 0.9;
const EMA_SATURATION_THRESHOLD = 0.3;   // ema_scored below this → saturation_ticks++
const SATURATION_TICKS = 5;             // consecutive EMA-saturated ticks → reopt
const FAST_FAIL_TICKS = 2;              // all-dedup consecutive ticks → immediate reopt
const REOPT_WINDOW_MS = 12 * 3600 * 1000;  // partial reset: remove seen_evidence older than 12h
const DIVERSITY_BOOST_TICKS = 10;       // ticks of diversity mode after each reopt
const DIVERSITY_LOOKBACK_TICKS = 5;    // history window for underrepresented category detection

const KNOWN_CATEGORIES = new Set([
  "routine-candidate",
  "release-freshness",
  "outbox-delivery",
  "memento-hygiene",
  "api-friction",
]);

// --- Scoring weights ---
const WEIGHT_BASE = 10;
const WEIGHT_RECENCY_BONUS = 10;   // occurred in last 30 min
const WEIGHT_FIRST_SEEN = 5;       // new candidate bonus

// --- Checkpoint helpers ---

function emptyCheckpoint() {
  return {
    version: CHECKPOINT_VERSION,
    cursors: {},
    candidates: {},
    suppressions: {},
    seen_evidence: {},   // evidence_key -> { seen_at, occurrences } (legacy ISO string accepted)
    recommendations: [],
    last_tick_at: null,
    // P0-E saturation tracking
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
      category_scored_history: [],   // [{cat: count, ...}, ...] last DIVERSITY_LOOKBACK_TICKS entries
    },
  };
}

function loadCheckpoint(raw) {
  if (!raw || typeof raw !== "object" || raw.version !== CHECKPOINT_VERSION) {
    return emptyCheckpoint();
  }
  const cp = Object.assign(emptyCheckpoint(), raw);
  cp.candidates = raw.candidates || {};
  cp.suppressions = raw.suppressions || {};
  cp.seen_evidence = raw.seen_evidence || {};
  cp.stats = Object.assign(emptyCheckpoint().stats, raw.stats || {});
  cp.stats.category_scored = (raw.stats && raw.stats.category_scored) ? raw.stats.category_scored : {};
  cp.stats.category_scored_history = (raw.stats && Array.isArray(raw.stats.category_scored_history))
    ? raw.stats.category_scored_history : [];
  // P0-E backward-compat defaults
  if (typeof cp.ema_scored !== "number") cp.ema_scored = 0;
  if (typeof cp.saturation_ticks !== "number") cp.saturation_ticks = 0;
  if (typeof cp.fast_fail_ticks !== "number") cp.fast_fail_ticks = 0;
  if (typeof cp.reopt_count !== "number") cp.reopt_count = 0;
  if (typeof cp.diversity_mode_ticks_remaining !== "number") cp.diversity_mode_ticks_remaining = 0;
  if (cp.last_reopt_at === undefined) cp.last_reopt_at = null;
  return cp;
}

function nowIso(now) {
  return typeof now === "string" ? now : now.toISOString ? now.toISOString() : String(now);
}

function dateOf(iso) {
  return iso ? iso.slice(0, 10) : null;
}

function addHours(isoStr, hours) {
  const ms = new Date(isoStr).getTime() + hours * 3600 * 1000;
  return new Date(ms).toISOString();
}

function simpleHash(str) {
  let h = 0;
  for (let i = 0; i < str.length; i++) {
    h = ((h << 5) - h + str.charCodeAt(i)) | 0;
  }
  return (h >>> 0).toString(16);
}

// --- seen_evidence dedup helpers ---

function seenEvidenceTimestamp(entry) {
  if (typeof entry === "string") return entry;
  if (entry && typeof entry === "object") {
    return entry.seen_at || entry.first_seen_at || null;
  }
  return null;
}

function seenEvidenceOccurrences(entry) {
  if (!entry || typeof entry !== "object") return 0;
  const value = Number(entry.occurrences || 0);
  if (!Number.isFinite(value) || value < 0) return 0;
  return Math.floor(value);
}

function isSeenEvidenceFresh(entry, nowStr) {
  const ts = seenEvidenceTimestamp(entry);
  if (!ts) return false;
  return new Date(nowStr).getTime() - new Date(ts).getTime() < SEEN_EVIDENCE_TTL_MS;
}

function evidenceKey(obs) {
  if (obs.evidence_ref) return obs.evidence_ref;
  return `${obs.source || ""}|${obs.category || ""}|${obs.signature || ""}|${simpleHash(String(obs.summary || ""))}`;
}

function seenOccurrences(cp, key, nowStr) {
  const entry = cp.seen_evidence[key];
  if (!isSeenEvidenceFresh(entry, nowStr)) return 0;
  return seenEvidenceOccurrences(entry);
}

function isEvidenceFullySeen(cp, key, nowStr, occurrences) {
  const entry = cp.seen_evidence[key];
  if (!isSeenEvidenceFresh(entry, nowStr)) return false;
  if (typeof entry === "string") return true;
  return seenEvidenceOccurrences(entry) >= occurrences;
}

function markEvidenceSeen(cp, key, nowStr, occurrences) {
  const value = Number(occurrences || 0);
  cp.seen_evidence[key] = {
    seen_at: nowStr,
    occurrences: Number.isFinite(value) && value > 0 ? Math.floor(value) : 0,
  };
}

function pruneSeenEvidence(cp, nowStr) {
  const cutoffMs = new Date(nowStr).getTime() - SEEN_EVIDENCE_TTL_MS;
  // Expire TTL-exceeded entries
  for (const key of Object.keys(cp.seen_evidence)) {
    const seenAt = seenEvidenceTimestamp(cp.seen_evidence[key]);
    if (!seenAt || new Date(seenAt).getTime() < cutoffMs) {
      delete cp.seen_evidence[key];
    }
  }
  // LRU eviction: drop oldest entries if still over cap
  const keys = Object.keys(cp.seen_evidence);
  if (keys.length > SEEN_EVIDENCE_MAX_ENTRIES) {
    keys.sort((a, b) =>
      new Date(seenEvidenceTimestamp(cp.seen_evidence[a])).getTime() -
      new Date(seenEvidenceTimestamp(cp.seen_evidence[b])).getTime()
    );
    for (const key of keys.slice(0, keys.length - SEEN_EVIDENCE_MAX_ENTRIES)) {
      delete cp.seen_evidence[key];
    }
  }
}

// --- P0-E: Saturation detection & re-optimization helpers ---

function updateEmaScored(cp, scoredThisTick) {
  cp.ema_scored = EMA_BETA * cp.ema_scored + (1 - EMA_BETA) * scoredThisTick;
}

function updateFastFailTicks(cp, scoringReport, obsCount) {
  // Fast-fail: all observations were deduped (no new scoring signal at all)
  const allDeduped =
    scoringReport.scored === 0 &&
    scoringReport.deduped > 0 &&
    scoringReport.deduped === obsCount;
  if (allDeduped) {
    cp.fast_fail_ticks++;
  } else {
    cp.fast_fail_ticks = 0;
  }
}

function updateSaturationTicks(cp) {
  if (cp.ema_scored < EMA_SATURATION_THRESHOLD) {
    cp.saturation_ticks++;
  } else {
    cp.saturation_ticks = 0;
  }
}

function shouldTriggerReopt(cp) {
  if (cp.fast_fail_ticks >= FAST_FAIL_TICKS) return { trigger: true, reason: "fast_fail" };
  if (cp.saturation_ticks >= SATURATION_TICKS) return { trigger: true, reason: "ema_saturation" };
  return { trigger: false, reason: null };
}

function partialResetSeenEvidence(cp, nowStr) {
  const cutoff = new Date(new Date(nowStr).getTime() - REOPT_WINDOW_MS).getTime();
  for (const [key, entry] of Object.entries(cp.seen_evidence)) {
    const seenAt = seenEvidenceTimestamp(entry);
    if (!seenAt || new Date(seenAt).getTime() < cutoff) {
      delete cp.seen_evidence[key];
    }
  }
}

function triggerReopt(cp, nowStr) {
  partialResetSeenEvidence(cp, nowStr);
  cp.diversity_mode_ticks_remaining = DIVERSITY_BOOST_TICKS;
  cp.reopt_count++;
  cp.saturation_ticks = 0;
  cp.fast_fail_ticks = 0;
  cp.last_reopt_at = nowStr;
}

// Returns Set of underrepresented categories using last DIVERSITY_LOOKBACK_TICKS tick history.
// A category is underrepresented when its summed scored count is below the per-category average.
function underrepresentedCatsFromHistory(cp) {
  const totals = {};
  for (const cat of KNOWN_CATEGORIES) totals[cat] = 0;
  for (const snapshot of cp.stats.category_scored_history || []) {
    for (const [cat, n] of Object.entries(snapshot)) {
      totals[cat] = (totals[cat] || 0) + n;
    }
  }
  const vals = Object.values(totals);
  const avg = vals.length ? vals.reduce((a, b) => a + b, 0) / vals.length : 0;
  return new Set(Object.entries(totals).filter(([, n]) => n < avg).map(([c]) => c));
}

function normalizeCategory(value) {
  if (typeof value === "string" && KNOWN_CATEGORIES.has(value)) {
    return value;
  }
  return "routine-candidate";
}

function observationOccurrences(obs) {
  const value = Number(obs.occurrences || obs.count || 1);
  if (!Number.isFinite(value) || value < 1) {
    return 1;
  }
  return Math.min(50, Math.floor(value));
}

function observationKey(obs) {
  if (typeof obs.key === "string") return obs.key;
  if (typeof obs.evidence_ref === "string") {
    if (obs.evidence_ref.startsWith("kv_meta:routine_observation:")) {
      return obs.evidence_ref.slice("kv_meta:".length);
    }
    if (obs.evidence_ref.startsWith("routine_observation:")) {
      return obs.evidence_ref;
    }
  }
  return "";
}

function compactText(value, maxChars) {
  return String(value || "")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, maxChars);
}

function topEvidenceForCandidate(candidate, limit) {
  return (candidate.examples || [])
    .slice(-limit)
    .reverse()
    .map((example) => ({
      summary: compactText(example.summary, 140),
      timestamp: example.timestamp || null,
      evidence_ref: example.evidence_ref || null,
      weight: example.weight || 1,
      occurrences: example.occurrences || 1,
    }));
}

function topEvidenceSummaryForCandidate(candidate) {
  const evidence = topEvidenceForCandidate(candidate, 2);
  if (evidence.length === 0) return "핵심 근거 없음";
  return evidence
    .map(
      (item, index) =>
        `${index + 1}) ${item.summary || "요약 없음"} (occurrences=${item.occurrences}, weight=${item.weight})`
    )
    .join(" / ");
}

function topCandidateEvidence(cp, limit) {
  return Object.entries(cp.candidates || {})
    .filter(([, candidate]) => candidate.state === "observing" || candidate.state === "recommended")
    .sort(([, a], [, b]) => (b.score || 0) - (a.score || 0))
    .slice(0, limit)
    .map(([patternId, candidate]) => ({
      pattern_id: patternId,
      score: candidate.score || 0,
      score_delta_last_tick: candidate.score_delta_last_tick || 0,
      evidence_count: candidate.evidence_count || 0,
      latest_evidence: topEvidenceSummaryForCandidate(candidate),
    }));
}

function topCandidateEvidenceSummary(cp) {
  const top = topCandidateEvidence(cp, 3);
  if (top.length === 0) return "관찰 중인 후보 없음";
  return top
    .map(
      (item, index) =>
        `${index + 1}) ${item.pattern_id}: score=${item.score}, delta=${item.score_delta_last_tick}, evidence=${item.evidence_count}, 근거=${item.latest_evidence}`
    )
    .join(" / ");
}

function suppressionSummary(suppressedObservations, droppedCandidates) {
  const parts = [];
  for (const item of (suppressedObservations || []).slice(0, 3)) {
    parts.push(`${item.pattern_id}: ${item.reason}`);
  }
  for (const item of (droppedCandidates || []).slice(0, 3)) {
    parts.push(`${item.pattern_id}: ${item.reason}`);
  }
  return parts.length ? parts.join(" / ") : "중복/억제 후보 없음";
}

function noEscalationReason(cp, nowStr) {
  if (cp.stats.recommendations_today >= DAILY_CAP) {
    return `보류 이유: 일일 추천 한도 ${DAILY_CAP}개에 도달했습니다.`;
  }
  const top = topCandidateEvidence(cp, 1)[0];
  if (!top) {
    return "보류 이유: 관찰된 후보가 없습니다.";
  }
  const candidate = cp.candidates[top.pattern_id] || {};
  if ((candidate.evidence_count || 0) < 5) {
    return `보류 이유: 최상위 후보 ${top.pattern_id}의 근거가 ${candidate.evidence_count || 0}회로 최소 5회 미만입니다.`;
  }
  if ((candidate.score || 0) < SCORE_THRESHOLD) {
    return `보류 이유: 최상위 후보 ${top.pattern_id}의 점수 ${candidate.score || 0}가 기준 ${SCORE_THRESHOLD} 미만입니다.`;
  }
  if (candidate.cooldown_until && candidate.cooldown_until > nowStr) {
    return `보류 이유: 최상위 후보 ${top.pattern_id}가 ${candidate.cooldown_until}까지 쿨다운 중입니다.`;
  }
  return `보류 이유: 최상위 후보 ${top.pattern_id}는 중복 추천 해시 또는 게이트 조건으로 보류됐습니다.`;
}

function utf8CharBytes(ch) {
  const codePoint = ch.codePointAt(0);
  if (codePoint <= 0x7f) return 1;
  if (codePoint <= 0x7ff) return 2;
  if (codePoint <= 0xffff) return 3;
  return 4;
}

function utf8ByteLength(value) {
  let bytes = 0;
  for (const ch of String(value || "")) {
    bytes += utf8CharBytes(ch);
  }
  return bytes;
}

function truncateUtf8(value, maxBytes) {
  if (maxBytes <= 0) return "";
  let bytes = 0;
  let out = "";
  for (const ch of String(value || "")) {
    const nextBytes = utf8CharBytes(ch);
    if (bytes + nextBytes > maxBytes) break;
    out += ch;
    bytes += nextBytes;
  }
  return out;
}

// --- Daily cap reset ---

function resetDailyCapIfNeeded(cp, nowStr) {
  const today = dateOf(nowStr);
  if (cp.stats.recommendation_day !== today) {
    cp.stats.recommendations_today = 0;
    cp.stats.recommendation_day = today;
  }
}

// --- Suppression / inventory filter ---

function hasDurableAcceptance(entry) {
  return Boolean(entry && (entry.automation_ref || entry.source_ref));
}

function buildSuppressedSet(cp, inventory, observations) {
  const exact = new Map();
  const prefixes = [];

  function addPattern(patternId, reason) {
    if (!patternId) return;
    if (patternId.endsWith(":*")) {
      prefixes.push({ prefix: patternId.slice(0, -1), reason });
    } else {
      exact.set(patternId, reason);
    }
  }

  // From checkpoint suppressions
  for (const [patternId, entry] of Object.entries(cp.suppressions || {})) {
    const state = entry.state;
    if (
      (state === "accepted" && hasDurableAcceptance(entry)) ||
      state === "implemented" ||
      state === "suppressed" ||
      state === "rejected"
    ) {
      addPattern(patternId, `체크포인트 억제 상태=${state}`);
    }
  }

  // From automation inventory (exact pattern_id match only)
  for (const item of inventory || []) {
    if (
      item.pattern_id &&
      ((item.status === "accepted" && hasDurableAcceptance(item)) ||
        item.status === "implemented" ||
        item.status === "suppressed" ||
        item.status === "rejected")
    ) {
      addPattern(
        item.pattern_id,
        `자동화 인벤토리 상태=${item.status}${item.source_ref ? ` ref=${item.source_ref}` : ""}`
      );
    }
  }

  // Candidate-level accepted/implemented/suppressed/rejected states
  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    const s = candidate.state;
    if (
      (s === "accepted" && hasDurableAcceptance(candidate)) ||
      s === "implemented" ||
      s === "suppressed" ||
      s === "rejected"
    ) {
      addPattern(patternId, `후보 상태=${s}`);
    }
  }

  // Contract: executor/dispatched markers must suffix the same signature used by obs.signature.
  // candidate_dispatched:* kv_meta observations → suppress re-recommendation (REQ-P1-004)
  for (const obs of observations || []) {
    const key = observationKey(obs);
    if (key.startsWith("routine_observation:candidate_dispatched:")) {
      const signature = key.replace("routine_observation:candidate_dispatched:", "");
      if (signature) addPattern(signature, "dispatched kv_meta 존재 (재추천 차단)");
    }
  }

  return {
    has(patternId) {
      return exact.has(patternId) || prefixes.some((item) => patternId.startsWith(item.prefix));
    },
    reason(patternId) {
      if (exact.has(patternId)) return exact.get(patternId);
      const matched = prefixes.find((item) => patternId.startsWith(item.prefix));
      return matched ? matched.reason : null;
    },
  };
}

function dropSuppressedCandidates(cp, suppressedSet) {
  const dropped = [];
  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    if (suppressedSet.has(patternId)) {
      const s = candidate.state;
      if (
        (s === "accepted" && hasDurableAcceptance(candidate)) ||
        s === "implemented" ||
        s === "suppressed" ||
        s === "rejected"
      ) {
        continue;
      }
      dropped.push({
        pattern_id: patternId,
        state: s,
        reason: suppressedSet.reason(patternId) || "인벤토리/체크포인트 기준으로 억제",
      });
      delete cp.candidates[patternId];
    }
  }

  if (Array.isArray(cp.recommendations)) {
    cp.recommendations = cp.recommendations.filter((item) => !suppressedSet.has(item.pattern_id));
  }

  return dropped.slice(0, 5);
}

// --- Expired suppression cleanup ---

function pruneExpiredSuppressions(cp, nowStr) {
  for (const [patternId, entry] of Object.entries(cp.suppressions || {})) {
    if (entry.expires_at && entry.expires_at < nowStr) {
      delete cp.suppressions[patternId];
      if (cp.candidates[patternId]) {
        cp.candidates[patternId].state = "observing";
      }
    }
  }
}

function expireStaleCandidates(cp, nowStr) {
  const cutoff = new Date(new Date(nowStr).getTime() - CANDIDATE_TTL_DAYS * 24 * 3600 * 1000).toISOString();
  for (const candidate of Object.values(cp.candidates || {})) {
    if (
      candidate.last_seen_at &&
      candidate.last_seen_at < cutoff &&
      (candidate.state === "observing" || candidate.state === "recommended")
    ) {
      candidate.state = "expired";
    }
  }
}

// --- Score observations into candidates ---

function scoreObservations(cp, observations, suppressedSet, nowStr, diversityMode) {
  const thirtyMinAgo = new Date(new Date(nowStr).getTime() - 30 * 60 * 1000).toISOString();
  const report = {
    scored: 0,
    deduped: 0,
    suppressed: [],
    category_scored_this_tick: {},
  };

  // Diversity mode: underrepresented categories from last DIVERSITY_LOOKBACK_TICKS tick history
  const underrepresentedCats = diversityMode ? underrepresentedCatsFromHistory(cp) : null;

  for (const candidate of Object.values(cp.candidates || {})) {
    candidate.score_delta_last_tick = 0;
    candidate.scored_observations_last_tick = 0;
    candidate.last_score_reason = null;
  }

  for (const obs of observations) {
    cp.stats.observations_seen++;

    const patternId = obs.signature;
    if (!patternId) continue;
    if (suppressedSet.has(patternId)) {
      report.suppressed.push({
        pattern_id: patternId,
        reason: suppressedSet.reason(patternId) || "인벤토리/체크포인트 기준으로 억제",
      });
      continue;
    }

    // Cross-tick dedup: skip evidence already scored in a previous tick, but
    // score only the newly increased occurrence count for rolling grouped sources.
    const eKey = evidenceKey(obs);
    const totalOccurrences = observationOccurrences(obs);
    if (isEvidenceFullySeen(cp, eKey, nowStr, totalOccurrences)) {
      markEvidenceSeen(cp, eKey, nowStr, totalOccurrences);
      report.deduped++;
      continue;
    }
    const priorOccurrences = seenOccurrences(cp, eKey, nowStr);
    const occurrences = Math.max(1, totalOccurrences - priorOccurrences);
    markEvidenceSeen(cp, eKey, nowStr, totalOccurrences);

    const category = normalizeCategory(obs.category);
    let candidate = cp.candidates[patternId];
    if (!candidate) {
      candidate = {
        category,
        state: "observing",
        score: WEIGHT_FIRST_SEEN,
        evidence_count: 0,
        first_seen_at: obs.timestamp || nowStr,
        last_seen_at: null,
        examples: [],
        last_recommended_at: null,
        last_recommendation_hash: null,
        cooldown_until: null,
        automation_ref: null,
        has_error_evidence: false,
      };
      cp.candidates[patternId] = candidate;
    } else {
      candidate.category = normalizeCategory(candidate.category || category);
    }

    // Skip if already resolved
    if (candidate.state === "accepted" && !hasDurableAcceptance(candidate)) {
      candidate.state = "recommended";
    }
    if (
      (candidate.state === "accepted" && hasDurableAcceptance(candidate)) ||
      candidate.state === "implemented" ||
      candidate.state === "suppressed" ||
      candidate.state === "rejected"
    ) {
      continue;
    }

    candidate.evidence_count += occurrences;
    candidate.last_seen_at = obs.timestamp || nowStr;

    // Score delta
    const weight = typeof obs.weight === "number" ? obs.weight : 1;
    const scoredOccurrences = Math.min(occurrences, 5);
    let delta = WEIGHT_BASE * weight * scoredOccurrences;
    if (weight === 2) {
      candidate.has_error_evidence = true;
    }

    // Recency bonus
    const recencyBonus = obs.timestamp && obs.timestamp >= thirtyMinAgo ? WEIGHT_RECENCY_BONUS : 0;
    if (obs.timestamp && obs.timestamp >= thirtyMinAgo) {
      delta += recencyBonus;
    }

    // Diversity mode: 1.5× boost for underrepresented categories
    const diversityBoost = (underrepresentedCats && underrepresentedCats.has(category)) ? 1.5 : 1;
    delta = delta * diversityBoost;

    candidate.score = Math.min(100, candidate.score + delta);
    candidate.score_delta_last_tick = (candidate.score_delta_last_tick || 0) + delta;
    candidate.scored_observations_last_tick =
      (candidate.scored_observations_last_tick || 0) + 1;
    candidate.last_score_reason =
      `weight=${weight}, occurrences=${occurrences}, recency_bonus=${recencyBonus}${diversityBoost > 1 ? ", diversity_boost=1.5" : ""}`;
    candidate.last_scored_at = nowStr;
    report.scored++;

    // Track per-category scoring stats (cumulative + per-tick for diversity history)
    cp.stats.category_scored[category] = (cp.stats.category_scored[category] || 0) + 1;
    report.category_scored_this_tick[category] = (report.category_scored_this_tick[category] || 0) + 1;

    // Keep up to 3 examples
    if (candidate.examples.length < MAX_EXAMPLES_PER_CANDIDATE) {
      candidate.examples.push({
        summary: obs.summary,
        timestamp: obs.timestamp,
        evidence_ref: obs.evidence_ref,
        source: obs.source,
        weight,
        occurrences,
      });
    }
  }

  return report;
}

// --- Find best escalation candidate ---

function findEscalationCandidate(cp, nowStr) {
  if (cp.stats.recommendations_today >= DAILY_CAP) {
    return null;
  }

  let best = null;
  let bestScore = SCORE_THRESHOLD - 1;

  for (const [patternId, candidate] of Object.entries(cp.candidates || {})) {
    if (candidate.state !== "observing" && candidate.state !== "recommended") {
      continue;
    }
    if (candidate.score <= bestScore) {
      continue;
    }
    if (candidate.evidence_count < 5) {
      continue;
    }
    if (candidate.cooldown_until && candidate.cooldown_until > nowStr) {
      continue;
    }

    // Dedupe: same hash within cooldown
    const hash = simpleHash(patternId + ":" + candidate.evidence_count);
    if (candidate.last_recommendation_hash === hash) {
      continue;
    }

    bestScore = candidate.score;
    best = { patternId, candidate, hash };
  }

  return best;
}

// --- Mark candidate as recommended ---

function markRecommended(cp, escalation, nowStr) {
  const { patternId, candidate, hash } = escalation;
  const assessment = candidateAssessment(patternId, candidate);
  const decisionSummary = candidateDecisionSummary(patternId, candidate, assessment);
  const topEvidenceSummary = topEvidenceSummaryForCandidate(candidate);
  candidate.state = "recommended";
  candidate.last_recommended_at = nowStr;
  candidate.last_recommendation_hash = hash;
  candidate.cooldown_until = addHours(nowStr, COOLDOWN_HOURS);
  candidate.suggested_automation = assessment.suggestedAutomation;
  candidate.recommended_execution = assessment.recommendedExecution;
  candidate.outcome_summary = assessment.outcomeSummary;
  candidate.before_after = assessment.beforeAfter;
  candidate.expected_files = assessment.expectedFiles;
  candidate.expected_side_effects = assessment.expectedSideEffects;
  candidate.verification_method = assessment.verificationMethod;
  candidate.gated_handoff = assessment.gatedHandoff;
  candidate.materialize_request = assessment.materializeRequest;
  candidate.decision_summary = decisionSummary;
  candidate.top_evidence = topEvidenceForCandidate(candidate, 3);
  candidate.top_evidence_summary = topEvidenceSummary;
  cp.stats.recommendations_today++;
  cp.stats.agent_escalations++;
  cp.recommendations.push({
    pattern_id: patternId,
    recommended_at: nowStr,
    hash,
    score: candidate.score,
    score_delta_last_tick: candidate.score_delta_last_tick || 0,
    evidence_count: candidate.evidence_count,
    outcome_summary: assessment.outcomeSummary,
    decision_summary: decisionSummary,
    top_evidence_summary: topEvidenceSummary,
    materialize_request: assessment.materializeRequest,
  });
  // Keep recommendations list bounded
  if (cp.recommendations.length > 50) {
    cp.recommendations = cp.recommendations.slice(-50);
  }
}

// --- Agent prompt builder ---

function buildOutcomeSummary(patternId, candidate, isErrorPattern, category) {
  const latestExample = (candidate.examples || []).slice(-1)[0] || {};
  const latestSummary = String(latestExample.summary || patternId)
    .replace(/\s+/g, " ")
    .slice(0, 120);
  const count = candidate.evidence_count || observationOccurrences(latestExample) || 0;
  const categoryLabel = {
    "routine-candidate": "루틴 반복",
    "release-freshness": "릴리스 신선도",
    "outbox-delivery": "메시지 발송",
    "memento-hygiene": "메모리 위생",
    "api-friction": "API 마찰",
  }[category] || "루틴 후보";
  const prefix = isErrorPattern || category === "outbox-delivery" || category === "api-friction"
    ? "실패 요약"
    : "성공 요약";
  const action = prefix === "실패 요약"
    ? "자동 복구나 알림 후보입니다"
    : "수동 확인 없이 루틴화할 후보입니다";
  return `${prefix}: ${categoryLabel} 패턴이 ${count}회 반복되어 ${action}. 최근 근거: ${latestSummary}`;
}

function candidateAssessment(patternId, candidate) {
  const isErrorPattern = Boolean(candidate.has_error_evidence) ||
    (candidate.examples || []).some((example) => example.weight === 2);
  const category = normalizeCategory(candidate.category);
  const categoryProfiles = {
    "routine-candidate": {
      suggestedAutomation: isErrorPattern
        ? "반복 실패 루틴에 대한 자동 재시도 또는 알림"
        : "반복 패턴을 자동 처리하는 예약 루틴",
      before: "반복 루틴 근거는 수동 로그 확인 후에만 보입니다.",
      after: "제한된 루틴/규칙이 반복 패턴을 처리하거나 쿨다운을 두고 에스컬레이션합니다.",
      files: ["routines/monitoring/*.js", "src/services/routines/*"],
      sideEffects: "루틴 또는 규칙 경로가 추가될 수 있으므로 쿨다운, 중복 제거, Discord 노이즈를 검증해야 합니다.",
      verification: "대상 루틴 로더 테스트를 실행하고 체크포인트 후보 필드를 확인합니다.",
    },
    "release-freshness": {
      suggestedAutomation: "오래된 배포, 버전, 생성 인벤토리 신호를 감지하는 릴리스 신선도 모니터",
      before: "버전이나 생성 문서가 오래된 뒤에야 사람이 릴리스 드리프트를 발견합니다.",
      after: "신선도 점검이 오래된 릴리스 상태가 누적되기 전에 업데이트 경로를 제안합니다.",
      files: ["scripts/*release*", "src/cli/*", "docs/generated/worker-inventory.md"],
      sideEffects: "읽기 전용 신선도 점검이 추가될 수 있으며 자동 게시, 태깅, 배포는 피해야 합니다.",
      verification: "스크립트 검사와 릴리스 부작용이 없음을 증명하는 신선도 픽스처를 실행합니다.",
    },
    "outbox-delivery": {
      suggestedAutomation: "반복 전송 또는 큐 적재 실패를 감지하는 메시지 아웃박스 전달 모니터",
      before: "전달 실패 반복 패턴을 찾으려면 DB/로그를 사람이 직접 확인해야 합니다.",
      after: "반복 아웃박스 실패가 명확한 전달 수정 경로를 가진 제한된 제안으로 묶입니다.",
      files: ["src/services/message_outbox.rs", "src/services/routines/discord_log.rs", "src/services/discord/*"],
      sideEffects: "알림 재시도 또는 폴백 동작이 바뀔 수 있으므로 중복 제거와 전달 대상을 검증해야 합니다.",
      verification: "아웃박스/루틴 대상 테스트를 실행하고 전달 실패 픽스처를 확인합니다.",
    },
    "memento-hygiene": {
      suggestedAutomation: "반복되는 메모리 품질 또는 라우팅 문제를 요약하는 Memento 위생 다이제스트 모니터",
      before: "메모리 위생 문제는 원문 노트에 흩어져 있어 안전하게 조치하기 어렵습니다.",
      after: "토픽/횟수/최신 예시 다이제스트만 제한된 제안으로 변환합니다.",
      files: ["src/services/memory/*", "src/services/routines/store.rs", "routines/monitoring/*.js"],
      sideEffects: "이 루틴은 원문 메모리 본문을 읽거나 쓰면 안 되며 다이제스트 절단을 검증해야 합니다.",
      verification: "추천기 다이제스트 픽스처를 실행하고 프롬프트에 원문 메모리 본문이 없는지 확인합니다.",
    },
    "api-friction": {
      suggestedAutomation: "반복되는 문서 또는 엔드포인트 워크플로 붕괴를 감지하는 API 마찰 모니터",
      before: "API 마찰이 에이전트 응답에서 반복되지만 통합 개선 제안으로 이어지지 않습니다.",
      after: "반복 마찰 마커가 지문별로 묶이고 문서 및 검증 가이드가 함께 제안됩니다.",
      files: ["src/services/api_friction.rs", "src/server/routes/*", "docs/*"],
      sideEffects: "문서 또는 API 라우팅이 바뀔 수 있으며 DB 직접 우회가 도입되지 않았는지 검증해야 합니다.",
      verification: "API 마찰 파싱 테스트와 대상 루틴 추천기 픽스처를 실행합니다.",
    },
  };
  const profile = categoryProfiles[category] || categoryProfiles["routine-candidate"];
  const recommendedExecution = category === "routine-candidate" && candidate.score < 90
    ? "rule"
    : "agent";
  const title = patternId.replace(/\s+/g, " ").slice(0, 96);
  const allowedWritePaths = candidateProgramAllowedPaths(profile.files);
  return {
    suggestedAutomation: profile.suggestedAutomation,
    recommendedExecution,
    outcomeSummary: buildOutcomeSummary(patternId, candidate, isErrorPattern, category),
    beforeAfter: {
      before: profile.before,
      after: profile.after,
    },
    expectedFiles: profile.files,
    expectedSideEffects: profile.sideEffects,
    verificationMethod: profile.verification,
    gatedHandoff: {
      status: "requires_human_approval",
      kanban_card_draft: {
        title: `[automation-candidate] ${title}`,
        category,
        acceptance: [
          "브랜치/카드 변경 전에 제안 승인이 완료되어야 합니다",
          "루틴은 제한적이고 멱등적으로 유지되어야 합니다",
          "검증 명령 또는 픽스처가 PR/카드에 기록되어야 합니다",
        ],
      },
      pr_draft: {
        title: `자동화 후보 구현: ${title}`,
        body_hint: "Before/After, 예상 파일, 부작용, 검증 근거를 포함합니다.",
      },
      side_effects: "사람이 게이트된 핸드오프를 명시적으로 승인하기 전까지는 없음",
    },
    materializeRequest: {
      endpoint: "POST /api/automation-candidates",
      body: {
        title: `[automation-candidate] ${title}`,
        source: "routine_recommender",
        dedupe_key: patternId,
        start_ready: false,
        program: {
          repo_dir: "<required: absolute repo path>",
          allowed_write_paths: allowedWritePaths,
          metric_name: candidateMetricName(category),
          metric_target: 0,
          metric_direction: "lower_is_better",
          final_gate: "manual_review",
          iteration_budget: 3,
        },
      },
      needs_human_fields: ["program.repo_dir"],
    },
  };
}

function candidateProgramAllowedPaths(files) {
  const out = [];
  for (const file of files || []) {
    let path = String(file || "").trim();
    if (!path || path.startsWith("/") || path.includes("..")) continue;
    const star = path.indexOf("*");
    if (star >= 0) path = path.slice(0, star);
    path = path.replace(/\/+$/, "");
    if (!path) continue;
    if (!out.includes(path)) out.push(path);
  }
  return out.length ? out : ["src"];
}

function candidateMetricName(category) {
  return {
    "routine-candidate": "manual_routine_touch_count",
    "release-freshness": "release_drift_count",
    "outbox-delivery": "outbox_delivery_failure_count",
    "memento-hygiene": "memento_hygiene_issue_count",
    "api-friction": "api_friction_count",
  }[category] || "automation_friction_count";
}

function candidateDecisionSummary(patternId, candidate, assessment) {
  const score = candidate.score || 0;
  const delta = candidate.score_delta_last_tick || 0;
  const evidenceCount = candidate.evidence_count || 0;
  const evidenceSummary = topEvidenceSummaryForCandidate(candidate);
  return `선택 이유: ${patternId} 후보가 score=${score}, delta=${delta}, evidence=${evidenceCount}로 기준을 충족했습니다. ${assessment.outcomeSummary} 핵심 근거: ${evidenceSummary}`;
}

function convergenceSummary(candidate) {
  if (!candidate.last_recommended_at && !candidate.last_recommendation_hash) {
    return "이 후보는 이전 추천 이력이 없습니다. 현재 tick의 새 근거를 중심으로 평가합니다.";
  }
  const parts = [];
  if (candidate.last_recommended_at) {
    parts.push(`이전 추천 시각=${candidate.last_recommended_at}`);
  }
  if (candidate.last_recommendation_hash) {
    parts.push(`이전 추천 해시=${candidate.last_recommendation_hash}`);
  }
  if (candidate.cooldown_until) {
    parts.push(`쿨다운 종료=${candidate.cooldown_until}`);
  }
  return `이 후보는 이전 추천/체크포인트 이력이 있습니다. ${parts.join(", ")}`;
}

function buildPrompt(escalation) {
  const { patternId, candidate } = escalation;
  const evidenceLines = (candidate.examples || [])
    .map((ex, i) => `${i + 1}. [${ex.timestamp || "?"}] ${ex.summary || ""} (occurrences=${ex.occurrences || 1})`)
    .join("\n");

  const {
    suggestedAutomation,
    recommendedExecution,
    beforeAfter,
    expectedFiles,
    expectedSideEffects,
    verificationMethod,
    outcomeSummary,
    gatedHandoff,
    materializeRequest,
  } = candidateAssessment(patternId, candidate);
  const decisionSummary = candidateDecisionSummary(patternId, candidate, {
    outcomeSummary,
  });
  const handoffAcceptance = (gatedHandoff.kanban_card_draft.acceptance || [])
    .map((item) => `- ${item}`)
    .join("\n");

  const raw = `# 자동화 후보 추천

패턴: ${patternId}
카테고리: ${normalizeCategory(candidate.category)}
점수: ${candidate.score}/100
근거: ${candidate.evidence_count}회 발생 (최초: ${candidate.first_seen_at || "?"}, 최신: ${candidate.last_seen_at || "?"})

## 근거 예시
${evidenceLines || "(기록 없음)"}

## 성공/실패 한 줄 요약
${outcomeSummary}

## 선택 판단 근거
${decisionSummary}

## 루트 기반 JS 자동화 패턴 탐지 가이드
- 이 제안은 runtime이 제공한 bounded observation, checkpoint, automation inventory만 근거로 판단합니다.
- 같은 증상이 아니라 같은 루트 원인 또는 같은 수동 작업이 반복되는지 pattern/category/count/first-last/example을 연결해 설명합니다.
- 단순 빈도만 보지 말고 최근성, 실패 weight, 운영 ROI, 부작용, 이미 구현/억제된 자동화와의 중복 가능성을 함께 평가합니다.
- 규칙으로 충분한 deterministic retry/check/threshold인지, 문맥 판단과 코드 변경 설계가 필요한 agent 주도 자동화인지 구분합니다.
- 근거가 부족하거나 오탐 가능성이 높으면 자동화 보류로 결론을 내립니다.

## 이전 작업/체크포인트 수렴 대응
${convergenceSummary(candidate)}
- 이전 추천, checkpoint, inventory를 보고 같은 결론에 수렴하더라도 "추가 개선 없음"으로만 끝내지 않습니다.
- 반복 제안이 되지 않게 새 근거, 새 실패 양상, 새 검증 경로, 기존 자동화의 누락 지점 중 하나를 반드시 확인합니다.
- 목표 달성을 위해 대체 탐색 경로를 최소 1개 제안합니다: observation 스키마 보강, 로그 수집 위치 변경, suppression/inventory 오탐 점검, rule 기반 작은 자동화, agent 주도 핸드오프 분리, 검증 metric 추가.
- 여전히 보류라면 다음 tick에서 어떤 근거를 더 수집해야 하는지 한 줄로 남깁니다.

## 이미 자동화됨 판단 기준
- automation inventory, checkpoint suppression, candidate state 중 하나가 같은 pattern_id 또는 wildcard prefix와 매칭되어야 합니다.
- status가 implemented/suppressed/rejected이면 이미 처리된 것으로 봅니다.
- status가 accepted인 경우에는 automation_ref 또는 source_ref 같은 지속 증거(durable proof)가 있을 때만 이미 자동화된 것으로 봅니다.
- 지속 증거가 없는 accepted 또는 단순 이전 추천 이력은 자동화 완료가 아니라 "이전 제안"으로만 봅니다.
- 이름이 비슷한 정도로 중복 처리하지 말고 pattern/category/evidence_ref가 같은 루트 원인이나 같은 반복 수동 작업을 가리키는지 확인합니다.

## 자료 범위 및 검색 정책
- 기본 판단 근거는 PostgreSQL-backed routine observation/checkpoint/inventory, 로그 기반 observation, memento-mcp digest observation입니다.
- 유저 프롬프트와 메모리는 runtime이 구조화 observation 또는 승인된 digest로 제공한 범위만 사용합니다.
- 외부 웹자료 검색은 기본 동작이 아닙니다. 최신 외부 문서/버전 확인이 꼭 필요하면 opt-in evidence provider 필요성을 제안만 합니다.
- 이 프롬프트 안에서 임의 웹 검색, 지속 크롤링, memento 쓰기, DB 직접 우회는 수행하지 않습니다.

## Before / After
- Before: ${beforeAfter.before}
- After: ${beforeAfter.after}

## 예상 구현 파일
${expectedFiles.map((file) => `- ${file}`).join("\n")}

## 판단
- 제안 자동화: ${suggestedAutomation}
- 권장 실행 방식: ${recommendedExecution} (규칙 기반 vs 에이전트 주도)
- 예상 부작용: ${expectedSideEffects}

## 검증 방법
${verificationMethod}

## 게이트된 핸드오프 초안
- 상태: ${gatedHandoff.status}
- Kanban 제목: ${gatedHandoff.kanban_card_draft.title}
- PR 제목: ${gatedHandoff.pr_draft.title}
- 핸드오프 부작용: ${gatedHandoff.side_effects}
${handoffAcceptance}

## 자동화 후보 카드 생성 초안
아래 JSON은 탐지기(detector)가 자동으로 카드를 생성할 때 사용합니다.
\`program.repo_dir\`에 현재 워크스페이스의 실제 절대 경로를 채워 kv_meta를 기록해주세요.
${JSON.stringify(materializeRequest, null, 2)}

## 지시사항
에이전트가 도출한 내용은 반드시 한국어로 작성합니다. 이 자동화를 구현할 가치가 있는지 평가하고 다음을 제공합니다:
1. 자동화 여부(예 / 아니오 / 보류), 신뢰도, 그리고 이유
2. 루트 원인 또는 반복 수동 작업 가설과 그 근거
3. 구현한다면 제안 구현 방식, rule-vs-agent 선택 이유, 영향 파일/루틴
4. 성공/실패에 대한 한 줄 요약
5. 예상 부작용, 오탐/중복 억제 방법, 자동화 동작 검증 방법
6. 이전 추천과 같은 결론이면 다른 탐색/진행 방식 또는 추가로 수집할 근거

## kv_meta 기록 지침

평가 완료 후 **자동화 가치가 있다(예 / 부분 예)** 고 판단한 경우에만 아래를 실행합니다.

**key** (TTL: 48h):
\`routine_observation:candidate_review:${patternId}\`

**value** (JSON):
\`\`\`json
{
  "score": ${candidate.score},
  "evidence_count": ${candidate.evidence_count || 0},
  "last_seen_at": "${candidate.last_seen_at || ""}",
  "category": "${normalizeCategory(candidate.category)}",
  "suggested_automation": "<평가에서 도출한 제안 자동화 내용>",
  "outcome_summary": "<평가에서 도출한 한 줄 결과 요약>",
  "materialize_request": <위 '자동화 후보 카드 생성 초안' JSON에서 program.repo_dir을 실제 절대 경로로 채운 버전>
}
\`\`\`

\`program.repo_dir\`는 현재 워크스페이스에서 확인한 agentdesk 리포지터리 절대 경로로 채웁니다.
자동화 가치가 없거나 보류라면 kv_meta를 기록하지 않습니다.
구현, 파일 수정, 서비스 재시작, memento 쓰기, PR/카드/이슈 생성은 금지합니다.`;

  if (utf8ByteLength(raw) <= PROMPT_CAP_BYTES) {
    return raw;
  }

  // Trim examples first while preserving policy and decision sections.
  const evidenceHeader = "## 근거 예시\n";
  const [header, restWithEvidence] = raw.split(evidenceHeader);
  const evidenceBlock = evidenceLines || "(기록 없음)";
  const rest = restWithEvidence.slice(evidenceBlock.length);
  const budget =
    PROMPT_CAP_BYTES -
    utf8ByteLength(header) -
    utf8ByteLength(evidenceHeader) -
    utf8ByteLength(rest) -
    20;
  const trimmedEvidence =
    budget > 32
      ? truncateUtf8(evidenceBlock, budget)
      : "(근거 예시는 크기 제한으로 생략됨)";
  return header + evidenceHeader + trimmedEvidence + rest;
}

// --- Checkpoint size guard ---

function guardCheckpointSize(cp) {
  const json = JSON.stringify(cp);
  if (json.length <= CHECKPOINT_CAP_BYTES) return cp;

  // Prune least-recently-observed candidates first.
  const entries = Object.entries(cp.candidates).sort(
    ([, a], [, b]) => String(a.last_seen_at || "").localeCompare(String(b.last_seen_at || ""))
  );
  let pruned = 0;
  for (const [patternId] of entries) {
    if (JSON.stringify(cp).length <= CHECKPOINT_CAP_BYTES) break;
    if (
      cp.candidates[patternId].state === "observing"
    ) {
      delete cp.candidates[patternId];
      pruned++;
    }
  }

  // Trim examples on remaining candidates
  for (const candidate of Object.values(cp.candidates)) {
    if (candidate.examples && candidate.examples.length > 1) {
      candidate.examples = candidate.examples.slice(-1);
    }
  }

  return cp;
}

// --- Main tick ---

agentdesk.routines.register({
  name: "Automation Candidate Recommender",

  tick(ctx) {
    const nowStr = nowIso(ctx.now);
    const cp = loadCheckpoint(ctx.checkpoint);
    const observations = ctx.observations || [];
    const inventory = ctx.automationInventory || [];

    resetDailyCapIfNeeded(cp, nowStr);
    pruneExpiredSuppressions(cp, nowStr);
    pruneSeenEvidence(cp, nowStr);
    expireStaleCandidates(cp, nowStr);

    const suppressedSet = buildSuppressedSet(cp, inventory, observations);
    const droppedCandidates = dropSuppressedCandidates(cp, suppressedSet);
    const diversityMode = cp.diversity_mode_ticks_remaining > 0;
    const scoringReport = scoreObservations(cp, observations, suppressedSet, nowStr, diversityMode);

    // Update category scored history for diversity mode (keep last DIVERSITY_LOOKBACK_TICKS entries)
    cp.stats.category_scored_history.push(scoringReport.category_scored_this_tick);
    if (cp.stats.category_scored_history.length > DIVERSITY_LOOKBACK_TICKS) {
      cp.stats.category_scored_history.shift();
    }

    // P0-E: saturation detection & re-optimization
    updateEmaScored(cp, scoringReport.scored);
    updateFastFailTicks(cp, scoringReport, observations.length);
    updateSaturationTicks(cp);
    const reoptCheck = shouldTriggerReopt(cp);
    if (reoptCheck.trigger) {
      triggerReopt(cp, nowStr);
    } else if (cp.diversity_mode_ticks_remaining > 0) {
      cp.diversity_mode_ticks_remaining--;
    }

    cp.stats.ticks++;
    cp.last_tick_at = nowStr;

    const escalation = findEscalationCandidate(cp, nowStr);

    if (!escalation) {
      const activeCandidates = Object.values(cp.candidates).filter(
        (c) => c.state === "observing" || c.state === "recommended"
      ).length;
      const summary = `관찰=${observations.length}, 후보=${activeCandidates}, 오늘 추천=${cp.stats.recommendations_today}`;
      const outcomeSummary = `성공 요약: 새 자동화 추천 후보 없음 (${summary})`;
      const decisionSummary = noEscalationReason(cp, nowStr);
      const topEvidenceSummary = topCandidateEvidenceSummary(cp);
      const suppressedSummary = suppressionSummary(scoringReport.suppressed, droppedCandidates);
      const scoringSummary = [
        `scored=${scoringReport.scored}`,
        `deduped=${scoringReport.deduped}`,
        `suppressed=${scoringReport.suppressed.length + droppedCandidates.length}`,
        `ema_scored=${cp.ema_scored.toFixed(3)}`,
        `saturation_ticks=${cp.saturation_ticks}`,
        `fast_fail_ticks=${cp.fast_fail_ticks}`,
        `reopt_count=${cp.reopt_count}`,
        diversityMode ? `diversity_mode_remaining=${cp.diversity_mode_ticks_remaining}` : null,
        reoptCheck.trigger ? `reopt_triggered=${reoptCheck.reason}` : null,
      ].filter(Boolean).join(", ");
      return {
        action: "complete",
        result: {
          status: "ok",
          summary,
          outcome_summary: outcomeSummary,
          decision_summary: decisionSummary,
          top_evidence_summary: topEvidenceSummary,
          suppression_summary: suppressedSummary,
          scoring_summary: scoringSummary,
          observation_count: observations.length,
          active_candidate_count: activeCandidates,
          recommendations_today: cp.stats.recommendations_today,
          reopt_count: cp.reopt_count,
        },
        checkpoint: guardCheckpointSize(cp),
        lastResult: outcomeSummary,
      };
    }

    const prompt = buildPrompt(escalation);
    markRecommended(cp, escalation, nowStr);

    return {
      action: "agent",
      prompt,
      checkpoint: guardCheckpointSize(cp),
    };
  },
});
